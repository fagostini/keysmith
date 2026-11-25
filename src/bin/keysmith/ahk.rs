//! `keysmith convert`: AutoHotkey (v1 or v2) to Luau, for the common patterns:
//! hotkeys (one-line and blocks), remaps, hotstrings and their options, Send
//! (with {Keys} and ^!+# modifiers), SendText/SendRaw, Sleep, Run, MsgBox,
//! ToolTip, #HotIf/#IfWinActive and `if WinActive(...)` blocks, comments.
//! Anything else becomes a `-- TODO(ahk):` line, counted in the result.
// ponytail: line-based, no expression parser. Variables, functions, loops and
// GUI code are out of scope for a converter (§2 Non-goals).

use keysmith::Combo;

/// AHK key name to evdev name.
fn key(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let mapped = match n.as_str() {
        "esc" | "escape" => "esc",
        "bs" | "backspace" => "backspace",
        "del" | "delete" => "delete",
        "ins" | "insert" => "insert",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        "printscreen" => "sysrq",
        "appskey" => "compose",
        "lwin" => "leftmeta",
        "rwin" => "rightmeta",
        "ctrl" | "control" | "lctrl" | "lcontrol" => "leftctrl",
        "rctrl" | "rcontrol" => "rightctrl",
        "shift" | "lshift" => "leftshift",
        "rshift" => "rightshift",
        "alt" | "lalt" => "leftalt",
        "ralt" => "rightalt",
        "volume_up" => "volumeup",
        "volume_down" => "volumedown",
        "volume_mute" => "mute",
        "media_play_pause" => "playpause",
        "media_next" => "nextsong",
        "media_prev" => "previoussong",
        "media_stop" => "stopcd",
        "browser_home" => "homepage",
        "numpadenter" => "kpenter",
        "numpadadd" => "kpplus",
        "numpadsub" => "kpminus",
        "numpadmult" => "kpasterisk",
        "numpaddiv" => "kpslash",
        "numpaddot" => "kpdot",
        "-" => "minus",
        "=" => "equal",
        "[" => "leftbrace",
        "]" => "rightbrace",
        ";" => "semicolon",
        "'" => "apostrophe",
        "`" => "grave",
        "\\" => "backslash",
        "," => "comma",
        "." => "dot",
        "/" => "slash",
        " " => "space",
        _ => match n.strip_prefix("numpad") {
            Some(d) => return key(&format!("kp{d}")),
            None => n.as_str(),
        },
    };
    let valid = format!("KEY_{}", mapped.to_ascii_uppercase()).parse::<evdev::KeyCode>().is_ok();
    valid.then(|| mapped.to_owned())
}

fn mod_name(c: char) -> Option<&'static str> {
    match c {
        '^' => Some("ctrl+"),
        '!' => Some("alt+"),
        '+' => Some("shift+"),
        '#' => Some("super+"),
        _ => None,
    }
}

/// Hotkey label (`^!t`, `~$CapsLock`, `#Enter`) to a combo string.
fn combo(lhs: &str) -> Result<String, String> {
    if lhs.contains(" & ") || lhs.to_ascii_lowercase().ends_with(" up") {
        return Err("custom combinations and key-up hotkeys aren't supported".into());
    }
    let mut s = lhs.trim_start_matches(['~', '*', '$']);
    let mut out = String::new();
    while s.chars().count() > 1 {
        let c = s.chars().next().unwrap();
        if let Some(m) = mod_name(c) {
            out += m;
        } else if !matches!(c, '<' | '>') {
            break;
        }
        s = &s[1..];
    }
    out += &key(s).ok_or(format!("unknown key {s:?}"))?;
    let c: Combo = out.parse()?;
    match c.refusal() {
        Some(why) => Err(format!("Keysmith refuses this hotkey: {why}")),
        None => Ok(out),
    }
}

/// AHK backtick escapes.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match (c, c == '`') {
            (_, true) => match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') | None => {}
                Some(o) => out.push(o),
            },
            _ => out.push(c),
        }
    }
    out
}

