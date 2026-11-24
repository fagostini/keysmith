//! User-session engine. Loads `hotkeys.conf` and `scripts/*.luau` from the config
//! dir, sends their rules to inputd, and runs commands / script callbacks on `fired`.
//!
//!   keysmith [CONFIG]        run (default ~/.config/keysmith/hotkeys.conf)
//!   keysmith trust SCRIPT    approve a script's declared capabilities

mod script;

use keysmith::{SOCKET, escape, parse_msg, unescape};
use script::{Reg, SendFn, report};
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

enum Target {
    Cmd(String),
    /// Hotstring text, kept for the clipboard fallback.
    Text(String),
    /// (script index, callback index)
    Script(usize, usize),
    None,
}

struct Rule {
    origin: String,
    wire: String,
    target: Target,
}

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

/// hotkeys.conf -> rules, plus `xkb_*` settings. A missing file is fine.
fn load_conf(path: &Path, rules: &mut Vec<Rule>) -> Result<Vec<(String, String)>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut xkb = Vec::new();
    for (n, line) in (1..).zip(text.lines()) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (lhs, rhs) = line.split_once('=').ok_or(format!("line {n}: expected `lhs = rhs`"))?;
        let (lhs, rhs, id) = (lhs.trim(), rhs.trim(), rules.len());
        if lhs.starts_with("xkb_") {
            xkb.push((lhs.to_owned(), rhs.to_owned()));
            continue;
        }
        let (wire, target) = if lhs == "taphold_ms" {
            (format!("taphold_ms {rhs}"), Target::None)
        } else if let Some(trigger) = lhs.strip_prefix(':') {
            let (opt, trigger) = trigger.strip_prefix('*').map_or(("-", trigger), |t| ("*", t));
            if trigger.contains(char::is_whitespace) {
                return Err(format!("line {n}: hotstring trigger cannot contain spaces"));
            }
            (format!("hotstring {id} {opt} {trigger} {rhs}"), Target::Text(unescape(rhs)))
        } else if let Some(src) = lhs.strip_prefix("remap ") {
            (format!("remap {id} {} {rhs}", src.trim()), Target::None)
        } else {
            (format!("bind {id} {lhs}"), Target::Cmd(rhs.to_owned()))
        };
        // Validate here for line numbers; inputd re-parses and has the final say.
        parse_msg(&wire).map_err(|(_, e)| format!("line {n}: {e}"))?;
        rules.push(Rule { origin: format!("{} line {n}", path.display()), wire, target });
    }
    Ok(xkb)
}

// ponytail: DefaultHasher is stable within a Rust release, not across them, so a
// toolchain upgrade asks to re-trust. It only detects edits (the trust file is as
// writable as the script), so it needn't be cryptographic.
fn trust_key(src: &str) -> String {
    let mut h = DefaultHasher::new();
    src.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn trust_file(dir: &Path) -> PathBuf {
    dir.join("trusted")
}

fn is_trusted(dir: &Path, name: &str, src: &str) -> bool {
    let want = format!("{} {name}", trust_key(src));
    std::fs::read_to_string(trust_file(dir)).is_ok_and(|t| t.lines().any(|l| l == want))
}

fn trust(script: &Path) -> Result<(), String> {
    let src = std::fs::read_to_string(script).map_err(|e| format!("{}: {e}", script.display()))?;
    let name = script.file_name().and_then(|n| n.to_str()).ok_or("bad script name")?;
    let dir = script.parent().and_then(Path::parent).ok_or("script must be in <config>/scripts/")?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(trust_file(dir)).map_err(|e| e.to_string())?;
    writeln!(f, "{} {name}", trust_key(&src)).map_err(|e| e.to_string())?;
    println!("trusted {name} with capabilities: {}", script::capabilities(&src).join(", "));
    Ok(())
}

/// Loads every `scripts/*.luau`; a broken script is reported and skipped.
fn load_scripts(dir: &Path, rules: &mut Vec<Rule>, send: &SendFn) -> Vec<Sender<usize>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir.join("scripts"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "luau"))
        .collect();
    paths.sort();
    let mut scripts = Vec::new();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(src) = std::fs::read_to_string(&path) else { continue };
        let caps = script::capabilities(&src);
        if !caps.is_empty() && !is_trusted(dir, &name, &src) {
            let cmd = format!("keysmith trust {}", path.display());
            report(&name, &format!("wants capabilities [{}]; run `{cmd}` to allow (again after edits)", caps.join(", ")));
            continue;
        }
        let loaded = match script::spawn(name.clone(), src, send.clone()) {
            Ok(l) => l,
            Err(e) => {
                report(&name, &e);
                continue;
            }
        };
        let si = scripts.len();
        for reg in loaded.regs {
            let id = rules.len();
            let (wire, target) = match reg {
                Reg::Hotkey(combo, cb) => (format!("bind {id} {combo}"), Target::Script(si, cb)),
                Reg::Remap(src, action) => (format!("remap {id} {src} {action}"), Target::None),
                Reg::Hotstring { trigger, immediate, text, cb } => {
                    let opt = format!("{}{}", if immediate { "*" } else { "" }, if cb.is_some() { "f" } else { "" });
                    let opt = if opt.is_empty() { "-".into() } else { opt };
                    let wire = format!("hotstring {id} {opt} {trigger} {}", text.as_deref().map(escape).unwrap_or_default());
                    let target = match (text, cb) {
                        (Some(t), _) => Target::Text(t),
                        (None, Some(cb)) => Target::Script(si, cb),
                        _ => Target::None,
                    };
                    (wire.trim_end().to_owned(), target)
                }
            };
            match parse_msg(&wire) {
                Ok(_) => rules.push(Rule { origin: name.clone(), wire, target }),
                Err((_, e)) => report(&name, &e),
            }
        }
        scripts.push(loaded.tx);
    }
    scripts
}

