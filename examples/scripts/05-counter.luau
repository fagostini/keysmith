-- Script state survives between calls.
local count = 0
hotkey("super+f2", function()
    count += 1
    notify("pressed " .. count .. " times")
end)
