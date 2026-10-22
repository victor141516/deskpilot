//! The adapter contract. Everything above this trait (strategy ladder, focus
//! navigation, verification, policy, tools, MCP, CLI, worker protocol) is
//! shared; an adapter only exposes native primitives.
//!
//! Adapters must work for one-shot processes: references (`WindowId`,
//! `ElementRef`) are re-resolved on every call and never rely on in-process
//! caches.

use std::io::{Read, Write};

use crate::error::Result;
use crate::keys::KeyStroke;
use crate::model::{
    Capture, ElementInfo, ElementNode, ElementRef, MonitorInfo, OcrLine, Point, Rect, WindowId,
    WindowInfo,
};

#[derive(Debug, Clone)]
pub struct PlatformInfo {
    pub os: &'static str,
    /// The current process runs with administrator rights.
    pub elevated: bool,
    /// The adapter can start an elevated worker.
    pub can_elevate: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct TreeOptions {
    /// Levels below the root to read (0 = root only).
    pub max_depth: usize,
    pub max_nodes: usize,
}

/// Accessibility operation executed through the platform accessibility API.
#[derive(Debug, Clone, PartialEq)]
pub enum AxOp {
    Invoke,
    Select,
    Toggle,
    Expand,
    Collapse,
    SetValue(String),
    SetRange(f64),
    Focus,
    ScrollIntoView,
    /// Legacy "default action" (MSAA DoDefaultAction / AXPress).
    DefaultAction,
}

/// Operation the adapter may implement with native window messages (no focus
/// change, no real input). Unsupported combinations return `Error::Unsupported`.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageOp {
    Press,
    Select,
    SetValue(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MouseOp {
    Move(Point),
    Click {
        at: Point,
        button: MouseButton,
        count: u32,
    },
    Drag {
        from: Point,
        to: Point,
        button: MouseButton,
    },
    /// Positive `dy` scrolls up, positive `dx` scrolls right (wheel notches).
    Scroll {
        at: Point,
        dx: i32,
        dy: i32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum WindowOp {
    /// Bring to the foreground and give it keyboard focus.
    Activate,
    Minimize,
    Maximize,
    Restore,
    /// Polite close request (the app may ask to save).
    Close,
    SetBounds(Rect),
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Executable path, document, URI (`ms-settings:sound`), shell target or app id.
    pub target: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ClipboardContent {
    pub text: Option<String>,
    pub image: Option<Capture>,
}

/// Bidirectional byte channel to an elevated worker.
pub trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

pub trait Platform: Send + Sync {
    fn info(&self) -> PlatformInfo;

    // ---- discovery -------------------------------------------------------
    fn monitors(&self) -> Result<Vec<MonitorInfo>>;
    /// Top-level windows in z-order (topmost first). With `include_hidden`
    /// also invisible, cloaked and untitled windows.
    fn windows(&self, include_hidden: bool) -> Result<Vec<WindowInfo>>;
    fn window(&self, id: &WindowId) -> Result<WindowInfo>;
    /// Top-level window under a screen point.
    fn window_at(&self, point: Point) -> Result<Option<WindowId>>;

    // ---- capture -----------------------------------------------------------
    /// Pixels currently shown on screen inside `rect`.
    fn capture_screen(&self, rect: Rect) -> Result<Capture>;
    /// The window's own pixels, even when covered by other windows if the
    /// platform allows it (`Capture::occlusion_free`).
    fn capture_window(&self, id: &WindowId) -> Result<Capture>;
    fn ocr(&self, image: &Capture, language: Option<&str>) -> Result<Vec<OcrLine>>;

    // ---- accessibility (non-invasive) -------------------------------------
    fn tree(&self, root: &TreeRoot, options: TreeOptions) -> Result<ElementNode>;
    fn element(&self, id: &ElementRef) -> Result<ElementInfo>;
    fn parent(&self, id: &ElementRef) -> Result<Option<ElementInfo>>;
    fn focused_element(&self) -> Result<Option<ElementInfo>>;
    fn element_at(&self, point: Point) -> Result<Option<ElementInfo>>;
    /// Full text content (documents, editors) when the element exposes it.
    fn element_text(&self, id: &ElementRef) -> Result<Option<String>>;
    fn ax_action(&self, id: &ElementRef, op: &AxOp) -> Result<()>;
    fn message_action(&self, element: &ElementInfo, op: &MessageOp) -> Result<()>;

    // ---- real input (steals focus) ----------------------------------------
    fn foreground_window(&self) -> Result<Option<WindowId>>;
    fn send_keys(&self, keys: &[KeyStroke]) -> Result<()>;
    fn mouse(&self, op: &MouseOp) -> Result<()>;
    fn cursor_position(&self) -> Result<Point>;
    fn set_cursor_position(&self, point: Point) -> Result<()>;

    // ---- windows and apps -------------------------------------------------
    fn window_op(&self, id: &WindowId, op: &WindowOp) -> Result<()>;
    fn launch(&self, spec: &LaunchSpec) -> Result<LaunchResult>;

    // ---- clipboard --------------------------------------------------------
    fn clipboard_get(&self) -> Result<ClipboardContent>;
    fn clipboard_set(&self, content: &ClipboardContent) -> Result<()>;

    // ---- elevation --------------------------------------------------------
    /// Starts this executable as an elevated worker (asks the user for
    /// consent) and returns the channel to it.
    /// `args` are extra command-line arguments for the worker (policy flags).
    fn spawn_elevated_worker(&self, args: &[String]) -> Result<Box<dyn Duplex>>;
    /// Worker side: connect to the endpoint the parent passed on the command line.
    fn connect_worker(&self, endpoint: &str) -> Result<Box<dyn Duplex>>;
}

#[derive(Debug, Clone)]
pub enum TreeRoot {
    Window(WindowId),
    Element(ElementRef),
}
