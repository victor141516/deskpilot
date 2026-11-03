//! The interaction engine: executes an intent on an element by climbing the
//! strategy ladder (accessibility → messages → keyboard → mouse) and verifying
//! each attempt through accessibility before escalating.

mod keyboard;
mod mouse;
pub mod wait;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::thread::sleep;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{
    ActionKind, ElementInfo, ElementNode, ElementRef, ExpandState, Role, ToggleState, WindowId,
    WindowInfo,
};
use crate::platform::{AxOp, MessageOp, Platform, TreeOptions, TreeRoot, WindowOp};
use crate::policy::{Policy, Strategy};

/// What the caller wants to happen to the element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Activate: button press, link follow, menu item, default action.
    Press,
    /// Select a tab, list item, radio button, tree item.
    Select,
    /// Flip a toggle / check box.
    Toggle,
    /// Ensure a check box / toggle is on.
    Check,
    /// Ensure a check box / toggle is off.
    Uncheck,
    Expand,
    Collapse,
    /// Replace the text/value of an edit, combo or slider.
    SetValue,
    /// Give the element keyboard focus.
    Focus,
    ScrollIntoView,
}

#[derive(Debug, Clone)]
pub struct ActOptions {
    /// Least invasive strategy to start from (skips the cheaper rungs).
    pub min_strategy: Strategy,
    pub max_strategy: Strategy,
    /// Give the foreground back to the previous window after keyboard/mouse
    /// strategies. `None` = automatic (yes, except for `expand`, whose popup
    /// would close).
    pub restore_focus: Option<bool>,
    /// For `press` (whose effect cannot always be observed), keep escalating
    /// when an attempt produced no observable change. Off by default because a
    /// second strategy could repeat an effect that did happen.
    pub escalate_unverified: bool,
    /// Maximum Tab/arrow presses while searching for the element.
    pub keyboard_max_steps: usize,
}

