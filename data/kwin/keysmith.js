// Loaded into KWin (Plasma 6) by the keysmith engine over D-Bus.
// Reports the active window's class and title to org.keysmith.Engine.
function report(w) {
    callDBus("org.keysmith.Engine", "/org/keysmith/Engine", "org.keysmith.Engine", "Focus",
             w ? String(w.resourceClass) : "", w ? String(w.caption) : "");
}
let current = null;
function onTitle() { report(current); }
function activated(w) {
    if (current) current.captionChanged.disconnect(onTitle);
    current = w;
    if (w) w.captionChanged.connect(onTitle);
    report(w);
}
workspace.windowActivated.connect(activated);
activated(workspace.activeWindow);
