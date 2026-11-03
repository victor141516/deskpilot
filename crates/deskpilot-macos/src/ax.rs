//! Accessibility (AXUIElement): tree, element references, actions.
//!
//! Element reference format: `<root>/<child path>~<fingerprint>`.
//! - `<root>` is a window id (CGWindowID in decimal), or `app<pid>/menubar`
//!   for elements under an application's menu bar.
//! - `<child path>` is the dot-separated list of `AXChildren` indices from the
//!   root element (empty for the root itself), e.g. `4182/0.3.1~1a2b3c4d`.
//! - `<fingerprint>` is an FNV-1a hash of role, subrole and identifier (title
//!   when the identifier is empty). A path that no longer leads to an element
//!   with that fingerprint falls back to a bounded subtree search for a unique
//!   match; otherwise the reference is `NotFound`, never a different element.
use std::collections::VecDeque;
use std::ptr::{self, NonNull};
use std::sync::Once;

use deskpilot_core::model::{
    ActionKind, ElementInfo, ElementNode, ElementRef, ExpandState, Point, RangeInfo, Rect, Role,
    ToggleState, WindowId,
};
use deskpilot_core::platform::{AxOp, TreeOptions, TreeRoot};
use deskpilot_core::{Error, Result};
use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXIsProcessTrusted, AXUIElement, AXValue,
    AXValueType,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFEqual, CFHash, CFNumber, CFRange, CFRetained, CFString, CFType, CGPoint,
    CGSize,
};

use crate::cf::{self, cfstr};
use crate::{window_id, window_number};

pub(crate) const AX_PERMISSION: &str =
    "Accessibility permission is missing: System Settings → Privacy & Security → Accessibility";

/// Messaging timeout for application elements, so a hung app cannot block us.
const APP_TIMEOUT: f32 = 1.0;
/// Process-wide fallback timeout (default is 6 s).
const GLOBAL_TIMEOUT: f32 = 2.0;
/// Nodes visited when a stale path is searched for its fingerprint.
const SEARCH_LIMIT: usize = 5000;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Private but stable (used by yabai, Hammerspoon, AltTab): CGWindowID of an AX window.
    fn _AXUIElementGetWindow(element: &AXUIElement, window: *mut u32) -> AXError;
}

// ---- low-level AX helpers (shared with windows.rs) -------------------------

pub(crate) fn err(e: AXError, context: &str) -> Error {
    // An untrusted process may get CannotComplete or other codes instead of APIDisabled.
    if e != AXError::Success && !unsafe { AXIsProcessTrusted() } {
        return Error::PermissionRequired(AX_PERMISSION.into());
    }
    match e {
        AXError::APIDisabled => Error::PermissionRequired(AX_PERMISSION.into()),
        AXError::InvalidUIElement => {
            Error::NotFound(format!("{context}: element no longer exists"))
        }
        AXError::ActionUnsupported
        | AXError::AttributeUnsupported
        | AXError::NotImplemented
        | AXError::IllegalArgument => {
            Error::Unsupported(format!("{context}: not supported by the element"))
        }
        AXError::CannotComplete => Error::Platform(format!("{context}: app did not respond")),
        e => Error::Platform(format!("{context}: AXError {}", e.0)),
    }
}

fn set_global_timeout() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain AX call on a valid element.
        unsafe {
            AXUIElement::new_system_wide().set_messaging_timeout(GLOBAL_TIMEOUT);
        }
    });
}

pub(crate) fn system_wide() -> CFRetained<AXUIElement> {
    set_global_timeout();
    // SAFETY: no preconditions.
    unsafe { AXUIElement::new_system_wide() }
}

pub(crate) fn app_element(pid: i32) -> CFRetained<AXUIElement> {
    set_global_timeout();
    // SAFETY: any pid is accepted; invalid ones yield errors on use.
    let app = unsafe { AXUIElement::new_application(pid) };
    // SAFETY: valid element.
    unsafe { app.set_messaging_timeout(APP_TIMEOUT) };
    enable_web_accessibility(pid, &app);
    app
}

/// Chromium browsers (Chrome, Edge, Brave, Arc, Helium…) and Electron apps
/// build their web-content accessibility tree only for clients that ask for
/// it: without this a browser window exposes the toolbar but no page.
/// `AXManualAccessibility` is Electron's switch; Chromium reacts to
/// `AXEnhancedUserInterface` (it answers "not implemented" but enables the tree).
/// Done once per process, and only for apps that embed a Chromium framework, so
/// native apps never see `AXEnhancedUserInterface` (it disables their window
/// animations).
fn enable_web_accessibility(pid: i32, app: &AXUIElement) {
    static DONE: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut done) = DONE.lock() else {
        return;
    };
    if done.contains(&pid) {
        return;
    }
    done.push(pid);
    match chromium_kind(pid) {
        Some(Chromium::Electron) => {
            let _ = set(app, "AXManualAccessibility", CFBoolean::new(true));
        }
        Some(Chromium::Browser) => {
            let enabled = attr(app, "AXEnhancedUserInterface")
                .and_then(|v| cf::boolean(&v))
                .unwrap_or(false);
            if !enabled {
                let _ = set(app, "AXEnhancedUserInterface", CFBoolean::new(true));
                // Chromium builds the page tree asynchronously after the switch:
                // wait (bounded) until the focused window shows a web area.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if attr_el(app, "AXFocusedWindow")
                        .or_else(|| attr_el(app, "AXMainWindow"))
                        .is_some_and(|w| has_web_area(&w, 12))
                    {
                        break;
                    }
                }
            }
        }
        None => {}
    }
}

enum Chromium {
    Electron,
    Browser,
}

/// Whether a non-empty `AXWebArea` appears within `depth` levels. Breadth-first
/// and bounded: gives up when a level holds more than 200 elements.
fn has_web_area(el: &AXUIElement, depth: usize) -> bool {
    let mut frontier = vec![objc2_core_foundation::Type::retain(el)];
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in &frontier {
            for c in children(node).unwrap_or_default() {
                if attr(&c, "AXRole").and_then(|r| cf::string(&r)).as_deref() == Some("AXWebArea")
                    && has_children(&c)
                {
                    return true;
                }
                next.push(c);
            }
        }
        if next.is_empty() || next.len() > 200 {
            return false;
        }
        frontier = next;
    }
    false
}

