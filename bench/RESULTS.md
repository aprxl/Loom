# Results

Wall clock, best of five, compilation and start-up included (about 10 ms for
each way of running). Lower is better.

## V1 (2026-10-02)

Same machine and build. Treadle against PUC Lua 5.4 running the idiomatic
Lua of each benchmark (`bench/lua/`), and the Lua backend running the same
Loom (`python bench/run.py --runs 5 --lua-backend`).

| Benchmark | Lua backend | PUC Lua 5.4, idiomatic | Treadle | Treadle vs Lua 5.4 |
|---|---|---|---|---|
| `fib` | 0.568 s | 0.083 s | **0.037 s** | 2.27× |
| `sieve` | 2.003 s | 0.336 s | **0.155 s** | 2.16× |
| `mandelbrot` | 0.877 s | 0.128 s | **0.077 s** | 1.68× |
| `nbody` | 2.261 s | 0.538 s | **0.350 s** | 1.54× |
| `spectral_norm` | 2.329 s | 0.146 s | **0.116 s** | 1.26× |
| `fannkuch` | 14.340 s | 1.460 s | **1.172 s** | 1.25× |
| `binary_trees` | 7.771 s | 2.620 s | **0.864 s** | 3.03× |
| `richards` | 2.444 s | 0.180 s | **0.069 s** | 2.62× |
| `word_freq` | 2.883 s | 0.233 s | **0.274 s** | 0.85× |
| `sort` | 0.981 s | 0.298 s | **0.123 s** | 2.42× |
| `streams` | 3.721 s | 0.893 s | **0.795 s** | 1.12× |
| `chip8` | 0.717 s | 0.099 s | **0.089 s** | 1.10× |

Geometric mean 1.65×, lowest 0.85× (`word_freq`). V1's gate (§16) is a
geomean of at least 1.5× with nothing below 0.8×: met. Runs vary by about
5%. `word_freq` is where Lua's interned strings win: each word is a fresh
`String` on Treadle, and a lookup hashes it. `bench/counts.txt` keeps each
benchmark's instruction count, so a change that grows one is caught.

chip8 plays `examples/digits.ch8` (a 30-byte ROM that draws the font's 16
digits) on Treadle: `loom run --backend vm examples/chip8.loom --
examples/digits.ch8`, the same frames as on Lua, and Ctrl-C restores the
terminal on the way out.

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
