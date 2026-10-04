# bench/chip8.loom in idiomatic Python, for comparison (bench/README.md).
program = [0x6000, 0x6101, 0x8014, 0x7105, 0xF01E, 0x3000, 0x1204, 0x7201, 0x1204]
mem = bytearray(4096)
v = [0] * 16
i, pc = 0, 0x200
for k, op in enumerate(program):
    at = 0x200 + 2 * k
    mem[at] = op >> 8
    mem[at + 1] = op & 0xFF
for _ in range(3_000_000):
    op = (mem[pc] << 8) | mem[(pc + 1) & 0xFFF]
    pc = (pc + 2) & 0xFFF
    hi = op >> 12
    x, y, kk, nnn = (op >> 8) & 0xF, (op >> 4) & 0xF, op & 0xFF, op & 0xFFF
    if hi == 0x1:
        pc = nnn
    elif hi == 0x3:
        if v[x] == kk:
            pc = (pc + 2) & 0xFFF
    elif hi == 0x6:
        v[x] = kk
    elif hi == 0x7:
        v[x] = (v[x] + kk) & 0xFF
    elif hi == 0x8 and op & 0xF == 4:
        s = v[x] + v[y]
        v[x] = s & 0xFF
        v[0xF] = s >> 8
    elif hi == 0xF and kk == 0x1E:
        i = (i + v[x]) & 0xFFF
print(v[0], v[1], v[2], v[0xF], i, pc)
