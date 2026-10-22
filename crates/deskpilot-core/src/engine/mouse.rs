//! Mouse strategy (last resort): bring the window forward, make sure the
//! element is really the thing under its centre point, click with the real
//! pointer and put the pointer back.

use std::thread::sleep;

use super::keyboard::bring_to_front;
use super::{Engine, Intent, KEY_SETTLE, SETTLE};
use crate::error::{Error, Result};
use crate::keys::{Key, KeyChord, KeyStroke};
use crate::model::{ElementInfo, Point, ToggleState};
use crate::platform::{AxOp, MouseButton, MouseOp, Platform};

pub(super) fn run(
    engine: &Engine,
    target: &ElementInfo,
    intent: &Intent,
    value: Option<&str>,
) -> Result<String> {
    let p = engine.platform;
    if *intent == Intent::ScrollIntoView {
        return Err(Error::Unsupported(
            "scroll_into_view has no mouse equivalent".into(),
        ));
    }
    let mut current = p.element(&target.id).unwrap_or_else(|_| target.clone());
    if current.offscreen {
        // Non-invasive attempt to make it visible first.
        let _ = p.ax_action(&target.id, &AxOp::ScrollIntoView);
        sleep(SETTLE);
        current = p.element(&target.id).unwrap_or(current);
    }
    let bounds = current
        .bounds
        .filter(|b| !b.is_empty() && !current.offscreen)
        .ok_or_else(|| Error::Unsupported("element has no on-screen bounds".into()))?;

    bring_to_front(p, target)?;
    let point = bounds.center();
    ensure_hit(p, target, point)?;

    let saved = p.cursor_position().ok();
    let result = click_for_intent(p, target, intent, value, point);
    if let Some(saved) = saved {
        let _ = p.set_cursor_position(saved);
    }
    result.map(|clicks| format!("mouse:click({},{})×{clicks}", point.x, point.y))
}

fn ensure_hit(p: &dyn Platform, target: &ElementInfo, point: Point) -> Result<()> {
    let Some(hit) = p.window_at(point)? else {
        return Err(Error::Platform(format!(
            "no window at ({}, {})",
            point.x, point.y
        )));
    };
    if hit == target.window {
        return Ok(());
    }
    let hit_info = p.window(&hit)?;
    if hit_info.pid == target.pid || hit_info.app_pid == Some(target.pid) {
        return Ok(());
    }
    Err(Error::Platform(format!(
        "element is covered at ({}, {}) by window {} \"{}\" ({})",
        point.x, point.y, hit_info.id, hit_info.title, hit_info.process_name
    )))
}

fn click_for_intent(
    p: &dyn Platform,
    target: &ElementInfo,
    intent: &Intent,
    value: Option<&str>,
    at: Point,
) -> Result<u32> {
    let click = || {
        p.mouse(&MouseOp::Click {
            at,
            button: MouseButton::Left,
            count: 1,
        })
    };
    match intent {
        Intent::Check | Intent::Uncheck => {
            let wanted = if *intent == Intent::Check {
                ToggleState::On
            } else {
                ToggleState::Off
            };
            let mut clicks = 0;
            for _ in 0..3 {
                if p.element(&target.id).ok().and_then(|e| e.toggle) == Some(wanted) {
                    break;
                }
                click()?;
                clicks += 1;
                sleep(SETTLE);
            }
            Ok(clicks)
        }
        Intent::SetValue => {
            click()?;
            sleep(KEY_SETTLE);
            p.send_keys(&[
                KeyStroke::Chord(KeyChord::with(Key::Char('a'), true, false, false)),
                KeyStroke::Text(value.unwrap_or_default().to_string()),
            ])?;
            Ok(1)
        }
        _ => {
            click()?;
            Ok(1)
        }
    }
}
