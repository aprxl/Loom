// bench/fannkuch.loom in idiomatic JavaScript, for comparison (bench/README.md).
const n = 10;
const perm1 = Array.from({ length: n }, (_, i) => i);
const perm = new Array(n).fill(0), count = new Array(n).fill(0);
let maxFlips = 0, checksum = 0, permCount = 0, r = n;
outer: for (;;) {
  while (r !== 1) {
    count[r - 1] = r;
    r--;
  }
  for (let i = 0; i < n; i++) perm[i] = perm1[i];
  let flips = 0;
  let k = perm[0];
  while (k !== 0) {
    for (let i = 0, j = k; i < j; i++, j--) [perm[i], perm[j]] = [perm[j], perm[i]];
    flips++;
    k = perm[0];
  }
  maxFlips = Math.max(maxFlips, flips);
  checksum += permCount % 2 === 0 ? flips : -flips;
  for (;;) {
    if (r === n) break outer;
    const first = perm1[0];
    for (let i = 0; i < r; i++) perm1[i] = perm1[i + 1];
    perm1[r] = first;
    count[r]--;
    if (count[r] > 0) break;
    r++;
  }
  permCount++;
}
console.log(checksum);
console.log(`Pfannkuchen(${n}) = ${maxFlips}`);
