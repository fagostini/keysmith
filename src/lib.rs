//! Shared by `ks-inputd` (root) and `keysmith` (user engine).
//!
//! Wire protocol: newline-delimited text over a Unix socket.
//!   engine -> inputd: `bind <id> <combo>`
//!   inputd -> engine: `fired <id>` | `err <id> <reason>`
//! Raw key events never cross the socket; only match IDs do.
// ponytail: text lines instead of bincode frames; switch when messages carry structured data.

use evdev::KeyCode;
use std::collections::HashSet;

pub const SOCKET: &str = "/run/keysmith/inputd.sock";
pub const MAX_BINDINGS: usize = 256;

pub const CTRL: u8 = 1;
pub const ALT: u8 = 2;
pub const SHIFT: u8 = 4;
pub const SUPER: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Combo {
    pub mods: u8,
    pub key: KeyCode,
}

pub fn mod_bit(k: KeyCode) -> u8 {
    match k {
        KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => CTRL,
        KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT => ALT,
        KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => SHIFT,
        KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => SUPER,
        _ => 0,
    }
}

/// Keys that produce text. A binding on one of these without ctrl/alt/super
/// would let a client reconstruct what the user types, so inputd refuses it.
fn is_typing_key(k: KeyCode) -> bool {
    let c = k.code();
    // KEY_1..=KEY_BACKSPACE, KEY_TAB..=KEY_ENTER, KEY_A..=KEY_GRAVE, KEY_BACKSLASH..=KEY_SLASH,
    // KEY_SPACE, KEY_KPASTERISK, keypad block.
    matches!(c, 2..=14 | 15..=28 | 30..=41 | 43..=53 | 55 | 57 | 71..=83 | 96 | 98)
}

impl Combo {
    /// Why this combo may not be bound, if anything.
    pub fn refusal(&self) -> Option<&'static str> {
        if mod_bit(self.key) != 0 {
            Some("modifier cannot be the main key")
        } else if self.mods & (CTRL | ALT | SUPER) == 0 && is_typing_key(self.key) {
            Some("text keys need ctrl, alt or super")
        } else {
            None
        }
    }
}

impl std::str::FromStr for Combo {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let mut mods = 0;
        let mut key = None;
        for part in s.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= CTRL,
                "alt" => mods |= ALT,
                "shift" => mods |= SHIFT,
                "super" | "meta" | "win" => mods |= SUPER,
                _ if key.is_some() => return Err(format!("more than one key in {s:?}")),
                _ => {
                    let name = format!("KEY_{}", part.to_ascii_uppercase());
                    key = Some(name.parse().map_err(|_| format!("unknown key {part:?}"))?);
                }
            }
        }
        Ok(Combo { mods, key: key.ok_or_else(|| format!("no key in {s:?}"))? })
    }
}

/// `bind <id> <combo>` -> (id, combo)
pub fn parse_bind(line: &str) -> Result<(u32, Combo), String> {
    let mut it = line.splitn(3, ' ');
    match (it.next(), it.next(), it.next()) {
        (Some("bind"), Some(id), Some(combo)) => Ok((
            id.parse().map_err(|_| format!("bad id {id:?}"))?,
            combo.parse()?,
        )),
        _ => Err(format!("bad message {line:?}")),
    }
}

/// Passive hotkey matcher. Fed every key event; returns the binding id on a match.
#[derive(Default)]
pub struct Matcher {
    held: HashSet<KeyCode>,
    pub bindings: Vec<(u32, Combo)>,
}

impl Matcher {
    /// `value`: 1 press, 0 release, 2 autorepeat (evdev convention).
    pub fn key(&mut self, code: KeyCode, value: i32) -> Option<u32> {
        if mod_bit(code) != 0 {
            if value == 0 {
                self.held.remove(&code);
            } else {
                self.held.insert(code);
            }
            return None;
        }
        if value != 1 {
            return None;
        }
        let mods = self.held.iter().fold(0, |a, k| a | mod_bit(*k));
        self.bindings.iter().find(|(_, c)| c.key == code && c.mods == mods).map(|(id, _)| *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyCode as K;

    #[test]
    fn parse_combo() {
        let c: Combo = "Ctrl+Alt+t".parse().unwrap();
        assert_eq!(c, Combo { mods: CTRL | ALT, key: K::KEY_T });
        assert_eq!("super+f5".parse::<Combo>().unwrap().key, K::KEY_F5);
        assert!("ctrl+a+b".parse::<Combo>().is_err());
        assert!("ctrl".parse::<Combo>().is_err());
        assert!("ctrl+nope".parse::<Combo>().is_err());
        assert_eq!(parse_bind("bind 7 ctrl+alt+t").unwrap().0, 7);
        assert!(parse_bind("fired 7").is_err());
    }

    #[test]
    fn refuses_text_bindings() {
        let r = |s: &str| s.parse::<Combo>().unwrap().refusal();
        assert!(r("a").is_some());
        assert!(r("shift+a").is_some());
        assert!(r("space").is_some());
        assert!(r("enter").is_some());
        assert!(r("kp5").is_some());
        assert!(r("leftctrl").is_some());
        assert!(r("ctrl+a").is_none());
        assert!(r("super+enter").is_none());
        assert!(r("f5").is_none());
        assert!(r("volumeup").is_none());
    }

    #[test]
    fn matcher() {
        let mut m = Matcher::default();
        m.bindings.push((1, "ctrl+alt+t".parse().unwrap()));
        assert_eq!(m.key(K::KEY_T, 1), None);
        m.key(K::KEY_LEFTCTRL, 1);
        m.key(K::KEY_RIGHTALT, 1);
        assert_eq!(m.key(K::KEY_T, 1), Some(1));
        assert_eq!(m.key(K::KEY_T, 2), None, "autorepeat must not refire");
        m.key(K::KEY_LEFTSHIFT, 1);
        assert_eq!(m.key(K::KEY_T, 1), None, "extra modifier must not match");
        m.key(K::KEY_LEFTSHIFT, 0);
        m.key(K::KEY_RIGHTALT, 0);
        assert_eq!(m.key(K::KEY_T, 1), None);
    }
}
