//! Keysmith manager: tray icon + window to enable, disable, allow and reload
//! scripts and read the engine log. Talks to the engine only through files in
//! its run dir (`status.json`, `engine.log`, `pid`) and SIGHUP.
//!
//!   ks-ui           open the manager window (and the tray icon)
//!   ks-ui --tray    start with only the tray icon (for autostart)

use gtk4::prelude::*;
use gtk4::{gio, glib};
use serde_json::Value;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

fn run_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("keysmith")
}

/// Engine pid, if it is running.
fn engine_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(run_dir().join("pid")).ok()?.trim().parse().ok()?;
    // Signal 0 checks existence; the comm check avoids signalling a recycled pid.
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    (comm.trim() == "keysmith").then_some(pid)
}

fn reload() -> Result<(), String> {
    use nix::sys::signal::{Signal, kill};
    let pid = engine_pid().ok_or("the engine is not running")?;
    kill(nix::unistd::Pid::from_raw(pid), Signal::SIGHUP).map_err(|e| e.to_string())
}

/// The `keysmith` binary next to us, else from PATH.
fn engine_bin() -> PathBuf {
    let here = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join("keysmith")));
    here.filter(|p| p.exists()).unwrap_or_else(|| "keysmith".into())
}

fn set_disabled(dir: &Path, name: &str, off: bool) -> std::io::Result<()> {
    let path = dir.join("disabled");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut names: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty() && *l != name).collect();
    if off {
        names.push(name);
    }
    std::fs::write(path, names.join("\n") + "\n")
}

enum TrayCmd {
    Open,
    Reload,
    Quit,
}

/// (commands to the UI, a recording is running)
struct Tray(mpsc::Sender<TrayCmd>, bool);

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "keysmith".into()
    }
    fn title(&self) -> String {
        if self.1 { "Keysmith: recording keys (Ctrl+Alt+Shift+R stops)" } else { "Keysmith" }.into()
    }
    fn icon_name(&self) -> String {
        if self.1 { "media-record" } else { "input-keyboard" }.into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.0.send(TrayCmd::Open);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let item = |label: &str, cmd: fn() -> TrayCmd| {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |t: &mut Self| drop(t.0.send(cmd()))),
                ..Default::default()
            }
            .into()
        };
        vec![
            item("Open manager", || TrayCmd::Open),
            item("Reload scripts", || TrayCmd::Reload),
            ksni::MenuItem::Separator,
            item("Quit manager", || TrayCmd::Quit),
        ]
    }
}

struct Ui {
    window: gtk4::ApplicationWindow,
    banner: gtk4::Label,
    list: gtk4::ListBox,
    log: gtk4::TextBuffer,
    log_view: gtk4::TextView,
    last_status: RefCell<String>,
    last_log: RefCell<String>,
}

impl Ui {
    fn error(&self, msg: &str) {
        gtk4::AlertDialog::builder().message("Keysmith").detail(msg).build().show(Some(&self.window));
    }

    fn reload(&self) {
        if let Err(e) = reload() {
            self.error(&e);
        }
    }

    /// Re-read status.json and engine.log; rebuild only what changed.
    fn refresh(self: &Rc<Self>) {
        let raw = std::fs::read_to_string(run_dir().join("status.json")).unwrap_or_default();
        let running = engine_pid().is_some();
        let key = format!("{running}{raw}");
        if *self.last_status.borrow() != key {
            *self.last_status.borrow_mut() = key;
            self.rebuild(running, serde_json::from_str(&raw).unwrap_or(Value::Null));
        }
        let log = std::fs::read_to_string(run_dir().join("engine.log")).unwrap_or_default();
        if *self.last_log.borrow() != log {
            self.log.set_text(&log);
            self.log_view.scroll_to_mark(&self.log.get_insert(), 0.0, false, 0.0, 0.0);
            let end = self.log.end_iter();
            self.log.place_cursor(&end);
            self.log_view.scroll_mark_onscreen(&self.log.get_insert());
            *self.last_log.borrow_mut() = log;
        }
    }

