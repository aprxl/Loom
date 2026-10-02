-- bench/chip8.loom in idiomatic Lua 5.4.
return function()
  local program = { 0x6000, 0x6101, 0x8014, 0x7105, 0xF01E, 0x3000, 0x1204, 0x7201, 0x1204 }
  local mem, v = {}, {}
  for a = 0, 4095 do mem[a] = 0 end
  for r = 0, 15 do v[r] = 0 end
  local i, pc = 0, 0x200
  for k, op in ipairs(program) do
    local at = 0x200 + 2 * (k - 1)
    mem[at] = op >> 8
    mem[at + 1] = op & 0xFF
  end
  for _ = 1, 3000000 do
    local op = (mem[pc] << 8) | mem[(pc + 1) & 0xFFF]
    pc = (pc + 2) & 0xFFF
    local hi = op >> 12
    local x, y, kk, nnn = (op >> 8) & 0xF, (op >> 4) & 0xF, op & 0xFF, op & 0xFFF
    if hi == 0x1 then
      pc = nnn
    elseif hi == 0x3 then
      if v[x] == kk then pc = (pc + 2) & 0xFFF end
    elseif hi == 0x6 then
      v[x] = kk
    elseif hi == 0x7 then
      v[x] = (v[x] + kk) & 0xFF
    elseif hi == 0x8 and op & 0xF == 4 then
      local sum = v[x] + v[y]
      v[x] = sum & 0xFF
      v[0xF] = sum >> 8
    elseif hi == 0xF and kk == 0x1E then
      i = (i + v[x]) & 0xFFF
    end
  end
  io.write(string.format("%d %d %d %d %d %d\n", v[0], v[1], v[2], v[0xF], i, pc))
end