/// Electron apps ship `Electron Framework.framework`; Chromium browsers (and
/// Chromium-based apps like ChatGPT's) ship `<Name> Framework.framework` with
/// a renderer helper in `Helpers/` (`<Name> Helper (Renderer).app`).
fn chromium_kind(pid: i32) -> Option<Chromium> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer and size are valid.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let exe = std::path::PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned());
    // …/Name.app/Contents/MacOS/Name → …/Name.app/Contents/Frameworks
    let frameworks = exe.parent()?.parent()?.join("Frameworks");
    let entries: Vec<String> = std::fs::read_dir(&frameworks)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    if entries.iter().any(|e| e == "Electron Framework.framework") {
        return Some(Chromium::Electron);
    }
    let has_renderer = |name: &str| {
        std::fs::read_dir(frameworks.join(format!("{name} Framework.framework/Helpers")))
            .map(|dir| {
                dir.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .any(|h| h.contains("Renderer)"))
            })
            .unwrap_or(false)
    };
    entries
        .iter()
        .filter_map(|e| e.strip_suffix(" Framework.framework"))
        .any(has_renderer)
        .then_some(Chromium::Browser)
}

/// Attribute value; `Ok(None)` when the element has no such attribute/value.
pub(crate) fn get(
    el: &AXUIElement,
    name: &str,
) -> std::result::Result<Option<CFRetained<CFType>>, AXError> {
    let mut out: *const CFType = ptr::null();
    // SAFETY: `out` is a valid out pointer; on success it holds a +1 reference.
    let e = unsafe { el.copy_attribute_value(&cfstr(name), NonNull::from(&mut out)) };
    match e {
        AXError::Success => {
            Ok(NonNull::new(out.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) }))
        }
        AXError::NoValue | AXError::AttributeUnsupported | AXError::Failure => Ok(None),
        e => Err(e),
    }
}

pub(crate) fn attr(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    get(el, name).ok().flatten()
}

pub(crate) fn attr_el(el: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    attr(el, name)?.downcast::<AXUIElement>().ok()
}

/// Missing values come back as CFNull or as an AXValue holding an AXError.
fn present(v: CFRetained<CFType>) -> Option<CFRetained<CFType>> {
    if let Some(ax) = v.downcast_ref::<AXValue>() {
        // SAFETY: valid AXValue.
        if unsafe { ax.r#type() } == AXValueType::AXError {
            return None;
        }
    }
    if v.downcast_ref::<objc2_core_foundation::CFNull>().is_some() {
        return None;
    }
    Some(v)
}

fn copy_multiple(
    el: &AXUIElement,
    names: &CFArray,
    count: usize,
) -> std::result::Result<Vec<Option<CFRetained<CFType>>>, AXError> {
    let mut out: *const CFArray = ptr::null();
    // SAFETY: `names` holds CFStrings; `out` is a valid out pointer (+1 on success).
    let e = unsafe {
        el.copy_multiple_attribute_values(
            names,
            AXCopyMultipleAttributeOptions::empty(),
            NonNull::from(&mut out),
        )
    };
    if e != AXError::Success {
        return Err(e);
    }
    let Some(arr) = NonNull::new(out.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) })
    else {
        return Ok(vec![None; count]);
    };
    let mut values: Vec<Option<CFRetained<CFType>>> =
        cf::array(&arr).into_iter().map(present).collect();
    values.resize(count, None);
    Ok(values)
}

/// Several attributes in one IPC round trip, in `names` order.
pub(crate) fn attrs(
    el: &AXUIElement,
    names: &[&str],
) -> std::result::Result<Vec<Option<CFRetained<CFType>>>, AXError> {
    let strings: Vec<CFRetained<CFString>> = names.iter().map(|n| cfstr(n)).collect();
    let arr = CFArray::from_retained_objects(&strings);
    copy_multiple(el, arr.as_opaque(), names.len())
}

pub(crate) fn set(el: &AXUIElement, name: &str, value: &CFType) -> std::result::Result<(), AXError> {
    // SAFETY: valid element, attribute name and CF value.
    match unsafe { el.set_attribute_value(&cfstr(name), value) } {
        AXError::Success => Ok(()),
        e => Err(e),
    }
}

fn settable(el: &AXUIElement, name: &str) -> bool {
    let mut b: u8 = 0;
    // SAFETY: `b` is a valid out pointer.
    let e = unsafe { el.is_attribute_settable(&cfstr(name), NonNull::from(&mut b)) };
    e == AXError::Success && b != 0
}

pub(crate) fn perform(el: &AXUIElement, action: &str) -> std::result::Result<(), AXError> {
    // SAFETY: valid element and action name.
    match unsafe { el.perform_action(&cfstr(action)) } {
        AXError::Success => Ok(()),
        e => Err(e),
    }
}

fn action_names(el: &AXUIElement) -> Vec<String> {
    let mut out: *const CFArray = ptr::null();
    // SAFETY: `out` is a valid out pointer (+1 on success).
    let e = unsafe { el.copy_action_names(NonNull::from(&mut out)) };
    if e != AXError::Success {
        return Vec::new();
    }
    let Some(arr) = NonNull::new(out.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) })
    else {
        return Vec::new();
    };
    cf::array(&arr).iter().filter_map(|v| cf::string(v)).collect()
}

pub(crate) fn pid_of(el: &AXUIElement) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    // SAFETY: `pid` is a valid out pointer.
    let e = unsafe { el.pid(NonNull::from(&mut pid)) };
    (e == AXError::Success && pid > 0).then_some(pid)
}

pub(crate) fn window_number_of(el: &AXUIElement) -> Option<u32> {
    let mut id = 0u32;
    // SAFETY: valid element and out pointer.
    let e = unsafe { _AXUIElementGetWindow(el, &mut id) };
    (e == AXError::Success && id != 0).then_some(id)
}

