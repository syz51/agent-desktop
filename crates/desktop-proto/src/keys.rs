use serde::{Deserialize, Serialize};

use crate::rpc::{ErrorCode, RpcError};

/// Portable modifier bitset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub fn_key: bool,
}

impl Modifiers {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !(self.cmd || self.ctrl || self.alt || self.shift || self.fn_key)
    }
}

/// Platform-independent logical key codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Minus,
    Equal,
    LeftBracket,
    RightBracket,
    Backslash,
    Semicolon,
    Quote,
    Comma,
    Period,
    Slash,
    Grave,
    Return,
    Tab,
    Space,
    Backspace,
    Delete,
    Escape,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl KeyCode {
    /// macOS ANSI virtual key code (`CGKeyCode`).
    #[must_use]
    pub fn macos_keycode(self) -> u16 {
        match self {
            Self::A => 0,
            Self::S => 1,
            Self::D => 2,
            Self::F => 3,
            Self::H => 4,
            Self::G => 5,
            Self::Z => 6,
            Self::X => 7,
            Self::C => 8,
            Self::V => 9,
            Self::B => 11,
            Self::Q => 12,
            Self::W => 13,
            Self::E => 14,
            Self::R => 15,
            Self::Y => 16,
            Self::T => 17,
            Self::Digit1 => 18,
            Self::Digit2 => 19,
            Self::Digit3 => 20,
            Self::Digit4 => 21,
            Self::Digit6 => 22,
            Self::Digit5 => 23,
            Self::Equal => 24,
            Self::Digit9 => 25,
            Self::Digit7 => 26,
            Self::Minus => 27,
            Self::Digit8 => 28,
            Self::Digit0 => 29,
            Self::RightBracket => 30,
            Self::O => 31,
            Self::U => 32,
            Self::LeftBracket => 33,
            Self::I => 34,
            Self::P => 35,
            Self::Return => 36,
            Self::L => 37,
            Self::J => 38,
            Self::Quote => 39,
            Self::K => 40,
            Self::Semicolon => 41,
            Self::Backslash => 42,
            Self::Comma => 43,
            Self::Slash => 44,
            Self::N => 45,
            Self::M => 46,
            Self::Period => 47,
            Self::Tab => 48,
            Self::Space => 49,
            Self::Grave => 50,
            Self::Backspace => 51,
            Self::Escape => 53,
            Self::F5 => 96,
            Self::F6 => 97,
            Self::F7 => 98,
            Self::F3 => 99,
            Self::F8 => 100,
            Self::F9 => 101,
            Self::F11 => 103,
            Self::F10 => 109,
            Self::F12 => 111,
            Self::Home => 115,
            Self::PageUp => 116,
            Self::Delete => 117,
            Self::F4 => 118,
            Self::End => 119,
            Self::F2 => 120,
            Self::PageDown => 121,
            Self::F1 => 122,
            Self::Left => 123,
            Self::Right => 124,
            Self::Down => 125,
            Self::Up => 126,
        }
    }

    /// Standard X11 keysym (`KeySym`) for Linux X11/XTest and XWayland input.
    #[must_use]
    pub fn x11_keysym(self) -> u32 {
        match self {
            Self::A => u32::from(b'a'),
            Self::B => u32::from(b'b'),
            Self::C => u32::from(b'c'),
            Self::D => u32::from(b'd'),
            Self::E => u32::from(b'e'),
            Self::F => u32::from(b'f'),
            Self::G => u32::from(b'g'),
            Self::H => u32::from(b'h'),
            Self::I => u32::from(b'i'),
            Self::J => u32::from(b'j'),
            Self::K => u32::from(b'k'),
            Self::L => u32::from(b'l'),
            Self::M => u32::from(b'm'),
            Self::N => u32::from(b'n'),
            Self::O => u32::from(b'o'),
            Self::P => u32::from(b'p'),
            Self::Q => u32::from(b'q'),
            Self::R => u32::from(b'r'),
            Self::S => u32::from(b's'),
            Self::T => u32::from(b't'),
            Self::U => u32::from(b'u'),
            Self::V => u32::from(b'v'),
            Self::W => u32::from(b'w'),
            Self::X => u32::from(b'x'),
            Self::Y => u32::from(b'y'),
            Self::Z => u32::from(b'z'),
            Self::Digit0 => u32::from(b'0'),
            Self::Digit1 => u32::from(b'1'),
            Self::Digit2 => u32::from(b'2'),
            Self::Digit3 => u32::from(b'3'),
            Self::Digit4 => u32::from(b'4'),
            Self::Digit5 => u32::from(b'5'),
            Self::Digit6 => u32::from(b'6'),
            Self::Digit7 => u32::from(b'7'),
            Self::Digit8 => u32::from(b'8'),
            Self::Digit9 => u32::from(b'9'),
            Self::Minus => u32::from(b'-'),
            Self::Equal => u32::from(b'='),
            Self::LeftBracket => u32::from(b'['),
            Self::RightBracket => u32::from(b']'),
            Self::Backslash => u32::from(b'\\'),
            Self::Semicolon => u32::from(b';'),
            Self::Quote => u32::from(b'\''),
            Self::Comma => u32::from(b','),
            Self::Period => u32::from(b'.'),
            Self::Slash => u32::from(b'/'),
            Self::Grave => u32::from(b'`'),
            Self::Return => 0xff0d,
            Self::Tab => 0xff09,
            Self::Space => 0x0020,
            Self::Backspace => 0xff08,
            Self::Delete => 0xffff,
            Self::Escape => 0xff1b,
            Self::Home => 0xff50,
            Self::End => 0xff57,
            Self::PageUp => 0xff55,
            Self::PageDown => 0xff56,
            Self::Left => 0xff51,
            Self::Up => 0xff52,
            Self::Right => 0xff53,
            Self::Down => 0xff54,
            Self::F1 => 0xffbe,
            Self::F2 => 0xffbf,
            Self::F3 => 0xffc0,
            Self::F4 => 0xffc1,
            Self::F5 => 0xffc2,
            Self::F6 => 0xffc3,
            Self::F7 => 0xffc4,
            Self::F8 => 0xffc5,
            Self::F9 => 0xffc6,
            Self::F10 => 0xffc7,
            Self::F11 => 0xffc8,
            Self::F12 => 0xffc9,
        }
    }
}

