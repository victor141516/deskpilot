//! macOS adapter for deskpilot: implements `deskpilot_core::platform::Platform`
//! with the Accessibility API (AXUIElement), CGWindowList, ScreenCaptureKit,
//! CGEvent, Vision and NSPasteboard.
//!
//! Screen units are points in the global display space (origin at the top-left
//! of the primary display), the space AX, CGWindowList and CGEvent share.
//! Captures carry `pixel_scale` (2.0 on Retina displays).
#![cfg(target_os = "macos")]

mod ax;
mod capture;
mod cf;
mod clipboard;
mod display;
mod input;
mod launch;
mod main_thread;
mod ocr;
mod permissions;
mod responsibility;
mod windows;

use deskpilot_core::keys::{Key, KeyChord, KeyStroke, Modifiers};
use deskpilot_core::model::{
    Capture, ElementInfo, ElementNode, ElementRef, MonitorInfo, OcrLine, Point, Rect, WindowId,
    WindowInfo,
};
use deskpilot_core::platform::{
    AxOp, ClipboardContent, Duplex, LaunchResult, LaunchSpec, MessageOp, MouseOp, Platform,
    PlatformInfo, Shortcut, TreeOptions, TreeRoot, WindowOp,
};
use deskpilot_core::{Error, Result};

pub use main_thread::run;
pub use responsibility::disclaim_responsibility;

pub struct MacPlatform;

impl MacPlatform {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// `WindowId` on macOS: the CGWindowID in decimal.
pub(crate) fn window_number(id: &WindowId) -> Result<u32> {
    id.0.parse::<u32>().map_err(|_| {
        Error::InvalidArgument(format!("'{id}' is not a window id (CGWindowID in decimal)"))
    })
}

pub(crate) fn window_id(number: u32) -> WindowId {
    WindowId(number.to_string())
}

impl Platform for MacPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            os: "macos",
            elevated: unsafe { libc::geteuid() } == 0,
            // TCC permissions, not administrator rights, gate accessibility,
            // input and capture; root gains nothing.
            can_elevate: false,
            messages: false,
            permissions: permissions::status(),
        }
    }

    fn shortcut(&self, shortcut: Shortcut) -> Option<KeyChord> {
        let cmd = |key| KeyChord {
            modifiers: Modifiers {
                meta: true,
                ..Modifiers::default()
            },
            key,
        };
        match shortcut {
            Shortcut::SelectAll => Some(cmd(Key::Char('a'))),
            // Pop-up buttons and combo boxes have no universal opening key.
            Shortcut::OpenDropdown | Shortcut::CloseDropdown => None,
            Shortcut::NextTab => Some(KeyChord::with(Key::Tab, true, false, false)),
        }
    }

    fn request_permissions(&self, wait: bool) {
        use permissions::GateOutcome;
        match permissions::gate(wait) {
            // A Screen Recording grant only reaches capture APIs in a new
            // process; a one-shot call can simply start over.
            GateOutcome::GrantedWhileWaiting
                if permissions::screen_recording_needs_restart() =>
            {
                responsibility::reexec();
            }
            GateOutcome::NoSession => eprintln!(
                "deskpilot: permissions missing and no GUI session to ask in: {}",
                permissions::status()
                    .iter()
                    .filter(|p| !p.granted)
                    .map(|p| p.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => {}
        }
    }

    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        display::monitors()
    }

    fn windows(&self, include_hidden: bool) -> Result<Vec<WindowInfo>> {
        windows::windows(include_hidden)
    }

    fn window(&self, id: &WindowId) -> Result<WindowInfo> {
        windows::window(id)
    }

    fn window_at(&self, point: Point) -> Result<Option<WindowId>> {
        windows::window_at(point)
    }

    fn capture_screen(&self, rect: Rect) -> Result<Capture> {
        capture::screen(rect)
    }

    fn capture_window(&self, id: &WindowId) -> Result<Capture> {
        capture::window(id)
    }

    fn ocr(&self, image: &Capture, language: Option<&str>) -> Result<Vec<OcrLine>> {
        ocr::recognize(image, language)
    }

    fn tree(&self, root: &TreeRoot, options: TreeOptions) -> Result<ElementNode> {
        ax::tree(root, options)
    }

    fn element(&self, id: &ElementRef) -> Result<ElementInfo> {
        ax::element(id)
    }

    fn parent(&self, id: &ElementRef) -> Result<Option<ElementInfo>> {
        ax::parent(id)
    }

    fn focused_element(&self) -> Result<Option<ElementInfo>> {
        ax::focused()
    }

    fn element_at(&self, point: Point) -> Result<Option<ElementInfo>> {
        ax::at(point)
    }

    fn element_text(&self, id: &ElementRef) -> Result<Option<String>> {
        ax::text(id)
    }

    fn ax_action(&self, id: &ElementRef, op: &AxOp) -> Result<()> {
        ax::action(id, op)
    }

    fn message_action(&self, _: &ElementInfo, _: &MessageOp) -> Result<()> {
        Err(Error::Unsupported(
            "macOS has no window messages for controls".into(),
        ))
    }

    fn foreground_window(&self) -> Result<Option<WindowId>> {
        windows::foreground()
    }

    fn send_keys(&self, keys: &[KeyStroke]) -> Result<()> {
        input::keys(keys)
    }

    fn mouse(&self, op: &MouseOp) -> Result<()> {
        input::mouse(op)
    }

    fn cursor_position(&self) -> Result<Point> {
        input::cursor()
    }

    fn set_cursor_position(&self, point: Point) -> Result<()> {
        input::set_cursor(point)
    }

    fn window_op(&self, id: &WindowId, op: &WindowOp) -> Result<()> {
        windows::window_op(id, op)
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<LaunchResult> {
        launch::launch(spec)
    }

    fn clipboard_get(&self) -> Result<ClipboardContent> {
        clipboard::get()
    }

    fn clipboard_set(&self, content: &ClipboardContent) -> Result<()> {
        clipboard::set(content)
    }

    fn spawn_elevated_worker(&self, _: &[String]) -> Result<Box<dyn Duplex>> {
        Err(Error::Unsupported(
            "macOS gates desktop access with privacy permissions, not administrator rights".into(),
        ))
    }

    fn connect_worker(&self, _: &str) -> Result<Box<dyn Duplex>> {
        Err(Error::Unsupported("no elevated worker on macOS".into()))
    }
}
