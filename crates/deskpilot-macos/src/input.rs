//! CGEvent keyboard and mouse, posted at the HID event tap.
//!
//! Points are global top-left points, the space CGEvent locations use, so no
//! conversion is needed.

use std::ffi::c_void;
use std::thread::sleep;
use std::time::Duration;

use deskpilot_core::keys::{Key, KeyChord, KeyStroke};
use deskpilot_core::model::Point;
use deskpilot_core::platform::{MouseButton, MouseOp};
use deskpilot_core::{Error, Result};
use objc2_application_services::AXIsProcessTrusted;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGError, CGEvent, CGEventField, CGEventFlags,
    CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType, CGMouseButton,
    CGScrollEventUnit, CGWarpMouseCursorPosition,
};

use crate::main_thread::on_main;

/// Pause between consecutive events so applications see them in order.
const STEP: Duration = Duration::from_millis(8);
/// Pause between the clicks of a multi-click.
const CLICK_GAP: Duration = Duration::from_millis(30);
/// Unicode keyboard events carry at most 20 UTF-16 units.
const MAX_UNITS: usize = 20;

const KVK_RETURN: u16 = 0x24;
const KVK_TAB: u16 = 0x30;
const KVK_COMMAND: u16 = 0x37;
const KVK_SHIFT: u16 = 0x38;
const KVK_OPTION: u16 = 0x3A;
const KVK_CONTROL: u16 = 0x3B;

fn require_accessibility() -> Result<()> {
    // CGEventPost silently drops events from untrusted processes.
    if unsafe { AXIsProcessTrusted() } {
        Ok(())
    } else {
        Err(Error::PermissionRequired(
            "Accessibility permission is missing: System Settings → Privacy & Security → Accessibility"
                .into(),
        ))
    }
}

fn source() -> Result<CFRetained<CGEventSource>> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .ok_or_else(|| Error::Platform("CGEventSourceCreate failed".into()))
}

fn post(event: &CFRetained<CGEvent>) {
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
}

fn key_event(
    src: &CGEventSource,
    keycode: u16,
    down: bool,
    flags: CGEventFlags,
) -> Result<CFRetained<CGEvent>> {
    let event = CGEvent::new_keyboard_event(Some(src), keycode, down)
        .ok_or_else(|| Error::Platform("CGEventCreateKeyboardEvent failed".into()))?;
    CGEvent::set_flags(Some(&event), flags);
    Ok(event)
}

fn tap_key(src: &CGEventSource, keycode: u16) -> Result<()> {
    post(&key_event(src, keycode, true, CGEventFlags::empty())?);
    sleep(STEP);
    post(&key_event(src, keycode, false, CGEventFlags::empty())?);
    sleep(STEP);
    Ok(())
}

fn unicode_chunk(src: &CGEventSource, units: &[u16]) -> Result<()> {
    for down in [true, false] {
        let event = key_event(src, 0, down, CGEventFlags::empty())?;
        // SAFETY: `units` is a live slice of `units.len()` UTF-16 code units.
        unsafe {
            CGEvent::keyboard_set_unicode_string(Some(&event), units.len() as _, units.as_ptr())
        };
        post(&event);
        sleep(STEP);
    }
    Ok(())
}

fn text(src: &CGEventSource, t: &str) -> Result<()> {
    let mut chunk: Vec<u16> = Vec::with_capacity(MAX_UNITS);
    let flush = |chunk: &mut Vec<u16>| -> Result<()> {
        if !chunk.is_empty() {
            unicode_chunk(src, chunk)?;
            chunk.clear();
        }
        Ok(())
    };
    for ch in t.chars() {
        match ch {
            '\r' => continue,
            '\n' => {
                flush(&mut chunk)?;
                tap_key(src, KVK_RETURN)?;
            }
            '\t' => {
                flush(&mut chunk)?;
                tap_key(src, KVK_TAB)?;
            }
            _ => {
                let mut buf = [0u16; 2];
                let units = ch.encode_utf16(&mut buf);
                // Never split a surrogate pair across events.
                if chunk.len() + units.len() > MAX_UNITS {
                    flush(&mut chunk)?;
                }
                chunk.extend_from_slice(units);
            }
        }
    }
    flush(&mut chunk)
}

