-- bench/sieve.loom in idiomatic Lua 5.4.
return function()
  local size = 10000000
  local composite = {}
  for i = 1, size do composite[i] = false end
  local count = 0
  for i = 2, size do
    if not composite[i] then
      count = count + 1
      for j = i * i, size, i do composite[j] = true end
    end
  end
  io.write(count, "\n")
end
