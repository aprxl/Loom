# Results

Wall clock, best of five, compilation and start-up included (about 10 ms for
each way of running). Lower is better.

## V3, after the run-time review (2026-10-11)

Same machine, best of five (`python bench/run.py --runs 5`). The changes of
design/vm-review-v3.md: LTO, one codegen unit and no unwinding, the
dispatch loop's `pc` in a register (a closure had taken its address), a
fused increment-and-branch for counted loops, the heap's free objects in
lists through themselves, values formatted straight into their sink, and
`sqrt`/`abs` as ops. Tried and dropped, for no gain against a ±2% noise:
natives returning 8 bytes, `ticks` as a local, casts without an extension
word, `target-cpu=native` (1.8% slower), and a branchy ordering of record
fields (fewer instructions, the same time: the comparisons mispredict).

| Benchmark | PUC Lua 5.4, idiomatic | Treadle | Treadle vs Lua 5.4 |
|---|---|---|---|
| `fib` | 0.086 s | **0.040 s** | 2.14× |
| `sieve` | 0.358 s | **0.121 s** | 2.96× |
| `mandelbrot` | 0.135 s | **0.065 s** | 2.08× |
| `nbody` | 0.534 s | **0.287 s** | 1.86× |
| `spectral_norm` | 0.156 s | **0.063 s** | 2.47× |
| `fannkuch` | 1.475 s | **0.752 s** | 1.96× |
| `binary_trees` | 2.723 s | **0.640 s** | 4.26× |
| `richards` | 0.180 s | **0.059 s** | 3.06× |
| `word_freq` | 0.239 s | **0.214 s** | 1.12× |
| `maps` | 0.125 s | **0.085 s** | 1.48× |
| `sort` | 0.317 s | **0.102 s** | 3.10× |
| `streams` | 0.919 s | **0.076 s** | 12.15× |
| `chip8` | 0.109 s | **0.058 s** | 1.89× |
| `lexer` | 1.474 s | **0.195 s** | 7.57× |

Geometric mean 2.74×, lowest 1.12× (`word_freq`): **V3's speed gate (2.5×)
is met.** Against the build before the review, interleaved best-of-5 A/Bs
put the geomean at +11.9%.

## V3 (2026-10-10)

The optimizer (design/vm.md §7). Same machine and build as V1 and V2, best
of five (`python bench/run.py --runs 5`), start-up and compilation included
(about 8 ms for an empty program on Treadle, against 5 ms for Lua: the
process itself, then in a cold start about 1.6 ms reading and resolving
`std`, 0.2 ms type checking and 0.9 ms compiling; making the VM and running
it is 30 µs; `-O2` is the default).

| Benchmark | PUC Lua 5.4, idiomatic | Treadle | Treadle vs Lua 5.4 |
|---|---|---|---|
| `fib` | 0.084 s | **0.038 s** | 2.20× |
| `sieve` | 0.332 s | **0.134 s** | 2.48× |
| `mandelbrot` | 0.115 s | **0.067 s** | 1.72× |
| `nbody` | 0.515 s | **0.309 s** | 1.67× |
| `spectral_norm` | 0.148 s | **0.070 s** | 2.12× |
| `fannkuch` | 1.445 s | **0.898 s** | 1.61× |
| `binary_trees` | 2.686 s | **0.716 s** | 3.75× |
| `richards` | 0.179 s | **0.056 s** | 3.19× |
| `word_freq` | 0.233 s | **0.235 s** | 0.99× |
| `maps` | 0.118 s | **0.107 s** | 1.11× |
| `sort` | 0.298 s | **0.107 s** | 2.79× |
| `streams` | 0.923 s | **0.081 s** | 11.44× |
| `chip8` | 0.097 s | **0.064 s** | 1.52× |
| `lexer` | 1.478 s | **0.217 s** | 6.81× |

Geometric mean 2.42×, lowest 0.99× (`word_freq`). **V3's speed gate (2.5×)
is not met**: runs vary by about 5% and the geomean sat at 2.40–2.43 through
the last changes. What's left is mostly not the optimizer's: `word_freq`
(0.99×) and `maps` (1.11×) spend their time in string building, hashing and
the map's natives (`calln`, 2 a trip), and a program pays about 3 ms before
it runs (reading `std` and compiling, cold), which is 3–8% of the ones that
take under 100 ms; but cutting it from the front end lowers `-O0` and `-O2`
alike, and so raises the compile-time ratio above. Those are V8's (design/vm.md §17).

