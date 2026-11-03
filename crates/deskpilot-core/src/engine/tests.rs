//! Engine behaviour against an in-memory desktop: escalation order, verification
//! driven escalation, strategy ceiling, privilege/policy guards and focus restore.

use std::sync::Mutex;

use super::*;
use crate::keys::{Key, KeyChord, KeyStroke};
use crate::model::{Capture, MonitorInfo, OcrLine, Point, Rect};
use crate::platform::{
    ClipboardContent, Duplex, LaunchResult, LaunchSpec, MouseOp, PlatformInfo, Shortcut,
};

const DIALOG: &str = "w-dialog";
const EDITOR: &str = "w-editor";

#[derive(Default)]
struct State {
    elements: Vec<ElementInfo>,
    /// (parent, child) pairs; root is the dialog element.
    parent_of: Vec<(String, String)>,
    tab_order: Vec<String>,
    focused: Option<String>,
    foreground: String,
    /// Accessibility actions report success but change nothing (broken provider).
    ax_noop: bool,
    /// The app reacts to nothing at all (press has no visible effect).
    inert: bool,
    log: Vec<String>,
}

struct Fake {
    s: Mutex<State>,
    elevated_target: bool,
}

fn el(id: &str, role: Role, name: &str, actions: Vec<ActionKind>, bounds: Rect) -> ElementInfo {
    ElementInfo {
        id: ElementRef(id.into()),
        window: WindowId(DIALOG.into()),
        role,
        native_role: None,
        name: name.into(),
        automation_id: String::new(),
        class_name: String::new(),
        bounds: Some(bounds),
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
        actions,
        native_window: None,
        pid: 10,
    }
}

impl Fake {
    /// Dialog with a tab strip (General selected, Playback not) and an OK button.
    fn dialog(tab_actions: Vec<ActionKind>) -> Self {
        let mut general = el(
            "general",
            Role::TabItem,
            "General",
            tab_actions.clone(),
            Rect::new(10, 10, 50, 20),
        );
        general.selected = Some(true);
        let mut playback = el(
            "playback",
            Role::TabItem,
            "Playback",
            tab_actions,
            Rect::new(60, 10, 50, 20),
        );
        playback.selected = Some(false);
        let ok = el(
            "ok",
            Role::Button,
            "OK",
            vec![],
            Rect::new(100, 200, 60, 25),
        );
        let root = el(
            "root",
            Role::Window,
            "Sound",
            vec![],
            Rect::new(0, 0, 300, 300),
        );
        let tabs = el("tabs", Role::Tab, "", vec![], Rect::new(0, 0, 300, 40));
        let state = State {
            elements: vec![root, tabs, general, playback, ok],
            parent_of: vec![
                ("root".into(), "tabs".into()),
                ("tabs".into(), "general".into()),
                ("tabs".into(), "playback".into()),
                ("root".into(), "ok".into()),
            ],
            tab_order: vec!["general".into(), "ok".into()],
            focused: None,
            foreground: EDITOR.into(),
            ..Default::default()
        };
        Self {
            s: Mutex::new(state),
            elevated_target: false,
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.s.lock().unwrap())
    }

    fn log(&self) -> Vec<String> {
        self.with(|s| s.log.clone())
    }

    fn get(&self, id: &str) -> ElementInfo {
        self.element(&ElementRef(id.into())).unwrap()
    }
}

impl State {
    fn find(&mut self, id: &str) -> Option<&mut ElementInfo> {
        self.elements.iter_mut().find(|e| e.id.0 == id)
    }

    /// Selecting a tab deselects its siblings.
    fn select(&mut self, id: &str) {
        if self.inert {
            return;
        }
        let parent = self
            .parent_of
            .iter()
            .find(|(_, c)| c == id)
            .map(|(p, _)| p.clone());
        let siblings: Vec<String> = self
            .parent_of
            .iter()
            .filter(|(p, _)| Some(p) == parent.as_ref())
            .map(|(_, c)| c.clone())
            .collect();
        for e in self.elements.iter_mut() {
            if siblings.contains(&e.id.0) && e.selected.is_some() {
                e.selected = Some(e.id.0 == id);
            }
        }
    }

    fn node(&self, id: &str, depth: usize) -> ElementNode {
        let mut info = self.elements.iter().find(|e| e.id.0 == id).unwrap().clone();
        info.focused = self.focused.as_deref() == Some(id);
        let children = if depth == 0 {
            vec![]
        } else {
            self.parent_of
                .iter()
                .filter(|(p, _)| p == id)
                .map(|(_, c)| self.node(c, depth - 1))
                .collect()
        };
        ElementNode {
            info,
            children,
            truncated: false,
        }
    }
}

