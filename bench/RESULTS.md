# Results

Wall clock, best of five, compilation and start-up included (about 10 ms for
each way of running). Lower is better.


## par_print (2026-11-10): one slow worker in some runs

`print_scales_to_8_workers` read 1.18x, 4.31x and 0.89x in three runs. On 8
workers `par_print` takes 70-95 ms, or, in some runs (from 1 in 50 to 1 in 4,
depending on the hour), 310-330 or 420-450 ms. It isn't the output (it does
it into NUL too, and the out lock is never waited on for long), the
scheduler (the 8 tasks start on 8 workers within a millisecond), or the
machine's load (CPU burners on every core make it 130 ms, not 400). A trace
kept in memory, with each worker's progress every 100 000 lines and the CPU
it was on, shows one to three workers slow from their first line to their
last at a steady 38-55 ms per 100 000 lines, against 6-10, whatever CPU they
moved to and after every other task had ended; the process's user time is
higher by about what they took, so they run slowly rather than wait. In all
10 slow workers of 7 slow runs caught, and in none of the fast ones, the
worker's line buffer (`Local.out`, a 32-byte block of the binary's size
classes) was at offset 0xc8b0 or 0xc8d0 of its chunk. What makes that place
slow isn't pinned down: not the digits buffer on the stack (writing them
straight into the buffer changed nothing) or the program's literals (their
addresses differed run to run).

Keeping the buffer at 2 KB or more (`LINE_BUF` in `loom_vm::natives`), so it
comes from the system's allocator, ends it. On one binary with the change
behind a flag, runs interleaved, 8 workers: 6 of 40 runs slow without and 0
of 40 with; at a busier hour 8 of 60 without and 0 of 60 with. Then the
gate three times in a row: 4.46x, 4.52x, 4.49x (87 ms on 8 workers, 389-393
ms on one, as before), every other perf gate passing. The suite: 2.60x PUC
Lua, as before (2.59x). The same experiment on the template sinks
(`par_fmt`, `par_text`) found no slow mode to remove, and a 2 KB sink made
`par_fmt` 4-8% slower, so they keep their buffers.


## PGO (2026-11-10): measured, not shipped

