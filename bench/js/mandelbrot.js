// bench/mandelbrot.loom in idiomatic JavaScript, for comparison (bench/README.md).
function escape(cx, cy, limit) {
  let x = 0, y = 0, i = 0;
  while (x * x + y * y <= 4.0 && i < limit) {
    [x, y] = [x * x - y * y + cx, 2.0 * x * y + cy];
    i++;
  }
  return i;
}

const size = 400;
let total = 0;
for (let row = 0; row < size; row++) {
  const cy = (row / size) * 2.4 - 1.2;
  for (let col = 0; col < size; col++) total += escape((col / size) * 3.5 - 2.5, cy, 200);
}
console.log(total);