/// Recently `send()`-ed texts by request id, for `paste` and `err` replies.
type Sent = Mutex<VecDeque<(u32, String, String)>>;

fn main() -> ExitCode {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.first().is_some_and(|a| a == "trust") {
        return match args.get(1).map(|p| trust(p)) {
            Some(Ok(())) => ExitCode::SUCCESS,
            Some(Err(e)) => {
                eprintln!("keysmith: {e}");
                ExitCode::FAILURE
            }
            None => {
                eprintln!("usage: keysmith trust <script.luau>");
                ExitCode::FAILURE
            }
        };
    }
    let conf = args.into_iter().next().unwrap_or_else(config_path);
    let dir = conf.parent().unwrap_or(Path::new(".")).to_owned();

    let sock = match UnixStream::connect(SOCKET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("keysmith: cannot reach ks-inputd at {SOCKET}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let writer = Arc::new(Mutex::new(sock.try_clone().expect("socket clone")));
    let sent: Arc<Sent> = Arc::default();
    // send() ids live far above rule ids so the two never collide.
    let next_id = Arc::new(AtomicU32::new(1 << 24));
    let send: SendFn = {
        let (w, sent) = (writer.clone(), sent.clone());
        Arc::new(move |origin: &str, text: &str| {
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let mut s = sent.lock().unwrap();
            s.push_back((id, origin.to_owned(), text.to_owned()));
            if s.len() > 64 {
                s.pop_front();
            }
            writeln!(w.lock().unwrap(), "type {id} {}", escape(text)).map_err(|e| e.to_string())
        })
    };

    let mut rules = Vec::new();
    let xkb = match load_conf(&conf, &mut rules) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("keysmith: {e}");
            return ExitCode::FAILURE;
        }
    };
    let scripts = load_scripts(&dir, &mut rules, &send);
    let layout = layout_line(&xkb);
    {
        let mut w = writer.lock().unwrap();
        for wire in std::iter::once(&layout).chain(rules.iter().map(|r| &r.wire)) {
            if writeln!(w, "{wire}").is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
    eprintln!("keysmith: {} rules, {} scripts, {layout}", rules.len(), scripts.len());

    for line in BufReader::new(sock).lines().map_while(Result::ok) {
        let mut parts = line.splitn(3, ' ');
        let (verb, id, rest) = (parts.next(), parts.next().and_then(|i| i.parse::<u32>().ok()), parts.next().unwrap_or(""));
        let rule = id.and_then(|i| rules.get(i as usize));
        let sent_entry = || id.and_then(|i| sent.lock().unwrap().iter().find(|(s, ..)| *s == i).cloned());
        match (verb, rule) {
            (Some("fired"), Some(r)) => match &r.target {
                Target::Cmd(cmd) => match Command::new("sh").arg("-c").arg(cmd).spawn() {
                    Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
                    Err(e) => report(&r.origin, &format!("{cmd}: {e}")),
                },
                Target::Script(si, cb) => drop(scripts[*si].send(*cb)),
                _ => {}
            },
            (Some("paste"), _) => {
                let text = match rule.map(|r| &r.target) {
                    Some(Target::Text(t)) => Some(t.clone()),
                    _ => sent_entry().map(|(_, _, t)| t),
                };
                let Some(text) = text else { continue };
                match clipboard_set(&text) {
                    Ok(old) => {
                        let _ = writeln!(writer.lock().unwrap(), "pasted {}", id.unwrap_or(0));
                        if let Some(old) = old {
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_secs(1));
                                let _ = clipboard_set(&old);
                            });
                        }
                    }
                    Err(e) => report("clipboard", &e),
                }
            }
            (Some("err"), Some(r)) => report(&r.origin, rest),
            (Some("err"), None) => match sent_entry() {
                Some((_, origin, _)) => report(&origin, rest),
                None => report("inputd", rest),
            },
            _ => eprintln!("keysmith: unexpected {line:?}"),
        }
    }
    // inputd went away (restart or user switch); systemd restarts us.
    eprintln!("keysmith: disconnected");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every example script loads in the sandbox and produces rules inputd accepts.
    #[test]
    fn example_scripts() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let send: SendFn = Arc::new(|_, _| Ok(()));
        let mut rules = Vec::new();
        let mut n = 0;
        for e in std::fs::read_dir(dir.join("scripts")).unwrap() {
            let path = e.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&path).unwrap();
            // Loaded directly: capabilities are exercised without the trust step.
            let loaded = script::spawn(name.clone(), src, send.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!loaded.regs.is_empty(), "{name} registers nothing");
            n += 1;
        }
        assert!(n >= 20, "only {n} example scripts");
        load_scripts(&dir, &mut rules, &send);
        for r in &rules {
            match parse_msg(&r.wire) {
                Ok(keysmith::Msg::Bind(_, c)) => assert!(c.refusal().is_none(), "{}: inputd would refuse {}", r.origin, r.wire),
                Ok(_) => {}
                Err(e) => panic!("{}: {e:?}", r.origin),
            }
        }
    }
}