/// Map a printable character (or `\n`/`\t`) to its X11 keysym.
#[must_use]
pub fn char_to_x11_keysym(ch: char) -> Option<u32> {
    match ch {
        '\n' | '\r' => Some(0xff0d),
        '\t' => Some(0xff09),
        c if (0x20..=0x7e).contains(&(c as u32)) => Some(c as u32),
        c if (0xa0..=0xff).contains(&(c as u32)) => Some(c as u32),
        c if (c as u32) >= 0x100 => Some(0x0100_0000 | (c as u32)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCombo {
    pub key: KeyCode,
    pub modifiers: Modifiers,
}

/// Parse a list of modifier names (`cmd`, `ctrl`, `alt`, `shift`, `fn`, or portable `mod`).
pub fn parse_modifiers<S: AsRef<str>>(names: &[S], is_macos: bool) -> Result<Modifiers, RpcError> {
    let mut mods = Modifiers::default();
    for raw in names {
        match raw.as_ref().trim().to_ascii_lowercase().as_str() {
            "cmd" | "command" | "meta" | "super" => mods.cmd = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "opt" | "option" => mods.alt = true,
            "shift" => mods.shift = true,
            "fn" => mods.fn_key = true,
            "mod" => {
                if is_macos {
                    mods.cmd = true;
                } else {
                    mods.ctrl = true;
                }
            }
            other => {
                return Err(RpcError::with_next(
                    ErrorCode::InvalidInput,
                    format!("unknown modifier {other:?}"),
                    "Use cmd, ctrl, alt, shift, fn, or mod.",
                ));
            }
        }
    }
    Ok(mods)
}

/// Parse a combo string such as `"cmd+shift+t"`, `"mod+c"`, `"Return"`, or `"Escape"`.
pub fn parse_key_combo(combo: &str, is_macos: bool) -> Result<KeyCombo, RpcError> {
    if combo.trim().is_empty() {
        return Err(RpcError::with_next(
            ErrorCode::InvalidInput,
            "key combo cannot be empty",
            "Give a key combo such as cmd+t, mod+c, Return, or Escape.",
        ));
    }
    let mut parts: Vec<&str> = combo.split('+').map(str::trim).collect();
    let key_raw = parts.pop().unwrap_or_default();
    if key_raw.is_empty() {
        return Err(RpcError::with_next(
            ErrorCode::InvalidInput,
            format!("missing key name in combo {combo:?}"),
            "End the combo with a key name such as Return, Tab, a, or Escape.",
        ));
    }
    let normalized = if key_raw.len() == 1 {
        key_raw.to_ascii_lowercase()
    } else {
        key_raw
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '_' && *c != '-')
            .collect()
    };
    let key = match normalized.as_str() {
        "a" => KeyCode::A,
        "b" => KeyCode::B,
        "c" => KeyCode::C,
        "d" => KeyCode::D,
        "e" => KeyCode::E,
        "f" => KeyCode::F,
        "g" => KeyCode::G,
        "h" => KeyCode::H,
        "i" => KeyCode::I,
        "j" => KeyCode::J,
        "k" => KeyCode::K,
        "l" => KeyCode::L,
        "m" => KeyCode::M,
        "n" => KeyCode::N,
        "o" => KeyCode::O,
        "p" => KeyCode::P,
        "q" => KeyCode::Q,
        "r" => KeyCode::R,
        "s" => KeyCode::S,
        "t" => KeyCode::T,
        "u" => KeyCode::U,
        "v" => KeyCode::V,
        "w" => KeyCode::W,
        "x" => KeyCode::X,
        "y" => KeyCode::Y,
        "z" => KeyCode::Z,
        "0" => KeyCode::Digit0,
        "1" => KeyCode::Digit1,
        "2" => KeyCode::Digit2,
        "3" => KeyCode::Digit3,
        "4" => KeyCode::Digit4,
        "5" => KeyCode::Digit5,
        "6" => KeyCode::Digit6,
        "7" => KeyCode::Digit7,
        "8" => KeyCode::Digit8,
        "9" => KeyCode::Digit9,
        "-" | "minus" => KeyCode::Minus,
        "=" | "equal" | "plus" => KeyCode::Equal,
        "[" | "leftbracket" => KeyCode::LeftBracket,
        "]" | "rightbracket" => KeyCode::RightBracket,
        "\\" | "backslash" => KeyCode::Backslash,
        ";" | "semicolon" => KeyCode::Semicolon,
        "'" | "quote" => KeyCode::Quote,
        "," | "comma" => KeyCode::Comma,
        "." | "period" => KeyCode::Period,
        "/" | "slash" => KeyCode::Slash,
        "`" | "grave" | "backtick" => KeyCode::Grave,
        "return" | "enter" => KeyCode::Return,
        "tab" => KeyCode::Tab,
        "space" | " " => KeyCode::Space,
        "backspace" | "deletebackward" => KeyCode::Backspace,
        "delete" | "forwarddelete" | "del" => KeyCode::Delete,
        "escape" | "esc" => KeyCode::Escape,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "left" | "arrowleft" => KeyCode::Left,
        "right" | "arrowright" => KeyCode::Right,
        "up" | "arrowup" => KeyCode::Up,
        "down" | "arrowdown" => KeyCode::Down,
        "f1" => KeyCode::F1,
        "f2" => KeyCode::F2,
        "f3" => KeyCode::F3,
        "f4" => KeyCode::F4,
        "f5" => KeyCode::F5,
        "f6" => KeyCode::F6,
        "f7" => KeyCode::F7,
        "f8" => KeyCode::F8,
        "f9" => KeyCode::F9,
        "f10" => KeyCode::F10,
        "f11" => KeyCode::F11,
        "f12" => KeyCode::F12,
        _ => {
            return Err(RpcError::with_next(
                ErrorCode::InvalidInput,
                format!("unknown key {key_raw:?} in combo {combo:?}"),
                "Use standard key names like a-z, 0-9, Return, Tab, Escape, Space, Backspace, Delete, Left/Right/Up/Down, F1-F12.",
            ));
        }
    };
    let modifiers = parse_modifiers(&parts, is_macos)?;
    Ok(KeyCombo { key, modifiers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combos_and_portable_mod() {
        let mac = parse_key_combo("mod+shift+P", true).unwrap();
        assert_eq!(mac.key, KeyCode::P);
        assert!(mac.modifiers.cmd);
        assert!(mac.modifiers.shift);
        assert!(!mac.modifiers.ctrl);

        let linux = parse_key_combo("mod+c", false).unwrap();
        assert_eq!(linux.key, KeyCode::C);
        assert!(linux.modifiers.ctrl);
        assert!(!linux.modifiers.cmd);

        let esc = parse_key_combo("esc", true).unwrap();
        assert_eq!(esc.key.macos_keycode(), 53);
        assert_eq!(esc.key.x11_keysym(), 0xff1b);
        assert_eq!(KeyCode::Return.x11_keysym(), 0xff0d);
        assert_eq!(KeyCode::A.x11_keysym(), u32::from(b'a'));
        assert_eq!(char_to_x11_keysym('A'), Some(u32::from(b'A')));
        assert_eq!(char_to_x11_keysym('\n'), Some(0xff0d));
        assert_eq!(char_to_x11_keysym('你'), Some(0x0100_4f60));
    }

    #[test]
    fn rejects_invalid_combos() {
        assert!(parse_key_combo("", true).is_err());
        assert!(parse_key_combo("cmd+", true).is_err());
        assert!(parse_key_combo("hyper+a", true).is_err());
        assert!(parse_key_combo("cmd+boguskey", true).is_err());
    }
}
