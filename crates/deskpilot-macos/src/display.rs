//! Displays: CoreGraphics for geometry (thread-safe), NSScreen (main thread)
//! for names, work areas and backing scale.
use deskpilot_core::model::{MonitorInfo, Rect};
use deskpilot_core::{Error, Result};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMirrorsDisplay,
    CGDisplayMode, CGDisplayPixelsWide, CGError, CGGetOnlineDisplayList, CGMainDisplayID,
};
use objc2_foundation::{NSNumber, NSString};

use crate::main_thread::on_main;

/// What NSScreen adds on top of CoreGraphics, per display.
struct ScreenExtra {
    id: CGDirectDisplayID,
    name: String,
    /// Cocoa coordinates: bottom-left origin of the primary display, y up.
    visible: CGRect,
    scale: f64,
}

pub fn monitors() -> Result<Vec<MonitorInfo>> {
    let ids = active_displays()?;
    let main = CGMainDisplayID();
    let primary_height = CGDisplayBounds(main).size.height;
    let extras = on_main(|mtm| {
        autoreleasepool(|_| {
            let key = NSString::from_str("NSScreenNumber");
            NSScreen::screens(mtm)
                .iter()
                .filter_map(|screen| {
                    let number = screen.deviceDescription().objectForKey(&key)?;
                    let id = number.downcast::<NSNumber>().ok()?.unsignedIntValue();
                    Some(ScreenExtra {
                        id,
                        name: screen.localizedName().to_string(),
                        visible: screen.visibleFrame(),
                        scale: screen.backingScaleFactor(),
                    })
                })
                .collect::<Vec<_>>()
        })
    });
    Ok(ids
        .into_iter()
        .map(|id| {
            let bounds = CGDisplayBounds(id);
            let extra = extras.iter().find(|e| e.id == id);
            let work_area = extra.map_or(bounds, |e| CGRect {
                origin: objc2_core_foundation::CGPoint {
                    x: e.visible.origin.x,
                    y: primary_height - (e.visible.origin.y + e.visible.size.height),
                },
                size: e.visible.size,
            });
            MonitorInfo {
                id: id.to_string(),
                name: extra.map_or_else(|| format!("Display {id}"), |e| e.name.clone()),
                bounds: rect_out(bounds),
                work_area: rect_out(work_area),
                scale: extra
                    .map(|e| e.scale)
                    .filter(|s| *s > 0.0)
                    .unwrap_or_else(|| pixel_scale(id, bounds)),
                primary: id == main,
            }
        })
        .collect())
}

/// The display containing most of `r`: (display id, bounds in points, pixel scale).
/// Uses CoreGraphics only, so it is callable from any thread.
pub(crate) fn display_for_rect(r: Rect) -> Option<(u32, Rect, f64)> {
    let ids = active_displays().ok()?;
    let main = CGDisplayBounds(CGMainDisplayID());
    let best = ids
        .into_iter()
        .map(|id| (id, CGDisplayBounds(id)))
        .max_by_key(|(_, b)| {
            rect_out(*b)
                .intersect(&r)
                .map_or(0i64, |i| i64::from(i.width) * i64::from(i.height))
        })?;
    // A rect touching no display (e.g. empty) falls back to the main display.
    let (id, bounds) = if rect_out(best.1).intersect(&r).is_some() {
        best
    } else {
        (CGMainDisplayID(), main)
    };
    Some((id, rect_out(bounds), pixel_scale(id, bounds)))
}

/// Active displays, each with its bounds in points and pixel scale.
pub(crate) fn displays() -> Result<Vec<(u32, Rect, f64)>> {
    Ok(active_displays()?
        .into_iter()
        .map(|id| {
            let b = CGDisplayBounds(id);
            (id, rect_out(b), pixel_scale(id, b))
        })
        .collect())
}

/// Displays that make up the desktop. The online list (not the active one)
/// keeps asleep displays, whose geometry still holds windows; mirror
/// secondaries are dropped because they duplicate their source.
fn active_displays() -> Result<Vec<CGDirectDisplayID>> {
    let mut ids = [0 as CGDirectDisplayID; 32];
    let mut count = 0u32;
    // SAFETY: `ids` holds 32 entries and `count` is a valid out pointer.
    let err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        return Err(Error::Platform(format!(
            "CGGetOnlineDisplayList failed ({})",
            err.0
        )));
    }
    Ok(ids[..count as usize]
        .iter()
        .copied()
        .filter(|&id| CGDisplayMirrorsDisplay(id) == 0)
        .collect())
}

/// Image pixels per point: native mode pixel width over its point width.
fn pixel_scale(id: CGDirectDisplayID, bounds: CGRect) -> f64 {
    let from_mode = CGDisplayCopyDisplayMode(id).and_then(|mode| {
        let points = CGDisplayMode::width(Some(&mode));
        let pixels = CGDisplayMode::pixel_width(Some(&mode));
        (points > 0 && pixels > 0).then(|| pixels as f64 / points as f64)
    });
    from_mode.unwrap_or_else(|| {
        let pixels = CGDisplayPixelsWide(id) as f64;
        if bounds.size.width > 0.0 && pixels > 0.0 {
            pixels / bounds.size.width
        } else {
            1.0
        }
    })
}

/// CGRect (points, f64) → integer Rect, rounded outward.
pub(crate) fn rect_out(r: CGRect) -> Rect {
    let x1 = r.origin.x.floor();
    let y1 = r.origin.y.floor();
    let x2 = (r.origin.x + r.size.width).ceil();
    let y2 = (r.origin.y + r.size.height).ceil();
    Rect::new(x1 as i32, y1 as i32, (x2 - x1) as i32, (y2 - y1) as i32)
}
