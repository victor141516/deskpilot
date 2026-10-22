//! Window selectors and element queries, matched in the agnostic layer over
//! data returned by the adapter.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{ElementInfo, ElementNode, Role, WindowId, WindowInfo};
use crate::platform::Platform;

/// Selects one top-level window. A plain string is a window id or, if no
/// window has that id, a case-insensitive title fragment.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum WindowSelector {
    IdOrTitle(String),
    Filter(WindowFilter),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WindowFilter {
    /// Exact window id from `list_windows`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Case-insensitive fragment of the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Process name without extension, case-insensitive (`"notepad"`, `"rundll32"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Native window class, exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    /// Also consider hidden, cloaked and untitled windows.
    #[serde(default)]
    pub include_hidden: bool,
}

impl WindowFilter {
    pub fn matches(&self, w: &WindowInfo) -> bool {
        if let Some(id) = &self.id {
            if w.id.0 != *id {
                return false;
            }
        }
        if let Some(title) = &self.title {
            if !contains_ci(&w.title, title) {
                return false;
            }
        }
        if let Some(process) = &self.process {
            let wanted = process.trim_end_matches(".exe");
            if !w.process_names().any(|p| p.eq_ignore_ascii_case(wanted)) {
                return false;
            }
        }
        if let Some(pid) = self.pid {
            if w.pid != pid && w.app_pid != Some(pid) {
                return false;
            }
        }
        if let Some(class) = &self.class_name {
            if w.class_name != *class {
                return false;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.title.is_none()
            && self.process.is_none()
            && self.pid.is_none()
            && self.class_name.is_none()
    }
}

impl WindowSelector {
    /// Resolves to the topmost matching window.
    pub fn resolve(&self, platform: &dyn Platform) -> Result<WindowInfo> {
        match self {
            WindowSelector::IdOrTitle(text) => {
                if let Ok(w) = platform.window(&WindowId(text.clone())) {
                    return Ok(w);
                }
                let filter = WindowFilter {
                    title: Some(text.clone()),
                    ..Default::default()
                };
                first_match(platform, &filter)
            }
            WindowSelector::Filter(filter) => {
                if let Some(id) = &filter.id {
                    let w = platform.window(&WindowId(id.clone()))?;
                    return if filter.matches(&w) {
                        Ok(w)
                    } else {
                        Err(Error::NotFound(format!(
                            "window {id} does not match the other filter fields"
                        )))
                    };
                }
                if filter.is_empty() {
                    return Err(Error::InvalidArgument(
                        "window selector needs at least one field".into(),
                    ));
                }
                first_match(platform, filter)
            }
        }
    }
}

fn first_match(platform: &dyn Platform, filter: &WindowFilter) -> Result<WindowInfo> {
    let windows = platform.windows(filter.include_hidden)?;
    windows
        .into_iter()
        .find(|w| filter.matches(w))
        .ok_or_else(|| {
            Error::NotFound(format!(
                "no window matches {}",
                serde_json::to_string(filter).unwrap_or_default()
            ))
        })
}

/// Filters accessibility elements. All given fields must match.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ElementQuery {
    /// Exact accessible name, case-insensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Case-insensitive fragment of the accessible name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// Exact automation id / identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    /// Case-insensitive fragment of the element value (edits, combos).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl ElementQuery {
    pub fn matches(&self, e: &ElementInfo) -> bool {
        if let Some(name) = &self.name {
            if e.name.trim().to_lowercase() != name.trim().to_lowercase() {
                return false;
            }
        }
        if let Some(fragment) = &self.name_contains {
            if !contains_ci(&e.name, fragment) {
                return false;
            }
        }
        if let Some(role) = self.role {
            if e.role != role {
                return false;
            }
        }
        if let Some(id) = &self.automation_id {
            if e.automation_id != *id {
                return false;
            }
        }
        if let Some(class) = &self.class_name {
            if e.class_name != *class {
                return false;
            }
        }
        if let Some(fragment) = &self.value_contains {
            if !e.value.as_deref().is_some_and(|v| contains_ci(v, fragment)) {
                return false;
            }
        }
        if let Some(enabled) = self.enabled {
            if e.enabled != enabled {
                return false;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.name_contains.is_none()
            && self.role.is_none()
            && self.automation_id.is_none()
            && self.class_name.is_none()
            && self.value_contains.is_none()
            && self.enabled.is_none()
    }

    /// Matches in document order (depth-first, pre-order).
    pub fn find<'a>(&self, tree: &'a ElementNode) -> Vec<&'a ElementInfo> {
        let mut nodes = Vec::new();
        tree.walk(&mut nodes);
        nodes
            .into_iter()
            .map(|n| &n.info)
            .filter(|e| self.matches(e))
            .collect()
    }
}

/// Case-insensitive substring test that also folds non-ASCII letters.
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ElementRef, WindowId};

    fn element(name: &str, role: Role) -> ElementInfo {
        ElementInfo {
            id: ElementRef(format!("e-{name}")),
            window: WindowId("w".into()),
            role,
            native_role: None,
            name: name.into(),
            automation_id: String::new(),
            class_name: String::new(),
            bounds: None,
            enabled: true,
            focused: false,
            focusable: true,
            offscreen: false,
            selected: None,
            toggle: None,
            expand: None,
            value: None,
            read_only: None,
            range: None,
            actions: vec![],
            native_window: None,
            pid: 1,
        }
    }

    #[test]
    fn query_matches_name_case_insensitively_and_role() {
        let q = ElementQuery {
            name: Some("playback".into()),
            role: Some(Role::TabItem),
            ..Default::default()
        };
        assert!(q.matches(&element("Playback", Role::TabItem)));
        assert!(!q.matches(&element("Playback", Role::Button)));
        assert!(!q.matches(&element("Playback devices", Role::TabItem)));
    }

    #[test]
    fn find_returns_document_order() {
        let tree = ElementNode {
            info: element("root", Role::Window),
            children: vec![
                ElementNode {
                    info: element("Grabar", Role::TabItem),
                    children: vec![],
                    truncated: false,
                },
                ElementNode {
                    info: element("panel", Role::Pane),
                    children: vec![ElementNode {
                        info: element("Grabación", Role::Text),
                        children: vec![],
                        truncated: false,
                    }],
                    truncated: false,
                },
            ],
            truncated: false,
        };
        let q = ElementQuery {
            name_contains: Some("GRAB".into()),
            ..Default::default()
        };
        let names: Vec<_> = q.find(&tree).iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Grabar", "Grabación"]);
    }
}
