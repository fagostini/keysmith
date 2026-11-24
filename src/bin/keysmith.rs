//! User-session engine (M1): reads `~/.config/keysmith/hotkeys.conf`,
//! registers the combos with inputd and runs the command when one fires.
// ponytail: plain `combo = command` config until the Luau API lands in M4.

use keysmith::{Combo, SOCKET};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("keysmith/hotkeys.conf")
}

fn load(path: &PathBuf) -> Result<Vec<(String, String)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (combo, cmd) = line.split_once('=').ok_or(format!("line {}: expected `combo = command`", n + 1))?;
        // Validate here for line numbers; inputd re-parses and has the final say.
        let combo = combo.trim();
        combo.parse::<Combo>().map_err(|e| format!("line {}: {e}", n + 1))?;
        out.push((combo.to_owned(), cmd.trim().to_owned()));
    }
    Ok(out)
}

fn main() -> ExitCode {
    let path = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(config_path);
    let hotkeys = match load(&path) {
        Ok(h) => h,
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
    for (id, (combo, _)) in hotkeys.iter().enumerate() {
        if writeln!(sock, "bind {id} {combo}").is_err() {
            return ExitCode::FAILURE;
        }
    }
    eprintln!("keysmith: {} hotkeys from {}", hotkeys.len(), path.display());

    for line in BufReader::new(sock).lines().map_while(Result::ok) {
        let mut parts = line.splitn(3, ' ');
        match (parts.next(), parts.next().and_then(|i| i.parse::<usize>().ok())) {
            (Some("fired"), Some(id)) if id < hotkeys.len() => {
                let cmd = &hotkeys[id].1;
                match Command::new("sh").arg("-c").arg(cmd).spawn() {
                    Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
                    Err(e) => eprintln!("keysmith: {cmd}: {e}"),
                }
            }
            (Some("err"), _) => eprintln!("keysmith: inputd: {line}"),
            _ => eprintln!("keysmith: unexpected {line:?}"),
        }
    }
    // inputd went away (restart or user switch); systemd restarts us.
    eprintln!("keysmith: disconnected");
    ExitCode::FAILURE
}
