//! UI Automation client: cached tree reads, element resolution by runtime id
//! and pattern-based actions.
//!
//! Element reference format: `<window hwnd>/<runtime id parts in hex>`, e.g.
//! `0x5E2A10/2A.5E2A10.4.1`. Runtime ids stay valid while the element exists,
//! so references work across one-shot processes.

use std::collections::VecDeque;

use deskpilot_core::model::{
    ActionKind, ElementInfo, ElementNode, ElementRef, ExpandState, Point, RangeInfo, Role,
    ToggleState, WindowId,
};
use deskpilot_core::platform::{AxOp, TreeOptions, TreeRoot};
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayCreateVector, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayPutElement,
};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VT_ARRAY, VT_BOOL, VT_BSTR, VT_I4, VT_R8};
use windows::Win32::UI::Accessibility::*;
use windows::core::{BSTR, Interface};

use crate::util::{Var, hwnd_id, rect};
use crate::win::root_of;

const PROPS: [UIA_PROPERTY_ID; 31] = [
    UIA_RuntimeIdPropertyId,
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_LocalizedControlTypePropertyId,
    UIA_AutomationIdPropertyId,
    UIA_ClassNamePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsKeyboardFocusablePropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_ProcessIdPropertyId,
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_IsRangeValuePatternAvailablePropertyId,
    UIA_IsScrollItemPatternAvailablePropertyId,
    UIA_IsLegacyIAccessiblePatternAvailablePropertyId,
    UIA_IsTextPatternAvailablePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_RangeValueValuePropertyId,
    UIA_RangeValueMinimumPropertyId,
    UIA_RangeValueMaximumPropertyId,
    UIA_LegacyIAccessibleDefaultActionPropertyId,
];

pub struct Uia {
    auto: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    walker: IUIAutomationTreeWalker,
}

fn uia_err(context: &str, e: windows::core::Error) -> Error {
    // UIA_E_ELEMENTNOTAVAILABLE: the element is gone.
    if e.code().0 as u32 == 0x8004_0201 {
        return Error::NotFound(format!("{context}: element no longer exists"));
    }
    if e.code().0 as u32 == 0x8004_0200 {
        return Error::Platform(format!("{context}: element is not enabled"));
    }
    Error::Platform(format!("{context}: {e}"))
}

impl Uia {
    pub fn new() -> Result<Self> {
        unsafe {
            // S_FALSE / RPC_E_CHANGED_MODE are fine: COM is usable on this thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| uia_err("CoCreateInstance(CUIAutomation8)", e))?;
            if let Ok(auto2) = auto.cast::<IUIAutomation2>() {
                // Do not hang forever on frozen applications.
                let _ = auto2.SetConnectionTimeout(5_000);
                let _ = auto2.SetTransactionTimeout(10_000);
            }
            let cache = auto
                .CreateCacheRequest()
                .map_err(|e| uia_err("CreateCacheRequest", e))?;
            for p in PROPS {
                cache
                    .AddProperty(p)
                    .map_err(|e| uia_err("AddProperty", e))?;
            }
            let walker = auto
                .ControlViewWalker()
                .map_err(|e| uia_err("ControlViewWalker", e))?;
            Ok(Self {
                auto,
                cache,
                walker,
            })
        }
    }

    // ---- cached property readers -----------------------------------------

