# Results

Wall clock, best of five, compilation and start-up included (about 10 ms for
each way of running). Lower is better.


## V8: follow-up gvn and sccp run on the functions changed since the last full pass (2026-10-28)

`Module.changed` says which functions a pass changed. `sccp` and `gvn` (the
first of each) run on every function and set it exactly; `sccp_changed` and
`gvn_changed` (the second round, and the one after `ranges`) run only on the
flagged functions and leave them flagged; `inline` adds the functions it
changed; `ranges` sets it exactly and tidies only what it changed. Counts
(`bench_counts`, `std_counts`, `check_counts`) are identical to before. Release,
quiet, two runs each (`compile_time`): source to image ratio 1.27, 1.27 to
1.25, 1.25; `compile_at` alone -O2 551.73 and 552.63 ms to 531.53 and 531.86 ms
(-3.8%), ratio 2.03, 2.04 to 1.98, 1.98.

Per pass, mean per -O2 compile over tests/run, examples and bench (276 files,
`LOOM_PASS_TIMES=1`, whose printing slows the test itself), in us, before and
after: inline 100.9, 101.9; prune 5.6, 5.7; sccp 70.9, 71.4; cells 30.1,
30.1; gvn 193.7, 196.0; sccp (now `sccp_changed`) 54.3, 43.3; inline 37.1,
37.1; prune 1.7, 1.7; gvn (now `gvn_changed`) 174.9, 148.0; sccp 57.1, 47.4;
ipcp 8.7, 8.5; ranges 120.2, 110.9; sccp 44.8, 24.0; licm 48.1, 49.3; cleanup
71.1, 70.6. Total of the passes 1019.2 to 945.9 us (-7.2%). The verify at the
end of `opt::run` is 43 us; the rest of the optimizer's measured time was the
timing prints.

## V8: GVN carries known words across a `copy` (2026-10-28)

A `copy` of a stream header (what moving a stream into its loop does) now
carries what was known of the source's words to the destination, so the step
function's number reaches the indirect call, which `devirtualize` turns into a
direct call that inlining copies in. `std_counts` (dispatch counts, std over
the hand loop), before and after: `min_by_key` 8.00x to 6.50x (640 112 to
520 101 instructions), `counts` 2.53x to 2.18x (860 026 to 740 012). The
others are unchanged (`resize` 2.23, `step_by` 2.00, `filter_map` 2.00).
Guards tightened to 7.5 and 2.5. `bench/counts.txt`: `deque` 6 949 479 to
6 949 247, `heap` 3 493 072 to 3 483 065 (-0.29%). Compile time (`compile_time`,
release): source to image 1.26 to 1.27 (asserted 1.75), `compile_at` alone
2.03 to 2.06. The perf gates still pass (box 10 ns, arena put 0.38, arena
trees 9.9x, `par_mandelbrot` 6.27x, spawn and join 994 ns). What is left of a
list stream's cost is its box, allocated per loop and read through memory
(scalar replacement, a later item). `projects/wordfreq` on a 10.9 MB corpus:
identical output, 782 ms.

## V8: fused pipelines read lists, slices and arrays in place (2026-10-28)

A list, slice or array that isn't a temporary is read by the fused loop
directly (the "list changed while a loop went over it" check is kept), with
no stream box and no indirect step per item; a leading `step_by` jumps the
index. `std_counts` (dispatch counts, std over the hand loop, 20 000 items),
before and after: `step_by` then `sum` 17.50x to 2.00x (466 706 to 53 342
instructions), `filter_map` then `sum` 5.30x to 2.00x (530 041 to 200 008),
`min_by_key` 8.25x to 8.00x, `counts` 2.59x to 2.53x. Guards tightened to
2.4. `list_ops` dispatch count 5 316 211 to 4 853 579 (-8.7%,
`bench/counts.txt`). The perf gates still pass (box 10 ns, arena put 0.38,
arena trees 9.9x, `par_mandelbrot` 6.19x, spawn and join 1.18 us).
`projects/wordfreq` on a 10.9 MB corpus: identical output, 769 ms.