fn named_keycode(key: Key) -> Result<Option<u16>> {
    let code = match key {
        Key::Tab => KVK_TAB,
        Key::Enter => KVK_RETURN,
        Key::Space => 0x31,
        Key::Escape => 0x35,
        Key::Backspace => 0x33,
        Key::Delete => 0x75,
        Key::Insert => 0x72, // Help, where PC keyboards have Insert
        Key::Home => 0x73,
        Key::End => 0x77,
        Key::PageUp => 0x74,
        Key::PageDown => 0x79,
        Key::Left => 0x7B,
        Key::Right => 0x7C,
        Key::Down => 0x7D,
        Key::Up => 0x7E,
        Key::F(n) => match n {
            1 => 0x7A,
            2 => 0x78,
            3 => 0x63,
            4 => 0x76,
            5 => 0x60,
            6 => 0x61,
            7 => 0x62,
            8 => 0x64,
            9 => 0x65,
            10 => 0x6D,
            11 => 0x67,
            12 => 0x6F,
            13 => 0x69,
            14 => 0x6B,
            15 => 0x71,
            16 => 0x6A,
            17 => 0x40,
            18 => 0x4F,
            19 => 0x50,
            20 => 0x5A,
            _ => {
                return Err(Error::Unsupported(format!(
                    "F{n} has no key on macOS (F1–F20 only)"
                )));
            }
        },
        Key::Menu => {
            return Err(Error::Unsupported(
                "Mac keyboards have no context-menu key".into(),
            ));
        }
        Key::PrintScreen => {
            return Err(Error::Unsupported(
                "Mac keyboards have no Print Screen key".into(),
            ));
        }
        Key::Char(_) => return Ok(None),
    };
    Ok(Some(code))
}

// --- Current keyboard layout (Carbon / HIToolbox) ---

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *mut c_void;
    static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        key_layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFRelease(cf: *const c_void);
}

const UC_KEY_ACTION_DISPLAY: u16 = 3;
const UC_KEY_TRANSLATE_NO_DEAD_KEYS: u32 = 1;
/// `shiftKey >> 8` (Carbon modifier state as UCKeyTranslate expects it).
const UC_SHIFT: u32 = 0x02;