/// A string argument: v2 `"text"` / `'text'`, or v1 bare text. None for expressions.
fn literal(arg: &str) -> Option<String> {
    let arg = arg.trim();
    if let Some(q) = arg.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let inner = arg.strip_prefix(q)?.strip_suffix(q)?;
        // An unescaped quote inside means concatenation: `"a" . x . "b"`.
        let bare = inner.replace(&format!("`{q}"), "").replace(&format!("{q}{q}"), "");
        return (!bare.contains(q)).then(|| unescape(inner).replace(&format!("{q}{q}"), &q.to_string()));
    }
    let var = arg.contains('%') || arg.starts_with("A_") || arg.is_empty();
    (!var).then(|| unescape(arg))
}

#[derive(Clone)]
enum Part {
    Text(String),
    Key(String),
}

/// Send-string: text, `{Key}` / `{Key N}`, and `^!+#` modifiers on the next key.
fn send_parts(s: &str, mut raw: bool) -> Result<Vec<Part>, String> {
    let mut parts: Vec<Part> = Vec::new();
    let mut push = |p: Part| match (parts.last_mut(), p) {
        (Some(Part::Text(t)), Part::Text(n)) => t.push_str(&n),
        (_, p) => parts.push(p),
    };
    let mut mods = String::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if raw {
            push(Part::Text(c.into()));
            continue;
        }
        if let Some(m) = mod_name(c) {
            mods += m;
            continue;
        }
        let (name, count) = if c == '{' {
            // Search from the second character, so `{}}` is a literal `}`.
            let from = i + 1 + s[i + 1..].chars().next().map_or(0, char::len_utf8);
            let end = s[from..].find('}').ok_or("unclosed {")? + from;
            let inner = &s[i + 1..end];
            while it.peek().is_some_and(|(j, _)| *j <= end) {
                it.next();
            }
            let (name, arg) = inner.split_once(' ').unwrap_or((inner, ""));
            match (name.to_ascii_lowercase().as_str(), arg.parse::<usize>()) {
                ("raw" | "text", _) => {
                    raw = true;
                    continue;
                }
                ("blind", _) => continue,
                (_, Ok(n)) => (name.to_owned(), n.min(100)),
                _ if arg.is_empty() => (name.to_owned(), 1),
                _ => return Err(format!("{{{inner}}} isn't supported")),
            }
        } else {
            (c.to_string(), 1)
        };
        let one = name.chars().count() == 1;
        let lname = name.to_ascii_lowercase();
        let part = if mods.is_empty() || mods == "shift+" && one && name.chars().all(char::is_alphabetic) {
            match (lname.as_str(), one) {
                (_, true) if mods.is_empty() => Part::Text(name.clone()),
                (_, true) => Part::Text(name.to_uppercase()),
                ("enter", _) => Part::Text("\n".into()),
                ("tab", _) => Part::Text("\t".into()),
                ("space", _) => Part::Text(" ".into()),
                _ => Part::Key(key(&name).ok_or(format!("unknown key {name:?}"))?),
            }
        } else {
            Part::Key(format!("{mods}{}", key(&name).ok_or(format!("unknown key {name:?}"))?))
        };
        mods.clear();
        for _ in 0..count {
            push(part.clone());
        }
    }
    Ok(parts)
}

/// `WinActive("ahk_exe firefox.exe")` and friends to a Luau condition.
fn win_cond(spec: &str) -> Option<String> {
    let spec = spec.trim();
    let mut words = spec.split_whitespace();
    match (words.next()?, words.next()) {
        ("ahk_exe", Some(exe)) => {
            let exe = exe.to_ascii_lowercase();
            Some(format!("string.lower(window.class) == {:?}", exe.strip_suffix(".exe").unwrap_or(&exe)))
        }
        ("ahk_class", Some(c)) => Some(format!("window.class == {c:?}")),
        (w, _) if w.starts_with("ahk_") => None,
        _ => Some(format!("string.find(window.title, {spec:?}, 1, true) ~= nil")),
    }
}

