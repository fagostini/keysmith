//! Active-window tracking, one backend per desktop. Each runs on its own thread
//! and calls `on_focus` whenever the focused window (or its title) changes.
//!
//!   GNOME (Wayland)   Shell extension -> D-Bus `org.keysmith.Engine.Focus`
//!   KDE Plasma 6      KWin script     -> D-Bus `org.keysmith.Engine.Focus`
//!   Sway              i3 IPC socket ($SWAYSOCK), `window` events
//!   Hyprland          socket2 `activewindow>>` events
//!   other wlroots     wlr-foreign-toplevel-management-v1
//!   X11 (any WM)      EWMH `_NET_ACTIVE_WINDOW` + WM_CLASS / _NET_WM_NAME

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    pub class: String,
    pub title: String,
}

pub type OnFocus = Arc<dyn Fn(Window) + Send + Sync>;
type Backend = dyn FnOnce(OnFocus) -> Result<(), String> + Send;

/// Terminals want Ctrl+Shift+V for paste.
// ponytail: name list; add a config knob when someone's terminal is missing.
pub fn is_terminal(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    [
        "terminal", "konsole", "kitty", "alacritty", "foot", "wezterm", "terminator", "xterm", "urxvt", "tilix",
        "ghostty", "kgx", "ptyxis", "st-256color", "terminology", "yakuake", "guake",
    ]
    .iter()
    .any(|t| c.contains(t))
}

/// Picks the backend for this session and starts it. Returns its name.
pub fn start(on_focus: OnFocus) -> Result<&'static str, String> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let desktop = env("XDG_CURRENT_DESKTOP").to_ascii_uppercase();
    let wayland = !env("WAYLAND_DISPLAY").is_empty();
    let (name, run): (&'static str, Box<Backend>) =
        if !env("HYPRLAND_INSTANCE_SIGNATURE").is_empty() {
            ("hyprland", Box::new(hyprland))
        } else if !env("SWAYSOCK").is_empty() {
            ("sway", Box::new(sway))
        } else if wayland && desktop.contains("KDE") {
            ("kwin", Box::new(kwin))
        } else if wayland && desktop.contains("GNOME") {
            ("gnome", Box::new(gnome))
        } else if wayland {
            ("wlr", Box::new(wlr))
        } else if !env("DISPLAY").is_empty() {
            ("x11", Box::new(x11))
        } else {
            return Err("no graphical session found".into());
        };
    std::thread::spawn(move || {
        if let Err(e) = run(on_focus) {
            crate::script::report(&format!("window backend {name}"), &e);
        }
    });
    Ok(name)
}

// ---------------------------------------------------------------- D-Bus (GNOME, KWin)

pub const BUS_NAME: &str = "org.keysmith.Engine";
const BUS_PATH: &str = "/org/keysmith/Engine";

struct Service(OnFocus);

// ponytail: any process in the session can call Focus and spoof the window. It
// already runs as the user, so it could just as well run its own scripts; inputd
// never trusts window info for anything security-relevant.
#[zbus::interface(name = "org.keysmith.Engine")]
impl Service {
    fn focus(&self, class: String, title: String) {
        (self.0)(Window { class, title });
    }
}

fn serve(on_focus: OnFocus) -> Result<zbus::blocking::Connection, String> {
    zbus::blocking::connection::Builder::session()
        .and_then(|b| b.name(BUS_NAME))
        .and_then(|b| b.serve_at(BUS_PATH, Service(on_focus)))
        .and_then(|b| b.build())
        .map_err(|e| format!("D-Bus: {e}"))
}

fn gnome(on_focus: OnFocus) -> Result<(), String> {
    let _conn = serve(on_focus)?;
    let ext = "keysmith@keysmith.github.io";
    let ok = std::process::Command::new("gnome-extensions")
        .args(["info", ext])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("ACTIVE"));
    if !ok {
        crate::script::report(
            "window backend gnome",
            &format!("enable the {ext} extension (see data/gnome-extension/README) for window info"),
        );
    }
    park()
}

const KWIN_SCRIPT: &str = include_str!("../../../data/kwin/keysmith.js");