/// Keycode (and whether Shift is needed) that types `ch` on the active layout.
fn layout_keycode(ch: char) -> Option<(u16, bool)> {
    let wanted: String = ch.to_lowercase().collect();
    unsafe {
        let source = TISCopyCurrentKeyboardLayoutInputSource();
        if source.is_null() {
            return None;
        }
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        let result = if data.is_null() {
            None
        } else {
            let layout = CFDataGetBytePtr(data) as *const c_void;
            let kbd = LMGetKbdType() as u32;
            let translate = |code: u16, mods: u32| -> Option<String> {
                let mut dead = 0u32;
                let mut buf = [0u16; 4];
                let mut len = 0usize;
                let status = UCKeyTranslate(
                    layout,
                    code,
                    UC_KEY_ACTION_DISPLAY,
                    mods,
                    kbd,
                    UC_KEY_TRANSLATE_NO_DEAD_KEYS,
                    &mut dead,
                    buf.len(),
                    &mut len,
                    buf.as_mut_ptr(),
                );
                (status == 0 && len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
            };
            let matches = |s: String| s.to_lowercase() == wanted;
            (0u16..128)
                .find(|&c| translate(c, 0).is_some_and(matches))
                .map(|c| (c, false))
                .or_else(|| {
                    (0u16..128)
                        .find(|&c| translate(c, UC_SHIFT).is_some_and(matches))
                        .map(|c| (c, true))
                })
        };
        CFRelease(source);
        result
    }
}

/// ANSI US keycodes, used when the layout cannot be queried.
fn us_keycode(ch: char) -> Option<(u16, bool)> {
    let ch = ch.to_ascii_lowercase();
    let plain = |c: char| -> Option<u16> {
        Some(match c {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0B,
            'q' => 0x0C,
            'w' => 0x0D,
            'e' => 0x0E,
            'r' => 0x0F,
            'y' => 0x10,
            't' => 0x11,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '6' => 0x16,
            '5' => 0x17,
            '=' => 0x18,
            '9' => 0x19,
            '7' => 0x1A,
            '-' => 0x1B,
            '8' => 0x1C,
            '0' => 0x1D,
            ']' => 0x1E,
            'o' => 0x1F,
            'u' => 0x20,
            '[' => 0x21,
            'i' => 0x22,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            '\'' => 0x27,
            'k' => 0x28,
            ';' => 0x29,
            '\\' => 0x2A,
            ',' => 0x2B,
            '/' => 0x2C,
            'n' => 0x2D,
            'm' => 0x2E,
            '.' => 0x2F,
            '`' => 0x32,
            ' ' => 0x31,
            _ => return None,
        })
    };
    if let Some(code) = plain(ch) {
        return Some((code, false));
    }
    let base = match ch {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    };
    plain(base).map(|c| (c, true))
}

fn chord(src: &CGEventSource, c: &KeyChord) -> Result<()> {
    let (keycode, needs_shift) = match (named_keycode(c.key)?, c.key) {
        (Some(code), _) => (code, false),
        (None, Key::Char(ch)) => {
            // TIS asserts it runs on the main queue (macOS 14+).
            on_main(move |_| layout_keycode(ch))
                .or_else(|| us_keycode(ch))
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "no key produces '{ch}' on the current keyboard layout"
                    ))
                })?
        }
        (None, key) => {
            return Err(Error::Unsupported(format!("no macOS key code for {key:?}")));
        }
    };
    let m = c.modifiers;
    let mut mods: Vec<(u16, CGEventFlags)> = Vec::with_capacity(4);
    if m.meta {
        mods.push((KVK_COMMAND, CGEventFlags::MaskCommand));
    }
    if m.ctrl {
        mods.push((KVK_CONTROL, CGEventFlags::MaskControl));
    }
    if m.alt {
        mods.push((KVK_OPTION, CGEventFlags::MaskAlternate));
    }
    if m.shift || needs_shift {
        mods.push((KVK_SHIFT, CGEventFlags::MaskShift));
    }
    let mut flags = CGEventFlags::empty();
    for &(code, flag) in &mods {
        flags |= flag;
        post(&key_event(src, code, true, flags)?);
        sleep(STEP);
    }
    post(&key_event(src, keycode, true, flags)?);
    sleep(STEP);
    post(&key_event(src, keycode, false, flags)?);
    sleep(STEP);
    for &(code, flag) in mods.iter().rev() {
        flags.remove(flag);
        post(&key_event(src, code, false, flags)?);
        sleep(STEP);
    }
    Ok(())
}

pub fn keys(strokes: &[KeyStroke]) -> Result<()> {
    require_accessibility()?;
    let src = source()?;
    for s in strokes {
        match s {
            KeyStroke::Chord(c) => chord(&src, c)?,
            KeyStroke::Text(t) => text(&src, t)?,
        }
        sleep(STEP);
    }
    Ok(())
}

// --- Mouse ---

struct ButtonEvents {
    down: CGEventType,
    up: CGEventType,
    dragged: CGEventType,
    button: CGMouseButton,
}

fn button_events(b: MouseButton) -> ButtonEvents {
    match b {
        MouseButton::Left => ButtonEvents {
            down: CGEventType::LeftMouseDown,
            up: CGEventType::LeftMouseUp,
            dragged: CGEventType::LeftMouseDragged,
            button: CGMouseButton::Left,
        },
        MouseButton::Right => ButtonEvents {
            down: CGEventType::RightMouseDown,
            up: CGEventType::RightMouseUp,
            dragged: CGEventType::RightMouseDragged,
            button: CGMouseButton::Right,
        },
        MouseButton::Middle => ButtonEvents {
            down: CGEventType::OtherMouseDown,
            up: CGEventType::OtherMouseUp,
            dragged: CGEventType::OtherMouseDragged,
            button: CGMouseButton::Center,
        },
    }
}