`python bench/pgo.py --gate` builds `loom` instrumented (with
`.cargo/config.toml`'s rustflags, which RUSTFLAGS would otherwise replace),
trains it on 281 runs that bench/run.py doesn't time (the `tests/run`
fixtures, the examples run and checked, the other benchmarks in `bench/`),
merges the profile with `llvm-profdata` 22.1.8 (rustc 1.98's LLVM is 22.1.8),
builds again with it, and times that against the plain release binary: best
of 5, the two interleaved. The gate: geomean at most 0.95, no benchmark more
than 3% slower, compiling not slower.

| Benchmark | plain | PGO | | trained on the suite | | no block alignment | |
|---|--:|--:|--:|--:|--:|--:|--:|
| `fib` | 0.037 s | 0.037 s | 0.985 | 0.036 s | 0.945 | 0.036 s | 0.969 |
| `sieve` | 0.120 s | 0.122 s | 1.012 | 0.120 s | 1.002 | 0.119 s | 1.004 |
| `mandelbrot` | 0.065 s | 0.066 s | 1.014 | 0.065 s | 0.994 | 0.066 s | 1.023 |
| `nbody` | 0.292 s | 0.281 s | 0.963 | 0.279 s | 0.942 | 0.277 s | 0.966 |
| `spectral_norm` | 0.062 s | 0.066 s | 1.051 | 0.064 s | 0.997 | 0.064 s | 1.026 |
| `fannkuch` | 0.768 s | 0.792 s | 1.032 | 0.807 s | 1.043 | 0.787 s | 1.022 |
| `binary_trees` | 0.667 s | 0.693 s | 1.039 | 0.647 s | 0.957 | 0.675 s | 1.018 |
| `richards` | 0.051 s | 0.052 s | 1.004 | 0.052 s | 0.984 | 0.052 s | 0.989 |
| `word_freq` | 0.221 s | 0.204 s | 0.921 | 0.185 s | 0.829 | 0.204 s | 0.973 |
| `maps` | 0.078 s | 0.077 s | 0.999 | 0.073 s | 0.959 | 0.072 s | 0.968 |
| `sort` | 0.101 s | 0.103 s | 1.021 | 0.101 s | 0.996 | 0.101 s | 1.009 |
| `streams` | 0.076 s | 0.075 s | 0.983 | 0.076 s | 0.996 | 0.074 s | 1.003 |
| `chip8` | 0.054 s | 0.054 s | 1.008 | 0.053 s | 0.969 | 0.053 s | 0.979 |
| `lexer` | 0.181 s | 0.188 s | 1.040 | 0.176 s | 0.987 | 0.186 s | 1.063 |
| geomean | | | **1.005** | | 0.970 | | 1.000 |
| compiling `chip8.loom` | 46.4 ms | 43.1 ms | 0.931 | 43.0 ms | 0.929 | 41.8 ms | 0.927 |

(Each variant has its own plain column in the logs, within 1-3% of the one
shown.) The gate fails: 1.005 against 0.95, with `spectral_norm`, `fannkuch`,
`binary_trees` and `lexer` 3-5% slower. A second run of the same script (the
training now run outside the repository's root, so the examples' files don't
land in it) read 0.981, `chip8`'s 0.867 of it a slow plain run, and compiling
0.985: the runs move by 2-3%, and PGO's gain on compiling is 1.5-7%. Trained on the very programs it is
timed on, the best it can do, PGO reaches 0.970, and dropping
`-align-all-nofallthru-blocks` so the profile lays the blocks out alone gives
1.000. The dispatch loop's flags (one copy of the dispatch per op, aligned
blocks) already give it the layout a profile would; what PGO adds is a few
percent on compiling, too little for a second build in every release. So `loom` ships
without it, and `bench/pgo.py` stays to measure it again when those flags
change.

`binary_trees` and `nbody`, which the start-up work read 3-4% slower (684-698
ms and 295 ms, below): the plain binary reads 663-676 ms and 287-297 ms in
these runs, so the loss isn't visible here, but nothing in this work aimed at
it (no Treadle code changed), the earlier numbers come from other runs, and
runs move by 2-3%: not reproduced, rather than fixed. PGO doesn't move
`binary_trees` (681-693 ms, 647 when trained on the suite) and takes `nbody`
to 277-281 ms.


## V9 switch (2026-11-10)

design/vm.md §18's list, checked before Treadle became the default for
`loom build` and `loom check` too (`run` and `test` already were):

1. `tests/vm_skip.txt` is empty.
2. All 8 examples give the same exit code and stdout on both backends (`loom
   run`, empty stdin; `chip8` needs a ROM, so `loom test` instead):
   `calc`, `chip8`, `ledger`, `lexer`, `parallel`, `tour` and `widgets` exit
   0, and `compile_time` stops at 2 on both (it uses `std.sys`). The four
   `.vm.out` files are rows of §14: `const_if` P6 (`target.pointer_bits`, 64
   against 63), `p1_mut_on_unwind` P1, `p4_hash` P4, `shuttle_drop` P2.
3. The checked build is clean over the corpus (`cargo test -p loom
   --features vm-checked,vm-stats`). The generator's 20 000 seeds (300 to
   20 299, `--test gen -- --ignored`, release) pass: 0 failed, 70.25 s on the
   ordinary build and 64.65 s with `--features vm-checked`.
