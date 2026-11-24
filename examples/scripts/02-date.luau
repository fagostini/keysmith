-- Type today's date as soon as you finish typing ";date".
hotstring(";date", function()
    send(os.date("%Y-%m-%d"))
end, { immediate = true })
