//! Privileged broker. Runs as root, reads evdev, matches hotkeys and applies
//! remaps itself, and tells the active seat user's engine only which binding fired.
//!
//! With no remaps, keyboards are only listened to (passive). With remaps, they are
//! grabbed and every key is re-emitted through a virtual keyboard after rewriting.
//! Emergency exit: hold Backspace + Esc + Enter. The kernel drops the grab and the
//! virtual device when the process dies.

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventSummary, EventType, InputEvent, KeyCode};
use keysmith::{MAX_BINDINGS, Matcher, Msg, Remapper, SOCKET, is_output_key, parse_msg};
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};
use std::{fs, thread};

nix::ioctl_write_int!(eviocgrab, b'E', 0x90);

enum Ev {
    Key(usize, KeyCode, i32),
    Lost(usize),
    Client(u64, UnixStream, u32),
    Msg(u64, Result<Msg, (u32, String)>),
    Gone(u64),
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

/// Starts one reader thread per keyboard; returns a dup of each fd for grabbing.
fn spawn_devices(tx: &Sender<Ev>) -> Vec<Option<OwnedFd>> {
    // ponytail: devices enumerated once at start; keyboards plugged in later are
    // ignored until restart. Add a udev monitor when that bites.
    let mut fds = Vec::new();
    for (path, mut dev) in evdev::enumerate() {
        let is_kbd = dev
            .supported_keys()
            .is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_ENTER));
        let Ok(fd) = dev.as_fd().try_clone_to_owned() else { continue };
        if !is_kbd {
            continue;
        }
        eprintln!("watching {} ({})", path.display(), dev.name().unwrap_or("?"));
        let (i, tx) = (fds.len(), tx.clone());
        fds.push(Some(fd));
        thread::spawn(move || {
            while let Ok(events) = dev.fetch_events() {
                for e in events {
                    if let EventSummary::Key(_, code, v) = e.destructure() {
                        let _ = tx.send(Ev::Key(i, code, v));
                    }
                }
            }
            eprintln!("lost {}", path.display());
            let _ = tx.send(Ev::Lost(i));
        });
    }
    fds
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
                if tx2.send(Ev::Msg(conn, parse_msg(&line))).is_err() {
                    return;
                }
            }
            let _ = tx2.send(Ev::Gone(conn));
        });
        let _ = tx.send(Ev::Client(conn, stream, uid));
    }
}

struct State {
    m: Matcher,
    r: Remapper,
    client: Option<(u64, UnixStream, u32)>,
    fds: Vec<Option<OwnedFd>>,
    vdev: VirtualDevice,
    grabbed: bool,
    /// Physical keys down, per device. Grab only starts when this is empty,
    /// so the desktop never misses a release for a key it saw go down.
    pressed: HashSet<(usize, KeyCode)>,
}

impl State {
    fn emit(&mut self, out: &[(KeyCode, i32)]) {
        let evs: Vec<_> = out.iter().map(|(k, v)| InputEvent::new(EventType::KEY.0, k.code(), *v)).collect();
        if !evs.is_empty() && self.vdev.emit(&evs).is_err() {
            eprintln!("virtual keyboard write failed");
        }
    }

    fn set_grab(&mut self, on: bool) {
        if self.grabbed == on {
            return;
        }
        for fd in self.fds.iter().flatten() {
            // SAFETY: EVIOCGRAB takes an int by value; fd is a live evdev descriptor we own.
            if let Err(e) = unsafe { eviocgrab(fd.as_raw_fd(), on as _) } {
                eprintln!("EVIOCGRAB({on}) failed: {e}");
            }
        }
        self.grabbed = on;
        eprintln!("{}", if on { "grabbed keyboards" } else { "released keyboards" });
    }

    fn maybe_grab(&mut self) {
        if !self.grabbed && !self.r.rules.is_empty() && self.pressed.is_empty() {
            self.set_grab(true);
        }
    }

    /// Drop all rules: release whatever the virtual keyboard holds, then ungrab.
    fn reset(&mut self) {
        let up = self.r.release_all();
        self.emit(&up);
        self.set_grab(false);
        self.r = Remapper::default();
        self.m.bindings.clear();
    }

