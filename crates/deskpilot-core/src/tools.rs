//! Tool catalogue shared by the MCP server, the CLI and the elevated worker.
//! Every tool is a typed argument struct (its JSON schema is the MCP input
//! schema) plus a handler over the `Platform` trait.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use base64::Engine as _;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::wait::poll;
use crate::engine::{ActOptions, Engine, Intent};
use crate::error::{Error, Result};
use crate::imaging::{self, ImageFormat};
use crate::keys::{self, KeyItem};
use crate::model::{AppInfo, AppWindow, Capture, ElementInfo, ElementRef, Point, Rect, WindowInfo};
use crate::platform::{
    ClipboardContent, LaunchSpec, MouseButton, MouseOp, Platform, PlatformInfo, TreeOptions,
    TreeRoot, WindowOp,
};
use crate::policy::{Policy, Strategy};
use crate::query::{ElementQuery, WindowFilter, WindowSelector};
use crate::render;
use crate::worker::WorkerClient;

/// Result of a tool call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Structured result (CLI JSON output, worker transport).
    pub data: Value,
    /// Compact text for agents; when absent, agents get `data` as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImagePayload {
    pub mime: String,
    /// Base64-encoded bytes.
    pub data: String,
}

impl ToolOutput {
    fn data(data: impl Serialize) -> Result<Self> {
        Ok(Self {
            data: serde_json::to_value(data).map_err(Error::platform)?,
            ..Default::default()
        })
    }

    fn with_text(mut self, text: String) -> Self {
        self.text = Some(text);
        self
    }

    /// What an agent should read: the compact text if any, else JSON.
    pub fn agent_text(&self) -> String {
        match &self.text {
            Some(t) => t.clone(),
            None => serde_json::to_string(&self.data).unwrap_or_default(),
        }
    }
}

pub struct Ctx<'a> {
    pub platform: &'a dyn Platform,
    pub policy: &'a Policy,
    pub worker: &'a Mutex<Option<WorkerClient>>,
    /// Command-line arguments that reproduce this process's policy in the worker.
    pub worker_args: &'a [String],
}

impl Ctx<'_> {
    fn engine(&self) -> Engine<'_> {
        Engine::new(self.platform, self.policy)
    }
}

type Handler = Box<dyn Fn(&Ctx, Value) -> Result<ToolOutput> + Send + Sync>;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// Changes something on the desktop (audited, honours dry-run).
    pub mutating: bool,
    pub input_schema: Value,
    handler: Handler,
}

impl ToolSpec {
    pub fn call(&self, ctx: &Ctx, args: Value) -> Result<ToolOutput> {
        (self.handler)(ctx, args)
    }
}

fn tool<A, F>(name: &'static str, mutating: bool, description: &'static str, f: F) -> ToolSpec
where
    A: DeserializeOwned + JsonSchema + 'static,
    F: Fn(&Ctx, A) -> Result<ToolOutput> + Send + Sync + 'static,
{
    let mut schema = serde_json::to_value(schemars::schema_for!(A)).expect("schema serializes");
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        obj.entry("type").or_insert(json!("object"));
        obj.entry("properties").or_insert_with(|| json!({}));
    }
    ToolSpec {
        name,
        description,
        mutating,
        input_schema: schema,
        handler: Box::new(move |ctx, args| {
            let args = if args.is_null() { json!({}) } else { args };
            let parsed: A = serde_json::from_value(args)
                .map_err(|e| Error::InvalidArgument(format!("{name}: {e}")))?;
            f(ctx, parsed)
        }),
    }
}

/// Elevation is offered when the platform has an administrator worker (or this
/// process is that worker).
pub fn offers_elevation(info: &PlatformInfo) -> bool {
    info.can_elevate || info.elevated
}

// ---------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListWindowsArgs {
    #[serde(flatten)]
    pub filter: WindowFilter,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAppsArgs {
    /// Include processes whose only windows are hidden or cloaked.
    #[serde(default)]
    pub include_hidden: bool,
}

