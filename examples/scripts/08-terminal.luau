--! capabilities: run
-- run() needs the `run` capability, approved once with `keysmith trust`.
hotkey("super+enter", function()
    run("x-terminal-emulator")
end)
