-- bench/maps.loom in idiomatic Lua 5.4.
return function()
  local size = 200000
  local m, n = {}, 0
  for i = 0, size - 1 do m[i << 12] = i; n = n + 1 end
  local total = 0
  for i = 0, size - 1 do total = total + m[i << 12] end
  local s, sn = {}, 0
  for i = 0, size - 1 do
    local k = "k" .. i
    if s[k] == nil then sn = sn + 1 end
    s[k] = i
  end
  local hits = 0
  for i = 0, size - 1 do
    if s["k" .. (i * 2)] ~= nil then hits = hits + 1 end
  end
  io.write(string.format("%d %d %d %d\n", n, total, sn, hits))
end
