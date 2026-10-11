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

## 2. What was done (2026-10-11)

Each by an interleaved A/B or a before/after timing, with the six gates and
the 20 000 generator seeds (which compare unravel locations on both
backends) after every change to the Lua backend.

**The Lua backend**, generated against the same benchmarks as §0
(`bench/RESULTS.md`; geometric mean **2.14×** faster than at the start of
the review):

| Change | Moved |
|---|---|
| Integer `+ - *`, their wrapping forms, `/ % << >>`, compound assignment and loop counters call a closure made for the type once (`rt.spec` → `O[n]`), not `rt.add(T[k], a, b)` | `fib` 2.1×, `spectral_norm` 2.3×, `chip8` 1.9× |
| An index check is `if i >= n then rt.oob(i, n) end`, not a call | `sieve`, `fannkuch` 2.5× |
| `(a, b) = (x, y)` and `let (a, b) = (x, y)` of plain values are one Lua multiple assignment, not a table made and taken apart | `mandelbrot` 3.5× |
| `as f64` is `v + 0.0` for what isn't a `u64`; list `.len()`/`.is_empty()` are a field | most loops |
| A copy of a record, tuple or sum is a function built for its descriptor (constructor source, a branch per tag), resolved once | `richards` 2.0×: copies were 80% of it |
| `rt.filled` doesn't copy scalars one by one; templates are a `..` chain; `chars()` takes the width from the lead byte | `sieve`, `word_freq` |
| Map `has`, `[k]`, `[k] = v` and integer casts are closures for the key or conversion type | `word_freq` 2.3× |
| `Slice.sort` compares with a function generated per descriptor | `sort` 2.0× |
| The host buffers stdout (32 KiB; each write on a terminal) and flushes before it waits, reads keys or ends | 300 000 prints 0.57 s → 0.25 s |

**Treadle:** `find`/`replace`/`split` by `memchr::memmem` (the crate was
already in `Cargo.lock`), `next_line` by `memchr`, an ASCII path in
`next_char` and `push_char`, `replace`/`case`/`repeat`/`split` without
extra copies, `push_str` copying only when its argument is in the buffer
that growing may move, and `concat` looking at each piece once: a
text-heavy program runs 37% faster (0.452 s → 0.286 s); the suite doesn't
move beyond noise. Two safety fixes: `find_int` no longer copies a
runtime-sized key past its buffer in a release build, and empty text never
makes a slice of a null pointer.

**The compiler and verifier:** the verifier keeps instruction starts in a
bitset and finds each native's signature once. It costs 55 µs on the
biggest image, so this is a tidy and not a speed-up.

## 3. What was found and not done

- **Bounds checks in generated Lua cost 17–23%** of `nbody` and `fannkuch`
  (measured by stripping the checks out of the generated bundle). Removing
  them needs a redundancy analysis (same list, same index, nothing in
  between that changes the list), which the Lua backend has no pass for: the
  largest remaining lever on that backend, and a design of its own. The
  same analysis would hoist `bodies[i]` loads out of loops (idiomatic Lua
  caches them in locals): `nbody` is 2.9× idiomatic Lua still.
- **Inline arithmetic checks as statements** (the advisor's stage two):
  `i += 1` is still a call. Worth doing only after the analysis above; local
  count and the 200-locals machinery are what it has to respect.
- **`String.push_str` on the Lua backend is quadratic** (`s .. more`): a
  change to what a String is there, which every String operation depends on.
- **PGO**: +1.5% run time, −3% compile time. Parked for V8.
- **The compiler's passes** (per-function dirty bits, a free-register
  bitset, borrowed GVN keys, inliner size caching): the allocator already
  binary-searches its fit and the biggest program compiles in 17 ms at `-O0`
  and 47 ms at `-O2`; the dirty bits are unsound without invalidating
  callers. None is an obvious win at the sizes that exist.
- **Map hash tags in Treadle's index** (§5 of the v3 review): a likely 5–10%
  on `maps`, in code that `heddle/build/map.rs` and `methods.rs` share, so
  all of it together.
- **Pre-existing, found on the way:** a nested generator's `defer` isn't run
  on the Lua backend when the outer one is dropped (filed; a cloud session
  has it), and `parse[f32]` parses as an `f64` and rounds, on both backends
  (not changed: the backends would then disagree).
