//! User-session engine: reads `~/.config/keysmith/hotkeys.conf`, sends the
//! rules to inputd and runs the command when a hotkey fires.
// ponytail: plain `lhs = rhs` config until the Luau API lands in M4.

use keysmith::{SOCKET, parse_msg, unescape};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

/// One config entry: (config line number, wire message, hotkey command or hotstring text).
type Rule = (usize, String, Option<String>);

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("keysmith/hotkeys.conf")
}

/// Session keyboard layout as `layout <layout> <variant> <options>`.
/// Config `xkb_layout` / `xkb_variant` / `xkb_options` win; else X11 `setxkbmap -query`;
/// else the system default from `localectl`.
// ponytail: read once at start. Wayland compositors' own layout settings (GNOME
// input-sources, KDE kxkbrc) come with the M5 backends; until then set xkb_layout.
fn layout_line(cfg: &[(String, String)]) -> String {
    let query = |cmd: &str, args: &[&str]| {
        Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let sys = std::env::var_os("DISPLAY")
        .and_then(|_| query("setxkbmap", &["-query"]))
        .filter(|o| o.contains("layout:"))
        .or_else(|| query("localectl", &["status"]))
        .unwrap_or_default();
    let field = |cfg_key: &str, sys_keys: &[&str]| {
        cfg.iter().find(|(k, _)| k == cfg_key).map(|(_, v)| v.clone()).or_else(|| {
            sys.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                sys_keys.contains(&k.trim()).then(|| v.trim().to_owned())
            })
        })
    };
    let or_dash = |v: Option<String>| v.filter(|v| !v.is_empty()).unwrap_or_else(|| "-".into());
    format!(
        "layout {} {} {}",
        field("xkb_layout", &["layout", "X11 Layout"]).unwrap_or_else(|| "us".into()),
        or_dash(field("xkb_variant", &["variant", "X11 Variant"])),
        or_dash(field("xkb_options", &["options", "X11 Options"])),
    )
}

/// Put `text` on the clipboard; returns the previous text to restore.
// ponytail: shells out to wl-copy/wl-paste or xclip, text only. Native clipboard
// (data-control protocol, GNOME extension) arrives with the M5 session backends.
fn clipboard_set(text: &str) -> Result<Option<String>, String> {
    let (get, set): (&[&str], &[&str]) = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        (&["wl-paste", "-n"], &["wl-copy"])
    } else {
        (&["xclip", "-selection", "clipboard", "-o"], &["xclip", "-selection", "clipboard"])
    };
    let old = Command::new(get[0]).args(&get[1..]).output().ok().filter(|o| o.status.success());
    let mut child = Command::new(set[0])
        .args(&set[1..])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{} (install it for characters your layout can't type): {e}", set[0]))?;
    child.stdin.take().unwrap().write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    child.wait().map_err(|e| e.to_string())?;
    Ok(old.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()))
}

fn load(path: &PathBuf) -> Result<(Vec<Rule>, String), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    let mut xkb = Vec::new();
    for (n, line) in (1..).zip(text.lines()) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (lhs, rhs) = line.split_once('=').ok_or(format!("line {n}: expected `lhs = rhs`"))?;
        let (lhs, rhs, id) = (lhs.trim(), rhs.trim(), out.len());
        if lhs.starts_with("xkb_") {
            xkb.push((lhs.to_owned(), rhs.to_owned()));
            continue;
        }
        let (wire, cmd) = if lhs == "taphold_ms" {
            (format!("taphold_ms {rhs}"), None)
        } else if let Some(trigger) = lhs.strip_prefix(':') {
            let (opt, trigger) = trigger.strip_prefix('*').map_or(("-", trigger), |t| ("*", t));
            if trigger.contains(char::is_whitespace) {
                return Err(format!("line {n}: hotstring trigger cannot contain spaces"));
            }
            (format!("hotstring {id} {opt} {trigger} {rhs}"), Some(unescape(rhs)))
        } else if let Some(src) = lhs.strip_prefix("remap ") {
            (format!("remap {id} {} {rhs}", src.trim()), None)
        } else {
            (format!("bind {id} {lhs}"), Some(rhs.to_owned()))
        };
        // Validate here for line numbers; inputd re-parses and has the final say.
        parse_msg(&wire).map_err(|(_, e)| format!("line {n}: {e}"))?;
        out.push((n, wire, cmd));
    }
    Ok((out, layout_line(&xkb)))
}

fn main() -> ExitCode {
    let path = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(config_path);
    let (rules, layout) = match load(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("keysmith: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut sock = match UnixStream::connect(SOCKET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("keysmith: cannot reach ks-inputd at {SOCKET}: {e}");
            return ExitCode::FAILURE;
        }
    };
    for wire in std::iter::once(&layout).chain(rules.iter().map(|(_, w, _)| w)) {
        if writeln!(sock, "{wire}").is_err() {
            return ExitCode::FAILURE;
        }
    }
    eprintln!("keysmith: {} rules from {}, {layout}", rules.len(), path.display());

    let mut reply = match sock.try_clone() {
        Ok(s) => s,
        Err(_) => return ExitCode::FAILURE,
    };
    for line in BufReader::new(sock).lines().map_while(Result::ok) {
        let mut parts = line.splitn(3, ' ');
        let (verb, id, rest) = (parts.next(), parts.next().and_then(|i| i.parse::<usize>().ok()), parts.next());
        match (verb, id.and_then(|i| rules.get(i))) {
            (Some("fired"), Some((_, _, Some(cmd)))) => match Command::new("sh").arg("-c").arg(cmd).spawn() {
                Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
                Err(e) => eprintln!("keysmith: {cmd}: {e}"),
            },
            (Some("paste"), Some((_, _, Some(text)))) => match clipboard_set(text) {
                Ok(old) => {
                    let _ = writeln!(reply, "pasted {}", id.unwrap_or(0));
                    if let Some(old) = old {
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_secs(1));
                            let _ = clipboard_set(&old);
                        });
                    }
                }
                Err(e) => eprintln!("keysmith: clipboard: {e}"),
            },
            (Some("err"), Some((n, ..))) => eprintln!("keysmith: line {n}: {}", rest.unwrap_or("")),
            (Some("err"), None) => eprintln!("keysmith: inputd: {}", rest.unwrap_or("")),
            _ => eprintln!("keysmith: unexpected {line:?}"),
        }
    }
    // inputd went away (restart or user switch); systemd restarts us.
    eprintln!("keysmith: disconnected");
    ExitCode::FAILURE
}