/// `WinActive("x")`, `!WinActive("x")`, `not WinActive("x")`, optionally in parens.
fn cond_expr(e: &str) -> Option<String> {
    let mut e = e.trim();
    while let Some(inner) = e.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        e = inner.trim();
    }
    if let Some(rest) = e.strip_prefix('!').or_else(|| e.strip_prefix("not ")) {
        return cond_expr(rest).map(|c| format!("not ({c})"));
    }
    let arg = e.strip_prefix("WinActive(")?.strip_suffix(')')?;
    win_cond(&literal(arg)?)
}

/// `Name arg`, `Name, arg` or `Name(arg)` -> (lowercase name, arg).
fn command(s: &str) -> Option<(String, &str)> {
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len());
    let (name, rest) = s.split_at(end);
    let arg = if let Some(inner) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        inner
    } else if let Some(a) = rest.strip_prefix(',') {
        a.trim()
    } else if rest.is_empty() || rest.starts_with([' ', '\t']) {
        rest.trim()
    } else {
        return None;
    };
    Some((name.to_ascii_lowercase(), arg))
}

const IGNORED: &[&str] = &[
    "#requires", "#singleinstance", "#noenv", "#persistent", "#warn", "#notrayicon", "#usehook", "#installkeybdhook",
    "#maxhotkeysperinterval", "sendmode", "setworkingdir", "settitlematchmode", "detecthiddenwindows", "persistent",
    "setbatchlines", "setkeydelay",
];

#[derive(Default)]
struct Conv {
    out: Vec<String>,
    todo: usize,
    run: bool,
    windows: bool,
    /// Luau condition from the active #HotIf / #IfWinActive.
    cond: Option<String>,
}

impl Conv {
    fn todo(&mut self, ind: &str, line: &str, why: &str) -> String {
        self.todo += 1;
        format!("{ind}-- TODO(ahk): {line}  ({why})")
    }

    fn send(&mut self, ind: &str, line: &str, text: &str, raw: bool) -> Vec<String> {
        match send_parts(text, raw) {
            Ok(parts) => parts
                .into_iter()
                .map(|p| match p {
                    Part::Text(t) => format!("{ind}send({t:?})"),
                    Part::Key(k) => format!("{ind}key({k:?})"),
                })
                .collect(),
            Err(e) => vec![self.todo(ind, line, &e)],
        }
    }

    /// One statement of a hotkey body.
    fn stmt(&mut self, ind: &str, s: &str) -> Vec<String> {
        let Some((cmd, arg)) = command(s) else { return vec![self.todo(ind, s, "not a supported command")] };
        let text = literal(arg);
        match (cmd.as_str(), text) {
            ("return", _) => vec![],
            ("send" | "sendinput" | "sendevent" | "sendplay", Some(t)) => self.send(ind, s, &t, false),
            ("sendtext" | "sendraw", Some(t)) => self.send(ind, s, &t, true),
            ("sleep", _) => match arg.parse::<u64>() {
                Ok(ms) => vec![format!("{ind}sleep({ms})")],
                Err(_) => vec![self.todo(ind, s, "sleep needs a number")],
            },
            ("run", Some(t)) if t.contains(":\\") => vec![self.todo(ind, s, "Windows path")],
            ("run", Some(t)) => {
                self.run = true;
                let cmd = if t.contains("://") { format!("xdg-open '{t}'") } else { t.trim_end_matches(".exe").to_owned() };
                vec![format!("{ind}run({cmd:?})")]
            }
            ("msgbox" | "tooltip" | "traytip", Some(t)) => vec![format!("{ind}notify({t:?})")],
            _ => vec![self.todo(ind, s, "not a supported command")],
        }
    }

