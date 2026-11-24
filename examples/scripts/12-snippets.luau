-- Register many hotstrings from a table.
local snippets = {
    omw = "on my way",
    thx = "thank you",
    afaik = "as far as I know",
    imho = "in my humble opinion",
}
for trigger, text in snippets do
    hotstring(trigger, text)
end
