-- bench/streams.loom in idiomatic Lua 5.4: iterator closures.
local function range(n)
  local i = -1
  return function()
    i = i + 1
    if i < n then return i end
  end
end

local function filter(it, f)
  return function()
    while true do
      local x = it()
      if x == nil or f(x) then return x end
    end
  end
end

local function map(it, f)
  return function()
    local x = it()
    if x ~= nil then return f(x) end
  end
end

local function take_while(it, f)
  local done = false
  return function()
    if done then return nil end
    local x = it()
    if x ~= nil and f(x) then return x end
    done = true
  end
end

local function sum(it)
  local s = 0
  for x in it do s = s + x end
  return s
end

return function()
  local total = 0
  for round = 0, 4 do
    total = total + sum(take_while(map(filter(range(3000000), function(x) return x % 3 ~= 0 end),
      function(x) return x * 2 + round end), function(x) return x < 5000000 end))
  end
  io.write(total, "\n")
end