    fn var(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<Var> {
        unsafe { el.GetCachedPropertyValueEx(id, false).ok().map(Var) }
    }

    fn bool_prop(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<bool> {
        let v = Self::var(el, id)?;
        (v.vt() == VT_BOOL.0).then(|| unsafe { v.0.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
    }

    fn i32_prop(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<i32> {
        let v = Self::var(el, id)?;
        (v.vt() == VT_I4.0).then(|| unsafe { v.0.Anonymous.Anonymous.Anonymous.lVal })
    }

    fn f64_prop(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<f64> {
        let v = Self::var(el, id)?;
        (v.vt() == VT_R8.0).then(|| unsafe { v.0.Anonymous.Anonymous.Anonymous.dblVal })
    }

    fn str_prop(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<String> {
        let v = Self::var(el, id)?;
        (v.vt() == VT_BSTR.0)
            .then(|| unsafe { v.0.Anonymous.Anonymous.Anonymous.bstrVal.to_string() })
    }

    fn runtime_id(el: &IUIAutomationElement) -> Vec<i32> {
        let Some(v) = Self::var(el, UIA_RuntimeIdPropertyId) else {
            return vec![];
        };
        if v.vt() != (VT_ARRAY.0 | VT_I4.0) {
            return vec![];
        }
        unsafe {
            let psa = v.0.Anonymous.Anonymous.Anonymous.parray;
            safearray_i32(psa)
        }
    }

    fn native_hwnd(el: &IUIAutomationElement) -> Option<HWND> {
        unsafe {
            el.CachedNativeWindowHandle()
                .ok()
                .filter(|h| !h.is_invalid())
        }
    }

    // ---- element description -----------------------------------------------

    fn info(&self, el: &IUIAutomationElement, window: &WindowId) -> ElementInfo {
        let rid = Self::runtime_id(el);
        let id = ElementRef(format!(
            "{window}/{}",
            rid.iter()
                .map(|p| format!("{:X}", *p as u32))
                .collect::<Vec<_>>()
                .join(".")
        ));
        let control_type = unsafe { el.CachedControlType().map(|c| c.0).unwrap_or(0) };
        let role = role_for(control_type);
        let has = |p| Self::bool_prop(el, p).unwrap_or(false);
        let mut actions = Vec::new();
        if has(UIA_IsInvokePatternAvailablePropertyId) {
            actions.push(ActionKind::Invoke);
        }
        let selectable = has(UIA_IsSelectionItemPatternAvailablePropertyId);
        if selectable {
            actions.push(ActionKind::Select);
        }
        let togglable = has(UIA_IsTogglePatternAvailablePropertyId);
        if togglable {
            actions.push(ActionKind::Toggle);
        }
        let expandable = has(UIA_IsExpandCollapsePatternAvailablePropertyId);
        if expandable {
            actions.push(ActionKind::ExpandCollapse);
        }
        let has_value = has(UIA_IsValuePatternAvailablePropertyId);
        let read_only =
            has_value.then(|| Self::bool_prop(el, UIA_ValueIsReadOnlyPropertyId).unwrap_or(false));
        if has_value && read_only == Some(false) {
            actions.push(ActionKind::SetValue);
        }
        let ranged = has(UIA_IsRangeValuePatternAvailablePropertyId);
        if ranged {
            actions.push(ActionKind::SetRange);
        }
        if has(UIA_IsScrollItemPatternAvailablePropertyId) {
            actions.push(ActionKind::ScrollIntoView);
        }
        let focusable = has(UIA_IsKeyboardFocusablePropertyId);
        if focusable {
            actions.push(ActionKind::Focus);
        }
        if has(UIA_IsLegacyIAccessiblePatternAvailablePropertyId)
            && Self::str_prop(el, UIA_LegacyIAccessibleDefaultActionPropertyId)
                .is_some_and(|a| !a.is_empty())
        {
            actions.push(ActionKind::DefaultAction);
        }
        if has(UIA_IsTextPatternAvailablePropertyId) {
            actions.push(ActionKind::Text);
        }
        let bounds = unsafe { el.CachedBoundingRectangle().ok() }
            .map(rect)
            .filter(|r| !r.is_empty());
        let str_or_empty = |p| Self::str_prop(el, p).unwrap_or_default();
        ElementInfo {
            id,
            window: window.clone(),
            role,
            native_role: (role == Role::Custom)
                .then(|| str_or_empty(UIA_LocalizedControlTypePropertyId))
                .filter(|s| !s.is_empty()),
            name: str_or_empty(UIA_NamePropertyId),
            automation_id: str_or_empty(UIA_AutomationIdPropertyId),
            class_name: str_or_empty(UIA_ClassNamePropertyId),
            bounds,
            enabled: has(UIA_IsEnabledPropertyId),
            focused: has(UIA_HasKeyboardFocusPropertyId),
            focusable,
            offscreen: has(UIA_IsOffscreenPropertyId),
            selected: selectable.then(|| has(UIA_SelectionItemIsSelectedPropertyId)),
            toggle: togglable.then(
                || match Self::i32_prop(el, UIA_ToggleToggleStatePropertyId) {
                    Some(1) => ToggleState::On,
                    Some(2) => ToggleState::Indeterminate,
                    _ => ToggleState::Off,
                },
            ),
            expand: expandable.then(|| {
                match Self::i32_prop(el, UIA_ExpandCollapseExpandCollapseStatePropertyId) {
                    Some(1) => ExpandState::Expanded,
                    Some(2) => ExpandState::PartiallyExpanded,
                    Some(3) => ExpandState::LeafNode,
                    _ => ExpandState::Collapsed,
                }
            }),
            value: if has_value {
                Self::str_prop(el, UIA_ValueValuePropertyId)
            } else {
                None
            },
            read_only,
            range: ranged.then(|| RangeInfo {
                value: Self::f64_prop(el, UIA_RangeValueValuePropertyId).unwrap_or(0.0),
                min: Self::f64_prop(el, UIA_RangeValueMinimumPropertyId).unwrap_or(0.0),
                max: Self::f64_prop(el, UIA_RangeValueMaximumPropertyId).unwrap_or(0.0),
            }),
            native_window: Self::native_hwnd(el).map(hwnd_id),
            pid: Self::i32_prop(el, UIA_ProcessIdPropertyId).unwrap_or(0) as u32,
            actions,
        }
    }

    /// Top-level window that contains an element (walks up to the first native window).
    fn top_window(&self, el: &IUIAutomationElement) -> Option<WindowId> {
        let mut cur = el.clone();
        for _ in 0..64 {
            if let Some(h) = Self::native_hwnd(&cur) {
                return Some(hwnd_id(root_of(h)));
            }
            cur = unsafe { self.walker.GetParentElementBuildCache(&cur, &self.cache) }.ok()?;
        }
        None
    }

    // ---- resolution ----------------------------------------------------------

    fn window_root(&self, hwnd: HWND) -> Result<IUIAutomationElement> {
        unsafe { self.auto.ElementFromHandleBuildCache(hwnd, &self.cache) }
            .map_err(|e| uia_err("ElementFromHandle", e))
    }

    fn resolve(&self, id: &ElementRef) -> Result<(IUIAutomationElement, WindowId)> {
        let (win, rid_text) = id.0.split_once('/').ok_or_else(|| {
            Error::InvalidArgument(format!(
                "'{id}' is not an element reference (window/runtime-id)"
            ))
        })?;
        let window = WindowId(win.to_string());
        let hwnd = crate::win::existing(&window)?;
        let rid: Vec<i32> = rid_text
            .split('.')
            .filter(|s| !s.is_empty())
            .map(|p| u32::from_str_radix(p, 16).map(|v| v as i32))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| Error::InvalidArgument(format!("'{id}' has an invalid runtime id")))?;
        let root = self.window_root(hwnd)?;
        if Self::runtime_id(&root) == rid {
            return Ok((root, window));
        }
        let cond = unsafe {
            let var = runtime_id_variant(&rid)?;
            self.auto
                .CreatePropertyCondition(UIA_RuntimeIdPropertyId, &var.0)
                .map_err(|e| uia_err("CreatePropertyCondition", e))?
        };
        let el = unsafe { root.FindFirstBuildCache(TreeScope_Descendants, &cond, &self.cache) }
            .map_err(|_| Error::NotFound(format!("element {id} no longer exists")))?;
        Ok((el, window))
    }

    // ---- public API ------------------------------------------------------------

    pub fn element(&self, id: &ElementRef) -> Result<ElementInfo> {
        let (el, window) = self.resolve(id)?;
        Ok(self.info(&el, &window))
    }

    pub fn tree(&self, root: &TreeRoot, options: TreeOptions) -> Result<ElementNode> {
        let (root_el, window) = match root {
            TreeRoot::Window(w) => (self.window_root(crate::win::existing(w)?)?, w.clone()),
            TreeRoot::Element(e) => self.resolve(e)?,
        };
        struct N {
            info: ElementInfo,
            children: Vec<usize>,
            truncated: bool,
        }
        let mut nodes = vec![N {
            info: self.info(&root_el, &window),
            children: vec![],
            truncated: false,
        }];
        let mut queue = VecDeque::from([(0usize, root_el, 0usize)]);
        let mut exhausted = false;
        while let Some((idx, el, depth)) = queue.pop_front() {
            if exhausted || depth >= options.max_depth {
                nodes[idx].truncated = unsafe { self.walker.GetFirstChildElement(&el) }.is_ok();
                continue;
            }
            let mut child =
                unsafe { self.walker.GetFirstChildElementBuildCache(&el, &self.cache) }.ok();
            while let Some(c) = child {
                if nodes.len() >= options.max_nodes {
                    nodes[idx].truncated = true;
                    exhausted = true;
                    break;
                }
                let ci = nodes.len();
                nodes.push(N {
                    info: self.info(&c, &window),
                    children: vec![],
                    truncated: false,
                });
                nodes[idx].children.push(ci);
                child =
                    unsafe { self.walker.GetNextSiblingElementBuildCache(&c, &self.cache) }.ok();
                queue.push_back((ci, c, depth + 1));
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

    pub fn parent(&self, id: &ElementRef) -> Result<Option<ElementInfo>> {
        let (el, window) = self.resolve(id)?;
        let Ok(parent) = (unsafe { self.walker.GetParentElementBuildCache(&el, &self.cache) })
        else {
            return Ok(None);
        };
        let desktop =
            unsafe { self.auto.GetRootElement() }.map_err(|e| uia_err("GetRootElement", e))?;
        if unsafe { self.auto.CompareElements(&parent, &desktop) }
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            return Ok(None);
        }
        Ok(Some(self.info(&parent, &window)))
    }

    pub fn focused(&self) -> Result<Option<ElementInfo>> {
        let Ok(el) = (unsafe { self.auto.GetFocusedElementBuildCache(&self.cache) }) else {
            return Ok(None);
        };
        Ok(self.top_window(&el).map(|w| self.info(&el, &w)))
    }

    pub fn at(&self, p: Point) -> Result<Option<ElementInfo>> {
        let Ok(el) = (unsafe {
            self.auto
                .ElementFromPointBuildCache(POINT { x: p.x, y: p.y }, &self.cache)
        }) else {
            return Ok(None);
        };
        Ok(self.top_window(&el).map(|w| self.info(&el, &w)))
    }

    pub fn text(&self, id: &ElementRef) -> Result<Option<String>> {
        let (el, _) = self.resolve(id)?;
        let Ok(pattern) =
            (unsafe { el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) })
        else {
            return Ok(None);
        };
        let range = unsafe { pattern.DocumentRange() }.map_err(|e| uia_err("DocumentRange", e))?;
        let text = unsafe { range.GetText(-1) }.map_err(|e| uia_err("GetText", e))?;
        Ok(Some(text.to_string()))
    }

    pub fn action(&self, id: &ElementRef, op: &AxOp) -> Result<()> {
        let (el, _) = self.resolve(id)?;
        unsafe fn pattern<T: Interface>(
            el: &IUIAutomationElement,
            pid: UIA_PATTERN_ID,
            name: &str,
        ) -> Result<T> {
            unsafe { el.GetCurrentPatternAs::<T>(pid) }
                .map_err(|_| Error::Unsupported(format!("element has no {name} pattern")))
        }
        let ctx = format!("{op:?}");
        unsafe {
            let r = match op {
                AxOp::Invoke => {
                    pattern::<IUIAutomationInvokePattern>(&el, UIA_InvokePatternId, "Invoke")?
                        .Invoke()
                }
                AxOp::Select => pattern::<IUIAutomationSelectionItemPattern>(
                    &el,
                    UIA_SelectionItemPatternId,
                    "SelectionItem",
                )?
                .Select(),
                AxOp::Toggle => {
                    pattern::<IUIAutomationTogglePattern>(&el, UIA_TogglePatternId, "Toggle")?
                        .Toggle()
                }
                AxOp::Expand => pattern::<IUIAutomationExpandCollapsePattern>(
                    &el,
                    UIA_ExpandCollapsePatternId,
                    "ExpandCollapse",
                )?
                .Expand(),
                AxOp::Collapse => pattern::<IUIAutomationExpandCollapsePattern>(
                    &el,
                    UIA_ExpandCollapsePatternId,
                    "ExpandCollapse",
                )?
                .Collapse(),
                AxOp::SetValue(v) => {
                    pattern::<IUIAutomationValuePattern>(&el, UIA_ValuePatternId, "Value")?
                        .SetValue(&BSTR::from(v.as_str()))
                }
                AxOp::SetRange(v) => pattern::<IUIAutomationRangeValuePattern>(
                    &el,
                    UIA_RangeValuePatternId,
                    "RangeValue",
                )?
                .SetValue(*v),
                AxOp::Focus => el.SetFocus(),
                AxOp::ScrollIntoView => pattern::<IUIAutomationScrollItemPattern>(
                    &el,
                    UIA_ScrollItemPatternId,
                    "ScrollItem",
                )?
                .ScrollIntoView(),
                AxOp::DefaultAction => pattern::<IUIAutomationLegacyIAccessiblePattern>(
                    &el,
                    UIA_LegacyIAccessiblePatternId,
                    "LegacyIAccessible",
                )?
                .DoDefaultAction(),
            };
            r.map_err(|e| uia_err(&ctx, e))
        }
    }

    /// Native window and sibling position of an element, for window messages.
    pub fn parent_hwnd_and_index(&self, id: &ElementRef) -> Result<Option<(HWND, usize)>> {
        let (el, _) = self.resolve(id)?;
        let Ok(parent) = (unsafe { self.walker.GetParentElementBuildCache(&el, &self.cache) })
        else {
            return Ok(None);
        };
        let Some(hwnd) = Self::native_hwnd(&parent) else {
            return Ok(None);
        };
        let mut index = 0usize;
        let mut child = unsafe {
            self.walker
                .GetFirstChildElementBuildCache(&parent, &self.cache)
        }
        .ok();
        while let Some(c) = child {
            if unsafe { self.auto.CompareElements(&c, &el) }
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                return Ok(Some((hwnd, index)));
            }
            index += 1;
            child = unsafe { self.walker.GetNextSiblingElementBuildCache(&c, &self.cache) }.ok();
        }
        Ok(None)
    }
}

/// `VT_ARRAY | VT_I4` variant holding a runtime id.
unsafe fn runtime_id_variant(rid: &[i32]) -> Result<Var> {
    unsafe {
        let psa = SafeArrayCreateVector(VT_I4, 0, rid.len() as u32);
        if psa.is_null() {
            return Err(Error::Platform("SafeArrayCreateVector failed".into()));
        }
        for (i, v) in rid.iter().enumerate() {
            let idx = i as i32;
            SafeArrayPutElement(psa, &idx, v as *const i32 as *const _)
                .map_err(|e| uia_err("SafeArrayPutElement", e))?;
        }
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VARENUM(VT_ARRAY.0 | VT_I4.0);
        (*var.Anonymous.Anonymous).Anonymous.parray = psa;
        Ok(Var(var))
    }
}

unsafe fn safearray_i32(psa: *mut SAFEARRAY) -> Vec<i32> {
    if psa.is_null() {
        return vec![];
    }
    unsafe {
        let (Ok(lo), Ok(hi)) = (SafeArrayGetLBound(psa, 1), SafeArrayGetUBound(psa, 1)) else {
            return vec![];
        };
        (lo..=hi)
            .filter_map(|i| {
                let mut v = 0i32;
                SafeArrayGetElement(psa, &i, &mut v as *mut i32 as *mut _)
                    .ok()
                    .map(|_| v)
            })
            .collect()
    }
}

fn role_for(control_type: i32) -> Role {
    match control_type {
        50000 => Role::Button,
        50001 => Role::Calendar,
        50002 => Role::CheckBox,
        50003 => Role::ComboBox,
        50004 => Role::Edit,
        50005 => Role::Hyperlink,
        50006 => Role::Image,
        50007 => Role::ListItem,
        50008 => Role::List,
        50009 => Role::Menu,
        50010 => Role::MenuBar,
        50011 => Role::MenuItem,
        50012 => Role::ProgressBar,
        50013 => Role::RadioButton,
        50014 => Role::ScrollBar,
        50015 => Role::Slider,
        50016 => Role::Spinner,
        50017 => Role::StatusBar,
        50018 => Role::Tab,
        50019 => Role::TabItem,
        50020 => Role::Text,
        50021 => Role::ToolBar,
        50022 => Role::ToolTip,
        50023 => Role::Tree,
        50024 => Role::TreeItem,
        50026 => Role::Group,
        50027 => Role::Thumb,
        50028 => Role::DataGrid,
        50029 => Role::DataItem,
        50030 => Role::Document,
        50031 => Role::SplitButton,
        50032 => Role::Window,
        50033 => Role::Pane,
        50034 => Role::Header,
        50035 => Role::HeaderItem,
        50036 => Role::Table,
        50037 => Role::TitleBar,
        50038 => Role::Separator,
        50039 => Role::SemanticZoom,
        50040 => Role::AppBar,
        _ => Role::Custom,
    }
}
