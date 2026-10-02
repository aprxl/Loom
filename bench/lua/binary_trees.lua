-- bench/binary_trees.loom in idiomatic Lua 5.4.
local function make(depth)
  if depth == 0 then return {} end
  return { make(depth - 1), make(depth - 1) }
end

local function check(t)
  local n = 1
  for _, k in ipairs(t) do n = n + check(k) end
  return n
end

return function()
  local max_depth = 16
  io.write("stretch tree of depth ", max_depth + 1, "\t check: ", check(make(max_depth + 1)), "\n")
  local long_lived = make(max_depth)
  for depth = 4, max_depth, 2 do
    local iterations = 1 << (max_depth - depth + 4)
    local total = 0
    for _ = 1, iterations do total = total + check(make(depth)) end
    io.write(iterations, "\t trees of depth ", depth, "\t check: ", total, "\n")
  end
  io.write("long lived tree of depth ", max_depth, "\t check: ", check(long_lived), "\n")
end
