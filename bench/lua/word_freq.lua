-- bench/word_freq.loom in idiomatic Lua 5.4.
return function()
  local syllables = { "ka", "lo", "mi", "ne", "su", "ta", "ro", "vi" }
  local counts, n = {}, 0
  local seed = 42
  for _ = 1, 4000000 do
    seed = seed * 6364136223846793005 + 1442695040888963407
    local a = syllables[((seed >> 40) % 8) + 1]
    local b = syllables[((seed >> 50) % 8) + 1]
    local word
    if (seed >> 60) % 2 == 0 then word = a .. b else word = a .. b .. a end
    local c = counts[word]
    if c then counts[word] = c + 1 else counts[word] = 1; n = n + 1 end
  end
  local top = {}
  for w, c in pairs(counts) do top[#top + 1] = { w, c } end
  table.sort(top, function(x, y) return x[1] < y[1] end)
  io.write(string.format("%d words, first (%s, %d), last (%s, %d)\n", n, top[1][1], top[1][2], top[#top][1], top[#top][2]))
end
