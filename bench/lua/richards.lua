-- bench/richards.loom in idiomatic Lua 5.4: tasks as objects with methods.
local WORK, DEVICE, IDLE = 1, 2, 3

local Worker = {}
Worker.__index = Worker
function Worker:run(p)
  self.count = self.count + 1
  self.seed = (self.seed * 1103515245 + 12345) & 0xFFFFFFFF
  if p.kind == WORK then
    if p.n > 0 then return { kind = WORK, n = p.n - 1, hops = p.hops + 1 } end
    return { kind = DEVICE, id = self.seed % 4, hops = p.hops + 1 }
  end
  return nil
end
function Worker:done() return self.count end

local Device = {}
Device.__index = Device
function Device:run(p)
  self.handled = self.handled + 1
  if p.hops % 7 == 0 then return { kind = IDLE, hops = 0 } end
  return { kind = WORK, n = 3, hops = p.hops + 1 }
end
function Device:done() return self.handled * 10 + self.id end

local Idler = {}
Idler.__index = Idler
function Idler:run(p)
  self.ticks = self.ticks + 1
  return { kind = WORK, n = self.ticks % 5, hops = p.hops }
end
function Idler:done() return self.ticks end

return function()
  local tasks = { setmetatable({ count = 0, seed = 7 }, Worker), setmetatable({ ticks = 0 }, Idler) }
  for id = 0, 3 do tasks[#tasks + 1] = setmetatable({ id = id, handled = 0 }, Device) end
  local queue = {}
  for i = 0, 7 do queue[#queue + 1] = { kind = WORK, n = i, hops = 0 } end
  local head, n = 1, #queue
  for _ = 1, 1000000 do
    local p = queue[head]
    head = head % n + 1
    local target
    if p.kind == WORK then target = 1 elseif p.kind == IDLE then target = 2 else target = 3 + p.id end
    local next = tasks[target]:run(p)
    if next then queue[(head - 2) % n + 1] = next end
  end
  local total = 0
  for _, t in ipairs(tasks) do total = (total + t:done()) & 0xFFFFFFFF end
  io.write(total, "\n")
end
