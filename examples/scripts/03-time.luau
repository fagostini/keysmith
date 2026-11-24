-- ";time" + space types the current time.
hotstring(";time", function()
    send(os.date("%H:%M") .. " ")
end)