4. The V3 speed gate (2.5x PUC Lua) holds: 2.59x, lowest 1.07x
   (`word_freq`), against the PUC Lua that got 10% faster with the new
   allocator (below):

   | Benchmark | PUC Lua 5.4 | Treadle | |
   |---|--:|--:|--:|
   | `fib` | 0.088 s | 0.039 s | 2.25x |
   | `sieve` | 0.345 s | 0.120 s | 2.87x |
   | `mandelbrot` | 0.133 s | 0.065 s | 2.03x |
   | `nbody` | 0.567 s | 0.294 s | 1.93x |
   | `spectral_norm` | 0.156 s | 0.063 s | 2.46x |
   | `fannkuch` | 1.502 s | 0.773 s | 1.94x |
   | `binary_trees` | 1.801 s | 0.673 s | 2.68x |
   | `richards` | 0.138 s | 0.055 s | 2.52x |
   | `word_freq` | 0.237 s | 0.221 s | 1.07x |
   | `maps` | 0.116 s | 0.078 s | 1.48x |
   | `sort` | 0.293 s | 0.105 s | 2.80x |
   | `streams` | 0.945 s | 0.077 s | 12.21x |
   | `chip8` | 0.103 s | 0.058 s | 1.76x |
   | `lexer` | 1.500 s | 0.187 s | 8.02x |

5. Nothing in LEDGER.md is wrong on Treadle and right on Lua. The open items
   that name a backend are wrong on Lua (a nested generator's `defer`) or on
   both (`parse[f32]`, a hand-written `Show` on a std warp). Images are
   trusted, not verified (Caio, 2026-11-03), which the docs now say.

The `#[ignore]`d gates on the result (release, one at a time): `perf_gates`
10 of 11 on the first run (a box 11 ns, an arena put 0.36 of a box, arena
trees 6.6x Lua, `par_chunks` 8.43x, `par_mandelbrot` 5.96x, spawn+join 797
ns on 8 workers, `par_build` 6.18x, `par_fmt` 5.43x, `par_text` 6.42x,
`par_mutex` 5.28x). `print_scales_to_8_workers` read 1.18x, then 4.31x and
0.89x when run alone again: bimodal, and the same on the binary from before
this work (12 interleaved runs of `par_print` on 8 workers, 210-219 ms on
both, one 561 ms outlier), so it's the gate's noise, not a change here.
`compile_time` and `build_exe`'s gates pass (an exe starts at the process
floor, 6.58 ms against 6.59).

`loom check` on Treadle (an `-O0` build of `main` and the tests, nothing
written) against the Lua check it replaces, over the 248 files of `examples/`
and `tests/run`, best of 5 each: 2706 ms before, 2867 ms after (1.06x; the
gate was 1.3x). `loom check --backend lua` prints exactly what the old check
did on all 248.


## Start-up and compile time (2026-11-10)

