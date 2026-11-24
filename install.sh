#!/bin/sh
# Install Keysmith from source: ./install.sh [PREFIX]   (default /usr/local; run as root)
# Packages (.deb, .rpm, AUR) install the same files under /usr.
set -eu
PREFIX=${1:-/usr/local}
cd "$(dirname "$0")"
[ "$(id -u)" = 0 ] || { echo "run as root (sudo $0)" >&2; exit 1; }
[ -x target/release/ks-ui ] || { echo "build first: cargo build --release --features ui" >&2; exit 1; }

install -Dm755 -t "$PREFIX/bin" target/release/ks-inputd target/release/keysmith target/release/ks-ui
sed "s|/usr/bin/ks-inputd|$PREFIX/bin/ks-inputd|" data/systemd/ks-inputd.service > /etc/systemd/system/ks-inputd.service
install -Dm644 -t "$PREFIX/share/applications" data/applications/*.desktop
install -Dm644 -t /etc/xdg/autostart data/autostart/*.desktop
ext=keysmith@fagostini.github.io
install -Dm644 -t "$PREFIX/share/gnome-shell/extensions/$ext" data/gnome-extension/$ext/*
install -Dm644 -t "$PREFIX/share/doc/keysmith/examples" examples/hotkeys.conf
install -Dm644 -t "$PREFIX/share/doc/keysmith/examples/scripts" examples/scripts/*

systemctl daemon-reload
systemctl enable --now ks-inputd.service
echo "Installed. Log out and back in to start the engine and tray (or run: keysmith & ks-ui)."
echo "GNOME: also run 'gnome-extensions enable $ext' as your user for window info."
