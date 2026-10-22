//! Top-level windows, monitors, window operations and foreground activation.

use std::thread::sleep;
use std::time::Duration;

use deskpilot_core::model::{MonitorInfo, Point, Rect, WindowId, WindowInfo};
use deskpilot_core::platform::WindowOp;
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW,
    MonitorFromWindow,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, FindWindowExW, GA_ROOT, GW_OWNER, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, MONITORINFOF_PRIMARY,
    PostMessageW, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SWP_NOACTIVATE, SWP_NOZORDER,
    SetForegroundWindow, SetWindowPos, ShowWindowAsync, WM_CLOSE, WindowFromPoint,
};
use windows::core::BOOL;
use windows::core::w;

use crate::process::ProcessTable;
use crate::util::{from_wide, hwnd_id, parse_hwnd, rect};

pub fn all_hwnds() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe { (*(lparam.0 as *mut Vec<HWND>)).push(hwnd) };
        BOOL(1)
    }
    let mut list: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    from_wide(&buf[..n.max(0) as usize])
}

pub fn title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        from_wide(&buf[..n.max(0) as usize])
    }
}

pub fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Visible frame (without the invisible resize borders) when DWM knows it.
pub fn frame_bounds(hwnd: HWND) -> Rect {
    unsafe {
        let mut r = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_ok()
        {
            return rect(r);
        }
        let _ = GetWindowRect(hwnd, &mut r);
        rect(r)
    }
}

fn cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut value as *mut _ as *mut _, 4).is_ok()
            && value != 0
    }
}

fn monitor_name(hmon: HMONITOR) -> Option<String> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        GetMonitorInfoW(hmon, &mut info as *mut _ as *mut _)
            .as_bool()
            .then(|| from_wide(&info.szDevice))
    }
}

/// UWP apps live in an ApplicationFrameWindow whose content belongs to another process.
fn hosted_app_pid(hwnd: HWND, class: &str) -> Option<u32> {
    if class != "ApplicationFrameWindow" {
        return None;
    }
    let core =
        unsafe { FindWindowExW(Some(hwnd), None, w!("Windows.UI.Core.CoreWindow"), None) }.ok()?;
    let pid = pid_of(core);
    (pid != 0 && pid != pid_of(hwnd)).then_some(pid)
}

pub fn info(hwnd: HWND, z_order: usize, procs: &mut ProcessTable, foreground: HWND) -> WindowInfo {
    let class_name = class_name(hwnd);
    let pid = pid_of(hwnd);
    let proc_info = procs.get(pid);
    let app_pid = hosted_app_pid(hwnd, &class_name);
    let app_name = app_pid.map(|p| procs.get(p).name);
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }
        .ok()
        .filter(|h| !h.is_invalid())
        .map(hwnd_id);
    let monitor = monitor_name(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) });
    WindowInfo {
        id: hwnd_id(hwnd),
        title: title(hwnd),
        class_name,
        pid,
        process_name: proc_info.name,
        exe_path: proc_info.exe_path,
        app_pid,
        app_process_name: app_name,
        bounds: frame_bounds(hwnd),
        visible: unsafe { IsWindowVisible(hwnd).as_bool() },
        minimized: unsafe { IsIconic(hwnd).as_bool() },
        maximized: unsafe { IsZoomed(hwnd).as_bool() },
        cloaked: cloaked(hwnd),
        foreground: hwnd == foreground,
        owner,
        monitor,
        elevated: proc_info.elevated,
        z_order,
    }
}

fn listed(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() && !cloaked(hwnd) && GetWindowTextLengthW(hwnd) > 0 }
}

pub fn windows(include_hidden: bool) -> Vec<WindowInfo> {
    let foreground = unsafe { GetForegroundWindow() };
    let mut procs = ProcessTable::default();
    all_hwnds()
        .into_iter()
        .filter(|h| include_hidden || listed(*h))
        .enumerate()
        .map(|(z, h)| info(h, z, &mut procs, foreground))
        .collect()
}

pub fn existing(id: &WindowId) -> Result<HWND> {
    let hwnd = parse_hwnd(id)?;
    if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return Err(Error::NotFound(format!("window {id}")));
    }
    Ok(hwnd)
}