fn same(a: &AXUIElement, b: &AXUIElement) -> bool {
    CFEqual(Some(a), Some(b))
}

fn ax_value_get<T: Default>(v: &CFType, kind: AXValueType) -> Option<T> {
    let ax = v.downcast_ref::<AXValue>()?;
    let mut out = T::default();
    // SAFETY: `out` matches the layout of `kind` (checked by AXValueGetValue).
    unsafe {
        (ax.r#type() == kind && ax.value(kind, NonNull::from(&mut out).cast())).then_some(out)
    }
}

pub(crate) fn point(v: &CFType) -> Option<CGPoint> {
    ax_value_get(v, AXValueType::CGPoint)
}

pub(crate) fn size(v: &CFType) -> Option<CGSize> {
    ax_value_get(v, AXValueType::CGSize)
}

pub(crate) fn ax_point(mut p: CGPoint) -> Option<CFRetained<AXValue>> {
    // SAFETY: pointer to a live CGPoint matching the type tag.
    unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut p).cast()) }
}

pub(crate) fn ax_size(mut s: CGSize) -> Option<CFRetained<AXValue>> {
    // SAFETY: pointer to a live CGSize matching the type tag.
    unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&mut s).cast()) }
}

fn children(el: &AXUIElement) -> Result<Vec<CFRetained<AXUIElement>>> {
    let v = get(el, "AXChildren").map_err(|e| err(e, "AXChildren"))?;
    Ok(v.map(|v| {
        cf::array(&v)
            .into_iter()
            .filter_map(|c| c.downcast::<AXUIElement>().ok())
            .collect()
    })
    .unwrap_or_default())
}

/// Row containers (tables, outlines, lists) can hold thousands of rows, most
/// of them scrolled out of view (Fork's history). Returns, per child, whether
/// to skip it: rows that are not among `AXVisibleRows`. Indices of the kept
/// children are unchanged, so references stay valid.
fn hidden_rows(el: &AXUIElement, role: &str, kids: &[CFRetained<AXUIElement>]) -> Vec<bool> {
    const MIN_ROWS: usize = 100;
    let mut skip = vec![false; kids.len()];
    if kids.len() < MIN_ROWS || !matches!(role, "AXTable" | "AXOutline" | "AXList" | "AXGrid") {
        return skip;
    }
    let Some(visible) = attr(el, "AXVisibleRows") else {
        return skip;
    };
    // Buckets by CFHash so membership is O(1) per child even for thousands of rows.
    let visible = cf::array(&visible);
    let mut buckets: std::collections::HashMap<usize, Vec<&CFType>> = Default::default();
    for v in &visible {
        buckets.entry(CFHash(Some(v)) as usize).or_default().push(v);
    }
    let is_visible = |kid: &CFType| {
        buckets
            .get(&(CFHash(Some(kid)) as usize))
            .is_some_and(|b| b.iter().any(|v| CFEqual(Some(v), Some(kid))))
    };
    for (i, kid) in kids.iter().enumerate() {
        // Only rows are candidates; columns, headers and scroll bars are always kept.
        skip[i] = !is_visible(kid.as_ref())
            && attr(kid, "AXRole").and_then(|r| cf::string(&r)).as_deref() == Some("AXRow");
    }
    skip
}

fn has_children(el: &AXUIElement) -> bool {
    let mut n: isize = 0;
    // SAFETY: valid out pointer.
    let e = unsafe { el.attribute_value_count(&cfstr("AXChildren"), NonNull::from(&mut n)) };
    e == AXError::Success && n > 0
}

fn string_of(v: &Option<CFRetained<CFType>>) -> String {
    v.as_deref().and_then(cf::string).unwrap_or_default()
}

// ---- references ----------------------------------------------------------------

#[derive(Clone)]
enum RootKind {
    Window(u32),
    MenuBar(i32),
}

/// Where a reference is rooted and which window it reports.
#[derive(Clone)]
struct Base {
    kind: RootKind,
    window: WindowId,
}

impl Base {
    fn prefix(&self) -> String {
        match self.kind {
            RootKind::Window(n) => n.to_string(),
            RootKind::MenuBar(pid) => format!("app{pid}/menubar"),
        }
    }

    fn menubar(pid: i32) -> Self {
        let app = app_element(pid);
        let window = attr_el(&app, "AXFocusedWindow")
            .or_else(|| attr_el(&app, "AXMainWindow"))
            .and_then(|w| window_number_of(&w))
            .map(window_id)
            .unwrap_or_else(|| WindowId(format!("app{pid}")));
        Base {
            kind: RootKind::MenuBar(pid),
            window,
        }
    }
}

fn fingerprint(role: &str, subrole: &str, identifier: &str, title: &str, root: bool) -> String {
    let mut h: u32 = 0x811c_9dc5;
    let mut feed = |s: &str| {
        for b in s.bytes().chain(std::iter::once(0)) {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    };
    feed(role);
    feed(subrole);
    // The root is already identified by its window id; its title changes freely.
    if !root {
        if identifier.is_empty() {
            feed(title);
        } else {
            feed(identifier);
        }
    }
    format!("{h:08x}")
}

fn fingerprint_of(el: &AXUIElement, root: bool) -> Result<String> {
    let v = attrs(el, &["AXRole", "AXSubrole", "AXIdentifier", "AXTitle"])
        .map_err(|e| err(e, "element attributes"))?;
    Ok(fingerprint(
        &string_of(&v[0]),
        &string_of(&v[1]),
        &string_of(&v[2]),
        &string_of(&v[3]),
        root,
    ))
}

fn encode(base: &Base, path: &[usize], fp: &str) -> ElementRef {
    let p: Vec<String> = path.iter().map(usize::to_string).collect();
    ElementRef(format!("{}/{}~{fp}", base.prefix(), p.join(".")))
}

struct Resolved {
    el: CFRetained<AXUIElement>,
    root: CFRetained<AXUIElement>,
    base: Base,
    path: Vec<usize>,
}

fn invalid(id: &ElementRef) -> Error {
    Error::InvalidArgument(format!(
        "'{id}' is not an element reference (<window>/<path>~<fingerprint>)"
    ))
}

fn root_for(prefix: &str, id: &ElementRef) -> Result<(CFRetained<AXUIElement>, Base)> {
    if let Some(rest) = prefix.strip_prefix("app") {
        let pid: i32 = rest
            .strip_suffix("/menubar")
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| invalid(id))?;
        let app = app_element(pid);
        let bar = get(&app, "AXMenuBar")
            .map_err(|e| err(e, &format!("menu bar of pid {pid}")))?
            .and_then(|v| v.downcast::<AXUIElement>().ok())
            .ok_or_else(|| Error::NotFound(format!("pid {pid} has no menu bar")))?;
        return Ok((bar, Base::menubar(pid)));
    }
    let window = WindowId(prefix.to_string());
    let number = window_number(&window)?;
    let (el, _) = crate::windows::ax_window(&window)?;
    Ok((
        el,
        Base {
            kind: RootKind::Window(number),
            window,
        },
    ))
}

