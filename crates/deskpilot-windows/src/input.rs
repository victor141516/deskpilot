//! Real keyboard and mouse input through SendInput.

use std::thread::sleep;
use std::time::Duration;

use deskpilot_core::keys::{Key, KeyChord, KeyStroke};
use deskpilot_core::model::Point;
use deskpilot_core::platform::{MouseButton, MouseOp};
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

const STEP: Duration = Duration::from_millis(12);
/// One wheel notch.
const WHEEL_DELTA: i32 = 120;
/// Pause between typed characters. Faster typing makes WinUI text boxes
/// (Windows 11 Notepad) drop or duplicate VK_PACKET characters.
const CHAR_DELAY: Duration = Duration::from_millis(10);

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // UIPI silently drops input aimed at higher-integrity windows; SendInput reports 0.
        return Err(Error::Platform(
            "SendInput was blocked (the foreground window may belong to an administrator process)"
                .into(),
        ));
    }
    Ok(())
}

fn extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_APPS
            | VK_SNAPSHOT
    )
}

fn vk_down(vk: VIRTUAL_KEY) -> INPUT {
    key_input(
        vk,
        0,
        if extended(vk) {
            KEYEVENTF_EXTENDEDKEY
        } else {
            KEYBD_EVENT_FLAGS(0)
        },
    )
}

fn vk_up(vk: VIRTUAL_KEY) -> INPUT {
    key_input(
        vk,
        0,
        KEYEVENTF_KEYUP
            | if extended(vk) {
                KEYEVENTF_EXTENDEDKEY
            } else {
                KEYBD_EVENT_FLAGS(0)
            },
    )
}

/// Virtual key for a key, plus extra Shift needed for characters like '+'.
fn virtual_key(key: Key) -> Result<(VIRTUAL_KEY, bool)> {
    let vk = match key {
        Key::Tab => VK_TAB,
        Key::Enter => VK_RETURN,
        Key::Space => VK_SPACE,
        Key::Escape => VK_ESCAPE,
        Key::Backspace => VK_BACK,
        Key::Delete => VK_DELETE,
        Key::Insert => VK_INSERT,
        Key::Home => VK_HOME,
        Key::End => VK_END,
        Key::PageUp => VK_PRIOR,
        Key::PageDown => VK_NEXT,
        Key::Up => VK_UP,
        Key::Down => VK_DOWN,
        Key::Left => VK_LEFT,
        Key::Right => VK_RIGHT,
        Key::Menu => VK_APPS,
        Key::PrintScreen => VK_SNAPSHOT,
        Key::F(n) => VIRTUAL_KEY(VK_F1.0 + n as u16 - 1),
        Key::Char(c) => {
            let c = if c.is_alphabetic() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c
            };
            let mut utf16 = [0u16; 2];
            if c.encode_utf16(&mut utf16).len() != 1 {
                return Err(Error::InvalidArgument(format!(
                    "'{c}' cannot be part of a key chord; send it as text"
                )));
            }
            let scan = unsafe { VkKeyScanW(utf16[0]) };
            if scan == -1 {
                return Err(Error::InvalidArgument(format!(
                    "'{c}' has no key on the current keyboard layout; send it as text"
                )));
            }
            let shift = (scan as u16 >> 8) & 1 == 1;
            return Ok((VIRTUAL_KEY(scan as u16 & 0xff), shift));
        }
    };
    Ok((vk, false))
}

fn chord(c: &KeyChord) -> Result<()> {
    let (vk, needs_shift) = virtual_key(c.key)?;
    let mut mods = Vec::new();
    if c.modifiers.ctrl {
        mods.push(VK_CONTROL);
    }
    if c.modifiers.alt {
        mods.push(VK_MENU);
    }
    if c.modifiers.shift || needs_shift {
        mods.push(VK_SHIFT);
    }
    if c.modifiers.meta {
        mods.push(VK_LWIN);
    }
    let mut inputs: Vec<INPUT> = mods.iter().map(|m| vk_down(*m)).collect();
    inputs.push(vk_down(vk));
    inputs.push(vk_up(vk));
    inputs.extend(mods.iter().rev().map(|m| vk_up(*m)));
    send(&inputs)
}

fn text(t: &str) -> Result<()> {
    for ch in t.chars() {
        match ch {
            '\n' => send(&[vk_down(VK_RETURN), vk_up(VK_RETURN)])?,
            '\r' => continue,
            '\t' => send(&[vk_down(VK_TAB), vk_up(VK_TAB)])?,
            _ => {
                let mut buf = [0u16; 2];
                let mut inputs = Vec::new();
                for unit in ch.encode_utf16(&mut buf) {
                    inputs.push(key_input(VIRTUAL_KEY(0), *unit, KEYEVENTF_UNICODE));
                    inputs.push(key_input(
                        VIRTUAL_KEY(0),
                        *unit,
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    ));
                }
                send(&inputs)?;
            }
        }
        sleep(CHAR_DELAY);
    }
    Ok(())
}

pub fn keys(strokes: &[KeyStroke]) -> Result<()> {
    for s in strokes {
        match s {
            KeyStroke::Chord(c) => chord(c)?,
            KeyStroke::Text(t) => text(t)?,
        }
        sleep(STEP);
    }
    Ok(())
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                mouseData: data as u32,
                ..Default::default()
            },
        },
    }
}

fn move_to(p: Point) -> Result<()> {
    unsafe { SetCursorPos(p.x, p.y) }.map_err(|e| Error::Platform(format!("SetCursorPos: {e}")))?;
    // A relative zero move makes applications see a real WM_MOUSEMOVE at the new position.
    send(&[mouse_input(MOUSEEVENTF_MOVE, 0)])
}

fn buttons(b: MouseButton) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match b {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

pub fn mouse(op: &MouseOp) -> Result<()> {
    match op {
        MouseOp::Move(p) => move_to(*p),
        MouseOp::Click { at, button, count } => {
            move_to(*at)?;
            sleep(STEP);
            let (down, up) = buttons(*button);
            for _ in 0..(*count).max(1) {
                send(&[mouse_input(down, 0), mouse_input(up, 0)])?;
                sleep(Duration::from_millis(30));
            }
            Ok(())
        }
        MouseOp::Drag { from, to, button } => {
            let (down, up) = buttons(*button);
            move_to(*from)?;
            sleep(STEP);
            send(&[mouse_input(down, 0)])?;
            let steps = 12;
            for i in 1..=steps {
                let p = Point {
                    x: from.x + (to.x - from.x) * i / steps,
                    y: from.y + (to.y - from.y) * i / steps,
                };
                move_to(p)?;
                sleep(STEP);
            }
            send(&[mouse_input(up, 0)])
        }
        MouseOp::Scroll { at, dx, dy } => {
            move_to(*at)?;
            sleep(STEP);
            if *dy != 0 {
                send(&[mouse_input(MOUSEEVENTF_WHEEL, dy * WHEEL_DELTA)])?;
            }
            if *dx != 0 {
                send(&[mouse_input(MOUSEEVENTF_HWHEEL, dx * WHEEL_DELTA)])?;
            }
            Ok(())
        }
    }
}

pub fn cursor() -> Result<Point> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }.map_err(|e| Error::Platform(format!("GetCursorPos: {e}")))?;
    Ok(Point { x: p.x, y: p.y })
}

pub fn set_cursor(p: Point) -> Result<()> {
    unsafe { SetCursorPos(p.x, p.y) }.map_err(|e| Error::Platform(format!("SetCursorPos: {e}")))
}
