-- bench/sort.loom in idiomatic Lua 5.4 (a stable sort, as List.sort is).
local function less(a, b)
  if a.age ~= b.age then return a.age < b.age end
  return a.id < b.id
end

return function()
  local size = 300000
  local people = {}
  local seed = 12345
  for i = 0, size - 1 do
    seed = seed * 6364136223846793005 + 1442695040888963407
    people[#people + 1] = { age = (seed >> 33) % 100, id = i }
  end
  -- ids are distinct, so table.sort's order is the stable one.
  table.sort(people, less)
  local function show(p) return string.format("Person(age: %d, id: %d)", p.age, p.id) end
  io.write(show(people[1]), " ", show(people[size // 2 + 1]), " ", show(people[size]), "\n")
end
