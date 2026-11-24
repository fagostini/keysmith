//! Privileged broker. Runs as root, reads evdev, matches hotkeys itself and
//! tells the active seat user's engine only which binding fired.

use evdev::{EventSummary, KeyCode};
use keysmith::{Combo, MAX_BINDINGS, Matcher, SOCKET, parse_bind};
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::mpsc::{Sender, channel};
use std::{fs, thread};

enum Ev {
    Key(KeyCode, i32),
    Client(u64, UnixStream, u32),
    Bind(u64, Result<(u32, Combo), String>),
}

/// UID owning the active session on seat0, per logind.
// ponytail: shells out to loginctl (~10 ms, only on connect and on a match);
// switch to logind D-Bus (zbus) if this ever runs per keystroke.
fn active_uid() -> Option<u32> {
    let get = |args: &[&str]| {
        let o = Command::new("/usr/bin/loginctl").args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let session = get(&["show-seat", "seat0", "-p", "ActiveSession", "--value"])?;
    get(&["show-session", &session, "-p", "User", "--value"])?.parse().ok()
}

fn spawn_devices(tx: &Sender<Ev>) {
    // ponytail: devices enumerated once at start; keyboards plugged in later are
    // ignored until restart. Add a udev monitor when that bites.
    for (path, mut dev) in evdev::enumerate() {
        let is_kbd = dev
            .supported_keys()
            .is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_ENTER));
        if !is_kbd {
            continue;
        }
        eprintln!("watching {} ({})", path.display(), dev.name().unwrap_or("?"));
        let tx = tx.clone();
        thread::spawn(move || {
            while let Ok(events) = dev.fetch_events() {
                for e in events {
                    if let EventSummary::Key(_, code, v) = e.destructure() {
                        let _ = tx.send(Ev::Key(code, v));
                    }
                }
            }
            eprintln!("lost {}", path.display());
        });
    }
}

fn serve(listener: UnixListener, tx: Sender<Ev>) {
    for (conn, stream) in (1u64..).zip(listener.incoming()) {
        let Ok(mut stream) = stream else { continue };
        let peer = getsockopt(&stream, PeerCredentials).map(|c| c.uid());
        let Ok(uid) = peer else { continue };
        if Some(uid) != active_uid() {
            let _ = writeln!(stream, "err 0 uid {uid} does not own the active seat");
            continue;
        }
        let Ok(reader) = stream.try_clone() else { continue };
        let tx2 = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                if tx2.send(Ev::Bind(conn, parse_bind(&line))).is_err() {
                    return;
                }
            }
        });
        let _ = tx.send(Ev::Client(conn, stream, uid));
    }
}

fn main() -> std::io::Result<()> {
    let dir = std::path::Path::new(SOCKET).parent().unwrap();
    fs::create_dir_all(dir)?;
    let _ = fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET)?;
    // World-connectable on purpose: access is decided per peer by SO_PEERCRED + logind.
    fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o666))?;

    let (tx, rx) = channel();
    spawn_devices(&tx);
    let tx2 = tx.clone();
    thread::spawn(move || serve(listener, tx2));

    let mut m = Matcher::default();
    // Only the newest authorised connection is served; a new one replaces it.
    let mut client: Option<(u64, UnixStream, u32)> = None;
    for ev in rx {
        match ev {
            Ev::Client(conn, stream, uid) => {
                eprintln!("client {conn} uid {uid}");
                m.bindings.clear();
                client = Some((conn, stream, uid));
            }
            Ev::Bind(conn, res) => {
                let Some((cur, stream, _)) = client.as_mut() else { continue };
                if conn != *cur {
                    continue;
                }
                let reply = match res {
                    Err(e) => Some(format!("err 0 {e}")),
                    Ok((id, _)) if m.bindings.len() >= MAX_BINDINGS => Some(format!("err {id} too many bindings")),
                    Ok((id, c)) => match c.refusal() {
                        Some(why) => Some(format!("err {id} {why}")),
                        None => {
                            m.bindings.push((id, c));
                            None
                        }
                    },
                };
                if let Some(r) = reply {
                    let _ = writeln!(stream, "{r}");
                }
            }
            Ev::Key(code, v) => {
                let Some(id) = m.key(code, v) else { continue };
                let Some((_, stream, uid)) = client.as_mut() else { continue };
                // Re-check on every match: after a user switch the old session must not see presses.
                let ok = active_uid() == Some(*uid) && writeln!(stream, "fired {id}").is_ok();
                if !ok {
                    eprintln!("dropping client");
                    m.bindings.clear();
                    client = None;
                }
            }
        }
    }
    Ok(())
}