fn kwin(on_focus: OnFocus) -> Result<(), String> {
    let conn = serve(on_focus)?;
    let path = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR unset")?).join("keysmith-kwin.js");
    std::fs::write(&path, KWIN_SCRIPT).map_err(|e| e.to_string())?;
    let kwin = Some("org.kde.KWin");
    let (scripting, iface) = ("/Scripting", Some("org.kde.kwin.Scripting"));
    let err = |m: &'static str| move |e: zbus::Error| format!("KWin {m}: {e}");
    let _ = conn.call_method(kwin, scripting, iface, "unloadScript", &("keysmith",));
    let id: i32 = conn
        .call_method(kwin, scripting, iface, "loadScript", &(path.to_string_lossy().as_ref(), "keysmith"))
        .map_err(err("loadScript"))?
        .body()
        .deserialize()
        .map_err(err("loadScript"))?;
    conn.call_method(kwin, format!("/Scripting/Script{id}").as_str(), Some("org.kde.kwin.Script"), "run", &())
        .map_err(err("run"))?;
    park()
}

fn park() -> Result<(), String> {
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------- Sway (i3 IPC)

const I3_MAGIC: &[u8] = b"i3-ipc";

fn i3_send(s: &mut UnixStream, kind: u32, payload: &str) -> std::io::Result<()> {
    let mut msg = I3_MAGIC.to_vec();
    msg.extend((payload.len() as u32).to_ne_bytes());
    msg.extend(kind.to_ne_bytes());
    msg.extend(payload.as_bytes());
    s.write_all(&msg)
}

fn i3_recv(s: &mut impl Read) -> std::io::Result<(u32, Vec<u8>)> {
    let mut head = [0u8; 14];
    s.read_exact(&mut head)?;
    if &head[..6] != I3_MAGIC {
        return Err(std::io::Error::other("bad i3-ipc magic"));
    }
    let len = u32::from_ne_bytes(head[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(head[10..14].try_into().unwrap());
    let mut body = vec![0; len];
    s.read_exact(&mut body)?;
    Ok((kind, body))
}

/// Window of a sway container: `app_id` for Wayland clients, X11 class under XWayland.
fn sway_window(c: &Value) -> Window {
    let class = c["app_id"].as_str().or_else(|| c["window_properties"]["class"].as_str()).unwrap_or_default();
    Window { class: class.into(), title: c["name"].as_str().unwrap_or_default().into() }
}

fn sway_focused(tree: &Value) -> Option<Window> {
    if tree["focused"] == true && tree["type"] != "workspace" {
        return Some(sway_window(tree));
    }
    let kids = tree["nodes"].as_array().into_iter().chain(tree["floating_nodes"].as_array()).flatten();
    kids.into_iter().find_map(sway_focused)
}

/// `window` event -> new focus, if it's a focus or title change of the focused window.
fn sway_event(ev: &Value) -> Option<Window> {
    let c = &ev["container"];
    match ev["change"].as_str()? {
        "focus" => Some(sway_window(c)),
        "title" if c["focused"] == true => Some(sway_window(c)),
        _ => None,
    }
}

fn sway(on_focus: OnFocus) -> Result<(), String> {
    let path = std::env::var("SWAYSOCK").map_err(|e| e.to_string())?;
    let mut s = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    let io = |e: std::io::Error| e.to_string();
    i3_send(&mut s, 4, "").map_err(io)?; // GET_TREE
    let (_, tree) = i3_recv(&mut s).map_err(io)?;
    if let Some(w) = serde_json::from_slice(&tree).ok().as_ref().and_then(sway_focused) {
        on_focus(w);
    }
    i3_send(&mut s, 2, r#"["window"]"#).map_err(io)?; // SUBSCRIBE
    loop {
        let (kind, body) = i3_recv(&mut s).map_err(io)?;
        if kind & 0x8000_0000 == 0 {
            continue; // subscribe reply
        }
        if let Some(w) = serde_json::from_slice(&body).ok().as_ref().and_then(sway_event) {
            on_focus(w);
        }
    }
}

// ---------------------------------------------------------------- Hyprland

fn hypr_dir() -> Result<PathBuf, String> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|e| e.to_string())?;
    let rt = std::env::var("XDG_RUNTIME_DIR").map_err(|e| e.to_string())?;
    Ok(PathBuf::from(rt).join("hypr").join(sig))
}

/// `activewindow>>class,title` (title may contain commas).
fn hypr_event(line: &str) -> Option<Window> {
    let (class, title) = line.strip_prefix("activewindow>>")?.split_once(',')?;
    Some(Window { class: class.into(), title: title.into() })
}

fn hyprland(on_focus: OnFocus) -> Result<(), String> {
    let dir = hypr_dir()?;
    if let Ok(mut s) = UnixStream::connect(dir.join(".socket.sock")) {
        let mut out = String::new();
        if s.write_all(b"j/activewindow").is_ok()
            && s.read_to_string(&mut out).is_ok()
            && let Ok(v) = serde_json::from_str::<Value>(&out)
        {
            let get = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
            on_focus(Window { class: get("class"), title: get("title") });
        }
    }
    let path = dir.join(".socket2.sock");
    let s = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    for line in BufReader::new(s).lines() {
        if let Some(w) = hypr_event(&line.map_err(|e| e.to_string())?) {
            on_focus(w);
        }
    }
    Err("Hyprland event socket closed".into())
}

// ---------------------------------------------------------------- wlr foreign toplevel

mod wlr_impl {
    use super::{OnFocus, Window};
    use std::collections::HashMap;
    use wayland_client::backend::ObjectId;
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
    use wayland_protocols_wlr::foreign_toplevel::v1::client::{
        zwlr_foreign_toplevel_handle_v1::{self as handle, ZwlrForeignToplevelHandleV1},
        zwlr_foreign_toplevel_manager_v1::{self as manager, ZwlrForeignToplevelManagerV1},
    };

    #[derive(Default)]
    struct Top {
        w: Window,
        active: bool,
    }

    pub struct State {
        on_focus: OnFocus,
        tops: HashMap<ObjectId, Top>,
        last: Option<Window>,
        bound: bool,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
            if let wl_registry::Event::Global { name, interface, version } = e
                && interface == ZwlrForeignToplevelManagerV1::interface().name
            {
                r.bind::<ZwlrForeignToplevelManagerV1, _, _>(name, version.min(3), qh, ());
                s.bound = true;
            }
        }
    }

    impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
        fn event(s: &mut Self, _: &ZwlrForeignToplevelManagerV1, e: manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let manager::Event::Toplevel { toplevel } = e {
                s.tops.insert(toplevel.id(), Top::default());
            }
        }
        event_created_child!(State, ZwlrForeignToplevelManagerV1, [
            manager::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
        ]);
    }

    impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
        fn event(s: &mut Self, h: &ZwlrForeignToplevelHandleV1, e: handle::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            let t = s.tops.entry(h.id()).or_default();
            match e {
                handle::Event::Title { title } => t.w.title = title,
                handle::Event::AppId { app_id } => t.w.class = app_id,
                handle::Event::State { state } => {
                    // Array of u32 enum values; 2 = activated.
                    t.active = state.as_chunks::<4>().0.iter().any(|c| u32::from_ne_bytes(*c) == 2);
                }
                handle::Event::Done if t.active && s.last.as_ref() != Some(&t.w) => {
                    s.last = Some(t.w.clone());
                    (s.on_focus)(t.w.clone());
                }
                handle::Event::Closed => {
                    s.tops.remove(&h.id());
                    h.destroy();
                }
                _ => {}
            }
        }
    }

    pub fn run(on_focus: OnFocus) -> Result<(), String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("Wayland: {e}"))?;
        let mut q = conn.new_event_queue();
        conn.display().get_registry(&q.handle(), ());
        let mut s = State { on_focus, tops: HashMap::new(), last: None, bound: false };
        q.roundtrip(&mut s).map_err(|e| e.to_string())?;
        if !s.bound {
            return Err("compositor lacks wlr-foreign-toplevel-management (GNOME, KDE and COSMIC have their own backends)".into());
        }
        loop {
            q.blocking_dispatch(&mut s).map_err(|e| e.to_string())?;
        }
    }
}

