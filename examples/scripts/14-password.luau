-- Type a random 16-character password.
local chars = "abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789"
hotkey("super+f3", function()
    local out = {}
    for i = 1, 16 do
        local n = math.random(#chars)
        out[i] = chars:sub(n, n)
    end
    send(table.concat(out))
end)
