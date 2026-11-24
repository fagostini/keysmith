-- Dated TODO marker.
hotstring("todo;", function()
    send("TODO " .. os.date("%Y-%m-%d") .. ": ")
end, { immediate = true })
