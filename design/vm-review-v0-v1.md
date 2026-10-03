# Review of Treadle V0 and V1 (2026-10-03)

A read-through of the V0/V1 code (`loom_vm`, `loom_isa`, `codegen.rs`, the
Heddle passes and builders, the host and interrupt plumbing, the tests and
the benchmarks), plus probe programs run on both backends. Nothing was
fixed: this is the list to work through later.

**Evidence tags.** *Reproduced* means a probe program showed it (numbers
are release builds, this machine). *Read* means it comes from reading the
code and wasn't run. *Unsure* means worth checking before acting.

Severity: **H** a wrong answer, a hang, or a cliff users will hit; **M**
a trap that costs real time or breaks parity; **L** polish, or only matters
later.

---

## 1. Findings, by priority

### H: reproduced

**H1. Integer map keys with trailing zero bits cluster badly.** *Reproduced.*
`Map[u64, u32]` with keys `i << 20` (20 000 of them): 911 ms on Treadle,
61 ms on the Lua backend. Keys `i << 12`, 200 000 of them: 1966 ms against
126 ms. Sequential keys and `i << 32` are fine.
`key_hash` (`methods.rs:459`, mirrored by the compiler's `mix` in
`heddle/build/hash.rs:21`) is `(OFFSET ^ word) * PRIME` folded by
`h ^ (h >> 32)`. A product's low bits depend only on the operands' low
bits, and PRIME is `2^40 + 435`, so the low `k` bits of the hash are
constant when the key's low `k` bits are, until the fold brings in bits
32+, which for small multiples of `2^k` are still zero. The index is
`h & mask`, so every such key starts probing at the same few slots.
Fix: a real finalizer on the one-word path (a murmur/splitmix `fmix64`) or
index by the high bits. The same shape is in `hash_into` for every part of a
structural key. `text_hash` (single multiply plus fold) is better but worth
the same test. Add a map benchmark with aligned and clustered keys.

*Fixed: Map keys go through splitmix64's mixer; i << 12 keys: 1966 ms to about 100 ms (bench/maps)*

**H2. `Map.remove` / `Set.remove` is O(n).** *Reproduced.* 30 000 removes
from a 30 000-entry map: 4.8 s on Treadle, 14.7 s on the Lua backend.
`remove_entry` (`heddle/build/map.rs:294`) moves every later entry down and
rebuilds the whole index (`reindex`). Any queue/cache/visited-set that
removes in a loop is quadratic. Fix: tombstones in the entries array (the
index keeps entry numbers) and compact when half are dead; keeps
insertion order. The Lua backend needs the same change (`runtime/rt/map`).

*Fixed: Removal is O(1) on both backends (tombstones, compacted once half are removed ones or before a walk): 30 000 removes in 0.17 s on Treadle, 0.05 s on Lua*

**H3. A runaway recursion takes 5 s and about 1 GiB on Treadle.**
*Reproduced:* `fn down(n) { 1 + down(n + 1) }` unravels with `stack
overflow` after 5.2 s (Lua: 124 ms). The default `stack_limit` is 1 GiB
(`lib.rs`, design §9.2/P7), so the stack really fills a gigabyte (about 1000
segments, each zero-filled), then `trace_at` (`exec.rs:1260`) builds a
`Vec` of every frame (tens of millions of entries) and `unwind` walks them
all. The most common bug in a recursive program is the slowest to report.
Fix: a much smaller default (64–256 MiB, or a call-depth limit), and keep
only the first and last N frames of the trace. `fixtures.rs` already runs
with 64 MiB, so the default isn't what is tested.

*Fixed: A stack overflow takes 0.3 s: 256 MiB default stack, and a backtrace of the first and last 64 frames*

**H4. A local array over about 512 KiB fails to compile for Treadle.**
*Reproduced:* `var a = [0u8; 600000]` gives `U0102: not implemented yet:
`main` needs more than 65535 registers` (works on Lua). Frames are
addressed by `u16` register numbers (`codegen.rs:183,213`) and arrays live
in the frame. Two problems: it's a parity hole V1 shouldn't have, and the
message ("not implemented yet") describes a hard limit. Fix options: large
slots (say over 4 KiB) go to the heap and are freed by ownership; or a
proper diagnostic. The same limit bites a function that inlines many
callees (see M9).