## V8: fused `filter_map`, `step_by` and `inspect` (2026-10-28)

The three adaptors run in the fused loop (`fuse.rs`) instead of through boxed
stream steps. `std_counts` (dispatch counts, std over the hand loop, 20 000
items), before and after: `step_by` then `sum` 25.75x to 17.50x (686 755 to
466 706 instructions; the hand loop jumps by 3, the fused one still pulls every
item from the list), `filter_map` then `sum` 8.60x to 5.30x (860 080 to
530 041). Guards tightened to 20 and 6.5. The others are unchanged: `resize`
2.23, `min_by_key` 8.25, `counts` 2.59, `sort_by` 0.91. `list_ops` dispatch
count 5 632 953 to 5 316 211 (-5.6%, `bench/counts.txt`). The perf gates still
pass (box 10 ns, arena put 0.37, arena trees 10.0x, `par_mandelbrot` 6.19x,
spawn and join 1.16 us).

## V8: a block's tail local hands its slot to the join (2026-10-28)

`let xs = { ...; l }}` and list comprehensions no longer copy and zero the
tail local into the join's slot. Release build, `--backend vm -O2`, 10 000 000
iterations of `let xs = [for i in 0..4 => i]; n += xs.len()`, three runs each,
run twice (ms): before 439 441 447 / 433 431 461 (median 441), after
425 412 415 / 412 412 412 (median 412), 6.6% faster. `list_ops` dispatch
count 7 628 190 to 5 632 953 (-26.2%, `bench/counts.txt`). The perf gates still
pass (box 10 ns, arena put 0.38, arena trees 10.0x, `par_mandelbrot` 6.27x,
spawn and join 984 ns). `projects/wordfreq` on a 10.9 MB corpus: identical
output, 794 ms before, 778 ms after.

## Arena and pool fixes, perf gates (2026-10-22)

Release build at a865299, the runtime gates run once, sequentially, on an idle
machine: `cargo test --release -p loom --test perf_gates -- --ignored --nocapture --test-threads=1`.

| Gate | Measured | Limit |
|---|---|---|
| Box allocate, read, free | 10 ns | at most 15 ns |
| Arena put over heap box | 0.37 | at most 0.5 |
| Binary trees in arenas, against Lua | 9.7x | at least 4x |
| `par_mandelbrot`, 8 workers | 6.03x | at least 5.6x |
| Spawn and join, 8 workers | 937 ns | at most 2 µs |

Compile time, `compile_time` (`-O2` over `-O0`): source to image 1.36 at
a865299 (asserted at 1.75), slowest example 41 ms (asserted at 200 ms).

`compile_at alone`, printed but not asserted, measured once at each commit:
2.22 at 37f24c1 (before V6), 2.26 at a865299. The change since 37f24c1 is
+0.04. The 1.63 recorded at V5 close isn't reproduced by this measure at
37f24c1, so the gap predates V6 or comes from how the number is taken. Not
investigated yet.

## V6: pointers and spools (2026-10-21)

Release build, best of 5, whole process (startup is about 30 ms), `cargo test --release -p loom --test perf_gates -- --ignored --nocapture --test-threads=1` (the gates measure wall time, so they must not share the machine).

| Benchmark | Treadle | Note |
|---|---|---|
| `ptr_alloc` (10M boxes: allocate, read, free) | 10 ns each | gate 15 ns |
| `arena_alloc` (10M `put`s, a reset every 4096) | 4 ns each, loop included | 0.39 of `ptr_alloc`, gate 0.5 |
| `ptr_trees` (`binary_trees` with `^Tree`) | 0.70 s | `binary_trees` with lists is 0.66 s; leaves are boxes here, so twice the allocations |
| `arena_trees` (`binary_trees`, trees in an arena, reset per tree) | 0.29 s | **9x PUC Lua** (2.64 s), gate 4x; 2.3x the heap version |

