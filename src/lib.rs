//! Shared by `ks-inputd` (root) and `keysmith` (user engine).
//!
//! Wire protocol: newline-delimited text over a Unix socket.
//!   engine -> inputd: `bind <id> <combo>` | `remap <id> <key> <action>` | `taphold_ms <ms>`
//!   inputd -> engine: `fired <id>` | `err <id> <reason>`
//! Raw key events never cross the socket; only match IDs do.
// ponytail: text lines instead of bincode frames; switch when messages carry structured data.

use evdev::KeyCode;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

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

/// Keys the virtual keyboard declares and a remap may output.
// ponytail: every code below BTN_MISC (0x100); narrow it if a compositor mis-classifies the device.
pub fn is_output_key(k: KeyCode) -> bool {
    (1..0x100).contains(&k.code())
}

fn key(name: &str) -> Result<KeyCode, String> {
    let k: KeyCode = format!("KEY_{}", name.to_ascii_uppercase()).parse().map_err(|_| format!("unknown key {name:?}"))?;
    if is_output_key(k) { Ok(k) } else { Err(format!("{name:?} is not a keyboard key")) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Key(KeyCode),
    TapHold { tap: KeyCode, hold: KeyCode },
}

impl std::str::FromStr for Action {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let mut words = s.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(one), None, _) if !one.contains(':') => Ok(Action::Key(key(one)?)),
            (Some(t), Some(h), None) => match (t.strip_prefix("tap:"), h.strip_prefix("hold:")) {
                (Some(t), Some(h)) => Ok(Action::TapHold { tap: key(t)?, hold: key(h)? }),
                _ => Err(format!("expected `tap:<key> hold:<key>`, got {s:?}")),
            },
            _ => Err(format!("expected `<key>` or `tap:<key> hold:<key>`, got {s:?}")),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Msg {
    Bind(u32, Combo),
    Remap(u32, KeyCode, Action),
    TapHoldMs(u64),
}

/// Parses one engine -> inputd line. Errors carry the id when one was readable.
pub fn parse_msg(line: &str) -> Result<Msg, (u32, String)> {
    let mut it = line.splitn(3, ' ');
    let (verb, a, rest) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
    if verb == "taphold_ms" {
        return match a.parse() {
            Ok(ms) if (50..=2000).contains(&ms) => Ok(Msg::TapHoldMs(ms)),
            _ => Err((0, format!("taphold_ms must be 50..=2000, got {a:?}"))),
        };
    }
    let id: u32 = a.parse().map_err(|_| (0, format!("bad message {line:?}")))?;
    let e = |m: String| (id, m);
    match verb {
        "bind" => Ok(Msg::Bind(id, rest.parse().map_err(e)?)),
        "remap" => {
            let (src, action) = rest.split_once(' ').ok_or_else(|| e(format!("bad remap {rest:?}")))?;
            Ok(Msg::Remap(id, key(src).map_err(e)?, action.parse().map_err(e)?))
        }
        _ => Err(e(format!("unknown message {verb:?}"))),
    }
}

/// Rewrites a grabbed keyboard's events. Output goes to the virtual keyboard.
pub struct Remapper {
    pub rules: HashMap<KeyCode, Action>,
    pub taphold: Duration,
    /// Tap-hold key pressed but not yet decided: (source key, press time).
    pending: Option<(KeyCode, Instant)>,
    /// Tap-hold sources currently acting as their hold key.
    holding: HashMap<KeyCode, KeyCode>,
    /// Output keys currently down, so they can all be released on teardown.
    down: HashSet<KeyCode>,
}

impl Default for Remapper {
    fn default() -> Self {
        Remapper {
            rules: HashMap::new(),
            taphold: Duration::from_millis(200),
            pending: None,
            holding: HashMap::new(),
            down: HashSet::new(),
        }
    }
}

impl Remapper {
    fn out(&mut self, out: &mut Vec<(KeyCode, i32)>, k: KeyCode, v: i32) {
        match v {
            0 if !self.down.remove(&k) => return, // never release what we did not press
            0 => {}
            _ => {
                self.down.insert(k);
            }
        }
        out.push((k, v));
    }

    /// Turn the pending tap-hold key into its hold key.
    fn resolve_hold(&mut self, out: &mut Vec<(KeyCode, i32)>) {
        if let Some((src, _)) = self.pending.take()
            && let Some(Action::TapHold { hold, .. }) = self.rules.get(&src).copied() {
                self.holding.insert(src, hold);
                self.out(out, hold, 1);
            }
    }

    /// When the caller must call `tick` if no event arrives first.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, t)| t + self.taphold)
    }

    pub fn tick(&mut self, now: Instant) -> Vec<(KeyCode, i32)> {
        let mut out = Vec::new();
        if self.deadline().is_some_and(|d| now >= d) {
            self.resolve_hold(&mut out);
        }
        out
    }

    /// `value`: 1 press, 0 release, 2 autorepeat.
    pub fn key(&mut self, k: KeyCode, v: i32, now: Instant) -> Vec<(KeyCode, i32)> {
        let mut out = self.tick(now);
        match (self.rules.get(&k).copied(), v) {
            (Some(Action::TapHold { tap, .. }), _) => match v {
                1 if self.pending.is_none() && !self.holding.contains_key(&k) => {
                    self.pending = Some((k, now));
                }
                0 if self.pending.is_some_and(|(p, _)| p == k) => {
                    self.pending = None;
                    self.out(&mut out, tap, 1);
                    self.out(&mut out, tap, 0);
                }
                0 => {
                    if let Some(hold) = self.holding.remove(&k) {
                        self.out(&mut out, hold, 0);
                    }
                }
                _ => {} // repeats, or a second tap-hold key while one is pending
            },
            (rule, _) => {
                if v == 1 {
                    // Another key went down during the tap window: it's a hold (Caps+C = Ctrl+C).
                    self.resolve_hold(&mut out);
                }
                let dst = match rule {
                    Some(Action::Key(d)) => d,
                    _ => k,
                };
                self.out(&mut out, dst, v);
            }
        }
        out
    }

    /// Releases every output key still down. Call before ungrabbing.
    pub fn release_all(&mut self) -> Vec<(KeyCode, i32)> {
        self.pending = None;
        self.holding.clear();
        
        self.down.drain().map(|k| (k, 0)).collect()
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
        assert!(matches!(parse_msg("bind 7 ctrl+alt+t"), Ok(Msg::Bind(7, _))));
        assert!(parse_msg("fired 7").is_err());
        assert_eq!(
            parse_msg("remap 3 capslock tap:esc hold:leftctrl"),
            Ok(Msg::Remap(3, K::KEY_CAPSLOCK, Action::TapHold { tap: K::KEY_ESC, hold: K::KEY_LEFTCTRL }))
        );
        assert_eq!(parse_msg("remap 4 capslock esc"), Ok(Msg::Remap(4, K::KEY_CAPSLOCK, Action::Key(K::KEY_ESC))));
        assert_eq!(parse_msg("remap 5 capslock btn_left").unwrap_err().0, 5);
        assert!(parse_msg("remap 5 capslock tap:esc").is_err());
        assert_eq!(parse_msg("taphold_ms 150"), Ok(Msg::TapHoldMs(150)));
        assert!(parse_msg("taphold_ms 5").is_err());
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

    fn caps() -> (Remapper, Instant) {
        let mut r = Remapper::default();
        r.rules.insert(K::KEY_CAPSLOCK, Action::TapHold { tap: K::KEY_ESC, hold: K::KEY_LEFTCTRL });
        r.rules.insert(K::KEY_RIGHTALT, Action::Key(K::KEY_LEFTMETA));
        (r, Instant::now())
    }
    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn tap_hold() {
        let (mut r, t) = caps();
        // Quick tap -> Esc.
        assert_eq!(r.key(K::KEY_CAPSLOCK, 1, t), vec![]);
        assert_eq!(r.key(K::KEY_CAPSLOCK, 0, t + 50 * MS), vec![(K::KEY_ESC, 1), (K::KEY_ESC, 0)]);
        // Caps + C inside the window -> Ctrl+C immediately.
        r.key(K::KEY_CAPSLOCK, 1, t);
        assert_eq!(r.key(K::KEY_C, 1, t + 10 * MS), vec![(K::KEY_LEFTCTRL, 1), (K::KEY_C, 1)]);
        assert_eq!(r.key(K::KEY_C, 0, t + 20 * MS), vec![(K::KEY_C, 0)]);
        assert_eq!(r.key(K::KEY_CAPSLOCK, 0, t + 30 * MS), vec![(K::KEY_LEFTCTRL, 0)]);
        // Held past the window -> Ctrl via tick, no Esc on release.
        r.key(K::KEY_CAPSLOCK, 1, t);
        assert_eq!(r.deadline(), Some(t + 200 * MS));
        assert_eq!(r.tick(t + 199 * MS), vec![]);
        assert_eq!(r.tick(t + 200 * MS), vec![(K::KEY_LEFTCTRL, 1)]);
        assert_eq!(r.key(K::KEY_CAPSLOCK, 2, t + 300 * MS), vec![]);
        assert_eq!(r.key(K::KEY_CAPSLOCK, 0, t + 400 * MS), vec![(K::KEY_LEFTCTRL, 0)]);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn plain_remap_and_passthrough() {
        let (mut r, t) = caps();
        assert_eq!(r.key(K::KEY_RIGHTALT, 1, t), vec![(K::KEY_LEFTMETA, 1)]);
        assert_eq!(r.key(K::KEY_RIGHTALT, 2, t), vec![(K::KEY_LEFTMETA, 2)]);
        assert_eq!(r.key(K::KEY_RIGHTALT, 0, t), vec![(K::KEY_LEFTMETA, 0)]);
        assert_eq!(r.key(K::KEY_A, 1, t), vec![(K::KEY_A, 1)]);
        // Release of a key pressed before the grab is swallowed, not forwarded.
        assert_eq!(r.key(K::KEY_B, 0, t), vec![]);
    }

    #[test]
    fn no_stuck_keys() {
        let (mut r, t) = caps();
        r.key(K::KEY_CAPSLOCK, 1, t);
        r.key(K::KEY_X, 1, t); // Ctrl + X down
        r.key(K::KEY_RIGHTALT, 1, t); // Meta down
        let mut up = r.release_all();
        up.sort_by_key(|(k, _)| k.code());
        assert_eq!(up, vec![(K::KEY_LEFTCTRL, 0), (K::KEY_X, 0), (K::KEY_LEFTMETA, 0)]);
        assert_eq!(r.release_all(), vec![]);
        // Releasing physical keys afterwards emits nothing.
        assert_eq!(r.key(K::KEY_CAPSLOCK, 0, t), vec![]);
        assert_eq!(r.key(K::KEY_X, 0, t), vec![]);
    }
}
