--! capabilities: run
-- Uses whichever screenshot tool is installed.
hotkey("super+print", function()
    run("gnome-screenshot -i || spectacle || grim ~/screenshot.png")
end)