    /// Hotkey body lines to Luau, with `if WinActive(...)` / `else` / braces.
    fn body(&mut self, lines: &[String], depth: usize) -> Vec<String> {
        // Split `} else {` / `} else` so each line holds one token.
        let mut ls: Vec<String> = Vec::new();
        for l in lines {
            match l.strip_prefix('}').map(str::trim) {
                Some(rest) if !rest.is_empty() => ls.extend(["}".to_owned(), rest.to_owned()]),
                _ => ls.push(l.clone()),
            }
        }
        let is_else = |l: Option<&String>| l.is_some_and(|l| l.to_ascii_lowercase().starts_with("else"));
        let (mut out, mut stack) = (Vec::new(), Vec::<bool>::new()); // true = braced block
        let mut i = 0;
        while i < ls.len() {
            let s = ls[i].trim().to_owned();
            i += 1;
            let ind = "    ".repeat(depth + stack.len());
            let low = s.to_ascii_lowercase();
            let opens = |s: &str, i: &mut usize| {
                let braced = s.ends_with('{') || ls.get(*i).is_some_and(|n| n.trim() == "{");
                if !s.ends_with('{') && braced {
                    *i += 1;
                }
                braced
            };
            if s.is_empty() || s == "{" {
                continue;
            } else if s.starts_with("--") {
                out.push(format!("{ind}{s}"));
                continue;
            } else if s == "}" {
                stack.pop();
                if !is_else(ls.get(i)) {
                    out.push(format!("{}end", "    ".repeat(depth + stack.len())));
                } else {
                    continue;
                }
            } else if low.starts_with("else") {
                let rest = s[4..].trim().trim_end_matches('{').trim().to_owned();
                out.push(format!("{}else", "    ".repeat(depth + stack.len())));
                if rest.is_empty() {
                    stack.push(opens(&s, &mut i));
                    continue;
                }
                // `else Statement` on one line.
                stack.push(false);
                let ind = "    ".repeat(depth + stack.len());
                out.extend(self.stmt(&ind, &rest));
            } else if let Some(spec) = low.starts_with("ifwin").then(|| command(s.trim_end_matches('{').trim()).map_or("", |c| c.1)) {
                let c = literal(spec).and_then(|t| win_cond(&t)).map(|c| match low.starts_with("ifwinnotactive") {
                    true => format!("not ({c})"),
                    false => c,
                });
                self.windows |= c.is_some();
                let c = c.unwrap_or_else(|| {
                    self.todo += 1;
                    format!("false --[[ TODO(ahk): {s} ]]")
                });
                out.push(format!("{ind}if {c} then"));
                stack.push(opens(&s, &mut i));
                continue;
            } else if let Some(e) = low.starts_with("if").then(|| s[2..].trim_end_matches('{').trim()).filter(|e| {
                e.starts_with(['(', ' ', '!']) || e.to_ascii_lowercase().starts_with("winactive")
            }) {
                let c = cond_expr(e);
                self.windows |= c.is_some();
                let c = c.unwrap_or_else(|| {
                    self.todo += 1;
                    format!("false --[[ TODO(ahk): if {e} ]]")
                });
                out.push(format!("{ind}if {c} then"));
                stack.push(opens(&s, &mut i));
                continue;
            } else {
                out.extend(self.stmt(&ind, &s));
            }
            // A statement ends any unbraced `if` / `else` it was the body of.
            while stack.last() == Some(&false) {
                stack.pop();
                if is_else(ls.get(i)) {
                    break;
                }
                out.push(format!("{}end", "    ".repeat(depth + stack.len())));
            }
        }
        for d in (0..stack.len()).rev() {
            out.push(format!("{}end", "    ".repeat(depth + d)));
        }
        out
    }

    /// Wraps a callback body in the active #HotIf condition.
    fn wrap(&self, body: Vec<String>, otherwise: Option<String>) -> Vec<String> {
        let Some(c) = &self.cond else { return body };
        let mut out = vec![format!("    if {c} then")];
        out.extend(body.into_iter().map(|l| format!("    {l}")));
        if let Some(o) = otherwise {
            out.extend(["    else".into(), format!("        {o}")]);
        }
        out.push("    end".into());
        out
    }