Every `loom` command paid about 12 ms before it did anything of its own: 5 ms
lexing, parsing and resolving `std` (120 KB, 17 000 tokens), 1.2 ms loading
`user32.dll` (and what it loads) for the one call `term.poll_keys` makes, and a
third of compiling in the system allocator. The changes: the lexer takes its
fast paths a byte at a time and finds punctuation by its first byte, the
parser stops cloning tokens and keeps one token buffer for every file of a
compile, and the resolver's scopes hash with FxHash; the `loom` binary has its
own allocator (`crates/loom/src/alloc.rs`: size classes per thread, as
Treadle's heap has); `loom` and the runner stub delay-load `user32.dll`; and
the tables every Treadle worker reads (code, functions, natives, literals,
format items, and the program's data) sit on cache lines of their own (`loom_vm::lines`), which the
new allocator needed (below).

Old and new binary, runs interleaved, best of 21, wall ms:

| | before | after | |
|---|--:|--:|--:|
| `loom run` hello world | 15.8 | 11.8 | 0.75 |
| `loom check` hello world | 14.8 | 11.1 | 0.75 |
| `loom build --emit bytecode bench/lexer.loom` | 20.0 | 15.1 | 0.76 |
| `loom check examples/chip8.loom` | 45.8 | 41.3 | 0.90 |
| `loom run examples/tour.loom` | 20.5 | 15.6 | 0.76 |

In the process (a harness calling the compiler, hello world): resolving went
from 2.7 to 1.2-1.4 ms warm and from 4.6 to 3.0-3.2 ms cold; compiling
`bench/lexer.loom` from 3.6 to 2.7 ms warm. The front end makes 29 000
allocations for hello world (11 MB in all).

**The PUC Lua baseline got faster too.** `loom lua-host` runs Lua in the same
binary, and mlua allocates through Rust's global allocator, so the new
allocator serves the baseline as well. Old and new binary on `bench/lua`, best
of 5, seconds:

| | before | after | |
|---|--:|--:|--:|
| `fib` | 0.090 | 0.084 | 0.94 |
| `sieve` | 0.345 | 0.339 | 0.98 |
| `mandelbrot` | 0.136 | 0.118 | 0.87 |
| `nbody` | 0.534 | 0.524 | 0.98 |
| `spectral_norm` | 0.160 | 0.155 | 0.97 |
| `fannkuch` | 1.491 | 1.452 | 0.97 |
| `binary_trees` | 2.705 | 1.740 | 0.64 |
| `richards` | 0.185 | 0.136 | 0.74 |
| `word_freq` | 0.245 | 0.234 | 0.95 |
| `maps` | 0.128 | 0.103 | 0.80 |
| `sort` | 0.308 | 0.269 | 0.87 |
| `streams` | 0.943 | 0.900 | 0.96 |
| `chip8` | 0.105 | 0.101 | 0.96 |
| `lexer` | 1.460 | 1.438 | 0.99 |

Geometric mean 0.895. So "against Lua" ratios from here on are against a
faster Lua (`--backend lua` gets the same speedup), and the gates that compare
with Lua are stricter: `arena_trees` reads 6.4x where it read 9.8-10.9x.
Treadle, old and new binary, both against the new binary's Lua (best of 9,
seconds):

| | Lua | Treadle before | Treadle after | before | after |
|---|--:|--:|--:|--:|--:|
| `fib` | 0.087 | 0.043 | 0.039 | 2.01x | 2.24x |
| `sieve` | 0.352 | 0.126 | 0.121 | 2.79x | 2.92x |
| `mandelbrot` | 0.131 | 0.071 | 0.067 | 1.86x | 1.97x |
| `nbody` | 0.521 | 0.287 | 0.295 | 1.81x | 1.76x |
| `spectral_norm` | 0.162 | 0.071 | 0.065 | 2.27x | 2.50x |
| `fannkuch` | 1.492 | 0.776 | 0.776 | 1.92x | 1.92x |
| `binary_trees` | 1.766 | 0.660 | 0.686 | 2.68x | 2.58x |
| `richards` | 0.135 | 0.061 | 0.053 | 2.20x | 2.53x |
| `word_freq` | 0.248 | 0.214 | 0.210 | 1.16x | 1.18x |
| `maps` | 0.107 | 0.082 | 0.078 | 1.30x | 1.37x |
| `sort` | 0.278 | 0.108 | 0.105 | 2.58x | 2.65x |
| `streams` | 0.879 | 0.082 | 0.076 | 10.71x | 11.59x |
| `chip8` | 0.104 | 0.060 | 0.060 | 1.74x | 1.72x |
| `lexer` | 1.436 | 0.188 | 0.182 | 7.63x | 7.88x |

Geometric mean 2.44x before, 2.55x after; most of the gain is the 4 ms of
start-up in every run. `binary_trees` and `nbody` lost 3-4%: the field the
format items added to the interpreter's `Env` moved the loop's code (a build
without it read 651-666 ms on `binary_trees`, against 672-676 before and
684-698 after), the layout effect section 9.1 of design/vm.md describes.

**False sharing.** With the allocator alone, `par_print` on 8 workers fell
from 3.56-3.71x to 2.45-3.03x. Moving the blocks of chosen size classes to
cache lines of their own (an experiment) found it: blocks of 96 and 160 bytes,
which a small program's format items and native tables take, were carved next
to what the main worker writes on every `print`, so each write took the line
from the other seven workers. The system allocator kept them apart by chance.
Treadle now puts those tables on whole cache lines wherever they come from:
`par_print` 3.93-4.12x.

Perf gates at the end (`perf_gates`, `compile_time`):

| Gate | Reading | Limit |
|---|---|---|
| Box allocate, read, free | 11 ns | at most 15 ns |
| Arena put over heap box | 0.37 | at most 0.5 |
| Binary trees in arenas against Lua | 6.4x (the faster Lua) | at least 4x |
| `par_mandelbrot`, 8 workers | 5.63-5.72x | at least 5.2x |
| Spawn and join, 8 workers | 0.79-0.84 us | at most 2 us |
| `par_text` | 6.0x | at least 4x |
| `par_build` | 5.9x | at least 4x |
| `par_mutex` | 5.0-5.1x | at least 4x |
| `par_chunks` | 7.8-7.9x | at least 4x |
| `par_fmt` | 5.1-5.3x | at least 3.5x |
| `par_print` | 3.93x | at least 2.5x |
| source to image at `-O2` over `-O0` | 1.33x | at most 1.75x |
| `compile_at` alone | 2.08x | at most 2.15x |

"Source to image" rose from 1.25-1.27x because `-O0` got faster: the front
end both share is a smaller part of it now.

## V8 summary (2026-11-02)

V8 (speed) is complete; the commits are in LEDGER.md. Geometric mean against PUC
Lua 5.4 (`python bench/run.py --runs 5`): 2.73x, lowest 1.15x (`word_freq`); the
V3 table below read 2.42x, lowest 0.99x. Design/vm.md section 16's V8 row (4x, none
under 2x) was not reached.

