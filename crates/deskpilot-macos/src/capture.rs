//! ScreenCaptureKit captures (macOS 14+: `SCScreenshotManager`).
//!
//! Completion handlers run on an SCK-internal queue, so blocking the calling
//! thread (main or not) on a channel is safe.
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use deskpilot_core::model::{Capture, Point, Rect, WindowId};
use deskpilot_core::{Error, Result};
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDisplayIsAsleep, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo, CGPreflightScreenCaptureAccess, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};

use crate::display::{self, rect_out};

const TIMEOUT: Duration = Duration::from_secs(5);
const PERMISSION: &str = "Screen Recording permission is missing: System Settings → Privacy & Security → Screen & System Audio Recording";
/// `SCStreamErrorUserDeclined`.
const USER_DECLINED: isize = -3801;

/// Moves an Objective-C object out of a completion handler. The objects we
/// send (SCShareableContent and its children) are immutable snapshots.
struct Sendable<T>(T);
// SAFETY: see above; the value is only touched by the receiving thread.
unsafe impl<T> Send for Sendable<T> {}

pub fn screen(r: Rect) -> Result<Capture> {
    if r.is_empty() {
        return Err(Error::InvalidArgument(format!(
            "capture rectangle {r:?} is empty"
        )));
    }
    autoreleasepool(|_| {
        let parts: Vec<(u32, Rect, Rect)> = display::displays()?
            .into_iter()
            .filter_map(|(id, bounds, _)| r.intersect(&bounds).map(|part| (id, bounds, part)))
            .collect();
        if parts.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "capture rectangle {r:?} lies outside every display"
            )));
        }
        // Mixed-DPI spans are composited at the scale of the display that
        // holds most of the rectangle; other parts are resampled to it.
        let scale = display::display_for_rect(r).map_or(1.0, |(_, _, s)| s);
        let width = px(r.width, scale);
        let height = px(r.height, scale);

        let content = shareable_content(true, true)?;
        let displays = unsafe { content.displays() };
        let mut canvas = Canvas::new(width, height)?;
        for (id, bounds, part) in parts {
            let display = displays
                .iter()
                .find(|d| unsafe { d.displayID() } == id)
                .ok_or_else(|| {
                    Error::Platform(if CGDisplayIsAsleep(id) {
                        format!("display {id} is asleep; ScreenCaptureKit cannot capture it")
                    } else {
                        format!("display {id} is not offered by ScreenCaptureKit")
                    })
                })?;
            // Pixel span of this part inside the output image.
            let edge = |points: i32, limit: u32| {
                ((f64::from(points) * scale).round().max(0.0) as u32).min(limit) as usize
            };
            let (x0, y0) = (edge(part.x - r.x, width), edge(part.y - r.y, height));
            let x1 = edge(part.x + part.width - r.x, width);
            let y1 = edge(part.y + part.height - r.y, height);
            let (pw, ph) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
            let image = capture_display(&display, bounds, part, pw, ph)?;
            canvas.draw(&image, x0, y0, pw, ph);
        }
        Ok(canvas.into_capture(Point { x: r.x, y: r.y }, scale, false))
    })
}

pub fn window(id: &WindowId) -> Result<Capture> {
    let number = crate::window_number(id)?;
    autoreleasepool(|_| {
        let content = shareable_content(false, false)?;
        let windows = unsafe { content.windows() };
        let window: Retained<SCWindow> = windows
            .iter()
            .find(|w| unsafe { w.windowID() } == number)
            .ok_or_else(|| Error::NotFound(format!("window {id}")))?;
        let frame = unsafe { window.frame() };
        let bounds = rect_out(frame);
        if bounds.is_empty() {
            return Err(Error::NotFound(format!("window {id} has no visible area")));
        }
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
        };
        let scale = match f64::from(unsafe { filter.pointPixelScale() }) {
            s if s > 0.0 => s,
            _ => display::display_for_rect(bounds).map_or(1.0, |(_, _, s)| s),
        };
        let width = px(bounds.width, scale);
        let height = px(bounds.height, scale);
        let config = configuration(width as usize, height as usize);
        unsafe { config.setIgnoreShadowsSingleWindow(true) };
        let image = screenshot(&filter, &config)?;
        let mut canvas = Canvas::new(width, height)?;
        canvas.draw(&image, 0, 0, width as usize, height as usize);
        Ok(canvas.into_capture(
            Point {
                x: bounds.x,
                y: bounds.y,
            },
            scale,
            true,
        ))
    })
}

/// Points → pixels at `scale`, at least one pixel.
fn px(points: i32, scale: f64) -> u32 {
    ((f64::from(points) * scale).round() as i64).max(1) as u32
}