The other gates of §17's V3 passing list:

| | Gate | Now |
|---|---|---|
| 4. checks gone at `-O2` | 80% of bounds, 60% of overflow | **87.4%** of 721 048 bounds checks, **62.4%** of 1 434 750 overflow checks, over `bench/` at `counts.txt`'s sizes (`check_counts.rs`) |
| 6. compile time at `-O2` | at most 1.5× `-O0`, no example over 200 ms | **1.48×** from source to image over `tests/run` and `examples` (`compile_at` alone, the front end left out: 1.7×); slowest example 61 ms; the benchmarks compile in 0.8 to 7 ms (`cargo test --release -p loom --test compile_time -- --ignored --nocapture`) |

The checks left at `-O2` (about 91 000 bounds checks): fannkuch's swap loop
(35 610 of its 39 230: `perm[j]` is checked on every trip, and only the
first trip's check is needed; proving the rest takes peeling the loop,
which is deferred for the code it grows; the other 3 620 are
`count[r - 1]`), chip8's `memory[addr]` (30 018: the length isn't a
constant), and richards' task table (20 000: the indexes are read from
data).

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

## Other languages (2026-10-04, during V3)

`python bench/compare.py --runs 3`: the best of 3, start-up included, on
the machine of the V0 table. Each language runs an idiomatic version of
each program (`bench/lua`, `bench/luajit` for the two that need 64-bit
integers, `bench/py`, `bench/js`), which prints exactly what the Loom
program does. CPython 3.14.7, LuaJIT 2.1.1788460057 and Bun 1.4.2, from
Scoop's main bucket. LuaJIT and Bun compile to machine code; Treadle and
the others interpret.

| Benchmark | Treadle | PUC Lua 5.4 | LuaJIT | CPython | Bun |
|---|---|---|---|---|---|
| `fib` | 0.038 s | 0.087 s | 0.025 s | 0.194 s | 0.032 s |
| `sieve` | 0.139 s | 0.339 s | 0.083 s | 1.141 s | 0.047 s |
| `mandelbrot` | 0.069 s | 0.133 s | 0.028 s | 0.486 s | 0.095 s |
| `nbody` | 0.321 s | 0.548 s | 0.055 s | 1.368 s | 0.060 s |
| `spectral_norm` | 0.071 s | 0.153 s | 0.021 s | 0.647 s | 0.032 s |
| `fannkuch` | 0.983 s | 1.487 s | 0.319 s | 10.169 s | 0.319 s |
| `binary_trees` | 0.731 s | 2.683 s | 0.461 s | 2.675 s | 0.363 s |
| `richards` | 0.057 s | 0.178 s | 0.063 s | 0.503 s | 0.041 s |
| `word_freq` | 0.249 s | 0.250 s | 0.222 s | 1.606 s | 0.639 s |
| `maps` | 0.111 s | 0.123 s | 0.067 s | 0.141 s | 0.097 s |
| `sort` | 0.108 s | 0.305 s | 0.214 s | 0.391 s | 0.090 s |
| `streams` | 0.082 s | 0.909 s | 0.374 s | 0.736 s | 0.233 s |
| `chip8` | 0.066 s | 0.109 s | 0.030 s | 1.125 s | 0.030 s |
| `lexer` | 0.218 s | 1.514 s | 0.079 s | 0.187 s | 0.056 s |

Geometric mean of each one's time over Treadle's: PUC Lua 5.4 2.44x,
CPython 5.33x, LuaJIT 0.63x, Bun 0.65x. Treadle beats both JITs on
`streams` and `sort`, LuaJIT on `richards`, and Bun on `mandelbrot` and
`word_freq`.

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

## V4 baseline: compile-time code on the Lua sandbox (2026-10-11)

Taken before V4 moved compile-time code onto Treadle (design/vm.md §17), so
its item 5 ("compile at least as fast") has something to compare against.
`lower::compile_time` alone, best of 5, release build, same machine as
above: `cargo test --release -p loom --test compile_time -- --ignored
--nocapture const_code`.

125 files (`tests/run`, `examples`, and `tests/fail`'s const and endless
fixtures that type-check): **495.19 ms in all.**