    fn hotstring(&mut self, line: &str, opts: &str, trigger: &str, repl: &str, block: &[String]) {
        let o = opts.to_ascii_uppercase();
        if trigger.chars().count() < keysmith::MIN_TRIGGER || trigger.contains(char::is_whitespace) {
            let l = self.todo("", line, "triggers need 3+ characters and no spaces");
            return self.out.push(l);
        }
        let imm = if o.contains('*') { ", { immediate = true }" } else { "" };
        let unsupported: String = o.chars().filter(|c| !matches!(c, '*' | 'R' | 'T' | 'X' | '?' | 'C' | 'O')).collect();
        if !unsupported.is_empty() {
            self.out.push(format!("-- note: hotstring option(s) {unsupported:?} ignored"));
        }
        let body = if o.contains('X') {
            self.body(&[repl.to_owned()], 1)
        } else if repl.is_empty() {
            self.body(block, 1)
        } else {
            let raw = o.contains('R') || o.contains('T');
            match send_parts(&unescape(repl), raw) {
                Ok(p) if self.cond.is_none() && p.iter().all(|p| matches!(p, Part::Text(_))) => {
                    let text: String = p.into_iter().map(|p| if let Part::Text(t) = p { t } else { String::new() }).collect();
                    return self.out.push(format!("hotstring({trigger:?}, {text:?}{imm})"));
                }
                _ => self.send("    ", line, &unescape(repl), raw),
            }
        };
        // Outside the #HotIf window, type the erased trigger back.
        let body = self.wrap(body, Some(format!("send({trigger:?})")));
        self.out.push(format!("hotstring({trigger:?}, function()"));
        self.out.extend(body);
        self.out.push(format!("end{imm})"));
    }

    fn hotkey(&mut self, line: &str, lhs: &str, rhs: &str, block: &[String]) {
        let src = lhs.trim_start_matches(['~', '*', '$']);
        let dst = key(rhs).filter(|_| !rhs.is_empty() && !matches!(rhs.to_ascii_lowercase().as_str(), "return" | "reload" | "exitapp"));
        // `CapsLock::Esc`, `RAlt::RCtrl`: a remap when the source has no modifiers.
        if let (Some(src), Some(dst), None) = (key(src), &dst, &self.cond) {
            return self.out.push(format!("remap({src:?}, {dst:?})"));
        }
        let c = match combo(lhs) {
            Ok(c) => c,
            Err(e) => {
                let l = self.todo("", line, &e);
                return self.out.push(l);
            }
        };
        if let Some(dst) = dst {
            let body = self.wrap(vec![format!("    key({dst:?})")], None);
            self.out.push(format!("hotkey({c:?}, function()"));
            self.out.extend(body);
            return self.out.push("end)".into());
        }
        let body = if rhs.is_empty() { self.body(block, 1) } else { self.body(&[rhs.to_owned()], 1) };
        let body = self.wrap(body, None);
        self.out.push(format!("hotkey({c:?}, function()"));
        self.out.extend(body);
        self.out.push("end)".into());
    }

    fn directive(&mut self, s: &str) {
        let (name, arg) = s.split_once([' ', '\t', ',']).map_or((s, ""), |(n, a)| (n, a.trim().trim_start_matches(',').trim()));
        let name = name.to_ascii_lowercase();
        let set = |me: &mut Self, c: Option<String>| match (arg.is_empty(), c) {
            (true, _) => me.cond = None,
            (false, Some(c)) => {
                me.windows = true;
                me.cond = Some(c);
            }
            (false, None) => {
                me.cond = Some("false".into());
                let l = me.todo("", s, "only WinActive conditions are supported; rules below are off");
                me.out.push(l);
            }
        };
        match name.as_str() {
            "#hotif" | "#if" => set(self, cond_expr(arg)),
            "#ifwinactive" => set(self, literal(arg).and_then(|a| win_cond(&a))),
            "#ifwinnotactive" => set(self, literal(arg).and_then(|a| win_cond(&a)).map(|c| format!("not ({c})"))),
            n if IGNORED.contains(&n) => self.out.push(format!("-- (ahk) {s}")),
            _ => {
                let l = self.todo("", s, "unsupported directive");
                self.out.push(l);
            }
        }
    }
}

/// Strips comments; full-line `;` comments become `--` lines.
fn clean(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for l in src.lines() {
        let t = l.trim();
        if in_block || t.starts_with("/*") {
            in_block = !t.contains("*/");
            continue;
        }
        if let Some(c) = t.strip_prefix(';') {
            out.push(format!("--{c}"));
            continue;
        }
        // ` ;` starts a comment; `` `; `` is a literal semicolon.
        let code = t.match_indices(" ;").chain(t.match_indices("\t;")).map(|(i, _)| i).min().map_or(t, |i| &t[..i]);
        out.push(code.trim_end().to_owned());
    }
    out
}

