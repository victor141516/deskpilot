//! Platform-agnostic description of the desktop: monitors, windows, apps and
//! accessibility elements. Adapters fill these; the engine and tools only
//! consume them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Rectangle in physical screen pixels (virtual-desktop coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn center(&self) -> Point {
        Point {
            x: self.x + self.width / 2,
            y: self.y + self.height / 2,
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.x + self.width && p.y < self.y + self.height
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = (self.x + self.width).min(other.x + other.width);
        let y2 = (self.y + self.height).min(other.y + other.height);
        (x2 > x1 && y2 > y1).then(|| Rect::new(x1, y1, x2 - x1, y2 - y1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Opaque, platform-encoded window identifier (on Windows: the HWND in hex).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct WindowId(pub String);

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque, platform-encoded element reference. It survives process restarts
/// (one-shot CLI calls) as long as the element itself still exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ElementRef(pub String);

impl std::fmt::Display for ElementRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MonitorInfo {
    pub id: String,
    pub name: String,
    pub bounds: Rect,
    pub work_area: Rect,
    /// Display scale factor (1.0 = 100 %).
    pub scale: f64,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub class_name: String,
    /// Process that owns the window.
    pub pid: u32,
    pub process_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
    /// For hosted apps (UWP frames) the process that renders the content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_process_name: Option<String>,
    /// Visible frame bounds in physical pixels.
    pub bounds: Rect,
    pub visible: bool,
    pub minimized: bool,
    pub maximized: bool,
    /// Hidden by the compositor (other virtual desktop, suspended app...).
    pub cloaked: bool,
    pub foreground: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<WindowId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    /// `None` when the process could not be inspected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevated: Option<bool>,
    /// 0 = topmost in z-order.
    pub z_order: usize,
}

impl WindowInfo {
    /// Process names this window answers to (frame host and hosted app).
    pub fn process_names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.process_name.as_str()).chain(self.app_process_name.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppInfo {
    pub pid: u32,
    pub process_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevated: Option<bool>,
    pub windows: Vec<AppWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppWindow {
    pub id: WindowId,
    pub title: String,
    pub foreground: bool,
    pub minimized: bool,
}

/// Normalised accessibility role. Adapters map native roles (UIA control
/// types, AX roles) onto this set; anything else becomes `custom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    AppBar,
    Button,
    Calendar,
    CheckBox,
    ComboBox,
    Custom,
    DataGrid,
    DataItem,
    Document,
    Edit,
    Group,
    Header,
    HeaderItem,
    Hyperlink,
    Image,
    List,
    ListItem,
    Menu,
    MenuBar,
    MenuItem,
    Pane,
    ProgressBar,
    RadioButton,
    ScrollBar,
    SemanticZoom,
    Separator,
    Slider,
    Spinner,
    SplitButton,
    StatusBar,
    Tab,
    TabItem,
    Table,
    Text,
    Thumb,
    TitleBar,
    ToolBar,
    ToolTip,
    Tree,
    TreeItem,
    Window,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::AppBar => "app_bar",
            Role::Button => "button",
            Role::Calendar => "calendar",
            Role::CheckBox => "check_box",
            Role::ComboBox => "combo_box",
            Role::Custom => "custom",
            Role::DataGrid => "data_grid",
            Role::DataItem => "data_item",
            Role::Document => "document",
            Role::Edit => "edit",
            Role::Group => "group",
            Role::Header => "header",
            Role::HeaderItem => "header_item",
            Role::Hyperlink => "hyperlink",
            Role::Image => "image",
            Role::List => "list",
            Role::ListItem => "list_item",
            Role::Menu => "menu",
            Role::MenuBar => "menu_bar",
            Role::MenuItem => "menu_item",
            Role::Pane => "pane",
            Role::ProgressBar => "progress_bar",
            Role::RadioButton => "radio_button",
            Role::ScrollBar => "scroll_bar",
            Role::SemanticZoom => "semantic_zoom",
            Role::Separator => "separator",
            Role::Slider => "slider",
            Role::Spinner => "spinner",
            Role::SplitButton => "split_button",
            Role::StatusBar => "status_bar",
            Role::Tab => "tab",
            Role::TabItem => "tab_item",
            Role::Table => "table",
            Role::Text => "text",
            Role::Thumb => "thumb",
            Role::TitleBar => "title_bar",
            Role::ToolBar => "tool_bar",
            Role::ToolTip => "tool_tip",
            Role::Tree => "tree",
            Role::TreeItem => "tree_item",
            Role::Window => "window",
        }
    }

    /// Containers whose children are navigated with arrow keys instead of Tab.
    pub fn arrow_axis(&self) -> Option<ArrowAxis> {
        match self {
            Role::Tab | Role::ToolBar | Role::MenuBar => Some(ArrowAxis::Horizontal),
            Role::List
            | Role::Tree
            | Role::Menu
            | Role::DataGrid
            | Role::Table
            | Role::ComboBox => Some(ArrowAxis::Vertical),
            Role::Group => Some(ArrowAxis::Both),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowAxis {
    Horizontal,
    Vertical,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToggleState {
    Off,
    On,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExpandState {
    Collapsed,
    Expanded,
    PartiallyExpanded,
    LeafNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RangeInfo {
    pub value: f64,
    pub min: f64,
    pub max: f64,
}

/// Accessibility operations an element advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Invoke,
    Select,
    Toggle,
    ExpandCollapse,
    SetValue,
    SetRange,
    ScrollIntoView,
    Focus,
    DefaultAction,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ElementInfo {
    pub id: ElementRef,
    /// Top-level window that contains the element.
    pub window: WindowId,
    pub role: Role,
    /// Native role name when `role` is `custom` or more specific information exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_role: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub automation_id: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    pub enabled: bool,
    pub focused: bool,
    pub focusable: bool,
    pub offscreen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle: Option<ToggleState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expand: Option<ExpandState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<RangeInfo>,
    pub actions: Vec<ActionKind>,
    /// Native handle of the element's own window, if it has one (Win32 controls).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_window: Option<WindowId>,
    pub pid: u32,
}

impl ElementInfo {
    pub fn supports(&self, kind: ActionKind) -> bool {
        self.actions.contains(&kind)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ElementNode {
    #[serde(flatten)]
    pub info: ElementInfo,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<ElementNode>,
    /// Children exist but were not read (depth or node limit).
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
}

impl ElementNode {
    pub fn walk<'a>(&'a self, out: &mut Vec<&'a ElementNode>) {
        out.push(self);
        for child in &self.children {
            child.walk(out);
        }
    }

    pub fn count(&self) -> usize {
        1 + self.children.iter().map(ElementNode::count).sum::<usize>()
    }
}

/// Raw RGBA8 image in physical pixels plus where it sits on the virtual desktop.
#[derive(Debug, Clone)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Screen position of pixel (0, 0).
    pub origin: Point,
    /// True when the pixels come from the window itself (not from whatever covers it).
    pub occlusion_free: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OcrLine {
    pub text: String,
    pub bounds: Rect,
    pub words: Vec<OcrWord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OcrWord {
    pub text: String,
    pub bounds: Rect,
}
