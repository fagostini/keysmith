// Reports the focused window's class and title to org.keysmith.Engine on the session bus.
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

export default class KeysmithExtension extends Extension {
    enable() {
        this._win = null;
        this._titleId = 0;
        this._focusId = global.display.connect('notify::focus-window', () => this._focus());
        this._focus();
    }

    disable() {
        global.display.disconnect(this._focusId);
        this._track(null);
    }

    _track(w) {
        if (this._win && this._titleId)
            this._win.disconnect(this._titleId);
        this._win = w;
        this._titleId = w ? w.connect('notify::title', () => this._report()) : 0;
    }

    _focus() {
        this._track(global.display.focus_window);
        this._report();
    }

    _report() {
        const w = this._win;
        const args = new GLib.Variant('(ss)', [w?.get_wm_class() ?? '', w?.get_title() ?? '']);
        // Fire and forget; the engine may not be running.
        Gio.DBus.session.call('org.keysmith.Engine', '/org/keysmith/Engine', 'org.keysmith.Engine', 'Focus',
            args, null, Gio.DBusCallFlags.NO_AUTO_START, 500, null, null);
    }
}