Perf gates at the end of V8 (`cargo test --release -p loom --test perf_gates --
--ignored --nocapture --test-threads=1`, `compile_time` for the last two; ranges are
what the readings of the last days spanned on this machine):

| Gate | Reading | Limit |
|---|---|---|
| Box allocate, read, free | 10 ns | at most 15 ns |
| Arena put over heap box | 0.37-0.40 | at most 0.5 |
| Binary trees in arenas against Lua | 9.8-10.9x | at least 4x |
| `par_mandelbrot`, 8 workers | 5.2-6.1x | at least 5.2x (0.65 a core) |
| Spawn and join, 8 workers | 0.75-0.84 us | at most 2 us |
| `par_text` | 5.6-6.4x | at least 4x |
| `par_build` | 5.0-6.2x (2.9x at a bad moment of the machine) | at least 4x |
| `par_mutex` | 4.4-5.7x | at least 4x |
| `par_chunks` (`loom for` over lists) | 7.4-8.3x | at least 4x |
| `par_fmt` (templates) | 3.6-5.2x | at least 3.5x |
| `par_print` (800 000 lines a task, to a file) | 2.75-3.8x (2.75, 2.88, 3.45, 3.48 on 2026-11-02) | at least 2.5x |
| source to image at `-O2` over `-O0` | 1.25-1.27x | at most 1.75x |
| `compile_at` alone | 1.95-2.11x | at most 2.15x |

Headline numbers, before to after (details in the sections below):

