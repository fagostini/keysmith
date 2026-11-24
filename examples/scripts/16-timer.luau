-- sleep() blocks only this script; the 1 s CPU budget excludes it.
hotkey("super+f6", function()
    notify("timer started (5 s)")
    sleep(5000)
    notify("timer done")
end)