*Fixed: Frame slots over 64 KiB live on the heap (allocated in the prologue, freed at a return and at the end of every landing pad); a 600 KB local array runs. The remaining 65 535-register limit's wording is card 9*

**H5. On the Lua backend, Ctrl-C isn't delivered inside a coroutine.**
*Reproduced:* a shuttle body that loops without yielding, with
`LOOM_INTERRUPT_AFTER_MS=300`: still running after 8 s (killed by
`timeout`). A loop in `main` is interrupted in 350 ms on both backends.
`lua_sethook` (`loom_host/src/run.rs`) sets the hook on the main
`lua_State` only; hooks are per thread, so a running coroutine never sees
it. The V1 note "Ctrl-C works on both backends" is true only outside
shuttles. A second real Ctrl-C then exits 130 without running defers.
Fix: set the hook on the running coroutine too (the runtime knows it:
`coroutine.running()`), or have `rt`'s resume wrapper install it. Treadle
V2 must tick inside shuttle tasks (see §4).

*Fixed: On the Lua backend the interrupt hook goes on every live shuttle's coroutine too; a shuttle that never yields stops in under a second*

**H6. An interrupt that arrives before the Lua watch is registered is lost
(Lua backend).** *Reproduced:* `LOOM_INTERRUPT_AFTER_MS=0` on a
`loop {}` hangs (killed at 4 s); 1 ms and 3 ms work. `interrupt::watch`
(`loom_sys/src/interrupt.rs:66`) only registers a callback; if the flag is
already set the callback never runs. Treadle polls the flag, so it can't
lose one. Fix: `watch` fires the callback at once when the flag is set.
Narrow in practice, but it's a lost wake-up on the kill path.

*Fixed: An interrupt before the Lua watch is registered is no longer lost*

### M: reproduced

**M1. `File.write_line` reopens the file on every call (both backends).**
*Reproduced:* 20 000 lines, 1.67 s on Treadle, 1.84 s on Lua (about 85 µs
a line, mostly Windows file open/close). `host.rs:122` (`append` opens with
`OpenOptions` each time); `File` holds only a path. A log loop is slow, and
anything that watches the file sees each write as a separate open. Fix: keep
an open handle in the host keyed by path (flushed at exit/unravel) or make
`File` hold a handle id.

*Fixed: A File stays open between writes (at most 64 open): 20 000 lines in 0.20 s on Treadle (from 1.7 s), 0.06 s on Lua*

**M2. `s.replace("", x)` and `"".split(",")` differ from Rust.** *Reproduced,
and both backends agree.* `"héllo".replace("", "-")` returns the text
unchanged (Rust gives `-h-é-…`). `"".split(",")` shows `[]`; Rust's is
`[""]`. Decide what the language means, write it in the std docs, and pin
it with a fixture (`methods.rs:127,160`).

