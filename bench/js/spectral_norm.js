// bench/spectral_norm.loom in idiomatic JavaScript, for comparison (bench/README.md).
const a = (i, j) => 1.0 / (((i + j) * (i + j + 1)) / 2 + i + 1);

function times(v, out) {
  const n = v.length;
  for (let i = 0; i < n; i++) {
    let sum = 0;
    for (let j = 0; j < n; j++) sum += a(i, j) * v[j];
    out[i] = sum;
  }
}

function timesTransposed(v, out) {
  const n = v.length;
  for (let i = 0; i < n; i++) {
    let sum = 0;
    for (let j = 0; j < n; j++) sum += a(j, i) * v[j];
    out[i] = sum;
  }
}

function timesBoth(v, out, tmp) {
  times(v, tmp);
  timesTransposed(tmp, out);
}

const n = 400;
const u = new Float64Array(n).fill(1), v = new Float64Array(n), tmp = new Float64Array(n);
for (let i = 0; i < 10; i++) {
  timesBoth(u, v, tmp);
  timesBoth(v, u, tmp);
}
let vbv = 0, vv = 0;
for (let i = 0; i < n; i++) {
  vbv += u[i] * v[i];
  vv += v[i] * v[i];
}
console.log(Math.sqrt(vbv / vv).toFixed(9));