    fn rebuild(self: &Rc<Self>, running: bool, status: Value) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        let banner = match (running, status["error"].as_str()) {
            (false, _) => "The engine is not running. It starts at login; to start it now, run `keysmith &` in a terminal.".into(),
            (true, Some(e)) => format!("Engine problem: {e}"),
            (true, None) => format!("Engine running · {} rules", status["rules"]),
        };
        self.banner.set_text(&banner);
        let dir = PathBuf::from(status["config_dir"].as_str().unwrap_or_default());
        for s in status["scripts"].as_array().into_iter().flatten() {
            self.list.append(&self.row(&dir, s));
        }
        if running && status["scripts"].as_array().is_none_or(|a| a.is_empty()) {
            let hint = format!(
                "No scripts yet. Put .luau files in {}/scripts/ — examples are in /usr/share/doc/keysmith/examples/.",
                dir.display()
            );
            self.list.append(&gtk4::Label::builder().label(&hint).wrap(true).margin_top(12).margin_bottom(12).build());
        }
    }

    fn row(self: &Rc<Self>, dir: &Path, s: &Value) -> gtk4::Box {
        let get = |k: &str| s[k].as_str().unwrap_or_default().to_owned();
        let (name, state, msg, path) = (get("name"), get("state"), get("message"), get("path"));
        let caps: Vec<String> = s["caps"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(Into::into)).collect();

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(12);
        row.set_margin_end(12);

        let enabled = gtk4::Switch::builder().active(state != "disabled").valign(gtk4::Align::Center).build();
        enabled.update_property(&[gtk4::accessible::Property::Label(&format!("Enable {name}"))]);
        let (ui, d, n) = (self.clone(), dir.to_owned(), name.clone());
        enabled.connect_state_set(move |_, on| {
            match set_disabled(&d, &n, !on) {
                Ok(()) => ui.reload(),
                Err(e) => ui.error(&format!("cannot update {}: {e}", d.join("disabled").display())),
            }
            glib::Propagation::Proceed
        });
        row.append(&enabled);

        let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let title = gtk4::Label::builder().label(&name).xalign(0.0).css_classes(["heading"]).build();
        let label = match state.as_str() {
            "loaded" => "Running".to_owned(),
            "disabled" => "Disabled".to_owned(),
            "untrusted" => format!("Needs permission: {}", caps.join(", ")),
            _ => format!("Error: {msg}"),
        };
        let sub = gtk4::Label::builder().label(&label).xalign(0.0).wrap(true).selectable(true).build();
        sub.add_css_class(if state == "error" { "error" } else { "dim-label" });
        text.append(&title);
        text.append(&sub);
        row.append(&text);

        if state == "untrusted" {
            let allow = gtk4::Button::with_label("Allow…");
            allow.set_valign(gtk4::Align::Center);
            let (ui, p, n) = (self.clone(), path.clone(), name.clone());
            allow.connect_clicked(move |_| ui.confirm_trust(&n, &p, &caps));
            row.append(&allow);
        }
        let open = gtk4::Button::from_icon_name("document-edit-symbolic");
        open.set_tooltip_text(Some("Open script"));
        open.update_property(&[gtk4::accessible::Property::Label(&format!("Open {name}"))]);
        open.set_valign(gtk4::Align::Center);
        let ui = self.clone();
        open.connect_clicked(move |_| {
            let file = gio::File::for_path(&path);
            gtk4::FileLauncher::new(Some(&file)).launch(Some(&ui.window), gio::Cancellable::NONE, |_| {});
        });
        row.append(&open);
        row
    }

    fn confirm_trust(self: &Rc<Self>, name: &str, path: &str, caps: &[String]) {
        let what: Vec<&str> = caps
            .iter()
            .map(|c| match c.as_str() {
                "run" => "run any command as you (run)",
                "fs" => "read and write your files (fs)",
                "http" => "make network requests (http)",
                other => other,
            })
            .collect();
        let dialog = gtk4::AlertDialog::builder()
            .message(format!("Allow {name}?"))
            .detail(format!(
                "This script asks to:\n• {}\n\nOnly allow scripts you wrote or have read. Editing the script asks again.",
                what.join("\n• ")
            ))
            .buttons(["Cancel", "Allow"])
            .cancel_button(0)
            .default_button(0)
            .modal(true)
            .build();
        let (ui, path) = (self.clone(), path.to_owned());
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |res| {
            if res != Ok(1) {
                return;
            }
            match Command::new(engine_bin()).arg("trust").arg(&path).output() {
                Ok(o) if o.status.success() => ui.reload(),
                Ok(o) => ui.error(&String::from_utf8_lossy(&o.stderr)),
                Err(e) => ui.error(&format!("cannot run keysmith: {e}")),
            }
        });
    }
}

