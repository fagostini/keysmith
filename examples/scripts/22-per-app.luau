-- Per-window behaviour (M5): one hotkey, different action per app.
-- window.class: X11 WM_CLASS class, or the Wayland app_id (e.g. "firefox", "org.gnome.Ptyxis").
hotkey("super+f9", function()
    if window.class == "firefox" then
        send("Firefox says hi")
    elseif window.class:lower():find("term") then
        send("echo hello from keysmith")
    else
        notify("focused: " .. window.class .. " — " .. window.title)
    end
end)
