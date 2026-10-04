// bench/sieve.loom in idiomatic JavaScript, for comparison (bench/README.md).
const size = 10_000_000;
const composite = new Uint8Array(size + 1);
let count = 0;
for (let i = 2; i <= size; i++) {
  if (!composite[i]) {
    count++;
    for (let j = i * i; j <= size; j += i) composite[j] = 1;
  }
}
console.log(count);
