// bench/binary_trees.loom in idiomatic JavaScript, for comparison (bench/README.md).
const make = (depth) => (depth === 0 ? [] : [make(depth - 1), make(depth - 1)]);
const check = (t) => t.reduce((n, k) => n + check(k), 1);

const maxDepth = 16;
console.log(`stretch tree of depth ${maxDepth + 1}\t check: ${check(make(maxDepth + 1))}`);
const longLived = make(maxDepth);
for (let depth = 4; depth <= maxDepth; depth += 2) {
  const iterations = 1 << (maxDepth - depth + 4);
  let total = 0;
  for (let i = 0; i < iterations; i++) total += check(make(depth));
  console.log(`${iterations}\t trees of depth ${depth}\t check: ${total}`);
}
console.log(`long lived tree of depth ${maxDepth}\t check: ${check(longLived)}`);
