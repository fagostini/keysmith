//! Luau script host. Each script gets its own thread and its own sandboxed `Lua`;
//! scripts never see key events, only their own callbacks being called.

use mlua::{Compiler, Function, Lua, LuaOptions, StdLib, Table, Value, Variadic, VmState};
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use crate::window::Window;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Sender, channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

/// Per-callback CPU budget; time spent in `sleep()` doesn't count.
const BUDGET: Duration = Duration::from_secs(1);
const MEMORY: usize = 16 << 20;
const MAX_SLEEP_MS: u64 = 10_000;

/// What a script asked for at load time. `usize` = index of its callback.
#[derive(Debug, PartialEq)]
pub enum Reg {
    Hotkey(String, usize),
    Remap(String, String),
    Hotstring { trigger: String, immediate: bool, text: Option<String>, cb: Option<usize> },
}

/// `send(text)` implementation provided by the engine: (script name, text).
pub type SendFn = Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

pub struct Loaded {
    pub regs: Vec<Reg>,
    /// Call callback N.
    pub tx: Sender<usize>,
}

/// Capabilities from `--! capabilities: run` lines at the top of the script.
pub fn capabilities(src: &str) -> Vec<String> {
    src.lines()
        .take_while(|l| l.starts_with("--"))
        .filter_map(|l| l.strip_prefix("--!")?.trim().strip_prefix("capabilities:"))
        .flat_map(|c| c.split([',', ' ']).filter(|s| !s.is_empty()).map(str::to_owned))
        .collect()
}

/// Log line to stderr and the runtime log the manager UI shows.
pub fn log(line: &str) {
    eprintln!("keysmith: {line}");
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    let f = std::fs::OpenOptions::new().create(true).append(true).open(crate::run_dir().join("engine.log"));
    if let Ok(mut f) = f {
        let _ = std::io::Write::write_all(&mut f, format!("{h:02}:{m:02}:{s:02} UTC  {line}\n").as_bytes());
    }
}

pub fn report(origin: &str, msg: &str) {
    log(&format!("{origin}: {msg}"));
    let _ = Command::new("notify-send").args(["-a", "keysmith", &format!("keysmith: {origin}"), msg]).spawn().map(reap);
}

fn reap(mut child: std::process::Child) {
    thread::spawn(move || child.wait());
}