| Bench | Before | After |
|---|---|---|
| `list_ops` dispatches | 7,628,190 | 4,853,579 |
| `step_by` then `sum`, over the hand loop (dispatches) | 25.75x | 2.00x |
| `filter_map` then `sum`, over the hand loop | 8.60x | 2.00x |
| `set_drain` dispatches | 6,786,096 | 3,250,121 |
| `maps` dispatches | 4,114,296 | 3,422,891 |
| 100,000-entry stack over `Map.last` | over 100 s | 84 ms |
| `fib(25)`, a thread on every call, 8 workers | 914 ms | 244 ms |
| `contains` / `Map.has` on 8 workers, 8 tasks | 1,440 / 1,270 ms | 82 / 64 ms |
| `par_build` on 8 workers | 6,639 ms | 125 ms |
| 8 tasks of templates, 8 workers (400k each) | 3,897 ms | 93 ms |
| 8 tasks printing 800,000 lines to a file, 8 workers | 958 ms | 140 ms |
| arena `put` in a `loom for` body, 8 workers | 1,330 ms | 136 ms |
| `compile_at`, `-O2` | 552 ms | 532 ms (-3.8%) |

## V8: per-worker queues for printed lines (2026-11-02)

A worker queues the whole lines its natives write and hands them to the one
output in batches of 16 KiB (a terminal still gets each line at once), and also
whenever a task parks, joins, ends, or reads standard input, and when the worker
stops. Lines stay whole and a task's own lines stay in order; lines of different
tasks may interleave differently. `bench/par_print` (8 tasks, 800 000 lines each,
stdout to a file), best of 5, wall ms:

| | before | after |
|---|---|---|
| 1 worker | 412 | 420 |
| 8 workers | 958 (0.43x) | 140 (3.0x in a shell run; 3.5-3.8x in the gate) |

At 100 000 lines a task the process's 25-35 ms of start-up caps the ratio at about
2.4x, which is why the bench is bigger. The 100 000-line scratch version: 8 workers
1 015 ms before the lock-free natives, 155 after them, 53 now.

## V8: a `spool` made in a `loom for` body is the task's own (2026-10-29)

The captures scan (`heddle/build/scan.rs`) never `declare`d the local of a
`spool`, of a named `given`, or of a `const for`'s pattern, so a use of one
inside a thread, lambda or `defer` counted as a capture of an outer variable.
For `loom for` that put the arena handle in a slot of the function around the
loom, which every task wrote: the tasks shared one arena and raced on its
bump pointer, `grow` and `reset` (a segfault at 8 MiB or less, any number of
workers above one). Now the arena is a slot of the task's own frame
(`tests/opt/spool_in_thread`). Wall ms, 8 tasks, 1 worker / 8 workers:

| Benchmark | before | after |
|---|---|---|
| 200 rounds of 100 000 `a.put` in a 64 MiB arena | 633 / 1 330 | 549 / 136 (4.0x) |
| the same with `str.new_in` copies, 200 rounds | 1 365 / 2 573 | 1 331 / 319 (4.2x) |
| `par_newin2`: 8 MiB arena, 200 rounds | 1 328 / crash | 1 527 / 326 (4.7x) |

## V8: templates, messages, `str.new_in` and constants off the pool lock (2026-10-29)

The message, the sinks `sink.push` makes and the line being written are the
worker's own (`Local`, which `Ctx` now holds as `cur`), and the natives that
write output, build text and read constants run on a `Worker` (its heap, its
`Local`, the output, the format items) without the pool's lock, in a build that
doesn't check. Whole lines reach the one output through its own mutex, when a
native is done. Wall ms, 8 tasks in `loom for`, best of 3, 1 worker / 8 workers:

