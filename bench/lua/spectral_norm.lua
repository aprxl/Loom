-- bench/spectral_norm.loom in idiomatic Lua 5.4.
local function a(i, j)
  local ij = i + j - 1
  return 1.0 / (ij * (ij - 1) * 0.5 + i)
end

local function times(v, out)
  local n = #v
  for i = 1, n do
    local sum = 0
    for j = 1, n do sum = sum + a(i, j) * v[j] end
    out[i] = sum
  end
end

local function times_transposed(v, out)
  local n = #v
  for i = 1, n do
    local sum = 0
    for j = 1, n do sum = sum + a(j, i) * v[j] end
    out[i] = sum
  end
end

local function times_both(v, out, tmp)
  times(v, tmp)
  times_transposed(tmp, out)
end

return function()
  local n = 400
  local u, v, tmp = {}, {}, {}
  for i = 1, n do u[i], v[i], tmp[i] = 1, 0, 0 end
  for _ = 1, 10 do
    times_both(u, v, tmp)
    times_both(v, u, tmp)
  end
  local vbv, vv = 0, 0
  for i = 1, n do
    vbv = vbv + u[i] * v[i]
    vv = vv + v[i] * v[i]
  end
  io.write(string.format("%.9f", math.sqrt(vbv / vv)), "\n")
end
