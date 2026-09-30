# Results

Wall clock, best of five, compilation and start-up included (about 10 ms for
each way of running). Lower is better.

## V0 (2026-09-29)

AMD Ryzen 7 9800X3D, Windows 11, Rust 1.98.0, release build. PUC Lua 5.4 is
the one bundled in `loom` (mlua, vendored, built with clang-cl).

| Benchmark | Lua backend | PUC Lua 5.4, idiomatic | Treadle | Treadle vs Lua backend | Treadle vs Lua 5.4 |
|---|---|---|---|---|---|
| `fib` (32) | 0.604 s | 0.091 s | **0.041 s** | 14.7× | 2.2× |
| `mandelbrot` (400) | 0.912 s | 0.138 s | **0.088 s** | 10.4× | 1.6× |

V0's gate (design/vm.md §16) is 2× faster than the Lua backend on both: met.
No optimizer runs yet (that's V3); what V0 has is typed instructions, a
register window for calls, compare-and-branch, immediate operands, and
register hints.

Treadle's instructions per iteration of mandelbrot's inner loop: 18, of
which the optimizer should remove about a third (shared `x * x` and `y * y`,
constants hoisted out of the loop, the overflow check on `i += 1`).