fn window(id: &str, elevated: bool) -> WindowInfo {
    WindowInfo {
        id: WindowId(id.into()),
        title: id.into(),
        class_name: String::new(),
        pid: if id == DIALOG { 10 } else { 20 },
        process_name: if id == DIALOG {
            "rundll32".into()
        } else {
            "editor".into()
        },
        exe_path: None,
        app_pid: None,
        app_process_name: None,
        bounds: Rect::new(0, 0, 300, 300),
        visible: true,
        minimized: false,
        maximized: false,
        cloaked: false,
        foreground: false,
        owner: None,
        monitor: None,
        elevated: Some(elevated),
        z_order: 0,
    }
}

fn unsupported<T>() -> Result<T> {
    Err(Error::Unsupported("fake".into()))
}

impl Platform for Fake {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            os: "fake",
            elevated: false,
            can_elevate: false,
            messages: true,
            permissions: vec![],
        }
    }
    fn shortcut(&self, s: Shortcut) -> Option<KeyChord> {
        Some(match s {
            Shortcut::SelectAll => KeyChord::with(Key::Char('a'), true, false, false),
            Shortcut::OpenDropdown => KeyChord::with(Key::Down, false, true, false),
            Shortcut::CloseDropdown => KeyChord::with(Key::Up, false, true, false),
            Shortcut::NextTab => KeyChord::with(Key::Tab, true, false, false),
        })
    }
    fn request_permissions(&self, _: bool) {}
    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        Ok(vec![])
    }
    fn windows(&self, _: bool) -> Result<Vec<WindowInfo>> {
        Ok(vec![
            window(DIALOG, self.elevated_target),
            window(EDITOR, false),
        ])
    }
    fn window(&self, id: &WindowId) -> Result<WindowInfo> {
        let mut w = window(&id.0, id.0 == DIALOG && self.elevated_target);
        w.foreground = self.with(|s| s.foreground == id.0);
        Ok(w)
    }
    fn window_at(&self, _: Point) -> Result<Option<WindowId>> {
        Ok(Some(WindowId(DIALOG.into())))
    }
    fn capture_screen(&self, _: Rect) -> Result<Capture> {
        unsupported()
    }
    fn capture_window(&self, _: &WindowId) -> Result<Capture> {
        unsupported()
    }
    fn ocr(&self, _: &Capture, _: Option<&str>) -> Result<Vec<OcrLine>> {
        unsupported()
    }
    fn tree(&self, root: &TreeRoot, o: TreeOptions) -> Result<ElementNode> {
        let id = match root {
            TreeRoot::Window(_) => "root".to_string(),
            TreeRoot::Element(e) => e.0.clone(),
        };
        Ok(self.with(|s| s.node(&id, o.max_depth)))
    }
    fn element(&self, id: &ElementRef) -> Result<ElementInfo> {
        self.with(|s| {
            let focused = s.focused.as_deref() == Some(id.0.as_str());
            s.find(&id.0)
                .map(|e| ElementInfo {
                    focused,
                    ..e.clone()
                })
                .ok_or_else(|| Error::NotFound(id.0.clone()))
        })
    }
    fn parent(&self, id: &ElementRef) -> Result<Option<ElementInfo>> {
        let parent = self.with(|s| {
            s.parent_of
                .iter()
                .find(|(_, c)| *c == id.0)
                .map(|(p, _)| p.clone())
        });
        parent.map(|p| self.element(&ElementRef(p))).transpose()
    }
    fn focused_element(&self) -> Result<Option<ElementInfo>> {
        let f = self.with(|s| s.focused.clone());
        f.map(|id| self.element(&ElementRef(id))).transpose()
    }
    fn element_at(&self, _: Point) -> Result<Option<ElementInfo>> {
        Ok(None)
    }
    fn element_text(&self, _: &ElementRef) -> Result<Option<String>> {
        Ok(None)
    }
    fn ax_action(&self, id: &ElementRef, op: &AxOp) -> Result<()> {
        self.with(|s| {
            s.log.push(format!("ax:{op:?}:{}", id.0));
            let supported = s.find(&id.0).map(|e| e.actions.clone()).unwrap_or_default();
            match op {
                AxOp::Focus => {
                    // Only reachable with the keyboard (Tab order), like many custom controls.
                    if s.tab_order.contains(&id.0) && s.foreground == DIALOG {
                        s.focused = Some(id.0.clone());
                    }
                    Ok(())
                }
                AxOp::Select | AxOp::Invoke
                    if supported.contains(&ActionKind::Select)
                        || supported.contains(&ActionKind::Invoke) =>
                {
                    if !s.ax_noop {
                        s.select(&id.0);
                    }
                    Ok(())
                }
                _ => Err(Error::Unsupported("fake ax".into())),
            }
        })
    }
    fn message_action(&self, e: &ElementInfo, op: &MessageOp) -> Result<()> {
        self.with(|s| s.log.push(format!("msg:{op:?}:{}", e.id.0)));
        unsupported()
    }
    fn foreground_window(&self) -> Result<Option<WindowId>> {
        Ok(Some(WindowId(self.with(|s| s.foreground.clone()))))
    }
    fn send_keys(&self, keys: &[KeyStroke]) -> Result<()> {
        self.with(|s| {
            for k in keys {
                s.log.push(format!("key:{k:?}"));
                if s.foreground != DIALOG {
                    continue;
                }
                match k {
                    KeyStroke::Chord(c) if c.key == Key::Tab && !c.modifiers.ctrl => {
                        let next = match &s.focused {
                            Some(f) => s
                                .tab_order
                                .iter()
                                .position(|t| t == f)
                                .map(|i| (i + 1) % s.tab_order.len())
                                .unwrap_or(0),
                            None => 0,
                        };
                        s.focused = Some(s.tab_order[next].clone());
                    }
                    KeyStroke::Chord(c) if c.key == Key::Right => {
                        // Arrow inside the tab strip moves focus and selection to the next tab.
                        if s.focused.as_deref() == Some("general") {
                            s.focused = Some("playback".into());
                            s.select("playback");
                        }
                    }
                    KeyStroke::Chord(c) if c.key == Key::Space => {
                        if let Some(f) = s.focused.clone() {
                            s.select(&f);
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        })
    }
    fn mouse(&self, op: &MouseOp) -> Result<()> {
        self.with(|s| {
            s.log.push(format!("mouse:{op:?}"));
            if let MouseOp::Click { at, .. } = op {
                let hit = s
                    .elements
                    .iter()
                    .filter(|e| e.role == Role::TabItem)
                    .find(|e| e.bounds.unwrap().contains(*at))
                    .map(|e| e.id.0.clone());
                if let Some(id) = hit {
                    s.select(&id);
                }
            }
            Ok(())
        })
    }
    fn cursor_position(&self) -> Result<Point> {
        Ok(Point { x: 1, y: 1 })
    }
    fn set_cursor_position(&self, p: Point) -> Result<()> {
        self.with(|s| s.log.push(format!("cursor:{},{}", p.x, p.y)));
        Ok(())
    }
    fn window_op(&self, id: &WindowId, op: &WindowOp) -> Result<()> {
        self.with(|s| {
            s.log.push(format!("window:{op:?}:{}", id.0));
            if *op == WindowOp::Activate {
                s.foreground = id.0.clone();
            }
        });
        Ok(())
    }
    fn launch(&self, _: &LaunchSpec) -> Result<LaunchResult> {
        unsupported()
    }
    fn clipboard_get(&self) -> Result<ClipboardContent> {
        unsupported()
    }
    fn clipboard_set(&self, _: &ClipboardContent) -> Result<()> {
        unsupported()
    }
    fn spawn_elevated_worker(&self, _: &[String]) -> Result<Box<dyn Duplex>> {
        unsupported()
    }
    fn connect_worker(&self, _: &str) -> Result<Box<dyn Duplex>> {
        unsupported()
    }
}

fn act(
    fake: &Fake,
    policy: &Policy,
    id: &str,
    intent: Intent,
    options: ActOptions,
) -> Result<ActReport> {
    let target = fake.get(id);
    Engine::new(fake, policy).act(&target, &intent, None, &options)
}

fn outcomes(r: &ActReport) -> Vec<(Strategy, AttemptOutcome)> {
    r.attempts.iter().map(|a| (a.strategy, a.outcome)).collect()
}

#[test]
fn accessibility_success_stops_the_ladder_without_touching_focus() {
    let fake = Fake::dialog(vec![ActionKind::Select]);
    let r = act(
        &fake,
        &Policy::default(),
        "playback",
        Intent::Select,
        ActOptions::default(),
    )
    .unwrap();
    assert!(r.success && r.verified);
    assert_eq!(
        outcomes(&r),
        [(Strategy::Accessibility, AttemptOutcome::Verified)]
    );
    assert_eq!(fake.get("playback").selected, Some(true));
    assert_eq!(
        fake.with(|s| s.foreground.clone()),
        EDITOR,
        "the user's foreground window was not touched"
    );
}

#[test]
fn unverified_accessibility_escalates_to_keyboard_navigation() {
    // The provider claims Select works but nothing changes; the keyboard must
    // Tab into the strip and use the arrow key to reach Playback.
    let fake = Fake::dialog(vec![ActionKind::Select]);
    fake.with(|s| s.ax_noop = true);
    let r = act(
        &fake,
        &Policy::default(),
        "playback",
        Intent::Select,
        ActOptions::default(),
    )
    .unwrap();
    assert!(r.success && r.verified, "{r:?}");
    assert_eq!(
        outcomes(&r),
        [
            (Strategy::Accessibility, AttemptOutcome::NotReached),
            (Strategy::Messages, AttemptOutcome::Unsupported),
            (Strategy::Keyboard, AttemptOutcome::Verified),
        ]
    );
    assert!(
        r.attempts[2].method.contains("arrows"),
        "{}",
        r.attempts[2].method
    );
    assert_eq!(fake.get("playback").selected, Some(true));
    assert_eq!(
        r.focus_restored_to,
        Some(WindowId(EDITOR.into())),
        "foreground handed back to the user"
    );
    assert_eq!(fake.with(|s| s.foreground.clone()), EDITOR);
}

#[test]
fn mouse_is_the_last_resort_and_restores_the_pointer() {
    let fake = Fake::dialog(vec![]);
    fake.with(|s| s.tab_order = vec!["ok".into()]); // tabs unreachable by keyboard
    let options = ActOptions {
        keyboard_max_steps: 3,
        ..Default::default()
    };
    let r = act(
        &fake,
        &Policy::default(),
        "playback",
        Intent::Select,
        options,
    )
    .unwrap();
    assert!(r.success && r.verified, "{r:?}");
    assert_eq!(r.strategy, Some(Strategy::Mouse));
    assert_eq!(
        r.attempts.iter().map(|a| a.strategy).collect::<Vec<_>>(),
        Strategy::ALL
    );
    let log = fake.log();
    let click = log
        .iter()
        .position(|l| l.starts_with("mouse:Click"))
        .unwrap();
    assert_eq!(
        log[click + 1],
        "cursor:1,1",
        "pointer put back right after the click"
    );
}

#[test]
fn ceiling_prevents_invasive_strategies() {
    let fake = Fake::dialog(vec![]);
    let policy = Policy {
        max_strategy: Strategy::Messages,
        ..Default::default()
    };
    let r = act(
        &fake,
        &policy,
        "playback",
        Intent::Select,
        ActOptions::default(),
    )
    .unwrap();
    assert!(!r.success);
    assert_eq!(r.attempts.len(), 2);
    assert!(
        fake.log().iter().all(|l| !l.starts_with("key:")
            && !l.starts_with("mouse:")
            && !l.starts_with("window:"))
    );
}

#[test]
fn press_without_observable_change_does_not_repeat_by_default() {
    let fake = Fake::dialog(vec![ActionKind::Invoke]);
    fake.with(|s| s.inert = true);
    let r = act(
        &fake,
        &Policy::default(),
        "general",
        Intent::Press,
        ActOptions::default(),
    )
    .unwrap();
    assert!(r.success && !r.verified);
    assert_eq!(
        outcomes(&r),
        [(Strategy::Accessibility, AttemptOutcome::NoObservableChange)]
    );

    let r = act(
        &fake,
        &Policy::default(),
        "general",
        Intent::Press,
        ActOptions {
            escalate_unverified: true,
            keyboard_max_steps: 3,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        r.attempts.len(),
        4,
        "every strategy tried when asked to: {r:?}"
    );
}

#[test]
fn elevated_targets_and_denied_processes_are_refused_before_any_input() {
    let mut fake = Fake::dialog(vec![ActionKind::Select]);
    fake.elevated_target = true;
    let err = act(
        &fake,
        &Policy::default(),
        "playback",
        Intent::Select,
        ActOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err.kind(), "elevation_required");

    let fake = Fake::dialog(vec![ActionKind::Select]);
    let policy = Policy {
        deny_processes: vec!["RUNDLL32.exe".into()],
        ..Default::default()
    };
    let err = act(
        &fake,
        &policy,
        "playback",
        Intent::Select,
        ActOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err.kind(), "policy_denied");
    assert!(fake.log().is_empty());
}

#[test]
fn dry_run_plans_without_acting() {
    let fake = Fake::dialog(vec![ActionKind::Select]);
    let policy = Policy {
        dry_run: true,
        ..Default::default()
    };
    let r = act(
        &fake,
        &policy,
        "playback",
        Intent::Select,
        ActOptions::default(),
    )
    .unwrap();
    assert!(r.dry_run && !r.success);
    assert_eq!(
        r.attempts
            .iter()
            .filter(|a| a.outcome == AttemptOutcome::Planned)
            .count(),
        4,
        "{r:?}"
    );
    assert!(fake.log().is_empty());
}