/// What to capture. With no field set: the whole virtual desktop.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CaptureTarget {
    /// Capture this window's own pixels (works when it is covered by other windows).
    #[serde(default)]
    pub window: Option<WindowSelector>,
    /// Monitor id, name or 0-based index from `list_monitors`.
    #[serde(default)]
    pub monitor: Option<String>,
    /// Screen rectangle in physical pixels.
    #[serde(default)]
    pub region: Option<Rect>,
    /// Capture just this element's bounds.
    #[serde(default)]
    pub element: Option<ElementRef>,
    /// For `window`/`element`: take what is visible on screen instead of the window's own pixels.
    #[serde(default)]
    pub screen_pixels: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScreenshotArgs {
    #[serde(flatten)]
    pub target: CaptureTarget,
    /// Longest side of the returned image in pixels (default 1568; 0 = original size).
    #[serde(default)]
    pub max_size: Option<u32>,
    #[serde(default)]
    pub format: ImageFormat,
    /// Also write the encoded image to this path.
    #[serde(default)]
    pub save_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TreeArgs {
    /// Window to read (default: foreground window).
    #[serde(default)]
    pub window: Option<WindowSelector>,
    /// Read only the subtree under this element.
    #[serde(default)]
    pub element: Option<ElementRef>,
    /// Levels below the root (default 12).
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// Maximum elements returned (default 400).
    #[serde(default)]
    pub max_nodes: Option<usize>,
    /// `text` (compact, default) or `json`.
    #[serde(default)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// Identifies one element: either `element`, or a query over `window` (or `within`).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ElementTarget {
    /// Element reference returned by `get_tree` / `find_elements`.
    #[serde(default)]
    pub element: Option<ElementRef>,
    /// Window to search (default: foreground window).
    #[serde(default)]
    pub window: Option<WindowSelector>,
    /// Search only below this element.
    #[serde(default)]
    pub within: Option<ElementRef>,
    #[serde(flatten)]
    pub query: ElementQuery,
    /// 0-based index among the matches in document order (default 0).
    #[serde(default)]
    pub index: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    #[serde(default)]
    pub window: Option<WindowSelector>,
    #[serde(default)]
    pub within: Option<ElementRef>,
    #[serde(flatten)]
    pub query: ElementQuery,
    /// Default 20.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Elements scanned at most (default 5000).
    #[serde(default)]
    pub max_nodes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ElementArgs {
    pub element: ElementRef,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PointArgs {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadTextArgs {
    #[serde(default)]
    pub element: Option<ElementRef>,
    #[serde(default)]
    pub window: Option<WindowSelector>,
    /// Maximum characters returned (default 20000).
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActArgs {
    #[serde(flatten)]
    pub target: ElementTarget,
    pub action: Intent,
    /// Text or number for `set_value`.
    #[serde(default)]
    pub value: Option<String>,
    /// Most invasive strategy allowed: accessibility < messages < keyboard < mouse (default mouse).
    #[serde(default)]
    pub max_strategy: Option<Strategy>,
    /// Strategy to start from, skipping cheaper ones known not to work for this app (default accessibility).
    #[serde(default)]
    pub min_strategy: Option<Strategy>,
    /// Give the foreground back after keyboard/mouse strategies (default: yes except for `expand`).
    #[serde(default)]
    pub restore_focus: Option<bool>,
    /// For `press`: escalate even when an attempt showed no observable change (may repeat the effect).
    #[serde(default)]
    pub escalate_unverified: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendKeysArgs {
    /// Window that must receive the keys; it is brought to the foreground first.
    /// Without it the keys go to the current foreground window.
    #[serde(default)]
    pub window: Option<WindowSelector>,
    /// Sequence of chords (`"ctrl+a"`, `"Tab"`, `"alt+F4"`) and `{ "text": "..." }` items.
    pub keys: Vec<KeyItem>,
    /// Give the foreground back to the previous window afterwards (default false).
    #[serde(default)]
    pub restore_focus: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    Click,
    DoubleClick,
    RightClick,
    MiddleClick,
    Move,
    Drag,
    Scroll,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MouseArgs {
    pub action: MouseAction,
    /// Screen x in physical pixels (or relative to `relative_to`). From a screenshot:
    /// `x = origin.x + image_x / scale`.
    pub x: i32,
    pub y: i32,
    /// Drag destination.
    #[serde(default)]
    pub to_x: Option<i32>,
    #[serde(default)]
    pub to_y: Option<i32>,
    /// Wheel notches for `scroll`: positive = up / right.
    #[serde(default)]
    pub dy: Option<i32>,
    #[serde(default)]
    pub dx: Option<i32>,
    /// Interpret coordinates relative to this window's top-left corner and bring it forward first.
    #[serde(default)]
    pub relative_to: Option<WindowSelector>,
    /// Put the pointer back where it was (default true).
    #[serde(default)]
    pub restore_cursor: Option<bool>,
    /// Give the foreground back to the previous window afterwards (default false).
    #[serde(default)]
    pub restore_focus: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    Activate,
    Minimize,
    Maximize,
    Restore,
    Close,
    /// Move and/or resize; give any of x, y, width, height.
    SetBounds,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WindowArgs {
    pub window: WindowSelector,
    pub op: WindowAction,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub width: Option<i32>,
    #[serde(default)]
    pub height: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaunchArgs {
    /// Executable path or name, document, folder, URI or app id. Windows: `ms-settings:sound`, `mmsys.cpl`. macOS: app name (`Safari`), bundle id (`com.apple.Safari`), `.app` path, `x-apple.systempreferences:` URI.
    pub target: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Wait until a new window appears and return it (default true).
    #[serde(default)]
    pub wait_for_window: Option<bool>,
    /// Default 15000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitCondition {
    WindowExists,
    WindowGone,
    ElementExists,
    ElementGone,
    ElementEnabled,
    ElementFocused,
    ElementSelected,
    /// Element value equals `value`.
    ValueEquals,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitArgs {
    pub condition: WaitCondition,
    /// For `window_*` conditions.
    #[serde(default)]
    pub window_filter: Option<WindowFilter>,
    /// For `element_*` conditions.
    #[serde(flatten)]
    pub target: ElementTarget,
    #[serde(default)]
    pub value: Option<String>,
    /// Default 10000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Default 250.
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardSetArgs {
    #[serde(default)]
    pub text: Option<String>,
    /// PNG/JPEG file to put on the clipboard as an image.
    #[serde(default)]
    pub image_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OcrArgs {
    #[serde(flatten)]
    pub target: CaptureTarget,
    /// BCP-47 language tag (default: user profile languages).
    #[serde(default)]
    pub language: Option<String>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Tools offered on this platform. `elevated` / `elevate` exist only where an
/// administrator worker can run.
pub fn registry(platform: &PlatformInfo) -> Vec<ToolSpec> {
    let elevation = offers_elevation(platform);
    let mut tools = vec![
        tool(
            "info",
            false,
            "Platform, privilege and policy status: OS, administrator state and elevated worker (where supported), OS privacy permissions, deny list, strategy ceiling, dry-run.",
            info,
        ),
        tool(
            "list_monitors",
            false,
            "List monitors with bounds, work area and scale factor (physical pixels).",
            list_monitors,
        ),
        tool(
            "list_windows",
            false,
            "List top-level windows in z-order (topmost first), including owned dialogs. Filter by title fragment, process, pid or class. Each line: id, title, process(pid), flags (fg=foreground, min, max, hidden, cloaked, admin) and bounds.",
            list_windows,
        ),
        tool(
            "list_apps",
            false,
            "List running applications (processes with windows) and their windows.",
            list_apps,
        ),
        tool(
            "screenshot",
            false,
            "Capture the screen, a monitor, a region, a window (its own pixels even if covered) or an element. Expensive in tokens: prefer get_tree/find_elements. Returns the image and its mapping: screen = origin + image_px / scale.",
            screenshot,
        ),
        tool(
            "get_tree",
            false,
            "Read the accessibility tree of a window (default: foreground) or element as compact text: [ref] role \"name\" #automation_id (x,y wxh) {states} value=… actions=…. Names and values longer than 120 characters end in …; read them whole with read_text on the [ref]. Use the [ref] in other tools.",
            get_tree,
        ),
        tool(
            "find_elements",
            false,
            "Find elements by name, name fragment, role, automation id, class or value inside a window or element. Cheaper than reading the whole tree.",
            find_elements,
        ),
        tool(
            "get_element",
            false,
            "Read the current state of one element.",
            get_element,
        ),
        tool(
            "get_focused",
            false,
            "Element that has keyboard focus, and its window.",
            get_focused,
        ),
        tool(
            "element_at",
            false,
            "Accessibility element at a screen point (useful to map a screenshot position to an element).",
            element_at,
        ),
        tool(
            "read_text",
            false,
            "Full text of an element (document/editor text, or every name and value in its subtree, unclipped) or the visible texts of a window. Use it for long names and values that get_tree/find_elements clip with ….",
            read_text,
        ),
        tool(
            "act",
            true,
            "Perform an action on an element, escalating from the least invasive strategy: accessibility action → window messages → keyboard (focus navigation verified via accessibility, steals focus) → real mouse click. Stops at the first verified success and reports every attempt. Actions: press, select, toggle, check, uncheck, expand, collapse, set_value, focus, scroll_into_view.",
            act,
        ),
        tool(
            "send_keys",
            true,
            "Type chords and text with the real keyboard into a window (brought to the foreground). Invasive: prefer `act`.",
            send_keys,
        ),
        tool(
            "mouse",
            true,
            "Real mouse input at screen coordinates: click, double/right/middle click, move, drag, scroll. Most invasive: use only when accessibility, messages and keyboard cannot do it.",
            mouse,
        ),
        tool(
            "window",
            true,
            "Window management: activate, minimize, maximize, restore, close, set_bounds.",
            window,
        ),
        tool(
            "launch",
            true,
            "Open an application, document, folder or URI and wait for its new window.",
            launch,
        ),
        tool(
            "wait_for",
            false,
            "Wait until a window appears/disappears or an element exists/disappears/becomes enabled, focused, selected or has a value.",
            wait_for,
        ),
        tool(
            "clipboard_get",
            false,
            "Read the clipboard (text and image).",
            clipboard_get,
        ),
        tool(
            "clipboard_set",
            true,
            "Put text or an image file on the clipboard.",
            clipboard_set,
        ),
        tool(
            "ocr",
            false,
            "Recognise text in a capture (same targets as screenshot) with the OS OCR engine. Returns lines and words with screen bounds. For apps without accessibility.",
            ocr,
        ),
        tool(
            "pixel",
            false,
            "Colour of the screen pixel at a point.",
            pixel,
        ),
    ];
    if elevation {
        tools.push(tool(
            "elevate",
            false,
            "Start the elevated (administrator) worker now. Shows a UAC prompt to the user. Afterwards pass \"elevated\": true to any tool to run it as administrator.",
            elevate,
        ));
        for t in &mut tools {
            t.input_schema["properties"]["elevated"] = json!({
                "type": "boolean",
                "description": "Run this call in the elevated (administrator) worker. Needed to read or control windows of processes running as administrator. The first use shows a UAC prompt to the user; the worker then lives for the rest of the session."
            });
        }
    }
    tools
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn foreground(ctx: &Ctx) -> Result<WindowInfo> {
    let id = ctx
        .platform
        .foreground_window()?
        .ok_or_else(|| Error::NotFound("there is no foreground window".into()))?;
    ctx.platform.window(&id)
}

fn window_or_foreground(ctx: &Ctx, sel: Option<&WindowSelector>) -> Result<WindowInfo> {
    match sel {
        Some(s) => s.resolve(ctx.platform),
        None => foreground(ctx),
    }
}

fn elevation_warning(ctx: &Ctx, w: &WindowInfo) -> Option<String> {
    (w.elevated == Some(true) && !ctx.platform.info().elevated).then(|| {
        format!(
            "{} runs as administrator; accessibility data and input are restricted for this process. Repeat with \"elevated\": true.",
            w.process_name
        )
    })
}

pub fn resolve_element(ctx: &Ctx, t: &ElementTarget) -> Result<ElementInfo> {
    if let Some(e) = &t.element {
        return ctx.platform.element(e);
    }
    if t.query.is_empty() {
        return Err(Error::InvalidArgument("give `element` or at least one query field (name, name_contains, role, automation_id, class_name, value_contains)".into()));
    }
    let (root, window) = match &t.within {
        Some(e) => (TreeRoot::Element(e.clone()), None),
        None => {
            let w = window_or_foreground(ctx, t.window.as_ref())?;
            (TreeRoot::Window(w.id.clone()), Some(w))
        }
    };
    let tree = ctx.platform.tree(
        &root,
        TreeOptions {
            max_depth: 64,
            max_nodes: 5000,
        },
    )?;
    let matches = t.query.find(&tree);
    let index = t.index.unwrap_or(0);
    if let Some(e) = matches.get(index) {
        return Ok((*e).clone());
    }
    // An administrator window looks empty to a normal process: say why.
    if let Some(warning) = window.as_ref().and_then(|w| elevation_warning(ctx, w)) {
        return Err(Error::ElevationRequired(warning));
    }
    Err(Error::NotFound(format!(
        "{} element(s) match {}; index {index} requested",
        matches.len(),
        serde_json::to_string(&t.query).unwrap_or_default()
    )))
}

fn window_line(w: &WindowInfo) -> String {
    let mut flags = Vec::new();
    if w.foreground {
        flags.push("fg");
    }
    if w.minimized {
        flags.push("min");
    }
    if w.maximized {
        flags.push("max");
    }
    if !w.visible {
        flags.push("hidden");
    }
    if w.cloaked {
        flags.push("cloaked");
    }
    if w.elevated == Some(true) {
        flags.push("admin");
    }
    let mut s = format!("{} \"{}\" {}({})", w.id, w.title, w.process_name, w.pid);
    if let (Some(app), Some(pid)) = (&w.app_process_name, w.app_pid) {
        s.push_str(&format!(" app={app}({pid})"));
    }
    if let Some(owner) = &w.owner {
        s.push_str(&format!(" owner={owner}"));
    }
    if !flags.is_empty() {
        s.push_str(&format!(" [{}]", flags.join(",")));
    }
    let b = w.bounds;
    s.push_str(&format!(
        " ({},{} {}x{}) .{}",
        b.x, b.y, b.width, b.height, w.class_name
    ));
    s
}

fn virtual_desktop(ctx: &Ctx) -> Result<Rect> {
    let monitors = ctx.platform.monitors()?;
    let mut it = monitors.iter().map(|m| m.bounds);
    let first = it
        .next()
        .ok_or_else(|| Error::NotFound("no monitors".into()))?;
    Ok(it.fold(first, |a, b| {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        let r = (a.x + a.width).max(b.x + b.width);
        let bt = (a.y + a.height).max(b.y + b.height);
        Rect::new(x, y, r - x, bt - y)
    }))
}

fn capture(ctx: &Ctx, t: &CaptureTarget) -> Result<(Capture, Option<String>)> {
    let p = ctx.platform;
    if let Some(e) = &t.element {
        let info = p.element(e)?;
        let bounds = info
            .bounds
            .filter(|b| !b.is_empty())
            .ok_or_else(|| Error::Unsupported("element has no bounds".into()))?;
        let window = p.window(&info.window)?;
        if !t.screen_pixels && !window.minimized {
            if let Ok(cap) = p.capture_window(&window.id) {
                if cap.occlusion_free {
                    return Ok((imaging::crop(&cap, bounds)?, None));
                }
            }
        }
        return Ok((p.capture_screen(bounds)?, None));
    }
    if let Some(sel) = &t.window {
        let w = sel.resolve(p)?;
        if w.minimized {
            return Err(Error::InvalidArgument(format!(
                "window {} is minimized; restore it first (tool `window`, op `restore`)",
                w.id
            )));
        }
        let warning = elevation_warning(ctx, &w);
        let cap = if t.screen_pixels {
            p.capture_screen(w.bounds)?
        } else {
            p.capture_window(&w.id)?
        };
        return Ok((cap, warning));
    }
    if let Some(m) = &t.monitor {
        let monitors = p.monitors()?;
        let found = monitors
            .iter()
            .enumerate()
            .find(|(i, mon)| {
                mon.id == *m || mon.name.eq_ignore_ascii_case(m) || i.to_string() == *m
            })
            .map(|(_, mon)| mon.bounds)
            .ok_or_else(|| Error::NotFound(format!("monitor '{m}'")))?;
        return Ok((p.capture_screen(found)?, None));
    }
    if let Some(r) = t.region {
        if r.is_empty() {
            return Err(Error::InvalidArgument("region is empty".into()));
        }
        return Ok((p.capture_screen(r)?, None));
    }
    Ok((p.capture_screen(virtual_desktop(ctx)?)?, None))
}

fn dry_run(ctx: &Ctx, what: String) -> Option<ToolOutput> {
    ctx.policy.dry_run.then(|| ToolOutput {
        data: json!({ "dry_run": true, "would": what }),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn info(ctx: &Ctx, _: NoArgs) -> Result<ToolOutput> {
    let i = ctx.platform.info();
    let worker_active = ctx.worker.lock().map(|w| w.is_some()).unwrap_or(false);
    ToolOutput::data(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": i.os,
        "elevated": i.elevated,
        "can_elevate": i.can_elevate,
        "elevated_worker_running": worker_active,
        "permissions": i.permissions,
        "policy": {
            "deny_processes": ctx.policy.deny_processes,
            "max_strategy": ctx.policy.max_strategy,
            "dry_run": ctx.policy.dry_run,
            "audit_log": ctx.policy.audit_path,
        },
        "strategies": Strategy::ALL
            .into_iter()
            .filter(|s| *s != Strategy::Messages || i.messages)
            .collect::<Vec<_>>(),
    }))
}

fn list_monitors(ctx: &Ctx, _: NoArgs) -> Result<ToolOutput> {
    ToolOutput::data(ctx.platform.monitors()?)
}

fn list_windows(ctx: &Ctx, a: ListWindowsArgs) -> Result<ToolOutput> {
    let windows: Vec<WindowInfo> = ctx
        .platform
        .windows(a.filter.include_hidden)?
        .into_iter()
        .filter(|w| a.filter.matches(w))
        .collect();
    let text = windows
        .iter()
        .map(window_line)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput::data(&windows)?.with_text(if text.is_empty() {
        "no windows match".into()
    } else {
        text
    }))
}

fn list_apps(ctx: &Ctx, a: ListAppsArgs) -> Result<ToolOutput> {
    let mut apps: Vec<AppInfo> = Vec::new();
    for w in ctx.platform.windows(a.include_hidden)? {
        if !a.include_hidden && w.title.is_empty() {
            continue;
        }
        let (pid, name) = match (w.app_pid, &w.app_process_name) {
            (Some(pid), Some(name)) => (pid, name.clone()),
            _ => (w.pid, w.process_name.clone()),
        };
        let entry = AppWindow {
            id: w.id.clone(),
            title: w.title.clone(),
            foreground: w.foreground,
            minimized: w.minimized,
        };
        match apps.iter_mut().find(|app| app.pid == pid) {
            Some(app) => app.windows.push(entry),
            None => apps.push(AppInfo {
                pid,
                process_name: name,
                exe_path: w.exe_path.clone(),
                elevated: w.elevated,
                windows: vec![entry],
            }),
        }
    }
    let text = apps
        .iter()
        .map(|app| {
            let titles: Vec<String> = app
                .windows
                .iter()
                .map(|w| format!("{} \"{}\"", w.id, w.title))
                .collect();
            let admin = if app.elevated == Some(true) {
                " [admin]"
            } else {
                ""
            };
            format!(
                "{}({}){admin}: {}",
                app.process_name,
                app.pid,
                titles.join("; ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput::data(&apps)?.with_text(text))
}

fn screenshot(ctx: &Ctx, a: ScreenshotArgs) -> Result<ToolOutput> {
    let (cap, warning) = capture(ctx, &a.target)?;
    let max = match a.max_size {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(1568),
    };
    let (img, mapping) = imaging::encode(&cap, max, a.format)?;
    if let Some(path) = &a.save_path {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(Error::platform)?;
        }
        std::fs::write(path, &img.data)
            .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.display())))?;
    }
    let mut data = json!({ "mapping": mapping });
    if let Some(path) = &a.save_path {
        data["saved_to"] = json!(path);
    }
    if let Some(w) = &warning {
        data["warning"] = json!(w);
    }
    let mut text = format!(
        "image {}x{} (source {}x{}), origin ({},{}), scale {:.4}; screen = origin + image_px / scale{}",
        mapping.width,
        mapping.height,
        mapping.source_width,
        mapping.source_height,
        mapping.origin.x,
        mapping.origin.y,
        mapping.scale,
        if mapping.occlusion_free {
            ""
        } else {
            "; pixels as shown on screen (may include covering windows)"
        }
    );
    if let Some(w) = warning {
        text = format!("warning: {w}\n{text}");
    }
    Ok(ToolOutput {
        data,
        text: Some(text),
        images: vec![ImagePayload {
            mime: img.mime.into(),
            data: base64::engine::general_purpose::STANDARD.encode(&img.data),
        }],
    })
}

fn get_tree(ctx: &Ctx, a: TreeArgs) -> Result<ToolOutput> {
    let (root, window) = match &a.element {
        Some(e) => {
            let info = ctx.platform.element(e)?;
            (
                TreeRoot::Element(e.clone()),
                ctx.platform.window(&info.window)?,
            )
        }
        None => {
            let w = window_or_foreground(ctx, a.window.as_ref())?;
            (TreeRoot::Window(w.id.clone()), w)
        }
    };
    let options = TreeOptions {
        max_depth: a.max_depth.unwrap_or(12),
        max_nodes: a.max_nodes.unwrap_or(400).max(1),
    };
    let tree = ctx.platform.tree(&root, options)?;
    let warning = elevation_warning(ctx, &window);
    let header = format!("window {}\n", window_line(&window));
    match a.format {
        OutputFormat::Json => {
            ToolOutput::data(json!({ "window": window, "tree": tree, "warning": warning }))
        }
        OutputFormat::Text => {
            let body = render::tree(&tree);
            let text = match &warning {
                Some(w) => format!("warning: {w}\n{header}{body}"),
                None => format!("{header}{body}"),
            };
            Ok(ToolOutput {
                data: json!({ "window": window.id, "nodes": tree.count(), "text": body, "warning": warning }),
                text: Some(text),
                images: vec![],
            })
        }
    }
}

fn find_elements(ctx: &Ctx, a: FindArgs) -> Result<ToolOutput> {
    if a.query.is_empty() {
        return Err(Error::InvalidArgument(
            "give at least one query field".into(),
        ));
    }
    let root = match &a.within {
        Some(e) => TreeRoot::Element(e.clone()),
        None => TreeRoot::Window(window_or_foreground(ctx, a.window.as_ref())?.id),
    };
    let tree = ctx.platform.tree(
        &root,
        TreeOptions {
            max_depth: 64,
            max_nodes: a.max_nodes.unwrap_or(5000),
        },
    )?;
    let matches: Vec<ElementInfo> = a
        .query
        .find(&tree)
        .into_iter()
        .take(a.max_results.unwrap_or(20))
        .cloned()
        .collect();
    let text = if matches.is_empty() {
        format!("no elements match (scanned {} elements)", tree.count())
    } else {
        matches
            .iter()
            .map(render::element_line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ToolOutput::data(&matches)?.with_text(text))
}

fn get_element(ctx: &Ctx, a: ElementArgs) -> Result<ToolOutput> {
    let e = ctx.platform.element(&a.element)?;
    let text = render::element_line(&e);
    Ok(ToolOutput::data(&e)?.with_text(text))
}

fn get_focused(ctx: &Ctx, _: NoArgs) -> Result<ToolOutput> {
    match ctx.platform.focused_element()? {
        Some(e) => {
            let text = render::element_line(&e);
            Ok(ToolOutput::data(&e)?.with_text(text))
        }
        None => Ok(ToolOutput::data(Value::Null)?.with_text("no focused element".into())),
    }
}

fn element_at(ctx: &Ctx, a: PointArgs) -> Result<ToolOutput> {
    match ctx.platform.element_at(Point { x: a.x, y: a.y })? {
        Some(e) => {
            let text = render::element_line(&e);
            Ok(ToolOutput::data(&e)?.with_text(text))
        }
        None => Ok(ToolOutput::data(Value::Null)?.with_text("no element at that point".into())),
    }
}

fn read_text(ctx: &Ctx, a: ReadTextArgs) -> Result<ToolOutput> {
    let max = a.max_chars.unwrap_or(20_000);
    let root = match &a.element {
        Some(e) => {
            if let Some(text) = ctx.platform.element_text(e)?.filter(|t| !t.trim().is_empty()) {
                return finish_text(text, max, "text_pattern");
            }
            TreeRoot::Element(e.clone())
        }
        None => TreeRoot::Window(window_or_foreground(ctx, a.window.as_ref())?.id),
    };
    let tree = ctx.platform.tree(
        &root,
        TreeOptions {
            max_depth: 40,
            max_nodes: 3000,
        },
    )?;
    let mut nodes = Vec::new();
    tree.walk(&mut nodes);
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for n in nodes {
        for t in [Some(n.info.name.as_str()), n.info.value.as_deref()]
            .into_iter()
            .flatten()
        {
            let t = t.trim();
            if !t.is_empty() && seen.insert(t.to_string()) {
                lines.push(t.to_string());
            }
        }
    }
    finish_text(lines.join("\n"), max, "accessible_names")
}

fn finish_text(text: String, max: usize, source: &str) -> Result<ToolOutput> {
    let total = text.chars().count();
    let clipped: String = text.chars().take(max).collect();
    let truncated = total > max;
    Ok(ToolOutput {
        data: json!({ "source": source, "chars": total, "truncated": truncated, "text": clipped }),
        text: Some(if truncated {
            format!("{clipped}\n… ({total} chars total)")
        } else {
            clipped
        }),
        images: vec![],
    })
}

fn act(ctx: &Ctx, a: ActArgs) -> Result<ToolOutput> {
    let target = resolve_element(ctx, &a.target)?;
    let options = ActOptions {
        min_strategy: a.min_strategy.unwrap_or(Strategy::Accessibility),
        max_strategy: a.max_strategy.unwrap_or(Strategy::Mouse),
        restore_focus: a.restore_focus,
        escalate_unverified: a.escalate_unverified,
        ..Default::default()
    };
    let report = ctx
        .engine()
        .act(&target, &a.action, a.value.as_deref(), &options)?;
    let mut text = format!(
        "{} {:?} on {} via {} (verified: {})",
        if report.dry_run {
            "plan:"
        } else if report.success {
            "done"
        } else {
            "FAILED"
        },
        a.action,
        render::element_line(&target),
        report
            .strategy
            .map(|s| format!("{s:?}").to_lowercase())
            .unwrap_or_else(|| "-".into()),
        report.verified
    );
    for at in &report.attempts {
        text.push_str(&format!(
            "\n  {:?}: {:?} {}{}",
            at.strategy,
            at.outcome,
            at.method,
            if at.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", at.detail)
            }
        ));
    }
    match &report.element {
        Some(e) => text.push_str(&format!("\n  now: {}", render::element_line(e))),
        // The element is gone: say where the app's view went (Fork shows each tab
        // as its own window, dialogs replace their parent...). Windows come in
        // z-order, so the first other titled window of the app is the one shown.
        None if !report.dry_run => {
            if let Some(w) = ctx
                .platform
                .windows(false)
                .unwrap_or_default()
                .into_iter()
                .find(|w| w.pid == target.pid && w.id != target.window && !w.title.is_empty())
            {
                text.push_str(&format!(
                    "\n  element gone; the app now shows {}",
                    window_line(&w)
                ));
            }
        }
        None => {}
    }
    if let Some(w) = &report.focus_restored_to {
        text.push_str(&format!("\n  foreground restored to {w}"));
    }
    Ok(ToolOutput::data(&report)?.with_text(text))
}

fn send_keys(ctx: &Ctx, a: SendKeysArgs) -> Result<ToolOutput> {
    let strokes = keys::parse_items(&a.keys)?;
    let p = ctx.platform;
    let previous = p.foreground_window()?;
    let target = window_or_foreground(ctx, a.window.as_ref())?;
    ctx.engine().guard_window(&target)?;
    if let Some(out) = dry_run(
        ctx,
        format!("send {} key item(s) to {}", strokes.len(), target.id),
    ) {
        return Ok(out);
    }
    if previous.as_ref() != Some(&target.id) {
        p.window_op(&target.id, &WindowOp::Activate)?;
        sleep(Duration::from_millis(120));
        if p.foreground_window()?.as_ref() != Some(&target.id) {
            return Err(Error::Platform(format!(
                "window {} could not be brought to the foreground",
                target.id
            )));
        }
    }
    p.send_keys(&strokes)?;
    let mut restored = None;
    if a.restore_focus {
        if let Some(prev) = previous.filter(|p| *p != target.id) {
            sleep(Duration::from_millis(80));
            p.window_op(&prev, &WindowOp::Activate)?;
            restored = Some(prev);
        }
    }
    ToolOutput::data(
        json!({ "sent": strokes.len(), "window": target.id, "focus_restored_to": restored }),
    )
}

fn mouse(ctx: &Ctx, a: MouseArgs) -> Result<ToolOutput> {
    let p = ctx.platform;
    let previous = p.foreground_window()?;
    let (offset, relative) = match &a.relative_to {
        Some(sel) => {
            let w = sel.resolve(p)?;
            (
                Point {
                    x: w.bounds.x,
                    y: w.bounds.y,
                },
                Some(w),
            )
        }
        None => (Point { x: 0, y: 0 }, None),
    };
    let at = Point {
        x: a.x + offset.x,
        y: a.y + offset.y,
    };
    let hit = match p.window_at(at)? {
        Some(id) => Some(p.window(&id)?),
        None => None,
    };
    for w in relative.iter().chain(hit.iter()) {
        ctx.engine().guard_window(w)?;
    }
    if let Some(out) = dry_run(ctx, format!("{:?} at ({}, {})", a.action, at.x, at.y)) {
        return Ok(out);
    }
    if let Some(w) = relative.as_ref().or(hit.as_ref()) {
        if previous.as_ref() != Some(&w.id) && !matches!(a.action, MouseAction::Move) {
            p.window_op(&w.id, &WindowOp::Activate)?;
            sleep(Duration::from_millis(120));
        }
    }
    let saved = p.cursor_position().ok();
    let op = match a.action {
        MouseAction::Click => MouseOp::Click {
            at,
            button: MouseButton::Left,
            count: 1,
        },
        MouseAction::DoubleClick => MouseOp::Click {
            at,
            button: MouseButton::Left,
            count: 2,
        },
        MouseAction::RightClick => MouseOp::Click {
            at,
            button: MouseButton::Right,
            count: 1,
        },
        MouseAction::MiddleClick => MouseOp::Click {
            at,
            button: MouseButton::Middle,
            count: 1,
        },
        MouseAction::Move => MouseOp::Move(at),
        MouseAction::Drag => {
            let (Some(tx), Some(ty)) = (a.to_x, a.to_y) else {
                return Err(Error::InvalidArgument("drag needs to_x and to_y".into()));
            };
            MouseOp::Drag {
                from: at,
                to: Point {
                    x: tx + offset.x,
                    y: ty + offset.y,
                },
                button: MouseButton::Left,
            }
        }
        MouseAction::Scroll => MouseOp::Scroll {
            at,
            dx: a.dx.unwrap_or(0),
            dy: a.dy.unwrap_or(0),
        },
    };
    p.mouse(&op)?;
    if a.restore_cursor.unwrap_or(true) && !matches!(a.action, MouseAction::Move) {
        if let Some(s) = saved {
            let _ = p.set_cursor_position(s);
        }
    }
    let mut restored = None;
    if a.restore_focus {
        if let Some(prev) = previous {
            sleep(Duration::from_millis(80));
            p.window_op(&prev, &WindowOp::Activate)?;
            restored = Some(prev);
        }
    }
    ToolOutput::data(
        json!({ "done": format!("{:?}", a.action).to_lowercase(), "at": at, "window": hit.map(|w| w.id), "focus_restored_to": restored }),
    )
}

fn window(ctx: &Ctx, a: WindowArgs) -> Result<ToolOutput> {
    let w = a.window.resolve(ctx.platform)?;
    ctx.engine().guard_window(&w)?;
    let op = match a.op {
        WindowAction::Activate => WindowOp::Activate,
        WindowAction::Minimize => WindowOp::Minimize,
        WindowAction::Maximize => WindowOp::Maximize,
        WindowAction::Restore => WindowOp::Restore,
        WindowAction::Close => WindowOp::Close,
        WindowAction::SetBounds => {
            if a.x.is_none() && a.y.is_none() && a.width.is_none() && a.height.is_none() {
                return Err(Error::InvalidArgument(
                    "set_bounds needs x, y, width or height".into(),
                ));
            }
            WindowOp::SetBounds(Rect::new(
                a.x.unwrap_or(w.bounds.x),
                a.y.unwrap_or(w.bounds.y),
                a.width.unwrap_or(w.bounds.width),
                a.height.unwrap_or(w.bounds.height),
            ))
        }
    };
    if let Some(out) = dry_run(ctx, format!("{op:?} on {}", w.id)) {
        return Ok(out);
    }
    ctx.platform.window_op(&w.id, &op)?;
    sleep(Duration::from_millis(150));
    let after = ctx.platform.window(&w.id).ok();
    let text = match &after {
        Some(now) => format!("{op:?} → {}", window_line(now)),
        None => format!("{op:?} → window {} is gone", w.id),
    };
    Ok(ToolOutput::data(json!({ "op": format!("{op:?}"), "window": after }))?.with_text(text))
}

fn launch(ctx: &Ctx, a: LaunchArgs) -> Result<ToolOutput> {
    if let Some(out) = dry_run(ctx, format!("launch {} {:?}", a.target, a.args)) {
        return Ok(out);
    }
    let before: HashSet<_> = ctx
        .platform
        .windows(false)?
        .into_iter()
        .map(|w| w.id)
        .collect();
    let result = ctx.platform.launch(&LaunchSpec {
        target: a.target.clone(),
        args: a.args,
        cwd: a.cwd,
    })?;
    let mut new_windows = Vec::new();
    let mut existing = Vec::new();
    if a.wait_for_window.unwrap_or(true) {
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(15_000));
        let waited = poll(
            timeout,
            Duration::from_millis(250),
            "waiting for a new window",
            || {
                let fresh: Vec<WindowInfo> = ctx
                    .platform
                    .windows(false)?
                    .into_iter()
                    .filter(|w| !before.contains(&w.id) && !w.title.is_empty())
                    .collect();
                Ok((!fresh.is_empty()).then_some(fresh))
            },
        );
        match waited {
            Ok(fresh) => new_windows = fresh,
            // An app that was already running just comes to the front without a
            // new window: hand back its existing windows instead of failing.
            Err(Error::Timeout(msg)) => {
                existing = match result.pid {
                    Some(pid) => ctx
                        .platform
                        .windows(false)?
                        .into_iter()
                        .filter(|w| w.pid == pid || w.app_pid == Some(pid))
                        .collect(),
                    None => Vec::new(),
                };
                if existing.is_empty() {
                    return Err(Error::Timeout(msg));
                }
            }
            Err(e) => return Err(e),
        }
    }
    let mut lines = vec![format!("launched {} (pid {:?})", a.target, result.pid)];
    lines.extend(new_windows.iter().map(window_line));
    if !existing.is_empty() {
        lines.push("no new window; the app was already running with:".into());
        lines.extend(existing.iter().map(window_line));
    }
    Ok(ToolOutput::data(
        json!({ "pid": result.pid, "new_windows": new_windows, "existing_windows": existing }),
    )?
    .with_text(lines.join("\n")))
}

fn wait_for(ctx: &Ctx, a: WaitArgs) -> Result<ToolOutput> {
    let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(10_000));
    let interval = Duration::from_millis(a.interval_ms.unwrap_or(250).max(20));
    let what = format!("{:?}", a.condition);
    match a.condition {
        WaitCondition::WindowExists | WaitCondition::WindowGone => {
            let filter = a.window_filter.ok_or_else(|| {
                Error::InvalidArgument("window conditions need window_filter".into())
            })?;
            if filter.is_empty() {
                return Err(Error::InvalidArgument(
                    "window_filter needs at least one field".into(),
                ));
            }
            let exists = a.condition == WaitCondition::WindowExists;
            let found = poll(timeout, interval, &what, || {
                let m = ctx
                    .platform
                    .windows(filter.include_hidden)?
                    .into_iter()
                    .find(|w| filter.matches(w));
                Ok(match (exists, m) {
                    (true, Some(w)) => Some(Some(w)),
                    (false, None) => Some(None),
                    _ => None,
                })
            })?;
            let text = match &found {
                Some(w) => format!("window appeared: {}", window_line(w)),
                None => "window is gone".into(),
            };
            Ok(ToolOutput::data(json!({ "condition": what, "window": found }))?.with_text(text))
        }
        WaitCondition::ElementGone => {
            poll(timeout, interval, &what, || {
                match resolve_element(ctx, &a.target) {
                    Ok(_) => Ok(None),
                    Err(Error::NotFound(_)) => Ok(Some(())),
                    Err(e) => Err(e),
                }
            })?;
            Ok(ToolOutput::data(json!({ "condition": what }))?.with_text("element is gone".into()))
        }
        cond => {
            if cond == WaitCondition::ValueEquals && a.value.is_none() {
                return Err(Error::InvalidArgument("value_equals needs value".into()));
            }
            let e = poll(timeout, interval, &what, || {
                let e = resolve_element(ctx, &a.target)?;
                let ok = match cond {
                    WaitCondition::ElementExists => true,
                    WaitCondition::ElementEnabled => e.enabled,
                    WaitCondition::ElementFocused => e.focused,
                    WaitCondition::ElementSelected => e.selected == Some(true),
                    WaitCondition::ValueEquals => {
                        e.value.as_deref().map(str::trim) == a.value.as_deref().map(str::trim)
                    }
                    _ => unreachable!("handled above"),
                };
                Ok(ok.then_some(e))
            })?;
            let text = render::element_line(&e);
            Ok(ToolOutput::data(json!({ "condition": what, "element": e }))?.with_text(text))
        }
    }
}

fn clipboard_get(ctx: &Ctx, _: NoArgs) -> Result<ToolOutput> {
    let content = ctx.platform.clipboard_get()?;
    let mut out =
        ToolOutput::data(json!({ "text": content.text, "has_image": content.image.is_some() }))?;
    if let Some(img) = &content.image {
        let (enc, _) = imaging::encode(img, Some(1568), ImageFormat::Png)?;
        out.images.push(ImagePayload {
            mime: enc.mime.into(),
            data: base64::engine::general_purpose::STANDARD.encode(&enc.data),
        });
    }
    Ok(out)
}

fn clipboard_set(ctx: &Ctx, a: ClipboardSetArgs) -> Result<ToolOutput> {
    if a.text.is_none() && a.image_path.is_none() {
        return Err(Error::InvalidArgument("give text or image_path".into()));
    }
    if let Some(out) = dry_run(ctx, "set clipboard".into()) {
        return Ok(out);
    }
    let image = a
        .image_path
        .as_deref()
        .map(imaging::decode_file)
        .transpose()?;
    ctx.platform.clipboard_set(&ClipboardContent {
        text: a.text.clone(),
        image,
    })?;
    ToolOutput::data(json!({ "text": a.text.is_some(), "image": a.image_path }))
}

fn ocr(ctx: &Ctx, a: OcrArgs) -> Result<ToolOutput> {
    let (cap, warning) = capture(ctx, &a.target)?;
    let mut lines = ctx.platform.ocr(&cap, a.language.as_deref())?;
    for line in &mut lines {
        line.bounds = cap.to_screen(line.bounds);
        for w in &mut line.words {
            w.bounds = cap.to_screen(w.bounds);
        }
    }
    let mut text: Vec<String> = warning
        .into_iter()
        .map(|w| format!("warning: {w}"))
        .collect();
    text.extend(lines.iter().map(|l| {
        format!(
            "({},{} {}x{}) {}",
            l.bounds.x, l.bounds.y, l.bounds.width, l.bounds.height, l.text
        )
    }));
    let joined = if text.is_empty() {
        "no text recognised".into()
    } else {
        text.join("\n")
    };
    Ok(ToolOutput::data(&lines)?.with_text(joined))
}

fn pixel(ctx: &Ctx, a: PointArgs) -> Result<ToolOutput> {
    let cap = ctx.platform.capture_screen(Rect::new(a.x, a.y, 1, 1))?;
    let [r, g, b] = [cap.rgba[0], cap.rgba[1], cap.rgba[2]];
    let hex = format!("#{r:02x}{g:02x}{b:02x}");
    Ok(
        ToolOutput::data(json!({ "x": a.x, "y": a.y, "rgb": [r, g, b], "hex": hex }))?
            .with_text(hex),
    )
}

fn elevate(ctx: &Ctx, _: NoArgs) -> Result<ToolOutput> {
    if ctx.platform.info().elevated {
        return ToolOutput::data(
            json!({ "elevated": true, "note": "this process already runs as administrator" }),
        );
    }
    crate::worker::ensure_worker(ctx.platform, ctx.worker, ctx.worker_args)?;
    ToolOutput::data(json!({ "elevated_worker_running": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(can_elevate: bool) -> PlatformInfo {
        PlatformInfo {
            os: "test",
            elevated: false,
            can_elevate,
            messages: true,
            permissions: vec![],
        }
    }

    #[test]
    fn elevation_is_offered_only_where_the_platform_supports_it() {
        for t in registry(&info(true)) {
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            assert_eq!(
                t.input_schema["properties"]["elevated"]["type"], "boolean",
                "{}",
                t.name
            );
        }
        let plain = registry(&info(false));
        assert!(plain.iter().all(|t| t.name != "elevate"));
        for t in &plain {
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            assert!(t.input_schema["properties"].get("elevated").is_none(), "{}", t.name);
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let tools = registry(&info(true));
        let names: HashSet<_> = tools.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), tools.len());
    }
}