    fn reply(&mut self, line: String) {
        if let Some((_, stream, _)) = self.client.as_mut() {
            let _ = writeln!(stream, "{line}");
        }
    }

    fn on_msg(&mut self, msg: Result<Msg, (u32, String)>) {
        let rules = self.m.bindings.len() + self.r.rules.len();
        let err = match msg {
            Err((id, e)) => Some((id, e)),
            Ok(Msg::Bind(id, _) | Msg::Remap(id, ..)) if rules >= MAX_BINDINGS => Some((id, "too many rules".into())),
            Ok(Msg::Bind(id, c)) => match c.refusal() {
                Some(why) => Some((id, why.into())),
                None => {
                    self.m.bindings.push((id, c));
                    None
                }
            },
            Ok(Msg::Remap(_, src, action)) => {
                self.r.rules.insert(src, action);
                self.maybe_grab();
                None
            }
            Ok(Msg::TapHoldMs(ms)) => {
                self.r.taphold = Duration::from_millis(ms);
                None
            }
        };
        if let Some((id, e)) = err {
            self.reply(format!("err {id} {e}"));
        }
    }

    fn on_key(&mut self, dev: usize, k: KeyCode, v: i32) {
        match v {
            0 => self.pressed.remove(&(dev, k)),
            _ => self.pressed.insert((dev, k)),
        };
        let panic_keys = [KeyCode::KEY_BACKSPACE, KeyCode::KEY_ESC, KeyCode::KEY_ENTER];
        if panic_keys.iter().all(|p| self.pressed.contains(&(dev, *p))) {
            eprintln!("emergency exit (Backspace+Esc+Enter)");
            std::process::exit(2);
        }
        if self.grabbed {
            let out = self.r.key(k, v, Instant::now());
            self.emit(&out);
            for (k, v) in out {
                self.hotkey(k, v);
            }
        } else {
            self.hotkey(k, v);
            self.maybe_grab();
        }
    }

    fn hotkey(&mut self, k: KeyCode, v: i32) {
        let Some(id) = self.m.key(k, v) else { return };
        let Some((_, stream, uid)) = self.client.as_mut() else { return };
        // Re-check on every match: after a user switch the old session must not see presses.
        let ok = active_uid() == Some(*uid) && writeln!(stream, "fired {id}").is_ok();
        if !ok {
            eprintln!("dropping client");
            self.client = None;
            self.reset();
        }
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
    let fds = spawn_devices(&tx);
    let keys: AttributeSet<KeyCode> = (1..0x100).map(KeyCode::new).filter(|k| is_output_key(*k)).collect();
    let vdev = VirtualDevice::builder()?.name("keysmith virtual keyboard").with_keys(&keys)?.build()?;
    let tx2 = tx.clone();
    thread::spawn(move || serve(listener, tx2));
    drop(tx);

    let mut s = State {
        m: Matcher::default(),
        r: Remapper::default(),
        client: None,
        fds,
        vdev,
        grabbed: false,
        pressed: HashSet::new(),
    };
    loop {
        let ev = match s.r.deadline() {
            Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match ev {
            Err(RecvTimeoutError::Timeout) => {
                let out = s.r.tick(Instant::now());
                s.emit(&out);
                for (k, v) in out {
                    s.hotkey(k, v);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Ev::Key(dev, k, v)) => s.on_key(dev, k, v),
            Ok(Ev::Lost(dev)) => {
                s.fds[dev] = None;
                s.pressed.retain(|(d, _)| *d != dev);
            }
            Ok(Ev::Client(conn, stream, uid)) => {
                eprintln!("client {conn} uid {uid}");
                s.reset();
                s.client = Some((conn, stream, uid));
            }
            Ok(Ev::Gone(conn)) => {
                if s.client.as_ref().is_some_and(|(cur, ..)| *cur == conn) {
                    eprintln!("client {conn} gone");
                    s.client = None;
                    s.reset();
                }
            }
            Ok(Ev::Msg(conn, msg)) => {
                if s.client.as_ref().is_some_and(|(cur, ..)| *cur == conn) {
                    s.on_msg(msg);
                }
            }
        }
    }
}