fn cg_point(p: Point) -> CGPoint {
    CGPoint::new(p.x as f64, p.y as f64)
}

fn mouse_event(
    src: &CGEventSource,
    kind: CGEventType,
    at: Point,
    button: CGMouseButton,
) -> Result<CFRetained<CGEvent>> {
    let event = CGEvent::new_mouse_event(Some(src), kind, cg_point(at), button)
        .ok_or_else(|| Error::Platform("CGEventCreateMouseEvent failed".into()))?;
    if button == CGMouseButton::Center {
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventButtonNumber,
            2,
        );
    }
    Ok(event)
}

fn move_to(src: &CGEventSource, p: Point) -> Result<()> {
    post(&mouse_event(
        src,
        CGEventType::MouseMoved,
        p,
        CGMouseButton::Left,
    )?);
    sleep(STEP);
    Ok(())
}

pub fn mouse(op: &MouseOp) -> Result<()> {
    require_accessibility()?;
    let src = source()?;
    match op {
        MouseOp::Move(p) => move_to(&src, *p),
        MouseOp::Click { at, button, count } => {
            move_to(&src, *at)?;
            let b = button_events(*button);
            for i in 1..=(*count).max(1) {
                for kind in [b.down, b.up] {
                    let event = mouse_event(&src, kind, *at, b.button)?;
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventClickState,
                        i as i64,
                    );
                    post(&event);
                    sleep(STEP);
                }
                sleep(CLICK_GAP);
            }
            Ok(())
        }
        MouseOp::Drag { from, to, button } => {
            let b = button_events(*button);
            move_to(&src, *from)?;
            post(&mouse_event(&src, b.down, *from, b.button)?);
            sleep(STEP);
            let steps = 10;
            for i in 1..=steps {
                let p = Point {
                    x: from.x + (to.x - from.x) * i / steps,
                    y: from.y + (to.y - from.y) * i / steps,
                };
                post(&mouse_event(&src, b.dragged, p, b.button)?);
                sleep(STEP);
            }
            post(&mouse_event(&src, b.up, *to, b.button)?);
            sleep(STEP);
            Ok(())
        }
        MouseOp::Scroll { at, dx, dy } => {
            move_to(&src, *at)?;
            // Line units: positive wheel1 scrolls up, positive wheel2 scrolls
            // left, so the horizontal axis is negated to match the core
            // convention (positive dx scrolls right).
            let event = CGEvent::new_scroll_wheel_event2(
                Some(&src),
                CGScrollEventUnit::Line,
                2,
                *dy,
                -*dx,
                0,
            )
            .ok_or_else(|| Error::Platform("CGEventCreateScrollWheelEvent2 failed".into()))?;
            CGEvent::set_location(Some(&event), cg_point(*at));
            post(&event);
            sleep(STEP);
            Ok(())
        }
    }
}

pub fn cursor() -> Result<Point> {
    let event =
        CGEvent::new(None).ok_or_else(|| Error::Platform("CGEventCreate failed".into()))?;
    let p = CGEvent::location(Some(&event));
    Ok(Point {
        x: p.x.round() as i32,
        y: p.y.round() as i32,
    })
}

fn cg_check(err: CGError, what: &str) -> Result<()> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(Error::Platform(format!("{what} failed (CGError {})", err.0)))
    }
}

pub fn set_cursor(p: Point) -> Result<()> {
    cg_check(
        CGWarpMouseCursorPosition(cg_point(p)),
        "CGWarpMouseCursorPosition",
    )?;
    // Warping suppresses mouse movement briefly; reconnect it immediately.
    cg_check(
        CGAssociateMouseAndMouseCursorPosition(true),
        "CGAssociateMouseAndMouseCursorPosition",
    )
}