/// Builds the sandboxed state and API. `regs` is Some only while the top level runs.
fn setup(
    win: Arc<Mutex<Window>>,
    name: &str,
    run_allowed: bool,
    send: SendFn,
    deadline: Rc<Cell<Instant>>,
    regs: Rc<RefCell<Option<Vec<Reg>>>>,
    cbs: Rc<RefCell<Vec<Function>>>,
) -> mlua::Result<Lua> {
    let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::OS, LuaOptions::default())?;
    lua.set_memory_limit(MEMORY)?;
    let d = deadline.clone();
    lua.set_interrupt(move |_| {
        if Instant::now() > d.get() {
            Err(mlua::Error::runtime("script ran too long (1 s budget per call, sleep() excluded)"))
        } else {
            Ok(VmState::Continue)
        }
    });
    let g = lua.globals();
    // Sandbox findings from §6.5: drop code loading and environment access.
    for f in ["loadstring", "getfenv", "setfenv"] {
        g.raw_set(f, Value::Nil)?;
    }

    // Registration helpers: only valid while the script's top level runs.
    let add = {
        let (regs, cbs) = (regs.clone(), cbs.clone());
        move |f: Option<Function>, mk: &dyn Fn(Option<usize>) -> Reg| -> mlua::Result<()> {
            let mut r = regs.borrow_mut();
            let r = r.as_mut().ok_or_else(|| mlua::Error::runtime("register hotkeys at the top level of the script"))?;
            let cb = f.map(|f| {
                cbs.borrow_mut().push(f);
                cbs.borrow().len() - 1
            });
            r.push(mk(cb));
            Ok(())
        }
    };
    let a = add.clone();
    g.set(
        "hotkey",
        lua.create_function(move |_, (combo, f): (String, Function)| a(Some(f), &|cb| Reg::Hotkey(combo.clone(), cb.unwrap())))?,
    )?;
    let a = add.clone();
    g.set(
        "remap",
        lua.create_function(move |_, (src, dst): (String, Value)| {
            let action = match dst {
                Value::String(s) => s.to_str()?.to_owned(),
                Value::Table(t) => format!("tap:{} hold:{}", t.get::<String>("tap")?, t.get::<String>("hold")?),
                _ => return Err(mlua::Error::runtime("remap(key, \"key\") or remap(key, {tap=..., hold=...})")),
            };
            a(None, &|_| Reg::Remap(src.clone(), action.clone()))
        })?,
    )?;
    let a = add;
    g.set(
        "hotstring",
        lua.create_function(move |_, (trigger, what, opts): (String, Value, Option<Table>)| {
            let immediate = opts.map(|o| o.get::<Option<bool>>("immediate")).transpose()?.flatten().unwrap_or(false);
            let (text, f) = match what {
                Value::String(s) => (Some(s.to_str()?.to_owned()), None),
                Value::Function(f) => (None, Some(f)),
                _ => return Err(mlua::Error::runtime("hotstring(trigger, \"text\" or function)")),
            };
            a(f, &|cb| Reg::Hotstring { trigger: trigger.clone(), immediate, text: text.clone(), cb })
        })?,
    )?;

    let n = name.to_owned();
    g.set(
        "send",
        lua.create_function(move |_, text: String| send(&n, &text).map_err(mlua::Error::runtime))?,
    )?;
    g.set(
        "run",
        lua.create_function(move |_, cmd: String| {
            if !run_allowed {
                return Err(mlua::Error::runtime("run() needs `--! capabilities: run` in the script header"));
            }
            Command::new("sh").arg("-c").arg(&cmd).spawn().map(reap).map_err(mlua::Error::runtime)
        })?,
    )?;
    g.set(
        "sleep",
        lua.create_function(move |_, ms: u64| {
            let dur = Duration::from_millis(ms.min(MAX_SLEEP_MS));
            thread::sleep(dur);
            deadline.set(deadline.get() + dur);
            Ok(())
        })?,
    )?;
    // `window.class` / `window.title`: read live from the window backend.
    let window = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set(
        "__index",
        lua.create_function(move |_, (_, k): (Value, String)| {
            let w = win.lock().unwrap();
            Ok(match k.as_str() {
                "class" => Some(w.class.clone()),
                "title" => Some(w.title.clone()),
                _ => None,
            })
        })?,
    )?;
    window.set_metatable(Some(meta))?;
    g.set("window", window)?;
    let n = name.to_owned();
    g.set("notify", lua.create_function(move |_, msg: String| {
        report(&n, &msg);
        Ok(())
    })?)?;
    let n = name.to_owned();
    g.set(
        "print",
        lua.create_function(move |_, args: Variadic<Value>| {
            let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect::<mlua::Result<_>>()?;
            log(&format!("[{n}] {}", parts.join("\t")));
            Ok(())
        })?,
    )?;
    // Freezes the globals above (scripts can't replace `send`) and isolates script globals.
    lua.sandbox(true)?;
    Ok(lua)
}

