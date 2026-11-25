# Open items

State as of v0.2.0 (2026-11-25). Milestones M1–M8 of the design plan are built;
these are the checks and steps still outstanding.

## Verification still to do

- [ ] **M5: window backends on real sessions.** Only X11 (Cinnamon) is verified.
      In a VM each, check that `keysmith windows` prints focus changes and that
      `examples/scripts/22-per-app.luau` (super+F9) reports the right class:
  - [ ] Sway (i3 IPC)
  - [ ] Hyprland (socket2)
  - [ ] another wlroots compositor (wlr-foreign-toplevel), e.g. labwc or river
  - [ ] KDE Plasma 6 (KWin script + D-Bus)
  - [ ] GNOME (Shell extension `keysmith@fagostini.github.io`)
- [ ] **M2: 1-hour typing test** with the Caps tap/hold remap on: no stuck
      keys, no lost input.
- [ ] **M3: non-US layout, live.** Hotstrings and `send()` on DE, FR or IT
      (the round trip is unit-tested; real typing isn't).
- [ ] **Packaged install on a real machine.** Install the `.deb`, log out and
      in. Check that `systemctl status ks-inputd` is active, that the engine
      and tray start via XDG autostart, and that super+F5 works. Containers
      can't run systemd or a session, so this has only been checked by hand-run
      binaries.

## Release steps

- [ ] **Submit the PKGBUILD to the AUR** (`packaging/aur/`). Needs the
      maintainer's AUR account and SSH key.
- [ ] Man pages for `keysmith`, `ks-inputd` and `ks-ui` (rpmlint warns about
      their absence).

## Known limitations (deliberate shortcuts, marked `ponytail:` in the code)

- Only xkb layout group 0: with `us,gb`, text is typed as `us`. Dead keys and
  compose aren't decoded for hotstring triggers (`src/xkb.rs`).
- The layout is read once at engine start, and GNOME/KDE layout settings on
  Wayland aren't read; set `xkb_layout` in `hotkeys.conf` there
  (`src/bin/keysmith/main.rs`).
- Clipboard fallback shells out to `wl-copy`/`xclip`, text only.
- Keyboards plugged in after `ks-inputd` starts are ignored until a restart;
  no udev monitor yet (`src/bin/ks-inputd.rs`).
- Any process in the session can call the D-Bus `Focus` method and spoof the
  focused window for scripts (`src/bin/keysmith/window.rs`).
- The trust file uses `DefaultHasher`, so a Rust toolchain upgrade can
  invalidate approvals; re-approve with `keysmith trust`
  (`src/bin/keysmith/main.rs`).
- Synthetic keys go out in one burst with no delay; add a 1 ms gap if an app
  drops keys.
- The AHK converter is line-based: variables, functions, loops and GUI code
  become `TODO(ahk)` lines.
- The terminal list for Ctrl+Shift+V paste is a fixed list of names.
- Running the engine by hand in a terminal: a replayed or typed Ctrl+S
  reaching that terminal freezes it (XOFF) and the engine blocks on logging.
  Ctrl+Q resumes. Doesn't affect the autostart engine.

## Later (not planned yet, from the design doc)

- Window actions: activate, list, move, resize, where compositors allow.
- Pixel search and screen OCR through the screenshot portal.
- Per-device rules (a macro pad separate from the main keyboard).
- Mouse in the recorder (Wayland gives no absolute positions).
- Password-field detection for hotstrings (AT-SPI).
- RemoteDesktop portal backend and COSMIC support (left out of v1 by decision).
