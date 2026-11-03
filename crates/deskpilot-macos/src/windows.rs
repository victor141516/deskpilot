//! Window discovery (CGWindowList) and window operations (AX).
//!
//! CGWindowList is the source of truth for ids, z-order, geometry and owner;
//! AX adds what CoreGraphics does not know (minimized, full screen, subrole,
//! titles when Screen Recording is not granted, the focused window).
use std::collections::HashMap;

use deskpilot_core::model::{Point, WindowId, WindowInfo};
use deskpilot_core::platform::WindowOp;
use deskpilot_core::{Error, Result};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGError, CGGetDisplaysWithPoint, CGRectMakeWithDictionaryRepresentation,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
};

use crate::ax;
use crate::cf::{self, dict_get};
use crate::display::rect_out;
use crate::{window_id, window_number};

/// One `CGWindowListCopyWindowInfo` entry.
struct Raw {
    id: u32,
    pid: i32,
    layer: i64,
    /// `None` when the key is absent (no Screen Recording permission).
    name: Option<String>,
    owner: String,
    bounds: CGRect,
    onscreen: bool,
}

impl Raw {
    fn has_area(&self) -> bool {
        self.bounds.size.width > 0.0 && self.bounds.size.height > 0.0
    }

    /// Normal application window currently on screen.
    fn normal_onscreen(&self) -> bool {
        self.layer == 0 && self.onscreen && self.has_area()
    }
}

