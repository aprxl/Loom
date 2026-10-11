# Review of run time and compile time, after V4 (2026-10-11)

What this reviews: the whole of what a user's program meets (the Lua
backend, which `loom run` still defaults to and every fixture runs on;
Treadle; the compiler), after V4 put compile-time code on Treadle.
`vm-review-v3.md` covered Treadle's loop, heap, formatting and ISA; this
looks at what it listed as not looked at, plus the Lua backend, the
compiler's own speed and the build.

**Evidence tags.** *Measured* is a timing from this review; *Verified* is
the code read, or a listing; *Inferred* is neither.

## 0. What was measured

**The Lua backend is far from idiomatic Lua.** Generated Lua against the
hand-written Lua of `bench/lua/` on the same machine (best of 3), and
Treadle for scale. *Measured.*

| Benchmark | Generated Lua | Idiomatic Lua | Treadle | Generated / idiomatic |
|---|---|---|---|---|
| `fib` | 0.564 s | 0.086 s | 0.036 s | 6.6× |
| `sieve` | 2.111 s | 0.345 s | 0.118 s | 6.1× |
| `mandelbrot` | 0.877 s | 0.119 s | 0.063 s | 7.4× |
| `nbody` | 2.269 s | 0.520 s | 0.280 s | 4.4× |
| `spectral_norm` | 2.367 s | 0.148 s | 0.061 s | 16.0× |
| `fannkuch` | 14.483 s | 1.454 s | 0.807 s | 10.0× |
| `richards` | 2.603 s | 0.182 s | 0.053 s | 14.3× |
| `sort` | 1.135 s | 0.328 s | 0.099 s | 3.5× |
| `maps` | 0.369 s | 0.121 s | 0.080 s | 3.0× |
| `word_freq` | 2.965 s | 0.241 s | 0.204 s | 12.3× |
| `chip8` | 0.745 s | 0.098 s | 0.053 s | 7.6× |

**PGO** (`-C profile-generate` over the benchmarks and the fixture corpus,
then `profile-use`; `llvm-profdata` is in the LLVM install): +1.5% run time
over the suite (`word_freq` -7%, `maps` -4%, `lexer` -3%, nothing else
beyond noise) and -3% compile time. Below the design's 3% bar for a
build-system change: parked for V8, recorded here so it isn't redone.

**A C core, C natives, SIMD.** No measured case has a C native beating an
LTO'd Rust one, and the loop's cost is dispatch, which §6 of
`vm-review-v3.md` already assigns to V8. SIMD in the sense of vector
instructions in hand-written code isn't worth its maintenance here; the
part of it that is (`memchr`/`memmem` for text search) is a crate that's
already in `Cargo.lock`.

## 1. Decisions

1. **Safety first.** `find_int` copies `bytes` guarded by a
   `debug_assert`; `Ctx::text` makes a slice from a null pointer for an
   empty text. Both small, both fixed before anything is faster.
2. **The Lua backend, in stages.** Its arithmetic is a call per operator
   (`rt.add(T[1], a, b)`: a descriptor lookup, a policy closure and a raw
   helper). Stage one keeps the expression shape and replaces the call with
   a closure specialized per (type, operation, policy), made once: no
   change to unravel locations, to local counts or to the 200-locals
   machinery, and testable against `ty::int` by `rt_int.rs`. Stage two
   (inline checks as statements) only if profiling after stage one still
   shows arithmetic on top.
3. **Cheap Lua items, one commit each,** each with an A/B: buffered
   stdout in the host, `rt.filled` without a copy per scalar, `Str.chars`
   by the lead byte, `same_len` only where the body can change the list.
   `String.push_str` is quadratic (`s .. more`), but fixing it changes the
   representation every String operation depends on: a design of its own.
4. **Treadle's text natives as one batch:** `position` by `memmem`,
   `next_line` by `memchr`, ASCII paths in `next_char` and `push_char`, a
   fixed-size key read in `find_int`, one pass in `concat`, no copies in
   `split`/`replace`/`case`, `push_str` copying only on overlap. Each
   behaves exactly as `runtime/rt/core.lua` does. The verifier gets a
   bitset for instruction starts and resolves native signatures once (each
   build makes a VM since V4).
5. **Compiler, cheap items only:** callee sizes cached in the inliner, a
   free-register bitset and a two-pointer clash test in the allocator,
   borrowed keys in GVN, sets in `jobs.rs`. Per-function dirty bits in the
   pass driver are *not* done: GVN and inlining read callee summaries, so
   skipping an unchanged function is unsound unless a callee's change
   invalidates its callers.
6. **Not done, on purpose:** C natives; the C core (V8's); PGO (parked);
   lazy `std` (saves at most 0.5 ms of a start-up that's dominated by the
   process itself).