The design (vm.md §12) hoped for an arena allocation a tenth of a heap one's. Allocating alone is: a heap box takes about 7 ns (alloc and free, a native call each), an arena `put` about 1 ns (one op, `arena.k`). The loop and the read around it cost the same either way, so the gate compares the whole (at most half of a box), not the allocation alone.

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
| 6. compile time at `-O2` | at most 1.5× `-O0`, no example over 200 ms | **1.48×** when measured at V3, **1.62–1.64×** now over the larger corpus (the front end got faster, which raises the ratio; `compile_time.rs` asserts 1.75×), from source to image over `tests/run` and `examples` (`compile_at` alone, the front end left out: 1.7×); slowest example 61 ms; the benchmarks compile in 0.8 to 7 ms (`cargo test --release -p loom --test compile_time -- --ignored --nocapture`) |

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
gate of 2 µs on 8 workers (this was the first measure; the sections below
fix it: `loom for` chunks, and spawn+join comes to 0.88 µs on 8 workers
without per-worker deques). The single-thread benchmarks are unchanged
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

### `loom for` chunks (2026-10-17)

`loom for` over an exclusive integer range gives each task a run of items
when there are several workers (`task.grain`: `n / (8 * workers)`, at
least 1; one worker keeps a thread per item). A cancellation point is
passed without the pool's lock until some task has been cancelled. Same
command and machine as the baseline above.

| Benchmark | workers | before | after |
|---|--:|--:|--:|
| `par_mandelbrot` | 1 | 0.461 | 0.450 |
| `par_mandelbrot` | 2 | 0.242 | 0.234 |
| `par_mandelbrot` | 4 | 0.130 | 0.120 |
| `par_mandelbrot` | 8 | 0.076 | 0.072 (6.2x) |
| `par_mandelbrot` | 16 | 0.056 | 0.054 |
| `par_spawn` | 1 | 0.077 | 0.077 |
| `par_spawn` | 2 | 0.172 | 0.028 |
| `par_spawn` | 4 | 0.251 | 0.057 |
| `par_spawn` | 8 | 0.557 | 0.073 |
| `par_spawn` | 16 | 0.638 | 0.062 |
| `par_spawn_join` | 8 | 0.455 | 0.434 (2.17 us each) |

What is left in `par_spawn` is the `Atomic.add` of every item: a native,
so it takes the pool's one lock.

### Spawn and join off the lock (2026-10-17)

The profile of `par_spawn_join` at 8 workers (2.15 us per spawn and join;
counters and timers in the pool, since removed): 8 acquisitions of the
pool's lock per spawn and join, and the work done under it is small (a
native 30 ns, `task.spawn` 260 ns,
`join` 30 ns, a thread's end 50 ns: about 0.4 us). The cost was the idle
workers: seven of them woke on a hint, all took the lock, one found the
task and six went to sleep for good (they spun once), so each spawn needed
a notify (210 000 sleeps and notifies for 200 000 spawns), and the lock was
contended by the spinners at every release, which is a system call for the
holder when others are queued. Per-worker deques would not have touched
either.

What fixed it: an idle worker spins on the scheduler's count of ready tasks
(an atomic, kept up to date under the lock) and takes the lock with
`try_lock` only when a task is queued; one that was woken spins again before
it sleeps (otherwise, with the spawner running ready tasks itself at its
joins, the sleepers woke to an empty queue and slept at once, a state it
stayed in with a notify per spawn: 5 us); and a notify goes out only when
the spinners and the sleepers already notified are fewer than the ready
tasks. Same machine; before and after are the two binaries run alternately, best of 21.

| Benchmark | workers | before | after |
|---|--:|--:|--:|
| `par_spawn_join` | 1 | 0.062 (0.31 us) | 0.062 (0.31 us) |
| `par_spawn_join` | 2 | 0.150 (0.75 us) | 0.146 (0.73 us) |
| `par_spawn_join` | 4 | 0.211 (1.06 us) | 0.170 (0.85 us) |
| `par_spawn_join` | 8 | 0.436 (2.18 us) | 0.177 (0.88 us) |
| `par_spawn_join` | 16 | 0.449 (2.24 us) | 0.174 (0.87 us) |
| `par_mandelbrot` | 8 | 0.072 (6.3x) | 0.072 (6.2x) |
| `par_spawn` | 8 | 0.068 | 0.067 |

