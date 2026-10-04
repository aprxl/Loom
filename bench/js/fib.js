// bench/fib.loom in idiomatic JavaScript, for comparison (bench/README.md).
const fib = (n) => (n < 2 ? n : fib(n - 1) + fib(n - 2));
console.log(fib(32));
