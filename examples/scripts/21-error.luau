-- Errors in callbacks are reported (log + notification) and don't stop the script.
hotkey("super+f8", function()
    error("this error is on purpose")
end)
