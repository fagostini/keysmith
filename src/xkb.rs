//! Keyboard-layout knowledge for inputd: which keys produce a character (typist)
//! and which character a key produced (hotstring buffer).
// ponytail: always layout group 0. A session with `us,gb` types as `us`; track the
// active group when the M5 window backends report layout switches.
// ponytail: dead keys / compose are not decoded, so a trigger typed through a dead
// key (DE `´`+`e`) won't match; and the typist never uses dead keys, so such
// characters fall back to the clipboard. Add xkb compose tables if that bites.

use evdev::KeyCode;
use std::collections::HashMap;
use xkbcommon::xkb;

/// evdev keycode -> xkb keycode (X11 heritage: +8).
fn xk(k: KeyCode) -> xkb::Keycode {
    xkb::Keycode::new(k.code() as u32 + 8)
}

/// Layout names come from the unprivileged engine and are fed to the xkb file
/// loader as root, so only allow plain names (no paths).
pub fn valid_name(s: &str) -> bool {
    s.len() < 256 && s.chars().all(|c| c.is_ascii_alphanumeric() || "_,:()+-".contains(c))
}

/// Modifier combinations the typist may hold for a character, fewest first.
const COMBOS: [&[KeyCode]; 4] = [
    &[],
    &[KeyCode::KEY_LEFTSHIFT],
    &[KeyCode::KEY_RIGHTALT],
    &[KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTALT],
];

pub struct Layout {
    keymap: xkb::Keymap,
    /// char -> (key, modifiers to hold)
    map: HashMap<char, (KeyCode, &'static [KeyCode])>,
}

impl Layout {
    pub fn new(layout: &str, variant: &str, options: &str) -> Result<Layout, String> {
        if ![layout, variant, options].iter().all(|s| valid_name(s)) {
            return Err("invalid layout name".into());
        }
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let opts = (!options.is_empty()).then(|| options.to_owned());
        let keymap = xkb::Keymap::new_from_names(&ctx, "", "", layout, variant, opts, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .ok_or_else(|| format!("xkb cannot compile layout {layout:?} variant {variant:?}"))?;
        let mut map = HashMap::new();
        for mods in COMBOS {
            let mut st = xkb::State::new(&keymap);
            for m in mods {
                st.update_key(xk(*m), xkb::KeyDirection::Down);
            }
            for code in 1..0x100u16 {
                let k = KeyCode::new(code);
                let utf8 = st.key_get_utf8(xk(k));
                let mut chars = utf8.chars();
                if let (Some(c), None) = (chars.next(), chars.next())
                    && (!c.is_control() || c == ' ') {
                        map.entry(c).or_insert((k, mods));
                    }
            }
        }
        // Control characters a replacement may contain.
        map.insert('\n', (KeyCode::KEY_ENTER, COMBOS[0]));
        map.insert('\t', (KeyCode::KEY_TAB, COMBOS[0]));
        Ok(Layout { keymap, map })
    }

    pub fn key_for(&self, c: char) -> Option<KeyCode> {
        self.map.get(&c).map(|(k, _)| *k)
    }

    /// Full press/release sequence typing `text`, or None if some character
    /// isn't on this layout (caller falls back to the clipboard).
    pub fn type_keys(&self, text: &str) -> Option<Vec<(KeyCode, i32)>> {
        let mut out = Vec::new();
        for c in text.chars() {
            let (k, mods) = self.map.get(&c)?;
            out.extend(mods.iter().map(|m| (*m, 1)));
            out.extend([(*k, 1), (*k, 0)]);
            out.extend(mods.iter().rev().map(|m| (*m, 0)));
        }
        Some(out)
    }

    pub fn decoder(&self) -> Decoder {
        Decoder(xkb::State::new(&self.keymap))
    }
}

/// Tracks modifier/lock state from the key stream the desktop sees.
pub struct Decoder(xkb::State);

impl Decoder {
    /// Character produced by a key press, if any. Call for every event (incl. releases).
    pub fn feed(&mut self, k: KeyCode, v: i32) -> Option<char> {
        let c = if v == 1 {
            let utf8 = self.0.key_get_utf8(xk(k));
            let mut s = utf8.chars();
            match (s.next(), s.next()) {
                (Some(c), None) if !c.is_control() || c == ' ' => Some(c),
                _ => None,
            }
        } else {
            None
        };
        match v {
            1 => drop(self.0.update_key(xk(k), xkb::KeyDirection::Down)),
            0 => drop(self.0.update_key(xk(k), xkb::KeyDirection::Up)),
            _ => {}
        }
        c
    }

    pub fn caps_lock(&self) -> bool {
        self.0.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typing `text` and decoding the keys with a fresh state must give `text` back.
    fn roundtrip(layout: &str, text: &str) {
        let l = Layout::new(layout, "", "").unwrap();
        let keys = l.type_keys(text).unwrap_or_else(|| panic!("{layout}: {text:?} not typable"));
        let mut d = l.decoder();
        let got: String = keys.iter().filter_map(|(k, v)| d.feed(*k, *v)).collect();
        assert_eq!(got, text, "layout {layout}");
    }

    #[test]
    fn layouts() {
        let common = "Hello, World! 123 (a+b)=c; x/y? \"q\" 'r' @#";
        for l in ["us", "de", "fr", "it"] {
            roundtrip(l, common);
        }
        roundtrip("us", "~`|\\{}[]<>$%^&*_");
        roundtrip("de", "äöüß ÄÖÜ € {[]}\\ |~ µ");
        roundtrip("fr", "éèàçù € ²");
        roundtrip("it", "àèéìòù € §");
        // Same character, different key per layout.
        let k = |l: &str, c| Layout::new(l, "", "").unwrap().key_for(c);
        assert_eq!(k("us", 'z'), Some(KeyCode::KEY_Z));
        assert_eq!(k("de", 'z'), Some(KeyCode::KEY_Y));
        assert_eq!(k("fr", 'a'), Some(KeyCode::KEY_Q));
        assert!(Layout::new("us", "", "").unwrap().type_keys("é🙂").is_none());
        assert!(Layout::new("../../etc/passwd", "", "").is_err());
    }
}