pub fn window(id: &WindowId) -> Result<WindowInfo> {
    let hwnd = existing(id)?;
    let z = all_hwnds()
        .into_iter()
        .filter(|h| listed(*h))
        .position(|h| h == hwnd)
        .unwrap_or(usize::MAX);
    Ok(info(hwnd, z, &mut ProcessTable::default(), unsafe {
        GetForegroundWindow()
    }))
}

pub fn root_of(hwnd: HWND) -> HWND {
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() { hwnd } else { root }
}

pub fn window_at(p: Point) -> Option<WindowId> {
    let hwnd = unsafe { WindowFromPoint(POINT { x: p.x, y: p.y }) };
    (!hwnd.is_invalid()).then(|| hwnd_id(root_of(hwnd)))
}

pub fn foreground() -> Option<WindowId> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_invalid()).then(|| hwnd_id(hwnd))
}

pub fn monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _: HDC,
        _: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        unsafe { (*(lparam.0 as *mut Vec<HMONITOR>)).push(hmon) };
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }
    handles
        .into_iter()
        .filter_map(|h| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            unsafe { GetMonitorInfoW(h, &mut info as *mut _ as *mut _) }
                .as_bool()
                .then(|| {
                    let (mut dx, mut dy) = (96u32, 96u32);
                    let _ = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
                    let name = from_wide(&info.szDevice);
                    MonitorInfo {
                        id: name.clone(),
                        name,
                        bounds: rect(info.monitorInfo.rcMonitor),
                        work_area: rect(info.monitorInfo.rcWork),
                        scale: dx as f64 / 96.0,
                        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                    }
                })
        })
        .collect()
}

fn is_foreground(hwnd: HWND) -> bool {
    unsafe { GetForegroundWindow() == hwnd }
}

/// Brings a window to the foreground despite the focus-stealing rules, in
/// increasing order of side effects.
pub fn activate(hwnd: HWND) -> Result<()> {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
            sleep(Duration::from_millis(150));
        }
        if SetForegroundWindow(hwnd).as_bool() && is_foreground(hwnd) {
            return Ok(());
        }
        // Share the input queue with the current foreground thread.
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool() {
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(me, fg_thread, false);
            if is_foreground(hwnd) {
                return Ok(());
            }
        }
        // Last resort: a synthetic Alt press makes this process the last input source.
        let alt = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        SendInput(&[alt(Default::default())], size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
        SendInput(&[alt(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
        sleep(Duration::from_millis(50));
        if is_foreground(hwnd) {
            Ok(())
        } else {
            Err(Error::Platform(format!(
                "Windows refused to bring {} to the foreground",
                hwnd_id(hwnd)
            )))
        }
    }
}

pub fn window_op(id: &WindowId, op: &WindowOp) -> Result<()> {
    let hwnd = existing(id)?;
    unsafe {
        match op {
            WindowOp::Activate => activate(hwnd)?,
            WindowOp::Minimize => {
                let _ = ShowWindowAsync(hwnd, SW_MINIMIZE);
            }
            WindowOp::Maximize => {
                let _ = ShowWindowAsync(hwnd, SW_MAXIMIZE);
            }
            WindowOp::Restore => {
                let _ = ShowWindowAsync(hwnd, SW_RESTORE);
            }
            WindowOp::Close => PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))
                .map_err(|e| crate::util::win_err("WM_CLOSE", e))?,
            WindowOp::SetBounds(target) => {
                if IsZoomed(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                    let _ = ShowWindowAsync(hwnd, SW_RESTORE);
                    sleep(Duration::from_millis(150));
                }
                // Convert visible-frame bounds into window-rect bounds (invisible borders).
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let frame = frame_bounds(hwnd);
                let left = frame.x - wr.left;
                let top = frame.y - wr.top;
                let right = wr.right - (frame.x + frame.width);
                let bottom = wr.bottom - (frame.y + frame.height);
                SetWindowPos(
                    hwnd,
                    None,
                    target.x - left,
                    target.y - top,
                    target.width + left + right,
                    target.height + top + bottom,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .map_err(|e| crate::util::win_err("SetWindowPos", e))?;
            }
        }
    }
    Ok(())
}