fn walk(root: &CFRetained<AXUIElement>, path: &[usize]) -> Result<Option<CFRetained<AXUIElement>>> {
    let mut cur = root.clone();
    for &i in path {
        let Some(next) = children(&cur)?.into_iter().nth(i) else {
            return Ok(None);
        };
        cur = next;
    }
    Ok(Some(cur))
}

/// Bounded BFS below `root` for elements satisfying `pred`; stops after two matches.
fn search(
    root: &CFRetained<AXUIElement>,
    mut pred: impl FnMut(&AXUIElement) -> bool,
) -> Result<Vec<(CFRetained<AXUIElement>, Vec<usize>)>> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.clone(), Vec::<usize>::new())]);
    let mut visited = 0usize;
    while let Some((el, path)) = queue.pop_front() {
        visited += 1;
        if visited > SEARCH_LIMIT {
            break;
        }
        if !path.is_empty() && pred(&el) {
            found.push((el.clone(), path.clone()));
            if found.len() > 1 {
                break;
            }
        }
        for (i, c) in children(&el)?.into_iter().enumerate() {
            let mut p = path.clone();
            p.push(i);
            queue.push_back((c, p));
        }
    }
    Ok(found)
}

fn resolve(id: &ElementRef) -> Result<Resolved> {
    let (left, fp) = id.0.rsplit_once('~').ok_or_else(|| invalid(id))?;
    let (prefix, path_text) = left.rsplit_once('/').ok_or_else(|| invalid(id))?;
    let path: Vec<usize> = if path_text.is_empty() {
        Vec::new()
    } else {
        path_text
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| invalid(id))?
    };
    let (root, base) = root_for(prefix, id)?;
    if let Some(el) = walk(&root, &path)?
        && fingerprint_of(&el, path.is_empty())? == fp
    {
        return Ok(Resolved {
            el,
            root,
            base,
            path,
        });
    }
    if path.is_empty() {
        return Err(Error::NotFound(format!("element {id} no longer exists")));
    }
    let mut matches = search(&root, |el| {
        fingerprint_of(el, false).is_ok_and(|f| f == fp)
    })?;
    if matches.len() != 1 {
        return Err(Error::NotFound(format!("element {id} no longer exists")));
    }
    let (el, path) = matches.remove(0);
    Ok(Resolved {
        el,
        root,
        base,
        path,
    })
}

/// Path of `el` below `root` (child indices), if it is a descendant.
fn path_between(
    root: &CFRetained<AXUIElement>,
    el: &CFRetained<AXUIElement>,
) -> Result<Option<Vec<usize>>> {
    if same(root, el) {
        return Ok(Some(Vec::new()));
    }
    // Ancestors from `el` up to (excluding) `root`.
    let mut chain = vec![el.clone()];
    let mut reached = false;
    for _ in 0..128 {
        let Some(parent) = attr_el(chain.last().expect("non-empty"), "AXParent") else {
            break;
        };
        if same(&parent, root) {
            reached = true;
            break;
        }
        chain.push(parent);
    }
    if reached {
        let mut path = Vec::with_capacity(chain.len());
        let mut parent = root.clone();
        let mut ok = true;
        for child in chain.iter().rev() {
            match children(&parent)?.iter().position(|c| same(c, child)) {
                Some(i) => path.push(i),
                None => {
                    ok = false;
                    break;
                }
            }
            parent = child.clone();
        }
        if ok {
            return Ok(Some(path));
        }
    }
    // AXParent disagrees with AXChildren: search downwards.
    let mut found = search(root, |c| same(c, el))?;
    Ok((!found.is_empty()).then(|| found.remove(0).1))
}

/// Reference root and path of an element obtained outside our own encoding.
fn locate(el: &CFRetained<AXUIElement>) -> Result<Option<Resolved>> {
    let role = get(el, "AXRole")
        .map_err(|e| err(e, "element"))?
        .and_then(|v| cf::string(&v))
        .unwrap_or_default();
    let window = if role == "AXWindow" {
        Some(el.clone())
    } else {
        attr_el(el, "AXWindow")
    };
    if let Some(win) = window
        && let Some(n) = window_number_of(&win)
        && let Some(path) = path_between(&win, el)?
    {
        return Ok(Some(Resolved {
            el: el.clone(),
            root: win,
            base: Base {
                kind: RootKind::Window(n),
                window: window_id(n),
            },
            path,
        }));
    }
    let Some(pid) = pid_of(el) else {
        return Ok(None);
    };
    let Some(bar) = attr_el(&app_element(pid), "AXMenuBar") else {
        return Ok(None);
    };
    Ok(path_between(&bar, el)?.map(|path| Resolved {
        el: el.clone(),
        root: bar,
        base: Base::menubar(pid),
        path,
    }))
}

// ---- element description ---------------------------------------------------------

const INFO_ATTRS: [&str; 19] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXIdentifier",
    "AXRoleDescription",
    "AXPosition",
    "AXSize",
    "AXEnabled",
    "AXFocused",
    "AXSelected",
    "AXValue",
    "AXMinValue",
    "AXMaxValue",
    "AXExpanded",
    "AXDisclosing",
    "AXTitleUIElement",
    "AXPlaceholderValue",
    "AXNumberOfCharacters",
];

