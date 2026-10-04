// bench/chip8.loom in idiomatic JavaScript, for comparison (bench/README.md).
const program = [0x6000, 0x6101, 0x8014, 0x7105, 0xF01E, 0x3000, 0x1204, 0x7201, 0x1204];
const mem = new Uint8Array(4096);
const v = new Uint8Array(16);
let i = 0, pc = 0x200;
program.forEach((op, k) => {
  mem[0x200 + 2 * k] = op >> 8;
  mem[0x200 + 2 * k + 1] = op & 0xFF;
});
for (let step = 0; step < 3_000_000; step++) {
  const op = (mem[pc] << 8) | mem[(pc + 1) & 0xFFF];
  pc = (pc + 2) & 0xFFF;
  const x = (op >> 8) & 0xF, y = (op >> 4) & 0xF, kk = op & 0xFF, nnn = op & 0xFFF;
  switch (op >> 12) {
    case 0x1: pc = nnn; break;
    case 0x3: if (v[x] === kk) pc = (pc + 2) & 0xFFF; break;
    case 0x6: v[x] = kk; break;
    case 0x7: v[x] = v[x] + kk; break;
    case 0x8:
      if ((op & 0xF) === 4) {
        const sum = v[x] + v[y];
        v[x] = sum;
        v[0xF] = sum >> 8;
      }
      break;
    case 0xF: if (kk === 0x1E) i = (i + v[x]) & 0xFFF; break;
  }
}
console.log(v[0], v[1], v[2], v[0xF], i, pc);