/// `:opts:trigger::repl` -> (opts, trigger, repl).
fn split_hotstring(s: &str) -> Option<(&str, &str, &str)> {
    let rest = s.strip_prefix(':')?;
    let (opts, rest) = rest.split_once(':')?;
    let (trigger, repl) = rest.split_once("::")?;
    Some((opts, trigger, repl.trim()))
}

/// `lhs::rhs` hotkey line.
fn split_hotkey(s: &str) -> Option<(&str, &str)> {
    let (lhs, rhs) = s.split_once("::")?;
    let ok = !lhs.is_empty() && !lhs.contains(['"', '(']) && (!lhs.contains(' ') || lhs.contains(" & ") || lhs.ends_with(" up"));
    ok.then(|| (lhs.trim(), rhs.trim()))
}

fn is_label(s: &str) -> bool {
    split_hotstring(s).is_some() || split_hotkey(s).is_some() || s.starts_with('#') && !s.contains("::")
}

/// The block after a hotkey/hotstring with no inline action: `{ ... }` (v2)
/// or lines up to `return` (v1). Returns (body, lines consumed).
fn block(lines: &[String], rhs: &str) -> (Vec<String>, usize) {
    let braced = rhs == "{" || rhs.is_empty() && lines.iter().find(|l| !l.is_empty() && !l.starts_with("--")).is_some_and(|l| l == "{");
    let mut depth = i32::from(rhs == "{");
    let mut body = Vec::new();
    for (n, l) in lines.iter().enumerate() {
        if braced {
            if l.starts_with('}') {
                depth -= 1;
            }
            if depth == 0 && l.starts_with('}') {
                return (body, n + 1);
            }
            if depth > 0 {
                body.push(l.clone());
            }
            if l.ends_with('{') {
                depth += 1;
            }
        } else if l.eq_ignore_ascii_case("return") {
            return (body, n + 1);
        } else if is_label(l) {
            return (body, n);
        } else {
            body.push(l.clone());
        }
    }
    (body, lines.len())
}

