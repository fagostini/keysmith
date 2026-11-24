-- A typing macro with pauses. Text waits until you release the hotkey's modifiers.
hotkey("super+f7", function()
    send("Hello")
    sleep(300)
    send(", world!")
end)