fn build(app: &gtk4::Application) -> Rc<Ui> {
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Keysmith")
        .default_width(640)
        .default_height(560)
        .hide_on_close(true)
        .build();
    let header = gtk4::HeaderBar::new();
    let reload_btn = gtk4::Button::with_label("Reload");
    reload_btn.set_tooltip_text(Some("Reload all scripts (after editing them)"));
    header.pack_start(&reload_btn);
    window.set_titlebar(Some(&header));

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let banner = gtk4::Label::builder().xalign(0.0).wrap(true).margin_top(8).margin_bottom(8).margin_start(12).build();
    root.append(&banner);

    let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).build();
    list.add_css_class("boxed-list");
    let scripts = gtk4::ScrolledWindow::builder().child(&list).vexpand(true).build();
    let log = gtk4::TextBuffer::new(None);
    let log_view = gtk4::TextView::builder().buffer(&log).editable(false).monospace(true).wrap_mode(gtk4::WrapMode::WordChar).build();
    log_view.update_property(&[gtk4::accessible::Property::Label("Engine log")]);
    let logs = gtk4::ScrolledWindow::builder().child(&log_view).vexpand(true).build();

    let stack = gtk4::Stack::new();
    stack.add_titled(&scripts, Some("scripts"), "Scripts");
    stack.add_titled(&logs, Some("log"), "Log");
    let switcher = gtk4::StackSwitcher::builder().stack(&stack).build();
    header.set_title_widget(Some(&switcher));
    root.append(&stack);
    window.set_child(Some(&root));

    let ui = Rc::new(Ui {
        window,
        banner,
        list,
        log,
        log_view,
        last_status: RefCell::default(),
        last_log: RefCell::default(),
    });
    let u = ui.clone();
    reload_btn.connect_clicked(move |_| u.reload());
    ui.refresh();
    let u = ui.clone();
    // ponytail: polls two small files every 500 ms; switch to gio::FileMonitor if that ever shows up in top.
    glib::timeout_add_local(Duration::from_millis(500), move || {
        u.refresh();
        glib::ControlFlow::Continue
    });
    ui
}

fn main() -> glib::ExitCode {
    let tray_only = std::env::args().any(|a| a == "--tray");
    let app = gtk4::Application::builder().application_id("io.github.fagostini.Keysmith").build();
    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();
    let started = Rc::new(std::cell::Cell::new(false));
    app.connect_activate(move |app| {
        let u = ui.borrow_mut().get_or_insert_with(|| build(app)).clone();
        if started.replace(true) {
            u.window.present(); // second launch, or tray click
            return;
        }
        // Keep running with the window hidden, so the tray stays.
        std::mem::forget(app.hold());
        let (tx, rx) = mpsc::channel();
        use ksni::blocking::TrayMethods;
        let tray = Tray(tx, false).spawn();
        if let Err(e) = &tray {
            eprintln!("ks-ui: no tray icon ({e}); closing the window quits");
            u.window.set_hide_on_close(false);
            let app = app.clone();
            u.window.connect_close_request(move |_| {
                app.quit();
                glib::Propagation::Proceed
            });
        }
        let (u2, app2) = (u.clone(), app.clone());
        let rec_flag = run_dir().join("recording");
        glib::timeout_add_local(Duration::from_millis(100), move || {
            if let Ok(t) = &tray {
                let rec = rec_flag.exists();
                if t.update(|t| std::mem::replace(&mut t.1, rec)) != Some(rec) {
                    u2.refresh();
                }
            }
            for cmd in rx.try_iter() {
                match cmd {
                    TrayCmd::Open => u2.window.present(),
                    TrayCmd::Reload => u2.reload(),
                    TrayCmd::Quit => app2.quit(),
                }
            }
            glib::ControlFlow::Continue
        });
        if !tray_only {
            u.window.present();
        }
    });
    // GApplication would reject our --tray flag otherwise.
    app.run_with_args(&std::env::args().take(1).collect::<Vec<_>>())
}