| Benchmark | before | after |
|---|---|---|
| `par_fmt` (400k templates a task) | 250 / 3 897 | 240 / 93 |
| `par_fmt` (3.2M a task, the gate's) | 1 534 / about 31 000 | 1 534 / 357 (4.3x; gate 4.0x, 3.8-5.2x over 12 runs) |
| `par_print` (100 000 lines a task) | 93 / 1 015 | 90 / 155 |
| `par_newin` (`str.new_in`) | 186 / 2 623 | 182 / 313 (the arena was shared: see the section above) |

Every call of these natives took the lock before (25.6M of 25.6M in `par_fmt`),
and none does now.

## V8: native map reindex and compact (2026-10-29)

`map.reindex(header, slots, esize)` frees the old index, makes an empty one of
`slots` slots and puts the live entries in it in order; `map.compact(header,
esize)` moves the live entries down a contiguous run at a time (one copy per
run), sets `used`, `head` and `tail`, and reindexes. Both run on the worker's
heap without the pool lock; the checked build validates the header, the entries
and the index first. The Heddle loops are gone. Best of seven, ms,
`--backend vm -O2`:

| Benchmark | before | after |
|---|---|---|
| `maps` | 127 | 116 |
| `set_drain` | 83 | 75 |
| `word_freq` | 255 | 251 |
| 100 000 `String` keys, 300 000 hits, 200 000 misses | 84 | 82 |
| `tests/run/map_grow` (200 000 entries, halved, grown, thirds, strings, set) | 157 | 141 |

Dispatch counts (`bench/counts.txt`): `maps` 4 554 300 to 3 422 891 (-24.8%),
`set_drain` 4 866 036 to 3 250 121 (-33.2%), `word_freq` 1 570 825 to 1 569 481.

## V8: a free list of task stacks (2026-10-28)

A finished task's stack goes back to `Sched.spare_stacks` reset to its first
segment (the other segments are freed, and a stack whose first segment is
bigger than the usual 2048 slots is freed), at most 64 for each worker, and a
spawn takes one before allocating. Medians of 7, ms, `--backend vm -O2`
(`bench/` scratch programs: `a_spawn` is 100 rounds of 1000 spawn+join,
`fib25 d=25` a thread on every call):

| Benchmark | workers | before | after |
|---|---|---|---|
| `a_spawn` | 1 / 2 / 4 / 8 | 84 / 107 / 131 / 155 | 73 / 86 / 104 / 131 |
| `fib25 d=25` | 1 / 2 / 4 / 8 | 94 / 160 / 207 / 232 | 83 / 125 / 183 / 230 |

Task stacks allocated (`LOOM_STATS=3` on a `vm-stats` build): `a_spawn` 100 000
to 1 / 217 / 14 / 25 and `fib25 d=25` 121 392 to 24 / 42 / 67 / 119 at 1 / 2 /
4 / 8 workers. The dispatch counts do not change.

## V8: Map.first, Map.last and Set.pop in O(1) (2026-10-28)

The map header gained `tail` (8 words), `head` is exact while the map has
entries, and a removed entry just behind a newest entry that was added past
removed ones says where they start (map.rs, header comment). `Map.first`,
`Map.last` and `Set.pop` read one entry from `head`/`tail` instead of building
a stream (`Map.last` walked every entry). Best of seven, `--backend vm -O2`,
ms:

| Benchmark | before | after |
|---|---|---|
| `set_drain` (100 000 `pop` and `first`+`remove`) | 91 | 83 |
| 100 000 inserts, `first`+`remove` until empty, 100 000 more, `last`+`remove` until empty | over 100 000 | 84 |
| a stack 50 000 deep, 200 000 times `insert`, `last`, `remove` | over 100 000 | 77 |
| `maps` | 120 | 123 |
| `word_freq` | 256 | 253 |

(Before, the two `last` programs were stopped at 100 s.) Dispatch counts
(`bench/counts.txt`): `set_drain` 6 786 096 to 4 866 036 (no stream box, no
list), `maps` 4 114 296 to 4 554 300 (+10.7%: each insert and each removal
keeps `head` and `tail`, about 7 dispatches), `word_freq` 1 570 318 to
1 570 825.

## V8: per-worker ready deques with stealing (2026-10-28)

`Sched.ready` is a deque per worker. A task started or woken is pushed on a
deque (a spawned task on its spawner's, a woken one on the deque of the worker
it last ran on); a worker takes the newest task of its own deque, else half of
the next non-empty deque, the oldest, moved to the back of its own. One worker
(no pool) keeps one deque and the old FIFO order. Medians of 5 runs at 8
workers, ms, against the commit before (same minutes, alternating):

| Benchmark | before | after |
|---|---|---|
| fib(25), a thread on every call (121 392 tasks) | 914 | 244 (3.7x) |
| `loom for` over 1M atomic adds | 96 | 86 |
| 100 000 spawn + join | 168 | 171 |
| 1M items through a stream (one task each) | 1214 | 1285 (+5.8%) |
| 64 tasks, the first 8 ten times heavier | 189 | 197 (+4%; +11% at 4 workers) |
| fib(40), depth 6 / 10 | 306 / 279 | 303 / 279 |

At 2 workers fib(25) with a thread per call went from 680 to 170 ms. The cost
is the order: with the newest-first own pops a heavy task spawned first can
run late, which is what the 64-task benchmark (heavy tasks first, built for a
FIFO queue) shows. Variants measured and dropped: stealing one task (flat
spawn +35%, stream +55%), a join that runs its own unstarted task (fib(25)
at 2 workers 1290 ms), a thief that runs its stolen batch oldest first
(64 tasks +13%, stream +10%), a separate stolen queue (stream +7%).

## V8: string-building natives run on the worker's heap without the pool lock (2026-10-28)

`concat`, `split`, `replace`, `repeat`, `case`, `string.of`, `push_char`,
`push_str`, `pop` and `str.new` allocate, but only on a heap and from text they
are given, so on a pool they now run on the worker's own heap (`Strings` is
implemented by the context and by `Own`, a worker's heap; non-checked builds).
`bench/par_build.loom`: 8 tasks, each building a `String` with `push_str`
and two `push`es and calling `to_upper`, `LOOM_THREADS` 1 and 8, ms (1 000 000
iterations per task, 3 runs):

| | 1 worker | 8 workers |
|---|---|---|
| before | 633, 460, 451 | 6639, 6589, 6729 |
| after | 476, 459, 467 | 125, 123, 122 |

New gate `string_building_scales_to_8_workers` (3 000 000 iterations per
task, best of 5): 1.208 s on one worker, 202.9 ms on 8, 5.95x (5.86x, 5.97x
earlier; gate 4x). `text_natives_scale_to_8_workers` 6.05x. The other gates:
box 10 ns, arena put 0.36, arena trees 9.9x, `par_mandelbrot` 5.93x, spawn and
join 1.13 us. `projects/wordfreq` on 10.9 MB: output identical at 1 and 8
workers, no change in time.

## V8: read-only text and map natives run without the pool lock (2026-10-28)

With several workers every native that wasn't a worker-local leaf took the
pool's one lock. `str.find`, `starts_with`, `ends_with`, `char_count`, `trim`,
`parse_int`, `parse_float`, `next_char`, `prev_char`, `next_line`, `next_word`,
`hash`, `eq`, `cmp`, `map.find_str` and `map.find_int` only read the text they
are given, so in non-checked builds they now run on the worker's own thread
(`Texts` is implemented by the context, which checks addresses in the checked
build, and by `Raw`, which doesn't). Scratch programs, `loom for i in 0..8`,
1 000 000 calls per task, `LOOM_THREADS` 1 and 8, ms:

| Native | 1 worker before | 8 workers before | 1 worker after | 8 workers after |
|---|---|---|---|---|
| `s.contains("zzz")` (110 bytes) | 254 | 1440 | 265 | 82 |
| `m.has("fox")` (`Map[str, u64]`) | 138 | 1270 | 134 | 64 |
| `x.count_ones()` (control) | 99 | 56 | 95 | 56 |

New gate `text_natives_scale_to_8_workers` (`bench/par_text.loom`, 4 000 000
iterations of `contains` and `has` per task, best of 5): 1.123 s on one
worker, 190.8 ms on 8, 5.89x (5.99x, 5.81x in two more runs; gate 4x). The
other gates are unchanged: box 10 ns, arena put 0.37, arena trees 10.2x,
`par_mandelbrot` 5.72x, spawn and join 1.16 us.

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
