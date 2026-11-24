//! User-session engine: reads `~/.config/keysmith/hotkeys.conf`, sends the
//! rules to inputd and runs the command when a hotkey fires.
// ponytail: plain `lhs = rhs` config until the Luau API lands in M4.

use keysmith::{SOCKET, parse_msg};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// One config entry: (config line number, wire message, command if it's a hotkey).
type Rule = (usize, String, Option<String>);

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("keysmith/hotkeys.conf")
}

fn load(path: &PathBuf) -> Result<Vec<Rule>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in (1..).zip(text.lines()) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (lhs, rhs) = line.split_once('=').ok_or(format!("line {n}: expected `lhs = rhs`"))?;
        let (lhs, rhs, id) = (lhs.trim(), rhs.trim(), out.len());
        let (wire, cmd) = if lhs == "taphold_ms" {
            (format!("taphold_ms {rhs}"), None)
        } else if let Some(src) = lhs.strip_prefix("remap ") {
            (format!("remap {id} {} {rhs}", src.trim()), None)
        } else {
            (format!("bind {id} {lhs}"), Some(rhs.to_owned()))
        };
        // Validate here for line numbers; inputd re-parses and has the final say.
        parse_msg(&wire).map_err(|(_, e)| format!("line {n}: {e}"))?;
        out.push((n, wire, cmd));
    }
    Ok(out)
}

fn main() -> ExitCode {
    let path = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(config_path);
    let rules = match load(&path) {
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
    for (_, wire, _) in &rules {
        if writeln!(sock, "{wire}").is_err() {
            return ExitCode::FAILURE;
        }
    }
    eprintln!("keysmith: {} rules from {}", rules.len(), path.display());

    for line in BufReader::new(sock).lines().map_while(Result::ok) {
        let mut parts = line.splitn(3, ' ');
        let (verb, id, rest) = (parts.next(), parts.next().and_then(|i| i.parse::<usize>().ok()), parts.next());
        match (verb, id.and_then(|i| rules.get(i))) {
            (Some("fired"), Some((_, _, Some(cmd)))) => match Command::new("sh").arg("-c").arg(cmd).spawn() {
                Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
                Err(e) => eprintln!("keysmith: {cmd}: {e}"),
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