fn raw_list(option: CGWindowListOption, relative_to: u32) -> Vec<Raw> {
    let Some(list) = CGWindowListCopyWindowInfo(option, relative_to) else {
        return Vec::new();
    };
    cf::array(&list)
        .iter()
        .filter_map(|item| {
            let d = item.downcast_ref::<CFDictionary>()?;
            let num = |k: &str| dict_get(d, k).and_then(|v| cf::number(&v));
            let text = |k: &str| dict_get(d, k).and_then(|v| cf::string(&v));
            let mut bounds = CGRect::default();
            if let Some(b) = dict_get(d, "kCGWindowBounds")
                && let Some(b) = b.downcast_ref::<CFDictionary>()
            {
                // SAFETY: `b` is the bounds dictionary and `bounds` a valid out pointer.
                unsafe { CGRectMakeWithDictionaryRepresentation(Some(b), &mut bounds) };
            }
            Some(Raw {
                id: num("kCGWindowNumber")? as u32,
                pid: num("kCGWindowOwnerPID").unwrap_or(0.0) as i32,
                layer: num("kCGWindowLayer").unwrap_or(0.0) as i64,
                name: text("kCGWindowName"),
                owner: text("kCGWindowOwnerName").unwrap_or_default(),
                bounds,
                onscreen: dict_get(d, "kCGWindowIsOnscreen")
                    .and_then(|v| cf::boolean(&v))
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// AX window elements of an application, keyed by CGWindowID. Sheets (save
/// dialogs attached to a window) have their own CGWindowID but appear only
/// among their parent window's children.
fn app_windows(pid: i32) -> std::result::Result<Vec<(u32, CFRetained<AXUIElement>)>, AXError> {
    let app = ax::app_element(pid);
    let Some(list) = ax::get(&app, "AXWindows")? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for w in cf::array(&list) {
        let Ok(w) = w.downcast::<AXUIElement>() else {
            continue;
        };
        if let Some(children) = ax::get(&w, "AXChildren").ok().flatten() {
            for c in cf::array(&children) {
                if let Ok(c) = c.downcast::<AXUIElement>()
                    && ax::attr(&c, "AXRole").and_then(|r| cf::string(&r)).as_deref()
                        == Some("AXSheet")
                    && let Some(n) = ax::window_number_of(&c)
                {
                    out.push((n, c));
                }
            }
        }
        if let Some(n) = ax::window_number_of(&w) {
            out.push((n, w));
        }
    }
    Ok(out)
}

/// AX element of a window plus its owner pid.
pub(crate) fn ax_window(id: &WindowId) -> Result<(CFRetained<AXUIElement>, i32)> {
    let number = window_number(id)?;
    // `OptionIncludingWindow` omits off-screen (minimized) windows.
    let raw = raw_list(CGWindowListOption::OptionAll, kCGNullWindowID)
        .into_iter()
        .find(|r| r.id == number)
        .ok_or_else(|| Error::NotFound(format!("window {id}")))?;
    let windows = app_windows(raw.pid).map_err(|e| ax::err(e, &format!("window {id}")))?;
    windows
        .into_iter()
        .find(|(n, _)| *n == number)
        .map(|(_, el)| (el, raw.pid))
        .ok_or_else(|| {
            Error::NotFound(format!(
                "window {id} has no accessibility element (not an application window, or on another Space)"
            ))
        })
}

/// Focused application pid and its focused (else main) window.
fn focused_window() -> std::result::Result<Option<(i32, Option<u32>)>, AXError> {
    let sys = ax::system_wide();
    let Some(app) = ax::get(&sys, "AXFocusedApplication")?
        .and_then(|v| v.downcast::<AXUIElement>().ok())
    else {
        return Ok(None);
    };
    let Some(pid) = ax::pid_of(&app) else {
        return Ok(None);
    };
    let window = ax::attr_el(&app, "AXFocusedWindow")
        .or_else(|| ax::attr_el(&app, "AXMainWindow"))
        .and_then(|w| ax::window_number_of(&w));
    Ok(Some((pid, window)))
}

fn exe_path(pid: i32) -> Option<String> {
    let from_app = autoreleasepool(|_| {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|app| app.executableURL())
            .and_then(|url| url.path())
            .map(|p| p.to_string())
    });
    from_app.or_else(|| {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buf` is writable for its full length.
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    })
}

/// CGDirectDisplayID (decimal) of the display containing a point.
fn display_at(p: CGPoint) -> Option<u32> {
    let mut id = 0u32;
    let mut count = 0u32;
    // SAFETY: room for one display id; valid out pointers.
    let e = unsafe { CGGetDisplaysWithPoint(p, 1, &mut id, &mut count) };
    (e == CGError::Success && count > 0).then_some(id)
}

/// What AX knows about one window.
#[derive(Default)]
struct AxDetails {
    title: Option<String>,
    minimized: Option<bool>,
    full_screen: bool,
    subrole: String,
}

fn ax_details(el: &AXUIElement) -> AxDetails {
    let Ok(v) = ax::attrs(el, &["AXTitle", "AXMinimized", "AXFullScreen", "AXSubrole"]) else {
        return AxDetails::default();
    };
    AxDetails {
        title: v[0].as_deref().and_then(cf::string),
        minimized: v[1].as_deref().and_then(cf::boolean),
        full_screen: v[2].as_deref().and_then(cf::boolean).unwrap_or(false),
        subrole: v[3].as_deref().and_then(cf::string).unwrap_or_default(),
    }
}

/// Lazily fetched per-process data shared by one listing.
#[derive(Default)]
struct Cache {
    ax: HashMap<i32, Vec<(u32, CFRetained<AXUIElement>)>>,
    exe: HashMap<i32, Option<String>>,
}

impl Cache {
    fn ax_window(&mut self, pid: i32, id: u32) -> Option<CFRetained<AXUIElement>> {
        self.ax
            .entry(pid)
            .or_insert_with(|| app_windows(pid).unwrap_or_default())
            .iter()
            .find(|(n, _)| *n == id)
            .map(|(_, el)| el.clone())
    }

    fn exe(&mut self, pid: i32) -> Option<String> {
        self.exe.entry(pid).or_insert_with(|| exe_path(pid)).clone()
    }
}

fn build(
    raw: &Raw,
    details: &AxDetails,
    z_order: usize,
    foreground: Option<u32>,
    cache: &mut Cache,
) -> WindowInfo {
    let title = match &raw.name {
        Some(name) if !name.is_empty() => name.clone(),
        _ => details.title.clone().unwrap_or_default(),
    };
    let b = raw.bounds;
    let centre = CGPoint {
        x: b.origin.x + b.size.width / 2.0,
        y: b.origin.y + b.size.height / 2.0,
    };
    WindowInfo {
        id: window_id(raw.id),
        title,
        class_name: details.subrole.clone(),
        pid: raw.pid.max(0) as u32,
        process_name: raw.owner.clone(),
        exe_path: cache.exe(raw.pid),
        app_pid: None,
        app_process_name: None,
        bounds: rect_out(b),
        visible: raw.onscreen,
        minimized: details.minimized.unwrap_or(false),
        maximized: details.full_screen,
        cloaked: false,
        foreground: foreground == Some(raw.id),
        owner: None,
        monitor: display_at(centre).map(|d| d.to_string()),
        elevated: None,
        z_order,
    }
}

fn foreground_id() -> Option<u32> {
    focused_window().ok().flatten().and_then(|(_, w)| w)
}

pub fn windows(include_hidden: bool) -> Result<Vec<WindowInfo>> {
    let option = if include_hidden {
        CGWindowListOption::OptionAll
    } else {
        // Off-screen windows are kept so minimized ones can be listed.
        CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements
    };
    let raws = raw_list(option, kCGNullWindowID);
    let foreground = foreground_id();
    let mut cache = Cache::default();
    let mut out = Vec::new();
    for raw in &raws {
        if !include_hidden && (raw.layer != 0 || !raw.has_area()) {
            continue;
        }
        let details = cache
            .ax_window(raw.pid, raw.id)
            .map(|el| ax_details(&el))
            .unwrap_or_default();
        if !include_hidden {
            // Unknown (no AX element) counts as not minimized.
            if !raw.onscreen && details.minimized != Some(true) {
                continue;
            }
            // Drop untitled windows only when the title is known to be empty.
            let cg_empty = raw.name.as_deref().is_some_and(str::is_empty);
            let ax_empty = details.title.as_deref().is_none_or(str::is_empty);
            let known_empty = match (&raw.name, &details.title) {
                (Some(_), _) => cg_empty && ax_empty,
                (None, Some(_)) => ax_empty,
                (None, None) => false,
            };
            if known_empty {
                continue;
            }
        }
        let z = out.len();
        out.push(build(raw, &details, z, foreground, &mut cache));
    }
    Ok(out)
}

pub fn window(id: &WindowId) -> Result<WindowInfo> {
    let number = window_number(id)?;
    let raws = raw_list(CGWindowListOption::OptionAll, kCGNullWindowID);
    let raw = raws
        .iter()
        .find(|r| r.id == number)
        .ok_or_else(|| Error::NotFound(format!("window {id}")))?;
    let z = raws
        .iter()
        .filter(|r| r.normal_onscreen())
        .position(|r| r.id == number)
        .unwrap_or(usize::MAX);
    let mut cache = Cache::default();
    let details = cache
        .ax_window(raw.pid, raw.id)
        .map(|el| ax_details(&el))
        .unwrap_or_default();
    Ok(build(raw, &details, z, foreground_id(), &mut cache))
}

pub fn window_at(p: Point) -> Result<Option<WindowId>> {
    let own = std::process::id() as i32;
    Ok(raw_list(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
    .into_iter()
    .find(|r| r.normal_onscreen() && r.pid != own && rect_out(r.bounds).contains(p))
    .map(|r| window_id(r.id)))
}

pub fn foreground() -> Result<Option<WindowId>> {
    match focused_window() {
        Ok(Some((_, Some(w)))) => Ok(Some(window_id(w))),
        Ok(_) => Ok(None),
        Err(AXError::APIDisabled) => Err(Error::PermissionRequired(ax::AX_PERMISSION.into())),
        Err(e) => Err(ax::err(e, "focused window")),
    }
}

fn set_bool(el: &AXUIElement, attr: &str, value: bool) -> Result<()> {
    ax::set(el, attr, CFBoolean::new(value)).map_err(|e| ax::err(e, attr))
}

fn set_frame(el: &AXUIElement, origin: CGPoint, size: CGSize) -> Result<()> {
    let pos = ax::ax_point(origin)
        .ok_or_else(|| Error::Platform("AXValueCreate(CGPoint) failed".into()))?;
    let sz =
        ax::ax_size(size).ok_or_else(|| Error::Platform("AXValueCreate(CGSize) failed".into()))?;
    ax::set(el, "AXPosition", &pos).map_err(|e| ax::err(e, "AXPosition"))?;
    ax::set(el, "AXSize", &sz).map_err(|e| ax::err(e, "AXSize"))?;
    // Moving across displays can clamp the size first; re-apply the origin.
    ax::set(el, "AXPosition", &pos).map_err(|e| ax::err(e, "AXPosition"))
}

fn unminimize(el: &AXUIElement) -> Result<()> {
    if ax::attr(el, "AXMinimized").as_deref().and_then(cf::boolean) == Some(true) {
        set_bool(el, "AXMinimized", false)?;
    }
    Ok(())
}

fn exit_full_screen(el: &AXUIElement) -> Result<()> {
    if ax::attr(el, "AXFullScreen").as_deref().and_then(cf::boolean) == Some(true) {
        set_bool(el, "AXFullScreen", false)?;
    }
    Ok(())
}

pub fn window_op(id: &WindowId, op: &WindowOp) -> Result<()> {
    let (el, pid) = ax_window(id)?;
    match op {
        WindowOp::Activate => {
            unminimize(&el)?;
            autoreleasepool(|_| {
                if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                {
                    app.activateWithOptions(NSApplicationActivationOptions::empty());
                }
            });
            // Cooperative activation (macOS 14+) may ignore the request from a
            // background process; AXFrontmost does not.
            let _ = ax::set(&ax::app_element(pid), "AXFrontmost", CFBoolean::new(true));
            ax::perform(&el, "AXRaise").map_err(|e| ax::err(e, "AXRaise"))?;
            let _ = ax::set(&el, "AXMain", CFBoolean::new(true));
            Ok(())
        }
        WindowOp::Minimize => set_bool(&el, "AXMinimized", true),
        WindowOp::Restore => {
            exit_full_screen(&el)?;
            unminimize(&el)
        }
        WindowOp::Maximize => {
            unminimize(&el)?;
            let origin = ax::attr(&el, "AXPosition")
                .as_deref()
                .and_then(ax::point)
                .unwrap_or_default();
            let size = ax::attr(&el, "AXSize")
                .as_deref()
                .and_then(ax::size)
                .unwrap_or_default();
            let centre = CGPoint {
                x: origin.x + size.width / 2.0,
                y: origin.y + size.height / 2.0,
            };
            let monitors = crate::display::monitors()?;
            let display = display_at(centre).map(|d| d.to_string());
            let monitor = monitors
                .iter()
                .find(|m| Some(&m.id) == display.as_ref())
                .or_else(|| monitors.iter().find(|m| m.primary))
                .ok_or_else(|| Error::Platform("no display found".into()))?;
            let w = monitor.work_area;
            set_frame(
                &el,
                CGPoint {
                    x: w.x as f64,
                    y: w.y as f64,
                },
                CGSize {
                    width: w.width as f64,
                    height: w.height as f64,
                },
            )
        }
        WindowOp::Close => {
            let button = ax::attr_el(&el, "AXCloseButton")
                .ok_or_else(|| Error::Unsupported(format!("window {id} has no close button")))?;
            ax::perform(&button, "AXPress").map_err(|e| ax::err(e, "close window"))
        }
        WindowOp::SetBounds(r) => {
            if r.is_empty() {
                return Err(Error::InvalidArgument(format!("empty bounds {r:?}")));
            }
            unminimize(&el)?;
            set_frame(
                &el,
                CGPoint {
                    x: r.x as f64,
                    y: r.y as f64,
                },
                CGSize {
                    width: r.width as f64,
                    height: r.height as f64,
                },
            )
        }
    }
}