| File | Time |
|---|---|
| `tests/fail/const_budget.loom` | 345.14 ms (the budget runs out, K0102) |
| `tests/run/const_eval.loom` | 115.18 ms |
| `tests/run/long_expressions.loom` | 2.79 ms |
| `examples/lexer.loom` | 2.45 ms |
| `examples/chip8.loom` | 1.92 ms |
| `tests/run/const_params.loom` | 1.57 ms |
| `tests/fail/const_eval.loom` | 1.58 ms |

Everything else is under 2 ms: starting the sandbox's Lua VM and loading
`RT_CORE` is most of that.

## V4 step 3: compile-time code on Treadle (2026-10-11)

Same measurement as the baseline above (`cargo test --release -p loom --test
compile_time -- --ignored --nocapture const_code`), with compile-time code
run as jobs of one `-O0` image in an in-process Treadle VM.

125 files: **238.72 ms in all**, against 495.19 ms on the Lua sandbox.

| File | Lua sandbox | Treadle |
|---|---|---|
| `tests/fail/const_budget.loom` (the budget runs out, K0102) | 345.14 ms | 231.05 ms |
| `tests/run/const_eval.loom` | 115.18 ms | 3.92 ms |
| `examples/lexer.loom` | 2.45 ms | 1.58 ms |

Everything else is under 0.4 ms: there's no Lua VM to start, and programs
without compile-time code build nothing for it.

- **The budget.** 100 M ticks (loop iterations and calls), 1 GiB, 20 s, as
  before. The most any `tests/run` or `examples` file uses is 5.8 M ticks
  (`const_heavy.loom`, which is made to be heavy; `const_eval.loom` uses
  0.66 M): at least 17× headroom. `LOOM_CONST_STATS=1 loom check f.loom`
  prints a file's. The budget fixture takes 231 ms to spend it: about 2.3 ns a
  tick at `-O0`.
- **`-O0` or `-O2` for the job image.** `loom check` of `const_budget`,
  `const_eval`, `chip8` and `const_params`: 242/241, 16/19, 41/41 and 11/11 ms
  at `-O0`/`-O2`. The images are tiny, so `-O0`.
- **Against the old build as an oracle** (the Lua-sandbox `loom` at the
  baseline commit), the full text of every diagnostic over `tests/fail`, the
  compile-time doc samples, `tests/run/const*` and `examples` is the same but
  for: a note naming the instruction the budget stopped at (new); the K0104
  note's location being the call in the function rather than the constant's
  line; `read_text` showing as `std.fs.read` (one native serves both); and
  the old build *panicking* when the budget ran out (an mlua hook error in
  the release build).

## V4 step 5: baking (2026-10-11)

What compile-time code computed is data in the image (with relocations)
and a literal in the Lua bundle.

- **Compile time.** Reading values back and running `ConstExit` (with a leak
  check under the checked build) on top of step 3: 131 files, 254.29 ms in
  all (const_eval.loom 4.39 ms). The Lua sandbox was 495 ms.
- **Run time** (pre-V4 build against this one, best of 9, wall clock with
  compilation): `word_freq` 0.210 s to 0.200 s, `chip8` 3% faster, and
  nothing else moved beyond noise (geometric mean 0.995 over the suite).
  Dispatch counts (`bench/counts.txt`): `word_freq` 9.3% fewer
  instructions, `chip8` 26 fewer, `spectral_norm` 2.