thread_local! {
    static INFO_NAMES: CFRetained<CFArray<CFString>> = {
        let strings: Vec<CFRetained<CFString>> = INFO_ATTRS.iter().map(|n| cfstr(n)).collect();
        CFArray::from_retained_objects(&strings)
    };
}

fn map_role(role: &str, subrole: &str, parent: Option<&str>) -> Role {
    match role {
        "AXButton" | "AXDisclosureTriangle" | "AXMenuButton" => Role::Button,
        "AXCheckBox" => Role::CheckBox,
        "AXRadioButton" if parent == Some("AXTabGroup") => Role::TabItem,
        "AXRadioButton" => Role::RadioButton,
        "AXPopUpButton" | "AXComboBox" => Role::ComboBox,
        "AXTextField" | "AXSecureTextField" | "AXSearchField" => Role::Edit,
        "AXTextArea" | "AXWebArea" => Role::Document,
        "AXStaticText" => Role::Text,
        "AXLink" => Role::Hyperlink,
        "AXImage" => Role::Image,
        "AXList" => Role::List,
        "AXTable" => Role::Table,
        "AXOutline" => Role::Tree,
        "AXRow" if parent == Some("AXOutline") => Role::TreeItem,
        "AXRow" | "AXCell" => Role::DataItem,
        "AXColumn" => Role::HeaderItem,
        "AXTabGroup" => Role::Tab,
        "AXMenuBar" => Role::MenuBar,
        "AXMenu" => Role::Menu,
        "AXMenuItem" | "AXMenuBarItem" => Role::MenuItem,
        "AXSlider" => Role::Slider,
        "AXIncrementor" | "AXStepper" => Role::Spinner,
        "AXProgressIndicator" | "AXBusyIndicator" => Role::ProgressBar,
        "AXScrollBar" => Role::ScrollBar,
        "AXScrollArea" | "AXSplitGroup" | "AXLayoutArea" => Role::Pane,
        "AXGroup" | "AXRadioGroup" => Role::Group,
        "AXToolbar" => Role::ToolBar,
        "AXWindow" | "AXSheet" | "AXDrawer" => Role::Window,
        "AXSplitter" => Role::Separator,
        "AXValueIndicator" => Role::Thumb,
        "AXHelpTag" => Role::ToolTip,
        "AXHeading" => Role::Header,
        "AXDateField" => Role::Calendar,
        _ if subrole == "AXSwitch" => Role::CheckBox,
        _ => Role::Custom,
    }
}

/// Roles whose parent role changes the mapping.
fn needs_parent(role: &str) -> bool {
    matches!(role, "AXRow" | "AXRadioButton" | "AXCell")
}

/// Per-call context: display geometry for the offscreen test and the element
/// that really has keyboard focus.
struct Ctx {
    displays: Vec<Rect>,
    /// Some apps (Fork) answer `AXFocused = true` for every cell of a table;
    /// focus is only trusted when the system-wide focused element agrees.
    focused: Option<CFRetained<AXUIElement>>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            displays: crate::display::displays()
                .map(|d| d.into_iter().map(|(_, r, _)| r).collect())
                .unwrap_or_default(),
            focused: attr_el(&system_wide(), "AXFocusedUIElement"),
        }
    }

    fn is_focused(&self, el: &AXUIElement) -> bool {
        self.focused
            .as_deref()
            .is_some_and(|f| CFEqual(Some(f.as_ref()), Some(el.as_ref())))
    }
}

/// An on-screen pop-up/context menu window (layer 101, kCGPopUpMenuWindowLevel)
/// owned by `pid`.
fn menu_open(pid: Option<i32>) -> bool {
    const POPUP_MENU_LAYER: f64 = 101.0;
    let Some(pid) = pid else {
        return false;
    };
    let Some(list) = objc2_core_graphics::CGWindowListCopyWindowInfo(
        objc2_core_graphics::CGWindowListOption::OptionOnScreenOnly,
        objc2_core_graphics::kCGNullWindowID,
    ) else {
        return false;
    };
    cf::array(&list).iter().any(|w| {
        let Some(d) = w.downcast_ref::<objc2_core_foundation::CFDictionary>() else {
            return false;
        };
        let num = |k: &str| cf::dict_get(d, k).and_then(|v| cf::number(&v));
        num("kCGWindowOwnerPID") == Some(pid as f64) && num("kCGWindowLayer") == Some(POPUP_MENU_LAYER)
    })
}

