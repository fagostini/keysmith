--! capabilities: run
hotkey("super+f11", function() run("pactl set-sink-volume @DEFAULT_SINK@ -5%") end)
hotkey("super+f12", function() run("pactl set-sink-volume @DEFAULT_SINK@ +5%") end)
