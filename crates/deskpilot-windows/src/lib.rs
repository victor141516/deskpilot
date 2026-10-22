//! Windows adapter for deskpilot: implements `deskpilot_core::platform::Platform`
//! with Win32 (windows, input, messages, clipboard, shell), UI Automation,
//! GDI capture and Windows.Media.Ocr.
#![cfg(windows)]

mod capture;
mod clipboard;
mod elevation;
mod input;
mod messages;
mod ocr;
mod process;
mod shell;
mod uia;
mod util;
mod win;

use deskpilot_core::Result;
use deskpilot_core::keys::KeyStroke;
use deskpilot_core::model::{
    Capture, ElementInfo, ElementNode, ElementRef, MonitorInfo, OcrLine, Point, Rect, WindowId,
    WindowInfo,
};
use deskpilot_core::platform::{
    AxOp, ClipboardContent, Duplex, LaunchResult, LaunchSpec, MessageOp, MouseOp, Platform,
    PlatformInfo, TreeOptions, TreeRoot, WindowOp,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

pub struct WindowsPlatform {
    uia: uia::Uia,
    elevated: bool,
}

// UI Automation's client objects are free-threaded and COM is initialised as
// MTA; the platform is used from one thread at a time in practice.
unsafe impl Send for WindowsPlatform {}
unsafe impl Sync for WindowsPlatform {}

impl WindowsPlatform {
    pub fn new() -> Result<Self> {
        // Physical pixels everywhere: capture, UIA bounds and input agree on every monitor.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Ok(Self {
            uia: uia::Uia::new()?,
            elevated: process::current_elevated(),
        })
    }
}

impl Platform for WindowsPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            os: "windows",
            elevated: self.elevated,
            can_elevate: !self.elevated,
        }
    }

    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        Ok(win::monitors())
    }

    fn windows(&self, include_hidden: bool) -> Result<Vec<WindowInfo>> {
        Ok(win::windows(include_hidden))
    }

    fn window(&self, id: &WindowId) -> Result<WindowInfo> {
        win::window(id)
    }

    fn window_at(&self, point: Point) -> Result<Option<WindowId>> {
        Ok(win::window_at(point))
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
        self.uia.tree(root, options)
    }

    fn element(&self, id: &ElementRef) -> Result<ElementInfo> {
        self.uia.element(id)
    }

    fn parent(&self, id: &ElementRef) -> Result<Option<ElementInfo>> {
        self.uia.parent(id)
    }

    fn focused_element(&self) -> Result<Option<ElementInfo>> {
        self.uia.focused()
    }

    fn element_at(&self, point: Point) -> Result<Option<ElementInfo>> {
        self.uia.at(point)
    }

    fn element_text(&self, id: &ElementRef) -> Result<Option<String>> {
        self.uia.text(id)
    }

    fn ax_action(&self, id: &ElementRef, op: &AxOp) -> Result<()> {
        self.uia.action(id, op)
    }

    fn message_action(&self, element: &ElementInfo, op: &MessageOp) -> Result<()> {
        messages::action(&self.uia, element, op)
    }

    fn foreground_window(&self) -> Result<Option<WindowId>> {
        Ok(win::foreground())
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
        win::window_op(id, op)
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<LaunchResult> {
        shell::launch(spec)
    }

    fn clipboard_get(&self) -> Result<ClipboardContent> {
        clipboard::get()
    }

    fn clipboard_set(&self, content: &ClipboardContent) -> Result<()> {
        clipboard::set(content)
    }

    fn spawn_elevated_worker(&self, args: &[String]) -> Result<Box<dyn Duplex>> {
        elevation::spawn_worker(args)
    }

    fn connect_worker(&self, endpoint: &str) -> Result<Box<dyn Duplex>> {
        elevation::connect(endpoint)
    }
}
