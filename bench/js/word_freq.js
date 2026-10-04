// bench/word_freq.loom in idiomatic JavaScript, for comparison (bench/README.md).
const SYLLABLES = ["ka", "lo", "mi", "ne", "su", "ta", "ro", "vi"];
const counts = new Map();
let seed = 42n;
for (let i = 0; i < 4_000_000; i++) {
  seed = BigInt.asUintN(64, seed * 6364136223846793005n + 1442695040888963407n);
  const a = SYLLABLES[Number((seed >> 40n) % 8n)];
  const b = SYLLABLES[Number((seed >> 50n) % 8n)];
  const word = (seed >> 60n) % 2n === 0n ? a + b : a + b + a;
  counts.set(word, (counts.get(word) ?? 0) + 1);
}
const top = [...counts].sort(([x], [y]) => (x < y ? -1 : x > y ? 1 : 0));
const [first, last] = [top[0], top[top.length - 1]];
console.log(`${counts.size} words, first (${first[0]}, ${first[1]}), last (${last[0]}, ${last[1]})`);