/// Runs the script's top level on its own thread; returns what it registered.
pub fn spawn(name: String, src: String, send: SendFn, win: Arc<Mutex<Window>>) -> Result<Loaded, String> {
    let run_allowed = capabilities(&src).iter().any(|c| c == "run");
    let (done_tx, done_rx) = sync_channel(1);
    let (tx, rx) = channel::<usize>();
    thread::spawn(move || {
        let deadline = Rc::new(Cell::new(Instant::now() + BUDGET));
        let regs = Rc::new(RefCell::new(Some(Vec::new())));
        let cbs = Rc::new(RefCell::new(Vec::new()));
        let loaded = setup(win, &name, run_allowed, send, deadline.clone(), regs.clone(), cbs.clone())
            .and_then(|lua| {
                // `window` changes under the script: stop Luau caching `window.class`
                // at load time (import resolution), or callbacks see the startup value.
                let compiler = Compiler::new().set_mutable_globals(["window"]);
                lua.load(&src).set_name(&name).set_compiler(compiler).exec().map(|_| lua)
            });
        let lua = match loaded {
            Ok(lua) => lua,
            Err(e) => return drop(done_tx.send(Err(e.to_string()))),
        };
        let _ = done_tx.send(Ok(regs.borrow_mut().take().unwrap()));
        for cb in rx {
            deadline.set(Instant::now() + BUDGET);
            let f = cbs.borrow().get(cb).cloned();
            if let Some(Err(e)) = f.map(|f| f.call::<()>(())) {
                report(&name, &e.to_string());
            }
        }
        drop(lua);
    });
    let regs = done_rx.recv().map_err(|_| "script thread died".to_owned())??;
    Ok(Loaded { regs, tx })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(name: String, src: String, send: SendFn) -> Result<Loaded, String> {
        super::spawn(name, src, send, Arc::new(Mutex::new(Window { class: "firefox".into(), title: "t".into() })))
    }

    fn load(src: &str) -> Result<Loaded, String> {
        spawn("t.luau".into(), src.into(), Arc::new(|_, _| Ok(())))
    }

    #[test]
    fn registers() {
        let l = load(
            r#"
            hotkey("ctrl+alt+t", function() end)
            remap("capslock", {tap="esc", hold="leftctrl"})
            remap("rightalt", "leftmeta")
            hotstring("btw", "by the way")
            hotstring(";dt", function() send(os.date("%Y-%m-%d")) end, {immediate=true})
            assert(window.class == "firefox" and window.title == "t" and window.nope == nil)
            "#,
        )
        .unwrap();
        assert_eq!(
            l.regs,
            vec![
                Reg::Hotkey("ctrl+alt+t".into(), 0),
                Reg::Remap("capslock".into(), "tap:esc hold:leftctrl".into()),
                Reg::Remap("rightalt".into(), "leftmeta".into()),
                Reg::Hotstring { trigger: "btw".into(), immediate: false, text: Some("by the way".into()), cb: None },
                Reg::Hotstring { trigger: ";dt".into(), immediate: true, text: None, cb: Some(1) },
            ]
        );
    }

    #[test]
    fn callbacks_run_and_send() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let send: SendFn = Arc::new(move |n, t| {
            g.lock().unwrap().push(format!("{n}:{t}"));
            Ok(())
        });
        let l = spawn("s.luau".into(), r#"local n = 0 hotkey("f5", function() n += 1 send("hi " .. n) end)"#.into(), send).unwrap();
        l.tx.send(0).unwrap();
        l.tx.send(0).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(*got.lock().unwrap(), vec!["s.luau:hi 1", "s.luau:hi 2"]);
    }

    #[test]
    fn sandbox() {
        let err = |src: &str| load(src).err().unwrap_or_else(|| panic!("should fail: {src}"));
        assert!(err("io.open('/etc/passwd')").contains("io"));
        err("os.execute('true')");
        err("loadstring('return 1')()");
        err("getfenv(0)");
        err("string.upper = nil"); // shared libraries are read-only (globals only shadow per script)
        assert!(err("while true do end").contains("too long"));
        assert!(err("local t = {} for i = 1, 1e8 do t[i] = i end").contains("memory"));
        assert!(err("run('true')").contains("capabilities"));
        load("--! capabilities: run\nrun('true')").unwrap();
        assert!(err("sleep(1) hotkey('f5', function() end) local f = function() hotkey('f6', function() end) end f() error('x')").contains('x'));
        assert_eq!(capabilities("--!strict\n--! capabilities: run, fs\nprint(1)"), vec!["run", "fs"]);
    }

    #[test]
    fn window_is_live() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let send: SendFn = Arc::new(move |_, t| {
            g.lock().unwrap().push(t.to_owned());
            Ok(())
        });
        let win = Arc::new(Mutex::new(Window::default()));
        let l = super::spawn("w.luau".into(), r#"hotkey("f5", function() send(window.class) end)"#.into(), send, win.clone()).unwrap();
        win.lock().unwrap().class = "firefox".into();
        l.tx.send(0).unwrap();
        thread::sleep(Duration::from_millis(100));
        win.lock().unwrap().class = "kitty".into();
        l.tx.send(0).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(*got.lock().unwrap(), vec!["firefox", "kitty"]);
    }

    #[test]
    fn late_registration_rejected() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let send: SendFn = Arc::new(move |_, t| {
            g.lock().unwrap().push(t.to_owned());
            Ok(())
        });
        let l = spawn(
            "late.luau".into(),
            r#"hotkey("f5", function() local ok = pcall(hotkey, "f6", function() end) send(tostring(ok)) end)"#.into(),
            send,
        )
        .unwrap();
        l.tx.send(0).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(*got.lock().unwrap(), vec!["false"]);
    }
}