/// Description of an element plus its raw role (children need it for mapping).
fn describe(
    ctx: &Ctx,
    el: &AXUIElement,
    base: &Base,
    path: &[usize],
    parent_role: Option<&str>,
) -> Result<(ElementInfo, String)> {
    let v = INFO_NAMES
        .with(|names| copy_multiple(el, names.as_opaque(), INFO_ATTRS.len()))
        .map_err(|e| err(e, "element attributes"))?;
    let s = |i: usize| string_of(&v[i]);
    let num = |i: usize| v[i].as_deref().and_then(cf::number);
    let flag = |i: usize| v[i].as_deref().and_then(cf::boolean);

    let ax_role = s(0);
    let subrole = s(1);
    let title = s(2);
    let identifier = s(4);
    let fetched_parent;
    let parent_role = match parent_role {
        Some(p) => Some(p),
        None if needs_parent(&ax_role) => {
            fetched_parent = attr_el(el, "AXParent")
                .and_then(|p| attr(&p, "AXRole"))
                .and_then(|r| cf::string(&r));
            fetched_parent.as_deref()
        }
        None => None,
    };
    let role = map_role(&ax_role, &subrole, parent_role);
    let fp = fingerprint(&ax_role, &subrole, &identifier, &title, path.is_empty());

    let name = [title.clone(), s(3)]
        .into_iter()
        .find(|n| !n.is_empty())
        .or_else(|| {
            v[16]
                .clone()
                .and_then(|t| t.downcast::<AXUIElement>().ok())
                .and_then(|t| attr(&t, "AXValue"))
                .and_then(|t| cf::string(&t))
                .filter(|n| !n.is_empty())
        })
        .or_else(|| Some(s(17)).filter(|n| !n.is_empty()))
        .or_else(|| (ax_role == "AXRow").then(|| row_text(el)).flatten())
        .unwrap_or_default();

    let bounds = match (
        v[6].as_deref().and_then(point),
        v[7].as_deref().and_then(size),
    ) {
        (Some(p), Some(sz)) => Some(crate::display::rect_out(objc2_core_foundation::CGRect {
            origin: p,
            size: sz,
        })),
        _ => None,
    }
    .filter(|r| !r.is_empty());
    let offscreen = match bounds {
        None => true,
        Some(b) => {
            !ctx.displays.is_empty() && !ctx.displays.iter().any(|d| d.intersect(&b).is_some())
        }
    };

    let is_switch = role == Role::CheckBox;
    let is_choice = matches!(role, Role::RadioButton | Role::TabItem);
    let value_obj = v[11].as_deref();
    let value_str = value_obj.and_then(cf::string);
    let value_num = value_obj
        .filter(|o| o.downcast_ref::<CFString>().is_none())
        .and_then(cf::number);
    let value_settable = value_obj.is_some() && settable(el, "AXValue");
    let focusable = settable(el, "AXFocused");
    let selected_settable = v[10].is_some() && settable(el, "AXSelected");
    let expanded_settable = v[14].is_some() && settable(el, "AXExpanded");
    let disclosing_settable = v[15].is_some() && settable(el, "AXDisclosing");
    let names = action_names(el);
    let has = |a: &str| names.iter().any(|n| n == a);
    let is_text_role = matches!(
        ax_role.as_str(),
        "AXTextField" | "AXSecureTextField" | "AXSearchField" | "AXTextArea" | "AXComboBox"
    );
    let text_capable = (is_text_role && value_str.is_some()) || v[18].is_some();

    let mut actions = Vec::new();
    let mut push = |a: ActionKind| {
        if !actions.contains(&a) {
            actions.push(a);
        }
    };
    if has("AXPress") {
        push(ActionKind::Invoke);
        if is_switch {
            push(ActionKind::Toggle);
        }
        if is_choice {
            push(ActionKind::Select);
        }
    }
    // Cells are selected through their row; `parent_role` is the row's AX role.
    let cell_in_row = ax_role == "AXCell" && parent_role == Some("AXRow");
    if has("AXPick") || selected_settable || cell_in_row {
        push(ActionKind::Select);
    }
    let menu_expandable = has("AXShowMenu")
        && matches!(
            ax_role.as_str(),
            "AXPopUpButton" | "AXComboBox" | "AXMenuButton" | "AXMenuBarItem" | "AXMenuItem"
        );
    if menu_expandable || expanded_settable || disclosing_settable {
        push(ActionKind::ExpandCollapse);
    }
    if value_settable && value_str.is_some() {
        push(ActionKind::SetValue);
    }
    if value_settable && value_num.is_some() && !is_switch && !is_choice {
        push(ActionKind::SetRange);
    }
    if has("AXScrollToVisible") {
        push(ActionKind::ScrollIntoView);
    }
    if focusable {
        push(ActionKind::Focus);
    }
    if has("AXPress") {
        push(ActionKind::DefaultAction);
    }
    if text_capable {
        push(ActionKind::Text);
    }

    let selected = if is_choice {
        value_num.map(|n| n == 1.0).or(flag(10))
    } else {
        flag(10)
    };
    let toggle = is_switch.then(|| match value_num.map(|n| n as i64) {
        Some(1) => ToggleState::On,
        Some(2) => ToggleState::Indeterminate,
        _ => ToggleState::Off,
    });
    // Only controls that really open and close carry an expand state; web
    // content reports `AXExpanded = false` on every element.
    let can_expand = expanded_settable
        || disclosing_settable
        || matches!(
            ax_role.as_str(),
            "AXRow" | "AXDisclosureTriangle" | "AXPopUpButton" | "AXComboBox" | "AXMenuButton"
        );
    let expanded = if !can_expand {
        None
    } else if ax_role == "AXRow" {
        flag(15).or(flag(14))
    } else if matches!(ax_role.as_str(), "AXPopUpButton" | "AXMenuButton") {
        // Chromium never updates AXExpanded on a <select>; an open pop-up menu
        // is a menu-level window of the same process.
        Some(flag(14) == Some(true) || (ctx.is_focused(el) && menu_open(pid_of(el))))
    } else {
        flag(14)
    };
    let expand = expanded.map(|e| {
        if e {
            ExpandState::Expanded
        } else {
            ExpandState::Collapsed
        }
    });
    let value = if is_switch || is_choice || ax_role == "AXDisclosureTriangle" {
        None
    } else if let Some(s) = value_str {
        Some(s)
    } else if matches!(
        role,
        Role::Slider | Role::ProgressBar | Role::Spinner | Role::ScrollBar
    ) {
        value_num.map(|n| n.to_string())
    } else {
        None
    };
    let range = match (value_num, num(12), num(13)) {
        (Some(value), Some(min), Some(max)) if !is_switch && !is_choice => {
            Some(RangeInfo { value, min, max })
        }
        _ => None,
    };
    // `AXComboBox` is the editable kind (text + list); `AXPopUpButton` is a
    // closed menu, left as `None`.
    let read_only = (matches!(role, Role::Edit | Role::Document) || ax_role == "AXComboBox")
        .then_some(!value_settable)
        .filter(|_| value_obj.is_some() || ax_role != "AXWebArea");
    let native_role = if subrole.is_empty() {
        ax_role.clone()
    } else {
        format!("{ax_role}/{subrole}")
    };
    let info = ElementInfo {
        id: encode(base, path, &fp),
        window: base.window.clone(),
        role,
        native_role: (!native_role.is_empty()).then_some(native_role),
        name,
        automation_id: identifier,
        class_name: s(5),
        bounds,
        enabled: flag(8).unwrap_or(true),
        focused: flag(9).unwrap_or(false) && ctx.is_focused(el),
        focusable,
        offscreen,
        selected,
        toggle,
        expand,
        value,
        read_only,
        range,
        actions,
        native_window: None,
        pid: pid_of(el).unwrap_or(0).max(0) as u32,
    };
    Ok((info, ax_role))
}

