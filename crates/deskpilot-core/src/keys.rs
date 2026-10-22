//! Platform-agnostic key names and chords (`"ctrl+shift+Tab"`, `"alt+F4"`,
//! `"Return"`). Adapters translate `Key` into native key codes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Tab,
    Enter,
    Space,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// Context-menu key.
    Menu,
    PrintScreen,
    F(u8),
    /// A printable character; adapters map it to the key that produces it on the
    /// active layout (letters are case-insensitive in chords).
    Char(char),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Windows key / Command key.
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl KeyChord {
    pub const fn plain(key: Key) -> Self {
        Self {
            modifiers: Modifiers {
                ctrl: false,
                alt: false,
                shift: false,
                meta: false,
            },
            key,
        }
    }

    pub const fn with(key: Key, ctrl: bool, alt: bool, shift: bool) -> Self {
        Self {
            modifiers: Modifiers {
                ctrl,
                alt,
                shift,
                meta: false,
            },
            key,
        }
    }

    /// Parses `mod+mod+key`. Modifier and key names are case-insensitive.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(Error::InvalidArgument("empty key".into()));
        }
        // A trailing "+" is the plus key itself ("ctrl++").
        let (mods_part, key_part) = if spec == "+" {
            ("", "+")
        } else if let Some(stripped) = spec.strip_suffix("++") {
            (stripped, "+")
        } else {
            match spec.rfind('+') {
                Some(i) => (&spec[..i], &spec[i + 1..]),
                None => ("", spec),
            }
        };
        let mut modifiers = Modifiers::default();
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            match m.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "ctl" => modifiers.ctrl = true,
                "alt" | "option" | "opt" => modifiers.alt = true,
                "shift" => modifiers.shift = true,
                "meta" | "super" | "win" | "windows" | "cmd" | "command" => modifiers.meta = true,
                other => {
                    return Err(Error::InvalidArgument(format!(
                        "unknown modifier '{other}' in '{spec}'"
                    )));
                }
            }
        }
        let key = parse_key(key_part.trim()).ok_or_else(|| {
            Error::InvalidArgument(format!("unknown key '{key_part}' in '{spec}'"))
        })?;
        Ok(Self { modifiers, key })
    }
}

fn parse_key(name: &str) -> Option<Key> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(if c == ' ' { Key::Space } else { Key::Char(c) });
    }
    let lower = name.to_ascii_lowercase();
    let key = match lower.as_str() {
        "tab" => Key::Tab,
        "enter" | "return" | "kp_enter" => Key::Enter,
        "space" | "spacebar" => Key::Space,
        "escape" | "esc" => Key::Escape,
        "backspace" | "back" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" | "prior" | "pgup" => Key::PageUp,
        "pagedown" | "page_down" | "next" | "pgdn" => Key::PageDown,
        "up" | "arrowup" => Key::Up,
        "down" | "arrowdown" => Key::Down,
        "left" | "arrowleft" => Key::Left,
        "right" | "arrowright" => Key::Right,
        "menu" | "apps" | "contextmenu" => Key::Menu,
        "printscreen" | "print" | "prtsc" => Key::PrintScreen,
        "plus" => Key::Char('+'),
        "minus" => Key::Char('-'),
        "comma" => Key::Char(','),
        "period" => Key::Char('.'),
        _ => {
            let n = lower.strip_prefix('f')?.parse::<u8>().ok()?;
            if (1..=24).contains(&n) {
                Key::F(n)
            } else {
                return None;
            }
        }
    };
    Some(key)
}

/// One step of keyboard input: a chord (`"ctrl+a"`) or literal text.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum KeyItem {
    /// Key chord such as `"Tab"`, `"ctrl+shift+Tab"`, `"alt+F4"`, `"Return"`.
    Chord(String),
    /// Literal text typed as Unicode characters (layout-independent).
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStroke {
    Chord(KeyChord),
    Text(String),
}

pub fn parse_items(items: &[KeyItem]) -> Result<Vec<KeyStroke>> {
    items
        .iter()
        .map(|item| match item {
            KeyItem::Chord(spec) => KeyChord::parse(spec).map(KeyStroke::Chord),
            KeyItem::Text { text } => Ok(KeyStroke::Text(text.clone())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_named_keys() {
        let chord = KeyChord::parse("Ctrl+Shift+tab").unwrap();
        assert!(chord.modifiers.ctrl && chord.modifiers.shift && !chord.modifiers.alt);
        assert_eq!(chord.key, Key::Tab);
        assert_eq!(KeyChord::parse("alt+F4").unwrap().key, Key::F(4));
        assert_eq!(KeyChord::parse("Return").unwrap().key, Key::Enter);
    }

    #[test]
    fn plus_key_and_single_chars() {
        let chord = KeyChord::parse("ctrl++").unwrap();
        assert!(chord.modifiers.ctrl);
        assert_eq!(chord.key, Key::Char('+'));
        assert_eq!(KeyChord::parse("+").unwrap().key, Key::Char('+'));
        assert_eq!(KeyChord::parse("ctrl+a").unwrap().key, Key::Char('a'));
    }

    #[test]
    fn rejects_unknown_names() {
        assert!(KeyChord::parse("hyper+a").is_err());
        assert!(KeyChord::parse("F25").is_err());
        assert!(KeyChord::parse("").is_err());
    }
}