**M3. Text methods are ASCII-only.** *Reproduced, both backends agree:*
`"héllo wörld ß".to_upper()` leaves `é`/`ö`/`ß` (`char` methods like
`is_alphabetic` weren't run: check them too). `methods.rs`'s header says so; check the std
docs and the language reference say it too, since the spec reads as if text
is Unicode (`char_count`, `chars()` and slicing are).

**M4. The unravel location is the statement, not the expression.**
*Reproduced, both backends agree:* an out-of-bounds slice in `let m =
s[2..4]` reports at the `let`; `return xs[5]` reports at `return`. A call
reports at the call. Fine for a V1 pin, but a caret under the faulty
expression is what people expect; it also applies to the backtrace lines.

### M: read

**M5. The checked build's leak check is skipped when the run doesn't finish
normally.** *Read.* `loom_vm::run` calls `vm.leaks()` only for `Ok` and
`Failed` (`lib.rs`). An unravel, `process.exit` or Ctrl-C outcome isn't
checked, but unwinding is the most error-prone ownership path (landing pads
that drop partial state). Most `unravel_*` fixtures and the generator's
unravels therefore don't verify they free everything. Fix: check leaks after
every outcome, in the checked build (an unravel inside a `defer` during an
unwind is the case to test).

*Fixed: The checked build checks for leaks after every outcome (found and fixed: a half-collected list leaked when its stream unravelled)*

**M6. Hashes are deterministic and unseeded.** *Read.* A program that
builds a map from untrusted keys can be driven into long probe chains.
Because maps iterate in insertion order, a per-process random seed
wouldn't change any output (only `x.hash()`, which D8 already lets differ).
Worth deciding before V5/V7, when programs take network input.

**M7. Natives can't be interrupted.** *Read.* Ticks happen only at calls
and loop back-edges. `time.sleep(huge)`, a very long `str.repeat`,
`sort` of a huge list, a regex-like `position` on big text (naive O(n·m),
`methods.rs:59`) and `probe` (`methods.rs:466`, loops until an empty slot:
an index with no empty slot would spin forever, uninterruptibly) hold the
loop past Ctrl-C. Sleep should wait on the interrupt (a condvar/event with a
timeout); `probe` should be bounded by `slots`.

**M8. Terminal and output state is lost on the hard paths.** *Read.* A
second Ctrl-C calls `std::process::exit(130)` from the handler thread
(`interrupt.rs:44`): the 32 KiB `Ctx.out` buffer is lost and raw mode isn't
undone. `Term` has no `Drop` guard (`term.rs:67` is only called on normal
exits), and a Rust panic (`main.rs:158`) exits with `ExitCode::FAILURE` (1,
the same as "main returned an error"; the plan says internal errors are
70) with the terminal still raw. Fix: a Drop guard for raw mode, a panic hook
that restores it and exits 70, and flush on the hard exit.

**M9. Inlining adds frame slots per call site.** *Read.* `inline_into`
appends the callee's slots to the caller for every inlined call (no sharing
between sites that can't be live together). A big `main` with hundreds of
inlined `to_string`/format helpers grows its frame towards the 65 535 limit
(H4) and makes each recursive call's frame bigger. Fix: reuse slots by
lifetime, or only inline callees whose slots are small.

**M10. Silent truncations in the encoder.** *Read.* (a) The format item
number is cast `as u16` (`heddle/build/mod.rs:1064`, and `CallN`'s `c`
field), so the 65 537th distinct item wraps and formats the wrong thing.
(b) `CallR`'s arity check packs `params << 8 | results` into 16 bits
(`codegen.rs:1690`): 256 or more parameter or result registers (a big tuple
passed to a lambda) corrupt the check and a correct call hits `crash 2`.
(c) `CallN`'s native index and `Native` windows are `u16`. All should be
checked and turned into a diagnostic.

*Fixed: a frame over 65 535 registers (allocate used to overflow and panic) and a function value called with over 255 arguments or results are U0103 "over one of Treadle's limits"; more than 65 536 formats too. (b) was already guarded, though reported as "not implemented yet".*

**M11. `loom test --backend vm` shares one `Machine` across tests.** *Read.*
`Vm::call` reuses the stack, heap, `Ctx.msg`/`bufs` and the lazily computed
constants in `data`. A test that unravels leaves partial sinks in `bufs`
(handles are positions, `natives.rs:150`) and anything it leaked. Probably
harmless today; will matter when a test timeout or threads (V5) arrive.
`Env.stopping` also resets per `execute` while the interrupt flag stays set,
so a second `call` after an interrupt would be interrupted again.

**M12. Message buffer reentrancy.** *Unsure.* `msg` (sink 1) is one global
`Vec`. A template being built into an unravel message whose argument
unravels (or another message build begins) leaves stale text that the next
`unravel` appends to. Check whether the compiler always evaluates the
arguments into registers before the first `msg.*` call; if so, say so in the
code.

### L

- **`fs.read` doesn't validate UTF-8** (`host.rs:156`): a binary file read
  as `String` becomes invalid text. The natives never use unchecked `str`,
  so it's safe, but `chars()`, `{s:?}` and slicing then see U+FFFD or odd
  byte runs. Decide: validate, or document `String` as bytes-with-a-promise.
  It also reads the whole file, then copies it into the heap (2x peak).
- **`parse[f32]` rounds twice** (`methods.rs:269`: parse as `f64`, then `as
  f32`). A decimal near a half-way point can differ from a direct `f32`
  parse. The Lua backend does the same, so they agree, but both are
  slightly wrong. Same for `float.math` on `f32` (`powf` in `f64`).
- **`{s:.3}` on text ignores the precision** (`fmt.rs` takes it only for
  finite floats); Rust truncates. `{x:?}` doesn't escape non-printable
  Unicode as Rust does.
- **`string.of` and `push_char` replace an invalid `char` with U+FFFD**
  silently (`methods.rs:322,348`); a `char` can't be invalid, so these should
  be unreachable (crash or debug-assert instead).
- **`map_find_int` copies `bytes` into `[u8; 8]`** (`methods.rs:520`): a key
  field over 8 bytes would overflow the copy. The compiler only passes up to
  8, but nothing asserts it.
- **The Lua interrupt hook is set from another thread** (`run.rs`,
  `lua_sethook` called by the Ctrl-C handler thread on Windows). The
  standalone `lua` does it from a signal handler; here it's a real
  cross-thread write to `L->hookmask` and the `trap` fields while the VM
  runs. It works in practice; it isn't formally sound. A flag the VM
  polls (a cheap check the runtime makes at loop heads) would be.
- **`Heap::realloc`'s same-class path doesn't re-check `LIMIT`**
  (`heap.rs:176`), so `live` can pass it by up to a quarter of an object;
  the next `LIMIT - self.live` would underflow. Never reachable in practice.
- **Heap never trims or recycles pages across size classes.** Allocating
  many 32-byte objects, freeing them, then allocating 48-byte ones doesn't
  reuse the memory. Fine for V1; a long-running program could ratchet.
- **`Heap::drop` frees every page**, so an object freed into a different heap
  than it came from is a use-after-free (see V5 below).
- **Doc comments on `Ctx::owns` and `Ctx::text` are swapped**
  (`natives.rs:95–109`: the `# Safety` text sits on `owns`; `text` has none),
  and `run.rs:78` (`stop`) and `methods.rs:485` have no `// SAFETY:`. 96
  `unsafe` sites in total, these are the 3 real misses (the script also
  matched `unsafe` in the parser's AST printer).
- **`ints::cast_t`/`cast_s` use `i128`** and the `Cast*` handlers decode the
  extension with `unpack_cast(..).expect(..)` on every execution
  (`exec.rs:854–868`); see P2.

---

## 2. Performance, not yet a bug (candidates for V3/V8)

| # | What | Where |
|---|---|---|
| P1 | The tick is a load/decrement/store in `Env` (memory) on every loop back-edge and call; the docs say "a local" (see §5) | `exec.rs:280` |
| P2 | Narrowing casts (`as u8`, `as u32`, `as%`…) go through generic `CastT/W/S`, which re-validate the extension word and use `i128`. Specialize the common widths (`as% uN` is an `AndK`/`Mov`) and pre-decode | `exec.rs:854`, `ints.rs:218` |
| P3 | Each interpolation allocates: `out_fmt`/`sink_fmt` start with `Vec::new()`, `int_text`/`radix`/`float_text` build `String`s. `fmtp` (300k `"{i}:{i*3}"`) is fine today, but print-heavy code pays; use `ctx.spare` and `itoa`-style writers | `natives.rs:252,353`, `fmt.rs` |
| P4 | `str.find/replace/split` are naive O(n·m) (`windows().position`), and `split`/`push_str` copy their inputs first (`to_vec`) though allocation can't move them. Use `memchr`/two-way search | `methods.rs:59,161,355` |
| P5 | `hoist_constants` lifts every loop constant to the function entry, so a function that returns before its loop still pays all the `ldi`s per call, and the constants hold registers across the whole function and its calls. Hoist to loop preheaders; dedupe equal constants (each `Iconst(0)` is its own value) | `codegen.rs:394` |
| P6 | No CSE, copy propagation or dead-store removal beyond `tidy`; the frontend emits many redundant `Offset`/`Index`/loads (the V3 plan, but note constants are the cheapest first win) | design §7 |
| P7 | The allocator's liveness uses `BTreeSet<Value>` per block to a fixpoint, and `emit_func` may run twice (`Retry`), plus `inline` clones the whole module. Compile time is flat on the benchmarks (≈8 ms for `loom run`); check a generated 50k-line `main` before V3 adds passes | `codegen.rs:743` |
| P8 | `CallN` looks the native up in `env.natives[b(w)]` (bounds-checked, indirect call, argument slice built). Hot natives (`str.eq`, `map.find_*`, `mem.alloc/free`) could be dedicated ops or handler-inlined | `exec.rs:903` |
| P9 | `Zero`/`Copy` match on 8/16/24 bytes and otherwise call `memmove`; the common 32/48-byte warps take the call | `exec.rs:1030` |
| P10 | Frame zeroing: segments are `vec![0; 1<<17]` (1 MiB each) per stack; see V2 | `stack.rs:42` |

---

## 3. Development traps (things that will bite while working)

1. **The speed depends on LLVM layout luck, and the guard rails are thin.**
   The 30% swings the V1 work fought are controlled by `-C llvm-args=
   -tail-dup-*` in `.cargo/config.toml`, but:
   - those flags are under `[target.x86_64-pc-windows-msvc]`, so a
     Linux/macOS build gets none of them (and slower code, silently);
   - setting the `RUSTFLAGS` environment variable replaces config-file
     rustflags, dropping them with no error;
   - they're LLVM-internal options that can be renamed or removed in a new
     LLVM;
   - `rust-toolchain.toml` says `channel = "stable"`, so every upgrade can
     re-roll the layout.
   Add a test that fails when the flags aren't in effect, pin (or at least
   record) the rustc version in `RESULTS.md`, and add an `--ignored` timing
   check against the bundled Lua with a ratio threshold, since
   `bench/counts.txt` counts instructions only and can't see a layout
   regression (or an op that got more expensive, like H1).
2. **The baseline is a Windows clang-cl build of PUC Lua 5.4.** clang-cl
   doesn't define `__GNUC__`, so `lvm.c` uses its `switch` dispatch, not
   computed gotos; a typical Linux `lua5.4` is faster. "1.65× PUC Lua" is
   against this build. Record the Lua build flags in `RESULTS.md`, and rerun
   against a gcc build before quoting the number outside the repo. (The
   timing also includes Treadle's front end, about 8 ms of `fib`'s 38 ms,
   which understates Treadle on the short benchmarks.)
3. **The benchmark set misses the cliffs.** `word_freq` has 64 distinct
   keys: no growth, no removal, no aligned keys (H1, H2). Nothing covers
   `sort` of strings, string building, `lines()` on big text, deep
   recursion, or a map with a million entries.
4. **The program generator covers integers, `bool`, `f64`, one record, one
   sum, lists, optionals, `?`/`mend`/`defer`.** It doesn't generate text,
   maps/sets, lambdas, `any`, streams or wefts, which is where V1's ownership
   code (drops, copies, moves of heap values) is hardest. Extending it with
   `String`, `List[String]`, `Map[String, _]` and closures is the best use of
   the next hour of test work. (My probe with lists of records, maps of lists,
   `any`, early `?` and `defer` ran clean on the checked build and agreed
   with Lua, so this is about coverage, not a known failure.)
5. **Verifier scope.** It guarantees registers, jumps, calls and frame
   offsets only. Every load, store, `copy`, `zero`, `mem.*` and `free` trusts
   the compiler; a codegen bug there is undefined behavior in a release
   build and only the `checked` build sees it. Keep the checked gate and
   the generator on checked; say plainly in the design that an `Image` is
   trusted input (matters if images are ever saved to disk, V9). Round-
   tripping pointers through `u64` registers is fine under exposed
   provenance; `.expose_provenance()`/`with_exposed_provenance` would make
   it explicit, and Miri (the risk table lists it) hasn't been run.
6. **Two `unsafe`-heavy features lean on one hack.** `black_box(&mut env)`
   (`exec.rs:198`) keeps `Env` out of registers; if the compiler stops
   respecting it the loop slows by a lot with no failing test (see 1).
7. **`.vm.out` files are the only allowed output differences;** the harness
   can't tell a *new* difference from an old one in the same file. Keep each
   comment specific to the row, and keep P6's usize 63/64 split the only
   width difference.
8. **Exit codes.** Panic = 1 (M8), `process.exit(n)` passes `n` through, so
   `process.exit(1)` is indistinguishable from `main` returning an error and
   `process.exit(101)`/`130` from an unravel/Ctrl-C. Probably acceptable
   (that is what `exit` means); the table in `lua-backend.md` should say so.
9. **The compiler runs on a 64 MiB thread** (`main.rs:150`) to survive deep
   recursion; the known recursive-warp overflow shows the real fix is a
   depth guard or an iterative walk, not more stack.
10. **Docs drift** (§5).

---

## 4. Look-aheads: what V2, V5 and V6 will trip over

- **V2 (shuttles): stack size.** `Stack::allocate` uses at least `SEGMENT` =
  `1 << 17` slots (1 MiB) for every segment, and `Stack::new` allocates one
  per task. A program with 10 000 live generators would commit 10 GiB. A
  task's first segment needs to be small (4–16 KiB) and grow.
- **V2: the interrupt flag** is read through `Env.interrupt` and
  `Env.stopping`, per `execute`. Shuttle tasks that run their own loop need
  to tick against the same flag, and `stopping` has to be machine-wide, not
  per task (H5 is the Lua version of the same bug).
- **V2: `unwind` and `trace_at` assume one linked stack** ending in a null
  return address. A generator's frames live on its own stack: an unravel
  inside one has to unwind into the consumer and run the consumer's pads,
  and the backtrace has to span both.
- **V2/V5: lazily computed constants** write into `data` on first read
  (`Gaddr`); with several tasks that's a race unless the flag write is
  atomic or the constants are computed at load.
- **V5 (threads): the heap is single-threaded by construction.** Free lists
  are per heap, objects have no header, and `Heap::drop` frees pages.
  Ownership that crosses threads (channels, `join` results) frees into the
  wrong heap's lists, and a heap dropping first leaves dangling free-list
  pointers (use-after-free). Choose before V5: one heap with a lock, per-
  thread heaps with remote-free queues, or pages that outlive threads.
  `LIMIT` and `live` also become shared.
- **V5: `Ctx` is `!Send`** (`Output::Capture(Rc<RefCell<..>>)`, the `msg` and
  `bufs` sinks, `term`, files); each task needs its own sinks (M11, M12).
- **V6 (spools, `given`):** the heap's "free with the size you allocated
  with" contract has to survive arenas, where nothing is freed one by one.
- **Interop (V7):** `Env`/`Ctx` hold raw pointers into the `Machine`;
  re-entrancy (Lua calling back into Treadle calling Lua) needs `Machine`
  to be re-enterable, and today `call` takes `&mut self` with one stack.

---

## 5. Docs that are out of step with the code

- `exec.rs:1–25` (module header) says `pc`, `fp`, `seg_end` and `ticks` live
  in locals. Only `pc` and `fp` do; the others are in `Env`.
- `design/vm.md` §9.1 ("the tick counter stays in locals") and §9.5 ("one
  decrement and one predicted branch") describe the old shape; the tick is
  now a memory read-modify-write (P1). §9.1's sketch also predates the
  labeled-trap-block structure.
- `design/vm.md` §19's risk row names Miri; it hasn't been run (§3.5).
- AGENTS.md item 23 says "the checked build leaks nothing"; true for runs
  that finish, not unravels (M5).
- `design/lua-backend.md` says the Lua interrupt "checks every 10 000
  instructions"; since the hook rewrite it's an asynchronous hook installed
  on Ctrl-C, and per-coroutine delivery doesn't work (H5).
- `bench/README.md`/`RESULTS.md` don't record the Lua build (§3.2) or the
  rustc version.
- `std` docs (`docs/`) should list what `to_upper`, `trim`, `is_alphabetic`
  and `replace("")` really do (M2, M3).

---

## 6. Test gaps worth filling

Fixtures for H1–H4 and M1–M2, each with the expected time bound where it
matters (a `tests/run` fixture can assert a count, not a time; use
`bench/` plus the timing check from §3.1). Also:

- a leak-after-unravel check in the checked build (M5);
- an interrupt inside a coroutine and an interrupt at 0 ms (H5, H6);
- more than 65 535 registers, more than 255 closure parameters, and more
  than 65 536 format items, each expecting a clean diagnostic (H4, M10);
- keys that are multiples of `2^k`, and a million-entry map;
- invalid UTF-8 through `fs.read`, and `Heap` unit tests for realloc across
  the small/big boundary and against `LIMIT`;
- `loom test --backend vm` where an early test unravels with `defer` and a
  later one builds an unravel message (M11, M12).

---

## 7. What held up

Worth keeping as is. The numbers agree with the Rust oracle and with Lua on
every edge probed: `i64::MIN / -1`, shifts, `u64::MAX` casts and ranges
including inclusive ranges ending at the type's maximum, float printing
(`-0`, `NaN`, `1e300`), `parse` edge cases, mid-character slices (both
unravel). Ownership held on a program mixing lists of records, maps of
lists, `any`, `?`/`mend` early exits, `defer` on an unravelling deep call,
`truncate` and replacement of owned elements, with the checked build
reporting nothing. Every `unsafe` in the interpreter loop is justified by the
verifier, the verifier has tests for each rejection it makes, and
unsupported features fail with `U0102` rather than miscompile. Ctrl-C in
`main` unwinds `defer`s and exits 130 on both backends.