fn wlr(on_focus: OnFocus) -> Result<(), String> {
    wlr_impl::run(on_focus)
}

// ---------------------------------------------------------------- X11 (EWMH)

fn x11(on_focus: OnFocus) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask};
    let e = |e: &dyn std::fmt::Display| format!("X11: {e}");
    let (c, screen) = x11rb::connect(None).map_err(|x| e(&x))?;
    let root = c.setup().roots[screen].root;
    let atom = |n: &str| c.intern_atom(false, n.as_bytes()).map_err(|x| e(&x))?.reply().map_err(|x| e(&x)).map(|r| r.atom);
    let (active, net_name, utf8) = (atom("_NET_ACTIVE_WINDOW")?, atom("_NET_WM_NAME")?, atom("UTF8_STRING")?);
    let watch = |w| c.change_window_attributes(w, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE));
    watch(root).map_err(|x| e(&x))?;
    let prop = |w, p, t, len| c.get_property(false, w, p, t, 0, len).ok()?.reply().ok().map(|r| r.value);
    let read = |w| {
        // WM_CLASS is "instance\0class\0"; the class is the conventional match key.
        let class = prop(w, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into(), 256).unwrap_or_default();
        let class = class.split(|b| *b == 0).nth(1).unwrap_or_default();
        let title = prop(w, net_name, utf8, 1024).filter(|t| !t.is_empty());
        let title = title.or_else(|| prop(w, AtomEnum::WM_NAME.into(), AtomEnum::ANY.into(), 1024)).unwrap_or_default();
        Window { class: String::from_utf8_lossy(class).into(), title: String::from_utf8_lossy(&title).into() }
    };
    let (mut cur, mut last) = (0, Window::default());
    loop {
        let w = prop(root, active, AtomEnum::WINDOW.into(), 1)
            .and_then(|v| Some(u32::from_ne_bytes(v.get(..4)?.try_into().ok()?)))
            .unwrap_or(0);
        if w != cur {
            cur = w;
            if w != 0 {
                let _ = watch(w); // title changes of the active window
            }
        }
        let now = if w == 0 { Window::default() } else { read(w) };
        if now != last {
            last = now.clone();
            on_focus(now);
        }
        let _ = c.flush();
        // Wait for the next relevant change.
        loop {
            match c.wait_for_event().map_err(|x| e(&x))? {
                Event::PropertyNotify(p) if p.window == root && p.atom == active => break,
                Event::PropertyNotify(p) if p.window == cur && (p.atom == net_name || p.atom == u32::from(AtomEnum::WM_NAME)) => break,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parsers() {
        assert_eq!(
            hypr_event("activewindow>>firefox,Hello, world — Mozilla Firefox"),
            Some(Window { class: "firefox".into(), title: "Hello, world — Mozilla Firefox".into() })
        );
        assert_eq!(hypr_event("workspace>>2"), None);

        let ff = json!({"type": "con", "focused": true, "app_id": "firefox", "name": "Example"});
        let xw = json!({"type": "con", "focused": false, "app_id": null, "window_properties": {"class": "Gimp"}, "name": "GIMP"});
        assert_eq!(sway_event(&json!({"change": "focus", "container": ff})).unwrap().class, "firefox");
        assert_eq!(sway_event(&json!({"change": "focus", "container": xw})).unwrap().class, "Gimp");
        assert_eq!(sway_event(&json!({"change": "title", "container": xw})), None, "title of an unfocused window");
        assert_eq!(sway_event(&json!({"change": "new", "container": ff})), None);
        let tree = json!({"type": "root", "nodes": [{"type": "output", "nodes": [{"type": "workspace", "focused": false,
            "nodes": [xw], "floating_nodes": [ff]}]}]});
        assert_eq!(sway_focused(&tree).unwrap().title, "Example");

        let mut framed = Vec::new();
        framed.extend(I3_MAGIC);
        framed.extend(2u32.to_ne_bytes());
        framed.extend(0x8000_0003u32.to_ne_bytes());
        framed.extend(b"{}");
        assert_eq!(i3_recv(&mut &framed[..]).unwrap(), (0x8000_0003, b"{}".to_vec()));

        assert!(is_terminal("org.gnome.Ptyxis") && is_terminal("Terminator") && is_terminal("kitty"));
        assert!(!is_terminal("firefox"));
    }
}
