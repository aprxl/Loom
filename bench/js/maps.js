// bench/maps.loom in idiomatic JavaScript, for comparison (bench/README.md).
const size = 200_000;
const m = new Map();
for (let i = 0; i < size; i++) m.set(i * 4096, i);
let total = 0;
for (let i = 0; i < size; i++) total += m.get(i * 4096);
const s = new Map();
for (let i = 0; i < size; i++) s.set(`k${i}`, i);
let hits = 0;
for (let i = 0; i < size; i++) if (s.has(`k${i * 2}`)) hits++;
for (let i = 0; i < size; i++) m.delete(i * 4096);
console.log(m.size, total, s.size, hits);
