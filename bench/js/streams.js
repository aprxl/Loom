// bench/streams.loom in idiomatic JavaScript, for comparison (bench/README.md).
function* range(n) { for (let i = 0; i < n; i++) yield i; }
function* filter(it, f) { for (const x of it) if (f(x)) yield x; }
function* map(it, f) { for (const x of it) yield f(x); }
function* takeWhile(it, f) { for (const x of it) { if (!f(x)) return; yield x; } }
function sum(it) { let s = 0; for (const x of it) s += x; return s; }

let total = 0;
for (let round = 0; round < 5; round++) {
  total += sum(takeWhile(map(filter(range(3_000_000), (x) => x % 3 !== 0), (x) => x * 2 + round), (x) => x < 5_000_000));
}
console.log(total);
