//! Compact, token-cheap text rendering of accessibility data.

use std::fmt::Write;

use crate::model::{ElementInfo, ElementNode, ExpandState, Role, ToggleState};

/// One line per element, indented by depth:
/// `[ref] role "name" #automation_id (x,y wxh) {states} value="…" actions=…`
pub fn tree(node: &ElementNode) -> String {
    let mut out = String::new();
    write_node(node, 0, &mut out);
    out
}

fn write_node(node: &ElementNode, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(&element_line(&node.info));
    if node.truncated {
        out.push_str(" …");
    }
    out.push('\n');
    for child in &node.children {
        write_node(child, depth + 1, out);
    }
}

pub fn element_line(e: &ElementInfo) -> String {
    let mut s = format!("[{}] {}", e.id, e.role.as_str());
    if let Some(native) = &e.native_role {
        let _ = write!(s, "({native})");
    }
    if !e.name.is_empty() {
        let _ = write!(s, " \"{}\"", truncate(&e.name, 120));
    }
    if !e.automation_id.is_empty() {
        let _ = write!(s, " #{}", e.automation_id);
    }
    if matches!(e.role, Role::Pane | Role::Custom | Role::Window) && !e.class_name.is_empty() {
        let _ = write!(s, " .{}", e.class_name);
    }
    if let Some(b) = e.bounds {
        let _ = write!(s, " ({},{} {}x{})", b.x, b.y, b.width, b.height);
    }
    let mut states: Vec<String> = Vec::new();
    if e.focused {
        states.push("focused".into());
    }
    if !e.enabled {
        states.push("disabled".into());
    }
    if e.offscreen {
        states.push("offscreen".into());
    }
    if e.selected == Some(true) {
        states.push("selected".into());
    }
    match e.toggle {
        Some(ToggleState::On) => states.push("on".into()),
        Some(ToggleState::Off) => states.push("off".into()),
        Some(ToggleState::Indeterminate) => states.push("mixed".into()),
        None => {}
    }
    match e.expand {
        Some(ExpandState::Expanded) => states.push("expanded".into()),
        Some(ExpandState::Collapsed) => states.push("collapsed".into()),
        Some(ExpandState::PartiallyExpanded) => states.push("partially_expanded".into()),
        _ => {}
    }
    if e.read_only == Some(true) {
        states.push("readonly".into());
    }
    if let Some(r) = e.range {
        states.push(format!("range={}/{}..{}", r.value, r.min, r.max));
    }
    if !states.is_empty() {
        let _ = write!(s, " {{{}}}", states.join(","));
    }
    if let Some(v) = &e.value {
        if !v.is_empty() && *v != e.name {
            let _ = write!(s, " value=\"{}\"", truncate(v, 120));
        }
    }
    if !e.actions.is_empty() {
        let names: Vec<_> = e
            .actions
            .iter()
            .map(|a| {
                serde_json::to_value(a)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            })
            .collect();
        let _ = write!(s, " actions={}", names.join(","));
    }
    s
}

fn truncate(text: &str, max: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= max {
        clean
    } else {
        let mut t: String = clean.chars().take(max).collect();
        t.push('…');
        t
    }
}
