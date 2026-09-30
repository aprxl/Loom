-- bench/mandelbrot.loom in idiomatic Lua 5.4, for comparison (bench/README.md).
local function escape(cx, cy, limit)
  local x, y, i = 0.0, 0.0, 0
  while x * x + y * y <= 4.0 and i < limit do
    x, y = x * x - y * y + cx, 2.0 * x * y + cy
    i = i + 1
  end
  return i
end

return function()
  local size = 400
  local total = 0
  for row = 0, size - 1 do
    local cy = row / size * 2.4 - 1.2
    for col = 0, size - 1 do
      local cx = col / size * 3.5 - 2.5
      total = total + escape(cx, cy, 200)
    end
  end
  io.write(total, "\n")
end