Padding the lock to a cache line of its own (the flags the running workers
read had shared it with the lock word) was found by `par_spawn` over 4
million items on 2 workers, which the change had made 27% slower; it is
0.32 s against 0.34 s before now, and `par_spawn_join` no longer has the
second state (0.27 to 0.33 s instead of 0.17) it had without. `fib`,
`richards` and `binary_trees` run the same machine code in the dispatch
loop; their times move by up to 6% either way between builds, with where
the linker puts what is around it (A/B against the commit before:
0.035/0.035, 0.051/0.054, 0.670/0.653).

### Per-worker heaps (2026-10-17)

Allocating and freeing no longer take the pool's lock: each worker has a
heap of its own (design/vm.md 5.5), and `mem.alloc`, `mem.free`,
`mem.calloc`, `mem.grow`, `mem.move` and the free of a list's buffer run on
it. `par_alloc` (new: each row builds and drops 40 trees of 511 small
lists, in `loom for`) was the benchmark that showed the lock: it got slower
with every worker added. Same machine as above; `python bench/run.py --par`.

| Benchmark | workers | before | after |
|---|--:|--:|--:|
| `par_alloc` | 1 | 0.311 | 0.316 |
| `par_alloc` | 2 | 0.338 | 0.173 (1.8x) |
| `par_alloc` | 4 | 0.620 | 0.112 (2.8x) |
| `par_alloc` | 8 | 0.852 | 0.065 (4.9x) |

The single-thread benchmarks, the unchanged commit rebuilt in the same
directory against the new one, run alternately, best of 15 to 25
(wall clock with compilation, so a short run moves with where the linker
puts the compiler too: `richards` runs 3% faster with 20 times the
iterations and 6% slower as shipped): `fib` 0.97, `binary_trees` 0.96 to
0.98, `word_freq` 1.00 to 1.01, `maps` 0.97 to 0.99, `richards` 0.97 (long)
and 1.05 (as shipped). `par_spawn_join` on 8 workers is 4 to 7% slower
(0.203 to 0.210 s before, 0.218 after): the spawner's allocations no longer
wait for the lock, and finding it faster to take the lock for `mem.alloc`
alone shows that this is where the time goes, but the saving on
`par_alloc` is a factor of 13. The checked build keeps its map of live
objects behind a mutex, and all its checks.

### V5 review: native routing and scheduler scans (2026-10-18)

Ordinary release binaries, original review commit `9f5e43b` against the
runtime fixes, before changing the optimizer. Each pair alternates order;
best of seven wall-clock runs, including compilation and process startup,
on the same machine as above. Outputs agree. The sequential suite uses
five pairs and has a before/after geometric speedup of 0.995x (individual
ratios 0.955–1.025x). No build or test was running alongside the timings.

| Benchmark | workers | before (s) | after (s) | speedup |
|---|--:|--:|--:|--:|
| `par_mandelbrot` | 1 | 0.4554 | 0.4502 | 1.01x |
| `par_mandelbrot` | 8 | 0.0742 | 0.0736 | 1.01x |
| `par_alloc` | 8 | 0.0459 | 0.0469 | 0.98x |
| `par_spawn` | 8 | 0.0713 | 0.0167 | 4.26x |
| `par_spawn_join` | 1 | 0.0654 | 0.0644 | 1.02x |
| `par_spawn_join` | 8 | 0.1971 | 0.2149 | 0.92x |
| `par_scalar` | 1 | 0.0345 | 0.0334 | 1.03x |
| `par_scalar` | 8 | 0.7127 | 0.0140 | 50.81x |
| `par_sleep` | 1 | 0.8139 | 0.8138 | 1.00x |
| `par_sleep` | 8 | 0.8144 | 0.1116 | 7.30x |
| `task_results` | 1 | 0.0438 | 0.0139 | 3.16x |
| `task_results` | 8 | 0.0583 | 0.0284 | 2.05x |