/// AHK source -> (Luau source, number of TODO lines).
pub fn convert(src: &str) -> (String, usize) {
    let lines = clean(src);
    let mut c = Conv::default();
    let mut i = 0;
    while i < lines.len() {
        let s = lines[i].clone();
        i += 1;
        if s.is_empty() || s.starts_with("--") {
            c.out.push(s);
        } else if let Some((opts, trigger, repl)) = split_hotstring(&s) {
            let (body, n) = if repl.is_empty() || repl == "{" { block(&lines[i..], repl) } else { (vec![], 0) };
            i += n;
            c.hotstring(&s, opts, trigger, if repl == "{" { "" } else { repl }, &body);
        } else if let Some((lhs, rhs)) = split_hotkey(&s) {
            let (body, n) = if rhs.is_empty() || rhs == "{" { block(&lines[i..], rhs) } else { (vec![], 0) };
            i += n;
            c.hotkey(&s, lhs, if rhs == "{" { "" } else { rhs }, &body);
        } else if s.starts_with('#') {
            c.directive(&s);
        } else if command(&s).is_some_and(|(n, _)| IGNORED.contains(&n.as_str())) {
            c.out.push(format!("-- (ahk) {s}"));
        } else {
            let l = c.todo("", &s, "top-level code runs once at startup in AHK");
            c.out.push(l);
        }
    }
    let mut head = vec!["-- Converted from AutoHotkey by `keysmith convert`.".to_owned()];
    if c.run {
        head.push("--! capabilities: run".into());
    }
    if c.windows {
        head.push("-- Window names differ on Linux: check the real class with `keysmith windows`.".into());
    }
    head.push(String::new());
    while c.out.first().is_some_and(String::is_empty) {
        c.out.remove(0);
    }
    (head.into_iter().chain(c.out).collect::<Vec<_>>().join("\n") + "\n", c.todo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(src: &str) -> String {
        let (lua, todo) = convert(src);
        assert_eq!(todo, 0, "{lua}");
        lua.split_once("\n\n").unwrap().1.trim_end().to_owned()
    }

    #[test]
    fn keys() {
        assert_eq!(combo("^!t").unwrap(), "ctrl+alt+t");
        assert_eq!(combo("~$#Enter").unwrap(), "super+enter");
        assert_eq!(combo("<^>!F5").unwrap(), "ctrl+alt+f5");
        assert_eq!(combo("Volume_Up").unwrap(), "volumeup");
        assert_eq!(combo("^Numpad1").unwrap(), "ctrl+kp1");
        assert!(combo("a").is_err()); // text key without modifiers
        assert!(combo("a & b").is_err());
    }

    #[test]
    fn one_liners() {
        assert_eq!(conv("^j::Send \"Hello{Enter}\""), "hotkey(\"ctrl+j\", function()\n    send(\"Hello\\n\")\nend)");
        assert_eq!(conv("^j::Send, Hi`tthere"), "hotkey(\"ctrl+j\", function()\n    send(\"Hi\\tthere\")\nend)");
        assert_eq!(conv("#c::Send \"^c\""), "hotkey(\"super+c\", function()\n    key(\"ctrl+c\")\nend)");
        assert_eq!(conv("F1::Send \"{Left 2}+a\""), "hotkey(\"f1\", function()\n    key(\"left\")\n    key(\"left\")\n    send(\"A\")\nend)");
        assert_eq!(conv("F2::SendText \"^{x}\""), "hotkey(\"f2\", function()\n    send(\"^{x}\")\nend)");
        assert_eq!(conv("CapsLock::Esc"), "remap(\"capslock\", \"esc\")");
        assert_eq!(conv("^h::Left"), "hotkey(\"ctrl+h\", function()\n    key(\"left\")\nend)");
        assert_eq!(conv("::btw::by the way"), "hotstring(\"btw\", \"by the way\")");
        assert_eq!(conv(":*:;em::me@example.com"), "hotstring(\";em\", \"me@example.com\", { immediate = true })");
    }

    #[test]
    fn blocks_and_conditions() {
        let v1 = "#IfWinActive ahk_exe firefox.exe\n^t::\nSend, ^t\nSleep, 100\nreturn\n#IfWinActive";
        assert_eq!(
            conv(v1),
            "hotkey(\"ctrl+t\", function()\n    if string.lower(window.class) == \"firefox\" then\n        key(\"ctrl+t\")\n        sleep(100)\n    end\nend)"
        );
        let v2 = "^k:: {\n    if WinActive(\"ahk_class kitty\") {\n        Send \"x\"\n    } else {\n        MsgBox \"no\"\n    }\n}";
        assert_eq!(
            conv(v2),
            "hotkey(\"ctrl+k\", function()\n    if window.class == \"kitty\" then\n        send(\"x\")\n    else\n        notify(\"no\")\n    end\nend)"
        );
    }
}

#[cfg(test)]
mod top20 {
    /// The 20 patterns in examples/ahk/top20.ahk convert, and the result loads.
    #[test]
    fn converts_and_loads() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ahk/top20.ahk")).unwrap();
        let (lua, todo) = super::convert(&src);
        assert_eq!(todo, 2, "{lua}"); // only the two marked unsupported
        assert_eq!(lua.matches("TODO(ahk)").count(), 2, "{lua}");
        let send: crate::script::SendFn = std::sync::Arc::new(|_, _, _| Ok(()));
        let loaded = crate::script::spawn("top20.luau".into(), lua.clone(), send, Default::default()).unwrap_or_else(|e| panic!("{e}\n{lua}"));
        assert_eq!(loaded.regs.len(), 22, "{lua}");
        for r in loaded.regs {
            let wire = match r {
                crate::script::Reg::Hotkey(c, _) => format!("bind 0 {c}"),
                crate::script::Reg::Remap(s, a) => format!("remap 0 {s} {a}"),
                crate::script::Reg::Hotstring { trigger, .. } => format!("hotstring 0 f {trigger}"),
            };
            keysmith::parse_msg(&wire).unwrap_or_else(|e| panic!("{wire}: {e:?}"));
        }
    }
}
