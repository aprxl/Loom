# Results

Wall clock, best of five, compilation and start-up included (about 10 ms for
each way of running). Lower is better.

## V2 (2026-10-03)

Same machine and build as V1, best of five (`python bench/run.py
--lua-backend`), with `lexer` added: a push lexer, a duplex shuttle fed one
char at a time, against Lua running the same lexer in a coroutine.

| Benchmark | Lua backend | PUC Lua 5.4, idiomatic | Treadle | Treadle vs Lua 5.4 |
|---|---|---|---|---|
| `fib` | 0.573 s | 0.083 s | **0.037 s** | 2.23× |
| `sieve` | 2.059 s | 0.335 s | **0.155 s** | 2.15× |
| `mandelbrot` | 0.939 s | 0.115 s | **0.082 s** | 1.40× |
| `nbody` | 2.232 s | 0.519 s | **0.360 s** | 1.44× |
| `spectral_norm` | 2.335 s | 0.151 s | **0.116 s** | 1.31× |
| `fannkuch` | 14.424 s | 1.427 s | **1.163 s** | 1.23× |
| `binary_trees` | 7.918 s | 2.662 s | **0.883 s** | 3.02× |
| `richards` | 2.458 s | 0.181 s | **0.072 s** | 2.50× |
| `word_freq` | 2.966 s | 0.229 s | **0.289 s** | 0.79× |
| `maps` | 0.356 s | 0.119 s | **0.112 s** | 1.05× |
| `sort` | 0.969 s | 0.298 s | **0.122 s** | 2.45× |
| `streams` | 3.776 s | 0.902 s | **0.832 s** | 1.08× |
| `chip8` | 0.730 s | 0.097 s | **0.091 s** | 1.06× |
| `lexer` | 3.486 s | 1.464 s | **0.249 s** | 5.87× |

Geometric mean 1.70× over the 14, 1.54× without `lexer`; the rest move
within a run's 5% of the review's numbers. A resume of a held shuttle is
about a call (the `lexer`'s, per char), where Lua switches coroutines.

V2's gate is about generators: `for x in g()` costs at most twice what
`for x in 0..n` does per item, on a trivial body. Summing 50M items: the
range loop 0.33 s; a generator built in place (what `for x in g()` does)
0.21 s; the same generator held in a variable, and so stepped, 1.91 s,
about 38 ns an item. Met, for the loop the gate names; resuming a held
shuttle cheaply is V8's.

## V1 (2026-10-02)

Same machine and build: rustc 1.98.0 (88d9e12ae 2026-08-18), release, with
the `-tail-dup-*` flags of `.cargo/config.toml` (a test checks they're in
effect). The Lua is the vendored 5.4 built with clang-cl, which doesn't
define `__GNUC__`, so its VM dispatches with `switch`, not computed gotos: a
gcc-built `lua5.4` is faster, and the ratios below are against this one.
Treadle against PUC Lua 5.4 running the idiomatic
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

### After the V0/V1 review's fixes (2026-10-03)

Same machine, best of 7 (`python bench/run.py --runs 7`), with `maps` (clustered
integer keys, text keys, removals) added to the set: geometric mean 1.58×
over the 13, lowest 0.82× (`word_freq`). Without `maps`, 1.64× over the
twelve of V1's table. `nbody` and `mandelbrot` read about 5% slower than in
the table above (1.39× and 1.54×); a run varies by about 5% from one
invocation to the next, and the interpreter's speed moves with unrelated
changes (design/vm-review-v0-v1.md, development trap 1), so a fix's cost
can't be told from that without a longer A/B. The pieces the review found
slow are fast now: 200 000 clustered keys 1966 ms to 70 ms, 30 000 removes
4.8 s to 0.17 s, a stack overflow 5.2 s to 0.3 s.

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