`par_scalar` deliberately stresses small scalar natives; the large gain is
removing the lock convoy, not making arithmetic 50x faster. Sleep still
occupies a worker; this is not a timer queue. Spawn/join regressed about
9% in this comparison despite fewer lock acquisitions (1.07 us per pair
afterward); no claim of an improvement there. No first-stack cache or
work-stealing change is included.

Diagnostic builds with `LOOM_STATS=3`, eight workers, separately from the
timed runs (counts before routing changes versus after):

| Workload / counter | before | after |
|---|--:|--:|
| `par_spawn`, generic locks for task natives | 200,013 | 0 |
| `par_spawn`, atomic native locks | 200,001 | 0 |
| `par_spawn_join`, generic locks for task natives | 400,800 | 0 |
| `par_scalar`, scalar native locks | 5,120,000 | 0 |
| `task_results`, leftover probes | 50,015,000 | 10,000 |
| `task_results`, unfinished probes | 29,997 | 10,000 |

Task operations still acquire the scheduler lock when needed (about
400,807 times for `par_spawn_join`), and the 200,000 task-stack allocations
remain. Counts exclude idle-worker queue acquisition. The scalar and
atomic paths never borrow the shared native context. Atomic add/sub use
checked CAS loops, so overflow leaves the value unchanged. Output suffixes
and completed task prefixes are scanned once.

The independent release perf gate passes: `par_mandelbrot` 453.603 ms /
75.102 ms = 6.04x on eight workers (gate 5.6x); spawn/join 1.072 us
(gate 2 us). All six repository correctness/lint gates passed for this runtime batch.

### V5 review: compiler passes (2026-10-18)

The optimizer keeps the same pass schedule and transformations. GVN uses
flat facts with a secondary root index for private-slot facts and zeroed
regions. Broad writes retain the complete alias scan; dominator undo
updates both tables. Inert/quiet instruction handling avoids owned
instruction clones, and zero/single-result type keys need no vector.
SCCP/GVN skip tidy when unchanged; `carry_last` indexes instruction readers
once per block.

Release `LOOM_PASS_TIMES=1`, `build FILE --emit heddle`, before/after
alternating, median of 15 samples per binary. Columns sum all invocations
of each pass in that compilation, in milliseconds; includes the cleanup
each pass performs.

| Program | GVN before | GVN after | SCCP before | SCCP after |
|---|--:|--:|--:|--:|
| `examples/chip8` | 1.839 | 1.649 | 1.426 | 1.165 |
| `examples/lexer` | 2.100 | 1.947 | 1.490 | 1.196 |
| `tests/run/iterators` | 6.070 | 5.830 | 6.000 | 5.457 |
| `tests/run/map_removals` | 4.730 | 4.590 | 4.478 | 4.097 |
| `tests/run/maps` | 4.840 | 4.580 | 5.010 | 4.830 |
| `bench/mandelbrot` | 0.089 | 0.077 | 0.062 | 0.057 |

A full nested map per root was rejected: it slowed GVN on large
iterator/map programs by 13–24%. A flat primary table removed most of
that cost, but indexing every fact still slowed `map_removals` by about
10%. The retained selective index improves GVN on all six above (3–15%)
without making broad scans traverse retained empty buckets. No separate
cleanup-pass improvement is claimed by these measurements.

Validation: optimizer and instruction-count snapshots are unchanged;
all 193 successfully compiled fixture/example/benchmark bytecode listings
match the pre-compiler-change binary (the same two unsupported examples
fail in both). The checked/statistics VM passes 20,000 additional generated
programs at `-O0` and `-O2` against Lua, the seven parallel fixtures 1,000
times each across 2/4/8/16 workers, and the thread fixtures 1,000 times
each on eight workers.

Three paired corpus runs of the release `optimizer_compile_time` test,
alternating binary order (each test takes the best of five per file and
level). Median aggregate milliseconds over fixtures/examples:

| Stage | before `-O0` | before `-O2` | after `-O0` | after `-O2` |
|---|--:|--:|--:|--:|
| Source to image | 396.13 | 646.84 | 395.78 | 631.31 |
| `compile_at` alone | 221.95 | 471.54 | 221.58 | 456.00 |

