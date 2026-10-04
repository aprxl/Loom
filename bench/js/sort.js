// bench/sort.loom in idiomatic JavaScript, for comparison (bench/README.md).
const size = 300_000;
const people = [];
let seed = 12345n;
for (let i = 0; i < size; i++) {
  seed = BigInt.asUintN(64, seed * 6364136223846793005n + 1442695040888963407n);
  people.push({ age: Number((seed >> 33n) % 100n), id: i });
}
people.sort((a, b) => a.age - b.age || a.id - b.id);
const show = (p) => `Person(age: ${p.age}, id: ${p.id})`;
console.log(show(people[0]), show(people[size / 2]), show(people[size - 1]));