fn describe_resolved(ctx: &Ctx, r: &Resolved) -> Result<ElementInfo> {
    describe(ctx, &r.el, &r.base, &r.path, None).map(|(i, _)| i)
}

// ---- public API ----------------------------------------------------------------------

pub fn tree(root: &TreeRoot, options: TreeOptions) -> Result<ElementNode> {
    let r = match root {
        TreeRoot::Window(w) => {
            let number = window_number(w)?;
            let (el, _) = crate::windows::ax_window(w)?;
            Resolved {
                root: el.clone(),
                el,
                base: Base {
                    kind: RootKind::Window(number),
                    window: w.clone(),
                },
                path: Vec::new(),
            }
        }
        TreeRoot::Element(e) => resolve(e)?,
    };
    let ctx = Ctx::new();
    struct N {
        info: ElementInfo,
        children: Vec<usize>,
        truncated: bool,
    }
    let (info, role) = describe(&ctx, &r.el, &r.base, &r.path, None)?;
    let mut nodes = vec![N {
        info,
        children: vec![],
        truncated: false,
    }];
    let mut queue = VecDeque::from([(0usize, r.el.clone(), r.path.clone(), role, 0usize)]);
    let mut exhausted = false;
    while let Some((idx, el, path, role, depth)) = queue.pop_front() {
        if exhausted || depth >= options.max_depth {
            nodes[idx].truncated = has_children(&el);
            continue;
        }
        let kids = match children(&el) {
            Ok(k) => k,
            // A subtree that stops answering is reported as unread, not fatal.
            Err(e @ Error::PermissionRequired(_)) => return Err(e),
            Err(_) => {
                nodes[idx].truncated = true;
                continue;
            }
        };
        let skip = hidden_rows(&el, &role, &kids);
        if skip.iter().any(|s| *s) {
            // Scrolled-out rows are not read; scroll to reach them.
            nodes[idx].truncated = true;
        }
        for (i, c) in kids.into_iter().enumerate() {
            if skip[i] {
                continue;
            }
            if nodes.len() >= options.max_nodes {
                nodes[idx].truncated = true;
                exhausted = true;
                break;
            }
            let mut p = path.clone();
            p.push(i);
            let Ok((info, crole)) = describe(&ctx, &c, &r.base, &p, Some(&role)) else {
                continue;
            };
            let ci = nodes.len();
            nodes.push(N {
                info,
                children: vec![],
                truncated: false,
            });
            nodes[idx].children.push(ci);
            queue.push_back((ci, c, p, crole, depth + 1));
        }
    }
    fn build(nodes: &mut Vec<Option<N>>, i: usize) -> ElementNode {
        let n = nodes[i].take().expect("each node is visited once");
        ElementNode {
            info: n.info,
            children: n.children.iter().map(|c| build(nodes, *c)).collect(),
            truncated: n.truncated,
        }
    }
    let mut slots: Vec<Option<N>> = nodes.into_iter().map(Some).collect();
    Ok(build(&mut slots, 0))
}

pub fn element(id: &ElementRef) -> Result<ElementInfo> {
    let r = resolve(id)?;
    describe_resolved(&Ctx::new(), &r)
}

pub fn parent(id: &ElementRef) -> Result<Option<ElementInfo>> {
    let r = resolve(id)?;
    let Some((_, parent_path)) = r.path.split_last() else {
        // The window (or menu bar) is the top of the reference space.
        return Ok(None);
    };
    let Some(el) = walk(&r.root, parent_path)? else {
        return Ok(None);
    };
    let ctx = Ctx::new();
    describe(&ctx, &el, &r.base, parent_path, None).map(|(i, _)| Some(i))
}

fn describe_foreign(el: CFRetained<AXUIElement>) -> Result<Option<ElementInfo>> {
    match locate(&el)? {
        Some(r) => describe_resolved(&Ctx::new(), &r).map(Some),
        None => Ok(None),
    }
}

pub fn focused() -> Result<Option<ElementInfo>> {
    let sys = system_wide();
    let el = match get(&sys, "AXFocusedUIElement") {
        Ok(v) => v.and_then(|v| v.downcast::<AXUIElement>().ok()),
        Err(AXError::APIDisabled) => return Err(Error::PermissionRequired(AX_PERMISSION.into())),
        Err(_) => None,
    };
    match el {
        Some(el) => describe_foreign(el),
        None => Ok(None),
    }
}

pub fn at(p: Point) -> Result<Option<ElementInfo>> {
    let sys = system_wide();
    let mut out: *const AXUIElement = ptr::null();
    // SAFETY: valid out pointer (+1 on success).
    let e = unsafe { sys.copy_element_at_position(p.x as f32, p.y as f32, NonNull::from(&mut out)) };
    match e {
        AXError::Success => {}
        AXError::APIDisabled => return Err(Error::PermissionRequired(AX_PERMISSION.into())),
        _ => return Ok(None),
    }
    let Some(el) = NonNull::new(out.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) }) else {
        return Ok(None);
    };
    describe_foreign(el)
}