impl Default for ActOptions {
    fn default() -> Self {
        Self {
            min_strategy: Strategy::Accessibility,
            max_strategy: Strategy::Mouse,
            restore_focus: None,
            escalate_unverified: false,
            keyboard_max_steps: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// The requested state was observed afterwards.
    Verified,
    /// No direct state to check, but the UI changed (focus, windows or tree).
    Changed,
    /// Executed without error but nothing observable changed.
    NoObservableChange,
    /// Executed but the requested state was not reached.
    NotReached,
    /// The strategy cannot express this intent for this element.
    Unsupported,
    Failed,
    /// Dry run: would be attempted.
    Planned,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Attempt {
    pub strategy: Strategy,
    pub outcome: AttemptOutcome,
    pub method: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActReport {
    /// An attempt completed without being contradicted by the observed state.
    pub success: bool,
    /// The effect was observed (state reached or UI changed). `success` with
    /// `verified: false` means "done, but nothing observable changed".
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<Strategy>,
    pub attempts: Vec<Attempt>,
    /// Element state after the action (absent if it disappeared).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<ElementInfo>,
    /// Foreground window given back to the user after keyboard/mouse input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_restored_to: Option<WindowId>,
    pub dry_run: bool,
}

pub struct Engine<'a> {
    pub platform: &'a dyn Platform,
    pub policy: &'a Policy,
}

/// Pause after an action so the target application can process it before we
/// read its state back.
pub(crate) const SETTLE: Duration = Duration::from_millis(120);
pub(crate) const KEY_SETTLE: Duration = Duration::from_millis(60);
/// Extra time a state change may take to show up before an attempt counts as
/// not reached.
pub(crate) const VERIFY_GRACE: Duration = Duration::from_millis(500);

impl<'a> Engine<'a> {
    pub fn new(platform: &'a dyn Platform, policy: &'a Policy) -> Self {
        Self { platform, policy }
    }

    /// Checks policy and privilege before touching a window.
    pub fn guard_window(&self, window: &WindowInfo) -> Result<()> {
        self.policy.check_window(window)?;
        if window.elevated == Some(true) && !self.platform.info().elevated {
            return Err(Error::ElevationRequired(format!(
                "window {} \"{}\" ({}) runs as administrator; repeat the call with \"elevated\": true",
                window.id, window.title, window.process_name
            )));
        }
        Ok(())
    }

    pub fn act(
        &self,
        target: &ElementInfo,
        intent: &Intent,
        value: Option<&str>,
        options: &ActOptions,
    ) -> Result<ActReport> {
        let window = self.platform.window(&target.window)?;
        self.guard_window(&window)?;
        if (*intent == Intent::SetValue) != value.is_some() {
            return Err(Error::InvalidArgument(if value.is_some() {
                "\"value\" is only valid with action \"set_value\"".into()
            } else {
                "action \"set_value\" needs \"value\"".into()
            }));
        }
        let ceiling = options.max_strategy.min(self.policy.max_strategy);
        let mut report = ActReport {
            success: false,
            verified: false,
            strategy: None,
            attempts: Vec::new(),
            element: None,
            focus_restored_to: None,
            dry_run: self.policy.dry_run,
        };

        if !target.enabled {
            return Err(Error::InvalidArgument(format!(
                "element \"{}\" is disabled",
                target.name
            )));
        }
        if already_satisfied(self.platform, target, intent, value) {
            report.success = true;
            report.verified = true;
            report.attempts.push(Attempt {
                strategy: Strategy::Accessibility,
                outcome: AttemptOutcome::Verified,
                method: "already_satisfied".into(),
                detail: String::new(),
            });
            report.element = Some(target.clone());
            return Ok(report);
        }

        let mut previous_foreground: Option<WindowId> = None;
        let has_messages = self.platform.info().messages;
        for strategy in Strategy::ALL
            .into_iter()
            .filter(|s| *s >= options.min_strategy && *s <= ceiling)
            .filter(|s| *s != Strategy::Messages || has_messages)
        {
            if self.policy.dry_run {
                let method = match strategy {
                    Strategy::Accessibility => {
                        ax_plan(target, intent, value).map(|ops| format!("{ops:?}"))
                    }
                    Strategy::Messages => message_op(intent, value).map(|op| format!("{op:?}")),
                    Strategy::Keyboard => {
                        Some("focus via accessibility/Tab/arrows, then key".into())
                    }
                    Strategy::Mouse => target.bounds.map(|b| format!("click at {:?}", b.center())),
                };
                report.attempts.push(Attempt {
                    strategy,
                    outcome: if method.is_some() {
                        AttemptOutcome::Planned
                    } else {
                        AttemptOutcome::Unsupported
                    },
                    method: method.unwrap_or_default(),
                    detail: String::new(),
                });
                continue;
            }

            if strategy.takes_focus() && previous_foreground.is_none() {
                previous_foreground = self.platform.foreground_window().ok().flatten();
            }

            let before = self
                .platform
                .element(&target.id)
                .unwrap_or_else(|_| target.clone());
            // Keyboard/mouse strategies move focus themselves, so focus is not evidence there.
            let baseline = needs_fingerprint(&before, intent)
                .then(|| Fingerprint::capture(self.platform, &window, !strategy.takes_focus()));

            let result = match strategy {
                Strategy::Accessibility => self.run_accessibility(&before, intent, value),
                Strategy::Messages => self.run_messages(&before, intent, value),
                Strategy::Keyboard => keyboard::run(self, &before, intent, value, options),
                Strategy::Mouse => mouse::run(self, &before, intent, value),
            };

            let method = match result {
                Ok(method) => method,
                Err(
                    e @ (Error::ElevationRequired(_)
                    | Error::PermissionRequired(_)
                    | Error::PolicyDenied(_)),
                ) => return Err(e),
                Err(Error::Unsupported(reason)) => {
                    report.attempts.push(Attempt {
                        strategy,
                        outcome: AttemptOutcome::Unsupported,
                        method: String::new(),
                        detail: reason,
                    });
                    continue;
                }
                Err(e) => {
                    report.attempts.push(Attempt {
                        strategy,
                        outcome: AttemptOutcome::Failed,
                        method: String::new(),
                        detail: e.to_string(),
                    });
                    continue;
                }
            };

            sleep(SETTLE);
            let check = || verify(self.platform, &before, intent, value, baseline.as_ref(), &window);
            let mut outcome = check();
            // Apps apply some changes asynchronously (Chromium closes a <select>
            // menu ~250 ms after Escape): re-read the state for a short while
            // before escalating, so the next strategy does not undo the change.
            let deadline = std::time::Instant::now() + VERIFY_GRACE;
            while outcome == AttemptOutcome::NotReached && std::time::Instant::now() < deadline {
                sleep(SETTLE);
                outcome = check();
            }
            report.attempts.push(Attempt {
                strategy,
                outcome,
                method,
                detail: String::new(),
            });
            let done = match outcome {
                AttemptOutcome::Verified | AttemptOutcome::Changed => true,
                AttemptOutcome::NoObservableChange => !options.escalate_unverified,
                _ => false,
            };
            if done {
                report.success = true;
                report.verified = outcome != AttemptOutcome::NoObservableChange;
                report.strategy = Some(strategy);
                break;
            }
        }

        report.element = self.platform.element(&target.id).ok();
        let restore = options.restore_focus.unwrap_or(*intent != Intent::Expand);
        if restore {
            if let Some(prev) = previous_foreground {
                if prev != target.window
                    && self.platform.window_op(&prev, &WindowOp::Activate).is_ok()
                {
                    report.focus_restored_to = Some(prev);
                }
            }
        }
        Ok(report)
    }

    fn run_accessibility(
        &self,
        target: &ElementInfo,
        intent: &Intent,
        value: Option<&str>,
    ) -> Result<String> {
        let ops = ax_plan(target, intent, value).ok_or_else(|| {
            Error::Unsupported(format!(
                "element advertises no accessibility action for {intent:?}"
            ))
        })?;
        let repeat = matches!(intent, Intent::Check | Intent::Uncheck);
        for _ in 0..if repeat { 3 } else { 1 } {
            self.platform.ax_action(&target.id, &ops)?;
            if !repeat || toggle_reached(self.platform, &target.id, intent) {
                break;
            }
            sleep(SETTLE);
        }
        Ok(format!("accessibility:{}", ax_op_name(&ops)))
    }

    fn run_messages(
        &self,
        target: &ElementInfo,
        intent: &Intent,
        value: Option<&str>,
    ) -> Result<String> {
        let op = message_op(intent, value)
            .ok_or_else(|| Error::Unsupported(format!("no window message for {intent:?}")))?;
        let repeat = matches!(intent, Intent::Check | Intent::Uncheck);
        for _ in 0..if repeat { 3 } else { 1 } {
            self.platform.message_action(target, &op)?;
            if !repeat {
                break;
            }
            sleep(SETTLE);
            if toggle_reached(self.platform, &target.id, intent) {
                break;
            }
        }
        Ok(format!("messages:{op:?}").to_lowercase())
    }
}

fn ax_op_name(op: &AxOp) -> &'static str {
    match op {
        AxOp::Invoke => "invoke",
        AxOp::Select => "select",
        AxOp::Toggle => "toggle",
        AxOp::Expand => "expand",
        AxOp::Collapse => "collapse",
        AxOp::SetValue(_) => "set_value",
        AxOp::SetRange(_) => "set_range",
        AxOp::Focus => "focus",
        AxOp::ScrollIntoView => "scroll_into_view",
        AxOp::DefaultAction => "default_action",
    }
}

/// Picks the accessibility operation for an intent from what the element advertises.
fn ax_plan(e: &ElementInfo, intent: &Intent, value: Option<&str>) -> Option<AxOp> {
    use ActionKind as K;
    let first = |candidates: &[(K, AxOp)]| {
        candidates
            .iter()
            .find(|(k, _)| e.supports(*k))
            .map(|(_, op)| op.clone())
    };
    let toggle_like = matches!(e.role, Role::CheckBox) || e.toggle.is_some();
    match intent {
        Intent::Press => {
            let mut c = vec![(K::Invoke, AxOp::Invoke)];
            if toggle_like {
                c.push((K::Toggle, AxOp::Toggle));
            }
            c.push((K::Select, AxOp::Select));
            c.push((
                K::ExpandCollapse,
                if e.expand == Some(ExpandState::Expanded) {
                    AxOp::Collapse
                } else {
                    AxOp::Expand
                },
            ));
            c.push((K::DefaultAction, AxOp::DefaultAction));
            first(&c)
        }
        Intent::Select => first(&[
            (K::Select, AxOp::Select),
            (K::Invoke, AxOp::Invoke),
            (K::DefaultAction, AxOp::DefaultAction),
        ]),
        Intent::Toggle | Intent::Check | Intent::Uncheck => first(&[
            (K::Toggle, AxOp::Toggle),
            (K::Invoke, AxOp::Invoke),
            (K::DefaultAction, AxOp::DefaultAction),
        ]),
        Intent::Expand => first(&[
            (K::ExpandCollapse, AxOp::Expand),
            (K::Invoke, AxOp::Invoke),
            (K::DefaultAction, AxOp::DefaultAction),
        ]),
        Intent::Collapse => first(&[(K::ExpandCollapse, AxOp::Collapse)]),
        Intent::SetValue => {
            let value = value?;
            if e.supports(K::SetValue) && e.read_only != Some(true) {
                Some(AxOp::SetValue(value.to_string()))
            } else if e.supports(K::SetRange) {
                value.trim().parse::<f64>().ok().map(AxOp::SetRange)
            } else {
                None
            }
        }
        Intent::Focus => (e.focusable || e.supports(K::Focus)).then_some(AxOp::Focus),
        Intent::ScrollIntoView => first(&[(K::ScrollIntoView, AxOp::ScrollIntoView)]),
    }
}

fn message_op(intent: &Intent, value: Option<&str>) -> Option<MessageOp> {
    match intent {
        Intent::Press | Intent::Toggle | Intent::Check | Intent::Uncheck => Some(MessageOp::Press),
        Intent::Select => Some(MessageOp::Select),
        Intent::SetValue => value.map(|v| MessageOp::SetValue(v.to_string())),
        _ => None,
    }
}

fn toggle_reached(p: &dyn Platform, id: &ElementRef, intent: &Intent) -> bool {
    let wanted = if *intent == Intent::Check {
        ToggleState::On
    } else {
        ToggleState::Off
    };
    p.element(id).ok().and_then(|e| e.toggle) == Some(wanted)
}

fn already_satisfied(
    p: &dyn Platform,
    e: &ElementInfo,
    intent: &Intent,
    value: Option<&str>,
) -> bool {
    match intent {
        Intent::Check => e.toggle == Some(ToggleState::On),
        Intent::Uncheck => e.toggle == Some(ToggleState::Off),
        Intent::Select => e.selected == Some(true),
        Intent::Expand => matches!(e.expand, Some(ExpandState::Expanded)),
        Intent::Collapse => matches!(
            e.expand,
            Some(ExpandState::Collapsed | ExpandState::LeafNode)
        ),
        Intent::SetValue => {
            value.is_some_and(|v| e.value.as_deref().map(str::trim) == Some(v.trim()))
        }
        Intent::Focus => {
            e.focused
                && p.focused_element()
                    .ok()
                    .flatten()
                    .is_some_and(|f| f.id == e.id)
        }
        _ => false,
    }
}

fn needs_fingerprint(before: &ElementInfo, intent: &Intent) -> bool {
    match intent {
        Intent::Press => true,
        Intent::Select => before.selected.is_none(),
        Intent::Toggle | Intent::Check | Intent::Uncheck => before.toggle.is_none(),
        Intent::Expand | Intent::Collapse => before.expand.is_none(),
        _ => false,
    }
}

fn verify(
    p: &dyn Platform,
    before: &ElementInfo,
    intent: &Intent,
    value: Option<&str>,
    baseline: Option<&Fingerprint>,
    window: &WindowInfo,
) -> AttemptOutcome {
    let Some(after) = p.element(&before.id).ok() else {
        // The element disappeared: the action had an effect (dialog closed, page
        // changed, tab switched to another window).
        return AttemptOutcome::Changed;
    };
    let by_change = || match baseline {
        Some(b) if *b != Fingerprint::capture(p, window, b.with_focus) => AttemptOutcome::Changed,
        _ => AttemptOutcome::NoObservableChange,
    };
    let state = |reached: bool| {
        if reached {
            AttemptOutcome::Verified
        } else {
            AttemptOutcome::NotReached
        }
    };
    match intent {
        Intent::Press => by_change(),
        Intent::Select => match after.selected {
            Some(s) => state(s),
            None => by_change(),
        },
        Intent::Toggle => match (before.toggle, after.toggle) {
            (Some(b), Some(a)) => state(a != b),
            _ => by_change(),
        },
        Intent::Check | Intent::Uncheck => match after.toggle {
            Some(t) => state(
                t == if *intent == Intent::Check {
                    ToggleState::On
                } else {
                    ToggleState::Off
                },
            ),
            None => by_change(),
        },
        Intent::Expand => match after.expand {
            Some(s) => state(matches!(
                s,
                ExpandState::Expanded | ExpandState::PartiallyExpanded
            )),
            None => by_change(),
        },
        Intent::Collapse => match after.expand {
            Some(s) => state(matches!(s, ExpandState::Collapsed | ExpandState::LeafNode)),
            None => by_change(),
        },
        Intent::SetValue => {
            let wanted = value.unwrap_or_default().trim();
            let current = after
                .value
                .clone()
                .or_else(|| p.element_text(&after.id).ok().flatten());
            state(current.as_deref().map(str::trim) == Some(wanted))
        }
        Intent::Focus => state(
            after.focused
                || p.focused_element()
                    .ok()
                    .flatten()
                    .is_some_and(|f| f.id == after.id),
        ),
        Intent::ScrollIntoView => state(!after.offscreen),
    }
}

/// Cheap summary of "what the user would notice": focused element (optional),
/// the set of visible windows and the target window's accessibility tree (no
/// geometry, so animations do not count as change).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    with_focus: bool,
    focused: Option<ElementRef>,
    windows: u64,
    tree: u64,
}

impl Fingerprint {
    pub(crate) fn capture(p: &dyn Platform, window: &WindowInfo, with_focus: bool) -> Self {
        let focused = if with_focus {
            p.focused_element().ok().flatten().map(|e| e.id)
        } else {
            None
        };
        let mut h = DefaultHasher::new();
        if let Ok(ws) = p.windows(false) {
            for w in ws {
                w.id.hash(&mut h);
                w.title.hash(&mut h);
                w.minimized.hash(&mut h);
            }
        }
        let windows = h.finish();
        let mut h = DefaultHasher::new();
        if let Ok(tree) = p.tree(
            &TreeRoot::Window(window.id.clone()),
            TreeOptions {
                max_depth: 12,
                max_nodes: 600,
            },
        ) {
            hash_tree(&tree, &mut h);
        }
        Self {
            with_focus,
            focused,
            windows,
            tree: h.finish(),
        }
    }
}

fn hash_tree(node: &ElementNode, h: &mut DefaultHasher) {
    let e = &node.info;
    e.role.hash(h);
    e.name.hash(h);
    e.value.hash(h);
    e.selected.hash(h);
    e.toggle.map(|t| t as u8).hash(h);
    e.expand.map(|t| t as u8).hash(h);
    e.enabled.hash(h);
    e.offscreen.hash(h);
    node.children.len().hash(h);
    for c in &node.children {
        hash_tree(c, h);
    }
}

#[cfg(test)]
mod tests;