- **`richards` is layout-sensitive.** Its instruction stream and its
  dispatch count are the same before and after, yet clean builds of
  commits that differ only in the compiler's code run it 0.838 s, 0.881 s
  and 0.929 s (at 20x its size). Two experiments (pages of the VM's heap
  aligned to 4 KiB; the program's data aligned to a cache line) moved it
  by -8% and +8% with no consistent cause, and were dropped. It's the
  indirect-call dispatch through `any`'s vtable meeting where the binary's
  code and data land; V8's dispatch experiments (design/vm.md D2) are
  where that gets looked at properly.
- **The generator** (20 000 seeds, each with constants and the functions
  that build the same values) agrees but for one seed, which is a
  pre-existing disagreement about dropping a generator that holds another
  (it reproduces on the pre-V4 build).

## The Lua backend, after the V4 review (2026-10-11)

`loom run` (the default backend), best of 3, against the same programs at the
start of the review (design/vm-review-v4.md). Idiomatic Lua and Treadle for
scale.

| Benchmark | Start | Now | Speed-up | Idiomatic Lua | Treadle |
|---|---|---|---|---|---|
| `fib` | 0.564 s | 0.262 s | 2.15× | 0.086 s | 0.036 s |
| `sieve` | 2.111 s | 0.816 s | 2.59× | 0.345 s | 0.118 s |
| `mandelbrot` | 0.877 s | 0.253 s | 3.47× | 0.119 s | 0.063 s |
| `nbody` | 2.269 s | 1.498 s | 1.51× | 0.520 s | 0.280 s |
| `spectral_norm` | 2.367 s | 1.040 s | 2.28× | 0.148 s | 0.061 s |
| `fannkuch` | 14.483 s | 5.823 s | 2.49× | 1.454 s | 0.807 s |
| `richards` | 2.603 s | 1.296 s | 2.01× | 0.182 s | 0.053 s |
| `sort` | 1.135 s | 0.564 s | 2.01× | 0.328 s | 0.099 s |
| `maps` | 0.369 s | 0.254 s | 1.46× | 0.121 s | 0.080 s |
| `word_freq` | 2.965 s | 1.271 s | 2.33× | 0.241 s | 0.204 s |
| `chip8` | 0.745 s | 0.395 s | 1.89× | 0.098 s | 0.053 s |

Geometric mean **2.14×**. Generated Lua is still 2 to 6 times idiomatic
Lua; what's left is mostly bounds checks and loads an optimizer would
remove (§3 of the review).

Treadle against the build before the review's natives: a text-heavy
program (split, replace, upper-casing, lines and chars over 4 MB) 0.452 s
to 0.286 s; the benchmark suite within noise.

## V5: several workers

`bench/par_mandelbrot.loom` (800 rows, one `loom for` iteration each), best of
3, release build, 16 logical cores (8 physical), `LOOM_THREADS=n`:

| workers | seconds | speedup |
|--------:|--------:|--------:|
| 1 | 0.444 | 1.0× |
| 2 | 0.236 | 1.9× |
| 4 | 0.124 | 3.6× |
| 8 | 0.070 | 6.3× (0.79 per worker) |
| 16 | 0.052 | 8.5× |

`bench/par_spawn.loom` (200 000 empty iterations: a spawn and a join each):
0.35 µs each on one worker, 0.8 µs on 2, 1.2 µs on 4, 2.7 µs on 8 (every
spawn and every wake goes through the pool's one lock). That misses V5's
gate of 2 µs on 8 workers; per-worker deques are the next step if it
matters, and `loom for` doesn't chunk its iterations yet, which is the
cheaper fix for small items (see `Range.chunks`). The single-thread benchmarks are unchanged
by the pool (A/B against the commit before it, within 2%).

### V5 close-out baseline (2026-10-17)

`python bench/run.py --par --runs 5`, release build, 16 logical cores (8
physical). `par_spawn_join` spawns and joins explicit threads, a thousand at
a time, so it keeps measuring spawn and join once `loom for` chunks.

| Benchmark | workers | seconds | speedup | per spawn+join |
|---|--:|--:|--:|--:|
| `par_mandelbrot` | 1 | 0.461 | 1.00x |  |
| `par_mandelbrot` | 2 | 0.242 | 1.91x |  |
| `par_mandelbrot` | 4 | 0.130 | 3.56x |  |
| `par_mandelbrot` | 8 | 0.076 | 6.04x |  |
| `par_mandelbrot` | 16 | 0.056 | 8.18x |  |
| `par_spawn` | 1 | 0.077 | 1.00x | 0.39 us |
| `par_spawn` | 2 | 0.172 | 0.45x | 0.86 us |
| `par_spawn` | 4 | 0.251 | 0.31x | 1.25 us |
| `par_spawn` | 8 | 0.557 | 0.14x | 2.78 us |
| `par_spawn` | 16 | 0.638 | 0.12x | 3.19 us |
| `par_spawn_join` | 1 | 0.062 | 1.00x | 0.31 us |
| `par_spawn_join` | 2 | 0.144 | 0.43x | 0.72 us |
| `par_spawn_join` | 4 | 0.219 | 0.28x | 1.10 us |
| `par_spawn_join` | 8 | 0.455 | 0.14x | 2.28 us |
| `par_spawn_join` | 16 | 0.450 | 0.14x | 2.25 us |