pub fn text(id: &ElementRef) -> Result<Option<String>> {
    let r = resolve(id)?;
    let v = attrs(&r.el, &["AXRole", "AXValue", "AXNumberOfCharacters"])
        .map_err(|e| err(e, "text"))?;
    let role = string_of(&v[0]);
    let text_role = matches!(
        role.as_str(),
        "AXTextArea" | "AXTextField" | "AXSecureTextField" | "AXSearchField" | "AXComboBox"
    );
    if text_role && let Some(s) = v[1].as_deref().and_then(cf::string) {
        return Ok(Some(s));
    }
    // Web groups report `AXNumberOfCharacters = 0`; that is "no text", so the
    // caller falls back to the accessible names of the subtree.
    let Some(n) = v[2].as_deref().and_then(cf::number).filter(|n| *n > 0.0) else {
        return Ok(None);
    };
    let mut range = CFRange {
        location: 0,
        length: n as isize,
    };
    // SAFETY: pointer to a live CFRange matching the type tag.
    let Some(param) = (unsafe { AXValue::new(AXValueType::CFRange, NonNull::from(&mut range).cast()) })
    else {
        return Err(Error::Platform("AXValueCreate(CFRange) failed".into()));
    };
    let mut out: *const CFType = ptr::null();
    // SAFETY: valid parameter and out pointer (+1 on success).
    let e = unsafe {
        r.el.copy_parameterized_attribute_value(
            &cfstr("AXStringForRange"),
            &param,
            NonNull::from(&mut out),
        )
    };
    match e {
        AXError::Success => Ok(NonNull::new(out.cast_mut())
            .map(|p| unsafe { CFRetained::<CFType>::from_raw(p) })
            .and_then(|v| cf::string(&v))
            .filter(|s| !s.is_empty())),
        AXError::APIDisabled => Err(Error::PermissionRequired(AX_PERMISSION.into())),
        AXError::InvalidUIElement => Err(err(e, "text")),
        _ => Ok(None),
    }
}

fn press(el: &AXUIElement, ctx: &str) -> Result<()> {
    perform(el, "AXPress").map_err(|e| err(e, ctx))
}

fn expand(el: &AXUIElement, open: bool) -> Result<()> {
    let ctx = if open { "expand" } else { "collapse" };
    for attr_name in ["AXExpanded", "AXDisclosing"] {
        if attr(el, attr_name).is_some() && settable(el, attr_name) {
            return set(el, attr_name, CFBoolean::new(open)).map_err(|e| err(e, ctx));
        }
    }
    let names = action_names(el);
    let action = if open { "AXShowMenu" } else { "AXCancel" };
    if names.iter().any(|n| n == action) {
        return perform(el, action).map_err(|e| err(e, ctx));
    }
    Err(Error::Unsupported(format!(
        "{ctx}: element has no AXExpanded/AXDisclosing or {action}"
    )))
}

/// Text of a table/outline row, which usually has no title of its own: the
/// first non-empty title/value among its cells (one level of nesting).
fn row_text(row: &AXUIElement) -> Option<String> {
    let text_of = |e: &AXUIElement| {
        ["AXTitle", "AXValue", "AXDescription"]
            .into_iter()
            .filter_map(|a| attr(e, a).and_then(|v| cf::string(&v)))
            .find(|s| !s.trim().is_empty())
            .or_else(|| {
                attr_el(e, "AXTitleUIElement")
                    .and_then(|t| attr(&t, "AXValue"))
                    .and_then(|v| cf::string(&v))
                    .filter(|s| !s.trim().is_empty())
            })
    };
    let cells = attr(row, "AXChildren").map(|c| cf::array(&c)).unwrap_or_default();
    cells
        .iter()
        .filter_map(|c| c.downcast_ref::<AXUIElement>())
        .find_map(|cell| {
            text_of(cell).or_else(|| {
                attr(cell, "AXChildren")
                    .map(|c| cf::array(&c))
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|c| c.downcast_ref::<AXUIElement>())
                    .find_map(text_of)
            })
        })
}

fn select(el: &CFRetained<AXUIElement>) -> Result<()> {
    // A cell is selected through its row (Finder, Fork and most NSTableViews).
    if attr(el, "AXRole").and_then(|r| cf::string(&r)).as_deref() == Some("AXCell")
        && let Some(row) = attr_el(el, "AXParent")
        && attr(&row, "AXRole").and_then(|r| cf::string(&r)).as_deref() == Some("AXRow")
    {
        return select(&row);
    }
    if attr(el, "AXSelected").is_some() && settable(el, "AXSelected") {
        return set(el, "AXSelected", CFBoolean::new(true)).map_err(|e| err(e, "select"));
    }
    let names = action_names(el);
    if names.iter().any(|n| n == "AXPick") {
        return perform(el, "AXPick").map_err(|e| err(e, "select"));
    }
    let role = attr(el, "AXRole")
        .and_then(|r| cf::string(&r))
        .unwrap_or_default();
    if role == "AXRadioButton" && names.iter().any(|n| n == "AXPress") {
        return press(el, "select");
    }
    if let Some(parent) = attr_el(el, "AXParent")
        && attr(&parent, "AXSelectedRows").is_some()
        && settable(&parent, "AXSelectedRows")
    {
        let rows = CFArray::from_retained_objects(std::slice::from_ref(el));
        return set(&parent, "AXSelectedRows", &rows).map_err(|e| err(e, "select"));
    }
    Err(Error::Unsupported(
        "select: element is not selectable through accessibility".into(),
    ))
}

fn focus(el: &AXUIElement) -> Result<()> {
    if let Some(win) = attr_el(el, "AXWindow") {
        let _ = perform(&win, "AXRaise");
        let _ = set(&win, "AXMain", CFBoolean::new(true));
    }
    set(el, "AXFocused", CFBoolean::new(true)).map_err(|e| err(e, "focus"))
}

pub fn action(id: &ElementRef, op: &AxOp) -> Result<()> {
    let r = resolve(id)?;
    let el = &r.el;
    match op {
        AxOp::Invoke | AxOp::DefaultAction | AxOp::Toggle => press(el, &format!("{op:?}")),
        AxOp::Select => select(el),
        AxOp::Expand => expand(el, true),
        AxOp::Collapse => expand(el, false),
        AxOp::SetValue(v) => set(el, "AXValue", &cfstr(v)).map_err(|e| err(e, "set value")),
        AxOp::SetRange(v) => {
            set(el, "AXValue", &CFNumber::new_f64(*v)).map_err(|e| err(e, "set range"))
        }
        AxOp::Focus => focus(el),
        AxOp::ScrollIntoView => {
            perform(el, "AXScrollToVisible").map_err(|e| err(e, "scroll into view"))
        }
    }
}
