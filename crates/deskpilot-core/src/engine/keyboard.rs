//! Keyboard strategy: bring the window forward, move keyboard focus to the
//! element (accessibility focus, then Tab, then arrows inside a container,
//! then Ctrl+Tab for tabs) verifying every step through accessibility, and
//! finally press the key that performs the intent.

use std::collections::HashSet;
use std::thread::sleep;

use super::{ActOptions, Engine, Intent, KEY_SETTLE, SETTLE};
use crate::error::{Error, Result};
use crate::keys::{Key, KeyChord, KeyStroke};
use crate::model::{ArrowAxis, ElementInfo, ElementRef, Role, ToggleState};
use crate::platform::{AxOp, Platform, Shortcut, TreeOptions, TreeRoot, WindowOp};

pub(super) fn run(
    engine: &Engine,
    target: &ElementInfo,
    intent: &Intent,
    value: Option<&str>,
    options: &ActOptions,
) -> Result<String> {
    let p = engine.platform;
    bring_to_front(p, target)?;
    let how = focus(p, target, options.keyboard_max_steps)?;

    let current = p.element(&target.id).unwrap_or_else(|_| target.clone());
    let keys: Vec<KeyStroke> = match intent {
        Intent::Focus | Intent::ScrollIntoView => vec![],
        Intent::Select => {
            if current.selected == Some(true) {
                vec![]
            } else {
                vec![chord(Key::Space)]
            }
        }
        Intent::Press => match current.role {
            Role::TabItem if current.selected == Some(true) => vec![],
            Role::Button | Role::CheckBox | Role::RadioButton | Role::TabItem => {
                vec![chord(Key::Space)]
            }
            _ => vec![chord(Key::Enter)],
        },
        Intent::Toggle => vec![chord(Key::Space)],
        Intent::Check | Intent::Uncheck => {
            let wanted = if *intent == Intent::Check {
                ToggleState::On
            } else {
                ToggleState::Off
            };
            for _ in 0..3 {
                if p.element(&target.id).ok().and_then(|e| e.toggle) == Some(wanted) {
                    break;
                }
                p.send_keys(&[chord(Key::Space)])?;
                sleep(SETTLE);
            }
            return Ok(format!("keyboard:{how}+space"));
        }
        Intent::Expand => match current.role {
            Role::TreeItem => vec![chord(Key::Right)],
            Role::MenuItem => vec![chord(Key::Enter)],
            _ => vec![KeyStroke::Chord(shortcut(p, Shortcut::OpenDropdown)?)],
        },
        Intent::Collapse => match current.role {
            Role::TreeItem => vec![chord(Key::Left)],
            Role::ComboBox => vec![
                p.shortcut(Shortcut::CloseDropdown)
                    .map_or_else(|| chord(Key::Escape), KeyStroke::Chord),
            ],
            _ => vec![chord(Key::Escape)],
        },
        // A closed, non-editable drop-down picks the option whose text is typed
        // (type-ahead); "select all" first would break that. Characters go as
        // individual keys: some pop-ups (Chromium's <select>) ignore synthesized
        // Unicode text events.
        Intent::SetValue if current.role == Role::ComboBox && current.read_only != Some(false) => {
            value
                .unwrap_or_default()
                .chars()
                .map(|c| chord(Key::Char(c)))
                .collect()
        }
        Intent::SetValue => vec![
            KeyStroke::Chord(shortcut(p, Shortcut::SelectAll)?),
            KeyStroke::Text(value.unwrap_or_default().to_string()),
        ],
    };
    if !keys.is_empty() {
        p.send_keys(&keys)?;
    }
    let names: Vec<String> = keys.iter().map(describe).collect();
    Ok(if names.is_empty() {
        format!("keyboard:{how}")
    } else {
        format!("keyboard:{how}+{}", names.join("+"))
    })
}

pub(super) fn bring_to_front(p: &dyn Platform, target: &ElementInfo) -> Result<()> {
    if p.foreground_window()?.as_ref() == Some(&target.window) {
        return Ok(());
    }
    p.window_op(&target.window, &WindowOp::Activate)?;
    sleep(SETTLE);
    match p.foreground_window()? {
        Some(fg) if fg == target.window => Ok(()),
        // An owned popup of the same app (menu, dropdown) may be in front; that is fine.
        Some(fg) if p.window(&fg).map(|w| w.pid == target.pid).unwrap_or(false) => Ok(()),
        _ => Err(Error::Platform(
            "the window could not be brought to the foreground".into(),
        )),
    }
}

fn chord(key: Key) -> KeyStroke {
    KeyStroke::Chord(KeyChord::plain(key))
}

/// The platform's chord for `s`, or `Unsupported` so the ladder moves on.
pub(super) fn shortcut(p: &dyn Platform, s: Shortcut) -> Result<KeyChord> {
    p.shortcut(s)
        .ok_or_else(|| Error::Unsupported(format!("no keyboard shortcut for {s:?} here")))
}

