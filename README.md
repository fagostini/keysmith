# Keysmith

AutoHotkey-style automation for Linux that works on **Wayland and X11**: global
hotkeys, key remaps with tap/hold, hotstrings (text expansion), and sandboxed
[Luau](https://luau.org) scripts that can react to the focused window.

```lua
-- ~/.config/keysmith/scripts/example.luau
remap("capslock", { tap = "esc", hold = "leftctrl" })
hotstring("btw", "by the way")
hotstring(";date", function() send(os.date("%Y-%m-%d")) end, { immediate = true })

hotkey("super+f9", function()
    if window.class == "firefox" then
        send("Hello from Firefox")
    else
        notify("focused: " .. window.class)
    end
end)
```

> **Status: early (0.2).** Tested on X11 (Cinnamon). The Wayland window
> backends (GNOME, KDE Plasma 6, Sway, Hyprland, other wlroots compositors)
> are implemented but not yet verified on real sessions.

## How it works, and why your typing stays private

Wayland doesn't let ordinary apps see or inject global key events, so Keysmith
uses the kernel's input layer (evdev/uinput), which needs root. To keep that
power contained, it's split in three:

| Program | Runs as | Does |
|---|---|---|
| `ks-inputd` | root (system service) | Reads keyboards, matches hotkeys, remaps and hotstrings, types text through a virtual keyboard. |
| `keysmith` | you | Loads your config and scripts, sends their rules to `ks-inputd`, runs callbacks. |
| `ks-ui` | you | Script manager window and tray icon. |

- **Raw keystrokes never leave `ks-inputd`.** The engine only hears *which
  rule fired*, never what you typed. Hotstrings are matched and expanded inside
  `ks-inputd` too.
- Hotkeys on plain text keys (`a`, `shift+a`, `space`…) are refused, and
  hotstring triggers need at least 3 characters, so a client can't rebuild your
  typing from match events.
- `ks-inputd` only accepts the user who owns the active seat (via logind),
  rechecked on every match, and runs under a tight systemd sandbox (no network,
  no capabilities, input devices only).
- Scripts run in a Luau sandbox: no file or network access, no `os.execute`,
  16 MB memory and 1 s of CPU per callback. `run()` needs an explicit
  capability you approve per script (and again after every edit).

**Emergency exit:** hold **Backspace + Esc + Enter** to stop `ks-inputd` and get
your keyboard back unmodified.

## Install

| Distro | |
|---|---|
| Debian / Ubuntu | download the `.deb` from [Releases](https://github.com/fagostini/keysmith/releases), `sudo apt install ./keysmith_*.deb` |
| Fedora / openSUSE | download the `.rpm` from Releases, `sudo dnf install ./keysmith-*.rpm` |
| Arch | `packaging/aur/PKGBUILD` (`makepkg -si`) |
| From source | see below |

Then **log out and back in**: the engine and tray start automatically.
On GNOME, also run `gnome-extensions enable keysmith@fagostini.github.io` so
scripts can see the focused window.

### From source

Needs Rust ≥ 1.88, a C++ compiler (for Luau), and the development files for libxkbcommon and GTK 4
(Debian/Ubuntu: `g++ libxkbcommon-dev libgtk-4-dev`; Fedora: `gcc-c++ libxkbcommon-devel gtk4-devel`).

```sh
cargo build --release --features ui
sudo ./install.sh            # installs to /usr/local, enables ks-inputd
```

Without `--features ui` you get `ks-inputd` and `keysmith` only (no GTK needed).

## Configure

Everything lives in `~/.config/keysmith/`. Examples are installed in
`/usr/share/doc/keysmith/examples/`.

### `hotkeys.conf`: quick rules, no scripting

```ini
ctrl+alt+t = kgx                        # hotkey = shell command
remap capslock = tap:esc hold:leftctrl  # or: remap pause = playpause
taphold_ms = 200
:btw = by the way                       # hotstring, fires on the following space
:*;sig = Best regards,\n                # `*` fires immediately; \n = Enter
xkb_layout = de                         # optional; detected from the session otherwise
```

Key names are evdev names without `KEY_`: `a`, `f5`, `enter`, `leftctrl`,
`volumeup`, `playpause`… Modifiers: `ctrl`, `alt`, `shift`, `super`.

### `scripts/*.luau`

| Function | |
|---|---|
| `hotkey(combo, fn)` | Call `fn` when `combo` is pressed. |
| `remap(key, "key")` / `remap(key, {tap=…, hold=…})` | Remap a key. |
| `hotstring(trigger, "text" \| fn, {immediate=true})` | Text expansion, or call `fn` (the trigger is erased). |
| `send(text)` | Type text, on any layout. Characters your layout lacks go through the clipboard (needs `xclip` or `wl-clipboard`). |
| `key(combo)` | Press a key or shortcut: `key("ctrl+s")`, `key("enter")`, `key("volumeup")`. |
| `run(cmd)` | Run a shell command. Needs `--! capabilities: run` in the script header and your approval. |
| `sleep(ms)`, `notify(msg)`, `print(…)` | |
| `window.class`, `window.title` | The focused window (X11 class or Wayland app id). |

`hotkey`, `remap` and `hotstring` are only allowed at the script's top level.
Errors appear in the manager's Log tab and as notifications.

Approve a script that needs capabilities in the manager (**Allow…**) or with
`keysmith trust ~/.config/keysmith/scripts/NAME.luau`.

### Recording macros

Press **Ctrl+Alt+Shift+R**, type, and press it again. Keysmith saves what you
typed as `scripts/recorded-<time>.luau`, **disabled**, bound to
`ctrl+alt+shift+p`: edit the hotkey, then enable it in the manager.

This is the one time key events leave `ks-inputd`, so the session is tightly
limited:
- Only a physical key press starts it. No program can start it for you.
- The tray icon turns into a record icon while it runs.
- It stops at the latest after 60 s, after 2000 keys, or when you switch apps.

Don't record passwords: the script stores what you typed in plain text.

### Converting AutoHotkey scripts

```sh
keysmith convert my.ahk > ~/.config/keysmith/scripts/my.luau
```

Handles both v1 and v2 syntax:
- hotkeys, remaps and hotstrings;
- `Send` (with `{Keys}` and `^!+#`), `SendText`, `Sleep`, `Run`, `MsgBox`;
- `#HotIf WinActive(...)`, `#IfWinActive` and `if WinActive(...)`.

Anything else becomes a `-- TODO(ahk):` line to finish by hand.
Windows program names (`ahk_exe firefox.exe`) become a window class check.
Confirm the real class with `keysmith windows`.
[`examples/ahk/top20.ahk`](examples/ahk/top20.ahk) shows what converts.

### Manager

`ks-ui` (or *Keysmith* in your app menu) lists scripts with on/off switches,
errors, permission prompts and the live engine log. Reload after editing
scripts. The tray icon needs StatusNotifierItem support (on GNOME: the
AppIndicator extension).

## Troubleshooting

- `keysmith windows` prints every focus change: checks the window backend.
- `journalctl -u ks-inputd` shows the input service's log.
- Engine log: the manager's Log tab, or `$XDG_RUNTIME_DIR/keysmith/engine.log`.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