That is 2.4% less source-to-image time and 3.3% less `compile_at` time at
`-O2`; `-O0` is essentially unchanged. The source-to-image ratio drops
from about 1.63x to 1.60x. A separate final gate run measured 1.61x
(395.68 / 638.62 ms), with the slowest example 39.25 ms against 200 ms.
The asserted 1.75x gate passes; the design's 1.5x target remains unmet.
Neither threshold was changed.

The final release parallel gates also pass: Mandelbrot 445.253 / 73.589 ms
= 6.05x on eight workers, and spawn/join 1.06 us against the 2 us limit.

Final ordinary release binary versus `9f5e43b`, alternating order, best of
seven, with outputs compared. All rows below use eight workers and include
compilation/process startup (milliseconds):

| Benchmark | before | final | speedup |
|---|--:|--:|--:|
| `par_mandelbrot` | 73.308 | 70.839 | 1.03x |
| `par_alloc` | 41.878 | 45.091 | 0.93x |
| `par_spawn` | 75.940 | 16.683 | 4.55x |
| `par_spawn_join` | 194.715 | 204.119 | 0.95x |
| `par_scalar` | 749.665 | 13.534 | 55.39x |
| `par_sleep` | 814.484 | 111.826 | 7.28x |
| `task_results` | 60.605 | 29.452 | 2.06x |

Repeating the two slower parallel cases with 21 pairs: `par_alloc`
43.781 / 43.897 ms (essentially unchanged), and `par_spawn_join`
190.740 / 203.735 ms (6.8% more time; 1.02 us per pair). Its one-worker
repeat is 64.553 / 62.844 ms. The eight-worker spawn/join regression is
retained and needs follow-up; the stack-allocation counter is unchanged.
No general speedup for allocation or task creation is claimed.

The final sequential suite, best of five pairs, has a geometric speedup
of 0.996x. Largest slower cases: `richards` 48.746 / 51.287 ms and
`word_freq` 209.094 / 218.891 ms. Largest faster cases: `fannkuch`
787.055 / 760.580 ms and `binary_trees` 664.996 / 649.806 ms. These are
whole-program timings, not timings of the unchanged bytecode in isolation.

## Standard library, phases 0, 1 and 4 (design/stdlib-plan.md)

Step-0 spike: dispatch counts (`crates/loom/tests/std_counts.rs`) of a std
method written in Loom against the same work as a hand loop, input building
taken away: `retain`, `dedup` and `swap_remove` are 1.00x (the method inlines
to the hand loop). Others miss the plan and are held by a guard in that test:
`resize` 2.2x (plan 1.1x), `counts` 2.6x (1.1x), `min_by_key` 8x and
`filter_map` then `sum` 16x (1.25x), `step_by` then `sum` 32x (1.25x; the
adaptor walks every item). `sort_by` costs 1.3x the native `sort`. An earlier
wall-clock spike (3M items, whole process) gave 1.0 to 1.1x, but it was
dominated by building the list and is not evidence.

Scaling gates (dispatch counts, `bench/counts.txt`, test
`std_collections_scale`): ten times the input costs this many times the
instructions: `deque` 10.0x (limit 10.5), `list_ops` (stable `sort_by`,
`dedup`, `retain`, `step_by`, `filter_map`) 10.2x (limit 12), `heap`
(`PriorityQueue`, `BitSet`) under the 13x limit.

`sort_by` over random `u64`s on Treadle, whole run including startup:
100 000 items about 50 ms, 1 000 000 items about 480 ms (the old
`sorted_by` was quadratic).

Compile time with the new modules: `-O2` over `-O0` 1.43x (limit 1.5,
asserted at 1.75x), measured on a loaded machine.

Known limits: `sort_by` and the other list code written in Loom keep about
95% of their bounds checks at `-O2` (`list_ops` in `check_counts`), so the
benchmarks for the standard library are left out of that aggregate; the
stream adaptors that are warps (`step_by`, `peekable`, `chunks`, `cycle`,
`flat_map`) are not fused into the loop that consumes them.