fn capture_display(
    display: &SCDisplay,
    bounds: Rect,
    part: Rect,
    width: usize,
    height: usize,
) -> Result<CFRetained<CGImage>> {
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::new(),
        )
    };
    let config = configuration(width, height);
    // sourceRect is in display-local points, top-left origin.
    unsafe {
        config.setSourceRect(CGRect {
            origin: CGPoint {
                x: f64::from(part.x - bounds.x),
                y: f64::from(part.y - bounds.y),
            },
            size: CGSize {
                width: f64::from(part.width),
                height: f64::from(part.height),
            },
        })
    };
    screenshot(&filter, &config)
}

fn configuration(width: usize, height: usize) -> Retained<SCStreamConfiguration> {
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setShowsCursor(false);
        config.setCapturesAudio(false);
        config.setColorSpaceName(kCGColorSpaceSRGB);
    }
    config
}

fn sck_error(err: &NSError, what: &str) -> Error {
    let domain = err.domain().to_string();
    // Without the TCC grant SCK reports userDeclined, but some OS versions
    // surface other codes; the (non-prompting) preflight disambiguates.
    if (domain == "com.apple.ScreenCaptureKit.SCStreamErrorDomain" && err.code() == USER_DECLINED)
        || !CGPreflightScreenCaptureAccess()
    {
        if crate::permissions::screen_recording_needs_restart() {
            return Error::PermissionRequired(
                "Screen Recording was granted after this deskpilot process started; macOS applies it to new processes only. Restart the MCP server (or rerun the command).".into(),
            );
        }
        return Error::PermissionRequired(PERMISSION.into());
    }
    Error::Platform(format!(
        "{what}: {} ({domain} {})",
        err.localizedDescription(),
        err.code()
    ))
}

fn wait<T>(rx: &mpsc::Receiver<Result<T>>, what: &str) -> Result<T> {
    match rx.recv_timeout(TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout(format!(
            "{what} did not complete within {} s",
            TIMEOUT.as_secs()
        ))),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Platform(format!(
            "{what} dropped its completion handler"
        ))),
    }
}

fn shareable_content(
    exclude_desktop: bool,
    on_screen_only: bool,
) -> Result<Retained<SCShareableContent>> {
    const WHAT: &str = "SCShareableContent";
    let (tx, rx) = mpsc::channel::<Result<Sendable<Retained<SCShareableContent>>>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: the handler's arguments are valid (or nil) for its duration.
        let result = match unsafe { (Retained::retain(content), Retained::retain(err)) } {
            (Some(content), _) => Ok(Sendable(content)),
            (None, Some(err)) => Err(sck_error(&err, WHAT)),
            (None, None) => Err(Error::Platform(format!("{WHAT} returned nothing"))),
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            exclude_desktop,
            on_screen_only,
            &block,
        )
    };
    wait(&rx, WHAT).map(|s| s.0)
}

fn screenshot(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> Result<CFRetained<CGImage>> {
    const WHAT: &str = "SCScreenshotManager capture";
    let (tx, rx) = mpsc::channel::<Result<CFRetained<CGImage>>>();
    let block = RcBlock::new(move |image: *mut CGImage, err: *mut NSError| {
        let result = match (NonNull::new(image), unsafe { Retained::retain(err) }) {
            // SAFETY: a non-null CGImage handed to the handler is valid.
            (Some(image), _) => Ok(unsafe { CFRetained::retain(image) }),
            (None, Some(err)) => Err(sck_error(&err, WHAT)),
            (None, None) => Err(Error::Platform(format!("{WHAT} returned no image"))),
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&block),
        )
    };
    wait(&rx, WHAT)
}

/// Opaque RGBA8 output image backed by a CG bitmap context.
struct Canvas {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    context: CFRetained<CGContext>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Result<Self> {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .ok_or_else(|| Error::Platform("CGColorSpaceCreateWithName(sRGB) failed".into()))?;
        // SAFETY: `rgba` outlives the context (both live in `Canvas`, and the
        // heap buffer does not move when the Vec is moved).
        let context = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                width as usize,
                height as usize,
                8,
                width as usize * 4,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }
        .ok_or_else(|| Error::Platform(format!("CGBitmapContextCreate {width}x{height} failed")))?;
        Ok(Self {
            width,
            height,
            rgba,
            context,
        })
    }

    /// Draws `image` scaled into the top-left-origin pixel rect (x, y, w, h).
    fn draw(&mut self, image: &CGImage, x: usize, y: usize, w: usize, h: usize) {
        // CG bitmap contexts have a bottom-left origin.
        let rect = CGRect {
            origin: CGPoint {
                x: x as f64,
                y: self.height as f64 - (y + h) as f64,
            },
            size: CGSize {
                width: w as f64,
                height: h as f64,
            },
        };
        CGContext::draw_image(Some(&self.context), rect, Some(image));
    }

    fn into_capture(self, origin: Point, pixel_scale: f64, occlusion_free: bool) -> Capture {
        let Canvas {
            width,
            height,
            mut rgba,
            context,
        } = self;
        drop(context);
        // Screen pixels are opaque; areas outside any display stay black.
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Capture {
            width,
            height,
            rgba,
            origin,
            pixel_scale,
            occlusion_free,
        }
    }
}