fn describe(k: &KeyStroke) -> String {
    match k {
        KeyStroke::Chord(c) => {
            let m = c.modifiers;
            let mut s = String::new();
            for (on, name) in [
                (m.ctrl, "ctrl+"),
                (m.alt, "alt+"),
                (m.shift, "shift+"),
                (m.meta, "meta+"),
            ] {
                if on {
                    s.push_str(name);
                }
            }
            s.push_str(&format!("{:?}", c.key).to_lowercase());
            s
        }
        KeyStroke::Text(t) => format!("text({} chars)", t.chars().count()),
    }
}

fn focused_id(p: &dyn Platform) -> Option<ElementRef> {
    p.focused_element().ok().flatten().map(|e| e.id)
}

fn is_target(p: &dyn Platform, target: &ElementInfo) -> bool {
    focused_id(p).as_ref() == Some(&target.id)
        || p.element(&target.id).map(|e| e.focused).unwrap_or(false)
}

/// Moves keyboard focus to `target`. Returns a short description of how.
fn focus(p: &dyn Platform, target: &ElementInfo, max_steps: usize) -> Result<String> {
    if is_target(p, target) {
        return Ok("already_focused".into());
    }
    if target.focusable && p.ax_action(&target.id, &AxOp::Focus).is_ok() {
        sleep(KEY_SETTLE);
        if is_target(p, target) {
            return Ok("ax_focus".into());
        }
    }

    let parent = p.parent(&target.id).ok().flatten();
    let siblings: Vec<ElementRef> = parent
        .as_ref()
        .and_then(|par| {
            p.tree(
                &TreeRoot::Element(par.id.clone()),
                TreeOptions {
                    max_depth: 1,
                    max_nodes: 500,
                },
            )
            .ok()
        })
        .map(|t| t.children.into_iter().map(|c| c.info.id).collect())
        .unwrap_or_default();

    // Tab through the window; switch to arrows once focus lands in the target's container.
    let mut seen = HashSet::new();
    for step in 0..max_steps {
        let focused = focused_id(p);
        if focused.as_ref() == Some(&target.id) {
            return Ok(format!("tab×{step}"));
        }
        if let (Some(f), Some(par)) = (&focused, &parent) {
            if siblings.contains(f) {
                if let Some(n) = arrows(
                    p,
                    target,
                    f,
                    &siblings,
                    par.role.arrow_axis().unwrap_or(ArrowAxis::Both),
                )? {
                    return Ok(format!("tab×{step}+arrows×{n}"));
                }
            }
        }
        if let Some(f) = focused {
            if !seen.insert(f) {
                break; // focus cycle completed without reaching the target
            }
        }
        p.send_keys(&[chord(Key::Tab)])?;
        sleep(KEY_SETTLE);
    }

    // Property sheets and tab views switch pages with a "next tab" chord regardless of focus.
    if let (Role::TabItem, Some(next)) = (target.role, p.shortcut(Shortcut::NextTab)) {
        for n in 1..=siblings.len().max(1) * 2 {
            p.send_keys(&[KeyStroke::Chord(next)])?;
            sleep(SETTLE);
            if p.element(&target.id).ok().and_then(|e| e.selected) == Some(true) {
                return Ok(format!("next_tab×{n}"));
            }
        }
    }
    Err(Error::NotFound(format!(
        "keyboard navigation did not reach \"{}\" within {max_steps} steps",
        target.name
    )))
}

/// Arrow-key walk among siblings. Returns the number of presses on success,
/// `None` if arrows do not move focus inside this container.
fn arrows(
    p: &dyn Platform,
    target: &ElementInfo,
    from: &ElementRef,
    siblings: &[ElementRef],
    axis: ArrowAxis,
) -> Result<Option<usize>> {
    let (Some(ti), Some(fi)) = (
        siblings.iter().position(|s| *s == target.id),
        siblings.iter().position(|s| s == from),
    ) else {
        return Ok(None);
    };
    let forward = ti > fi;
    let distance = ti.abs_diff(fi);
    let axes: &[ArrowAxis] = match axis {
        ArrowAxis::Both => &[ArrowAxis::Vertical, ArrowAxis::Horizontal],
        ArrowAxis::Horizontal => &[ArrowAxis::Horizontal],
        ArrowAxis::Vertical => &[ArrowAxis::Vertical],
    };
    let mut presses = 0;
    for ax in axes {
        let key = match (ax, forward) {
            (ArrowAxis::Horizontal, true) => Key::Right,
            (ArrowAxis::Horizontal, false) => Key::Left,
            (_, true) => Key::Down,
            (_, false) => Key::Up,
        };
        let mut last = Some(from.clone());
        // Allow detours (disabled items are skipped, layouts may wrap).
        for _ in 0..distance + siblings.len() {
            p.send_keys(&[chord(key)])?;
            presses += 1;
            sleep(KEY_SETTLE);
            let now = focused_id(p);
            if now.as_ref() == Some(&target.id) {
                return Ok(Some(presses));
            }
            if now == last {
                break; // this axis does not move focus here
            }
            last = now;
        }
    }
    Ok(None)
}
