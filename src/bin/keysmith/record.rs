//! Macro recorder, engine side: turns the `rec` events of one session into a
//! Luau script. Typed characters become `send()`, other keys `key()`. Pauses of
//! 300 ms or more around shortcuts become `sleep()`, capped at 1 s; pauses
//! while typing are dropped (they're hesitation, not waiting for an app).

use keysmith::xkb::Layout;
use keysmith::{ALT, CTRL, Combo, SUPER, mod_bit};
use evdev::KeyCode;

/// The default hotkey written into recordings; the user picks their own.
pub const PLAY: &str = "ctrl+alt+shift+p";

enum Step {
    Text(String),
    Key(String),
    Sleep(u64),
}

/// `events`: (evdev code, 1 press / 0 release, ms since start).
pub fn to_luau(events: &[(u16, i32, u64)], layout: Option<&Layout>, header: &str) -> String {
    let mut dec = layout.map(Layout::decoder);
    let (mut steps, mut mods, mut last) = (Vec::new(), 0u8, None::<u64>);
    for &(code, v, ms) in events {
        let k = KeyCode::new(code);
        let c = dec.as_mut().and_then(|d| d.feed(k, v));
        let bit = mod_bit(k);
        if bit != 0 {
            mods = if v == 0 { mods & !bit } else { mods | bit };
            continue;
        }
        if v != 1 {
            continue;
        }
        let plain = mods & (CTRL | ALT | SUPER) == 0;
        let typing = plain && c.is_some() && matches!(steps.last(), Some(Step::Text(_)));
        if let Some(prev) = last.replace(ms)
            && ms - prev >= 300
            && !typing
        {
            steps.push(Step::Sleep((ms - prev).min(1000) / 10 * 10));
        }
        match (steps.last_mut(), c) {
            (Some(Step::Text(t)), Some(c)) if plain => t.push(c),
            (_, Some(c)) if plain => steps.push(Step::Text(c.into())),
            (Some(Step::Text(t)), None) if plain && k == KeyCode::KEY_BACKSPACE => {
                t.pop();
                if t.is_empty() {
                    steps.pop();
                }
            }
            _ => steps.push(Step::Key(Combo { mods, key: k }.to_string())),
        }
    }
    let mut out = format!("{header}\nhotkey(\"{PLAY}\", function()\n");
    for s in steps {
        out += &match s {
            Step::Text(t) => format!("    send({t:?})\n"),
            Step::Key(k) => format!("    key({k:?})\n"),
            Step::Sleep(ms) => format!("    sleep({ms})\n"),
        };
    }
    out + "end)\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyCode as K;

    #[test]
    fn converts() {
        let us = Layout::new("us", "", "").unwrap();
        let ev = |k: K, v, ms| (k.code(), v, ms);
        let events = [
            // "Hi" with shift, a typo fixed with backspace
            ev(K::KEY_LEFTSHIFT, 1, 0),
            ev(K::KEY_H, 1, 10),
            ev(K::KEY_H, 0, 20),
            ev(K::KEY_LEFTSHIFT, 0, 30),
            ev(K::KEY_I, 1, 700), // hesitation while typing: dropped
            ev(K::KEY_X, 1, 710),
            ev(K::KEY_BACKSPACE, 1, 720),
            // long pause (capped at 1 s), then ctrl+s and Enter
            ev(K::KEY_LEFTCTRL, 1, 6000),
            ev(K::KEY_S, 1, 6010),
            ev(K::KEY_LEFTCTRL, 0, 6020),
            ev(K::KEY_ENTER, 1, 6030),
            ev(K::KEY_F5, 1, 6040),
        ];
        let lua = to_luau(&events, Some(&us), "-- test");
        assert_eq!(
            lua,
            "-- test\nhotkey(\"ctrl+alt+shift+p\", function()\n    send(\"Hi\")\n    sleep(1000)\n    key(\"ctrl+s\")\n    key(\"enter\")\n    key(\"f5\")\nend)\n"
        );
        // Output loads as a script.
        let send: crate::script::SendFn = std::sync::Arc::new(|_, _, _| Ok(()));
        crate::script::spawn("r.luau".into(), lua, send, Default::default()).unwrap();
    }
}
