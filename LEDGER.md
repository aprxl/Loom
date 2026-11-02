# LEDGER

Running log of milestone work and decisions. Newest first. Replaces `ledger.log`.

## V6 review fixes and open items (2026-10-22)

Done, each its own commit with fixtures on both backends where the behaviour is visible: `mem.swap`/`replace` move spool pointers by reference (9d04867); `owns_heap` looks into user warps, and `put` rejects text, lists and boxes (8b8b6df); the region checks cover `SharedArena` and `Pool` through `#spool_put`/`#spool_reset` markers, and an unknown attribute is N0101 (6ccdd31); documented but unbuilt attributes give U0102 (4d30018); text copied into a spool is typed by its region, `String@sp`, so R0101 and R0102 apply to it (c638e67); a zero-slot `Pool` has no slots (82c3e31); arena sizes are checked and an arena past 8 GiB unravels out of memory on both backends (a865299); the cleanup items (a4f36ea, 7779f67, 5f911e2, 283860f) and one builder for the hidden region parameter (fafedcc, byte-identical Lua, Heddle and bytecode across the corpus); the `usize` maximum literal is by design, Lua's `usize` is 63 bits and Treadle's 64, so `tests/fail/usize_literal.loom` pins it (f0762ab); `std_counts` asserts its setups build N items (f10b8e1); Lua shares one vtable for spool and heap text (468df7f, Treadle unchanged); a `Pool` on Lua takes its slot size from `loom_vm::slot_size` and unravels past 8 GiB as Treadle does, with `pool_huge`, `pool_past_limit` and `pool_under_limit` (Treadle's bytecode and Heddle for the old corpus unchanged).

Open:
- Item 14 (closed, unreachable): no Loom type is aligned past 8 bytes. The widest integer is 8 bytes (`int_bytes`), a scalar's alignment is its size, and handles and headers are 8. So the Treadle guards on `Pool`, `put` and arena allocation could not fire, and Lua accepts every type Treadle does. The guards are gone; a `debug_assert` in `Layouts::of` (ty/layout.rs) trips if a future type breaks the 16-byte bound.
- Not tracked: `Pool.free` doesn't give R0102, and a copy of a spool pointer survives `free` (`^T@s` is `Copy`). See Q11.
- `compile_at alone` (`compile_time`, printed, not asserted): 2.22 at 37f24c1, 2.26 at HEAD, against a 1.5 target. The 1.63 recorded at V5 close isn't reproduced by this measure. Not investigated.
- The perf gates pass (bench/RESULTS.md, 821e7cc).

## V6.5 and V6 loose ends (2026-10-22)

Done: `std.mem.swap`/`replace`/`take` (builtin free functions; both backends; `tests/run/mem_swap`); the arena-mutation rule (T0107: a field behind a pointer into a spool is read-only unless the target is plain data); the hand-written `Show` on std warps was ignored because std weaves of plain types were never checked (now checked for the types a program uses, and std may weave its own wefts into its own types), so `Duration` has `Show`; `Default` for tuples of four and five; `Default` for arrays `[T; N]` and `mem.take` on arrays (the weave matches once a length is interned: `tests/run/std_default_arrays`). Still open from V6.5: `Self` for numbers (`pow_checked`, `abs_diff`, ...), `where T: [U]`, generic wefts, `collect_as` errors at the call site, `Fmt`. Still open from V6: storable closures, I/O and sleep as cancel points, `loom(given: pool)`, `yield`/`pull` in a `loom` body (done, 6d47848).

## Closures without captures (2026-10-22, Caio chose option B)

Done: `std.iter.from_fn`, `successors` and `Stream.scan` take plain function values (`take f`); the state travels as an argument and a result (`S => ?(S, T)`). A function value is `Copy` now (it is a copy of a borrow; capturing lambdas still can't escape, M0106), which lets a warp hold one and call it (`let g = self.f; g(x)`). Owning closures (`^(…)` boxes with their own drop) are deferred until user code needs to store a capturing closure. `inspect`-style steps stay builtins.

## V6 slice 4: pools, shared arenas, threads (2026-10-21)

Done (both backends; `tests/run/spool_pool`, `spool_pool_full`, `spool_shared`, `tests/fail/region_threads`, `spool_collections`): `Pool[T]`, `SharedArena`, R0103, the guard against collections in spool warps, `bench/pool_cycle`. Left out on purpose: lists, maps and sets that grow in a spool. A list's header has no room for the spool it belongs to, and the pushes the optimizer inlines compare `len` with `cap`, so the spool-number-in-the-capacity-word of vm.md §12 would put a mask on every push. The cheap sound version is what is built (such a warp is refused); the real one wants a region on `List` like the one warps have, which is a decision for V8's collections work.

## V6 slice 3: given in general (2026-10-21)

Done (both backends; `tests/run/given_values`, docs sample `own_given`): `given name = value` for any weft's type, `#given_default(Type)` on a weft (the call makes the default with the type's `default`), the gate that an unused `given` costs 0 dispatches (`bench_counts.rs`, `given_plain` and `given_unused`). Not done: inferring the `given`s of private functions (the design's rule; today every function spells them, which is the honest `pub` rule applied everywhere), `yield`/`pull` in a `loom` body (done, 6d47848), the ambient `Rng`/`Clock`/`Logger` of std (the mechanism is in).
Done (2026-10-23): ambient `Writer` for `print` (`Stdout` default, `Buffer`); a `print` with none given compiles as before (`tests/run/given_writer`).
Done (2026-10-24): `loom(given: Crew(workers: n))` caps a loom's running threads (`tests/run/loom_crew`).

## V6 slice 2: spools, regions, given spools (2026-10-21)

Done (both backends; `tests/run/spool_basic`, `spool_tree`, `given_spool`, `tests/fail/region_errors`, `tests/examples/calc`, docs samples `own_spools`, `own_err_region`): `Arena`, `Heap`, `spool`, `put`, `reset`, `^T@s`, hidden region parameter on warps with pointers, `given sp: Spool` and `given:` for spools, `ref warp`, the roots pass (R0101, R0102, R0104), arena-owned `String`s. `examples/calc.loom` runs and leaks nothing in the checked build. Also fixed: on the Lua backend `a is .X(x) and b is .Y(y)` bound `y` in a nested block (it was nil in the body). Gates: an arena `put` is one op (`arena.k`) and costs 0.39 of a heap box (the design's "10x" is for the allocation alone: about 1 ns against 7; asserted as half of the whole), `arena_trees` is 9x PUC Lua (gate 4x), a box 10 ns (gate 15 ns); `bench/RESULTS.md`. Not done: R0103, collections in a spool.

## V6 slice 1: heap pointers (2026-10-21)

Done (both backends; `tests/run/ptr_basic`, `ptr_more`, `tests/fail/ptr_errors`, `docs/lang/samples/own_pointers`): `^T`, `^e`, `p^`, auto-deref through fields, methods, patterns, templates, `==`/`<`/hash, recursive warps, `?^T`, assignment through a pointer, moving out of a box by pattern. See design/v6-spools.md for the model and the slices ahead. Gates: allocate, read and free a box 10 ns (gate 15 ns, `perf_gates.rs`); no existing `bench/counts.txt` row moved. Not done: `.copy()` on a pointer, the whole-target move of a `once`, `ref warp`, regions. Decision pending Caio: arena pointers read-only unless the target is plain data (open question 1, option c).

## Roadmap: deferred work by milestone (2026-10-20)

Built from the ledger below, `design/stdlib-plan.md`, `design/stdlib-collect-default.md`, and `design/vm-review-v3/v4/v5.md`. Caio is on Treadle (V6 next); the stdlib is paused, so "Std backlog" means nothing blocks it and it waits for a stdlib session. Judgment calls are marked (J).

**V6 (spools, regions, `given`, storable closures)**
- I/O and `time.sleep` as cancel points (vm.md 11.4; sleep is outside cooperative cancellation today).
- Storable closures: `scan`, `from_fn`, `successors`, and any adaptor that keeps a user callback; `inspect`-style steps move from builtins to Loom once a lambda can be stored in a warp field or returned (M0106).
- `mem.swap`/`replace`/`take`/`drop` (needs a rule for moving out of a `mut` parameter; lands with the ownership rules of regions). (J: here, not a separate item, because the rule is a move/region question.)
- Stdlib phase 6: `Spool`, `Arena`, `SharedArena`, `Pool[T]`; spool-aware collection constructors and `copy`; `Deque`/`SortedMap` nodes in a spool.
- `given` defaults (D7, a declared default per weft) and the ambient `Writer`, `Rng`, `Clock`, `Logger`, `Pool` givens; `loom(given: pool)`, `yield`/`pull` in a loom (V5 leftovers; done, step 4).
- Gates: the V6 rows of vm.md section 17 and 16 (`binary_trees` at least 4x; `Arena` alloc at least 10x `heap`, `Pool` alloc/free at most 5 dispatches, an unused `given` costs 0 dispatches).

**V6.5 (new): language gaps the stdlib hit.** Small typeck/weave items, none about memory, so they don't belong in V6. (J: a milestone of its own after V6, because V6 rewrites the checker's ownership side and these touch `Self`, bounds and weaves; doing them in the same pass risks conflicts. Fold into V6 if the checker work overlaps anyway.)
- `Self` for `Int`/`Float` and gauge types (the `self` of a Loom method body is the warp, not the number): unblocks `pow_checked`, `abs_diff`, `to_string_radix`, `total_cmp`, `to_bits`/`from_bits`, `to_be_bytes`/`from_be_bytes`.
- Expressing "items are slices" (`where T: [U]`) for `concat`/`join` on slices of slices. (done: shape bounds, and `concat` and `join` on `Slice` (method lookup skips it when the items can't be slices, so text `join` still applies).)
- Generic wefts (`FromStream[T]` is an associated `Item` instead). (deferred by Caio 2026-10-23.)
- A hand-written `Show` on a warp declared in `std` is silently ignored (the debug form is used; `Duration` has `to_string` because of it). Bug: find the cause in the weave recording; give `Duration` its `Show`.
- `Default` for arrays `[T; N]`, `Deque`, tuple sizes beyond three, and tuple warps (`warp Meters(f64)`).
- `collect_as`: item-type errors are reported inside std, not at the call site; `str` items for `String`; `BitSet`.
- `Fmt` specs/`pad`/`debug_struct` and `dbg` (the compiler passes the source text and location; the writer comes from the ambient `Writer` given, so after V6). (done: `dbg`, to stderr; `Fmt.spec`/`pad`/`debug_struct` deferred by Caio 2026-10-23; spec padding on user `Show` already works.)

**V7 (Lua interop)**: deferred indefinitely (Caio, 2026-10-25). Std's host externs are `extern "host"` and need no Lua.

**V8 (speed): COMPLETE (2026-11-02, Caio).** All Treadle perf work; the numbers behind each line are in `bench/RESULTS.md` ("V8 summary" at its top). Geometric mean against PUC Lua 5.4 at the end: 2.73x, lowest 1.15x (`word_freq`); `design/vm.md` section 16's V8 row (4x, nothing under 2x) was not reached, and V8 closes on the list below instead.

Done (commit, headline):
- A block's tail local hands its slot to the join, no copy and zero (093f6eb): `list_ops` 7,628,190 to 5,632,953 dispatches (-26%).
- Fused `filter_map`, `step_by`, `inspect` (d0d8532) and fused loops that read lists, slices and arrays in place (def0e51): `step_by`.sum over the hand loop 25.75x to 2.00x, `filter_map`.sum 8.60x to 2.00x; guards at 2.4.
- GVN carries known words across a `copy` (311d5cf): list streams devirtualize; `min_by_key` 8.00x to 6.50x, `counts` 2.53x to 2.18x.
- Bounds checks in std code measured and closed (96ef588): `list_ops` keeps 80.6% and `heap` 36.4% of them, but they are fused into checked loads and stores, so removing them saves almost no dispatch.
- Follow-up `sccp` and `gvn` only on the functions changed since the last full pass (a72212a): optimizer passes -7.2%, `compile_at` 552 to 532 ms; and GVN reuses modref's aliases (9ef9735, counts unchanged).
- Natives off the pool lock: read-only text and map natives (2a1a2c0: `contains` on 8 workers 1,440 to 82 ms, gate `par_text` 6.4x), string building on the worker's heap (4ad1cca: `par_build` on 8 workers 6,639 to 125 ms, gate 6.0x), an uncontended mutex (6465ebb: gate `par_mutex` 5.0x), templates, messages, `str.new_in` and constants on the worker's own buffers (25040fb: 8 tasks of templates 3,897 to 93 ms, gate 3.6x), per-worker print queues (322f456: 800,000 lines a task to a file on 8 workers 958 to 140 ms, gate 3.2-3.7x).
- `loom for` over lists, slices, arrays and their chunks (8316439): gate `par_chunks` 8.1x.
- Per-worker ready deques with stealing of half (c59ae69): a thread on every call of `fib(25)` on 8 workers 914 to 244 ms (3.7x); and a free list of task stacks (66d5d40): 121,392 stack allocations to 24-119.
- `Map.first`, `Map.last` and `Set.pop` in O(1) (f0c2252): a 100,000-entry stack over `last` went from over 100 s to 84 ms; native map `reindex` and `compact` (d238381): `maps` -25% and `set_drain` -33% dispatches (`set_drain` 6,786,096 to 3,250,121 over V8).
- A `spool` (or named `given`, or `const for` pattern) made in a `loom for` body is the task's own (6f5c072): a segfault and a shared arena fixed; arena `put` on 8 workers 1,330 to 136 ms.
- Gates: `par_mandelbrot` 0.65 a core and templates 3.5x (237c9b9), `print_scales_to_8_workers` 3.0x (322f456), `compile_at` alone at most 2.15x (8552814; source to image 1.25x, `compile_at` 1.95-2.11x, the design's 1.5x deferred).

Deferred: see "Later (deferred from V8)" below.

**Later (deferred from V8; none blocks anything, each has its measured payoff)**
- Map hash tags in the index (the high 32 bits of the hash beside a 32-bit entry number in each slot): 0.44-0.53 of the entry reads per lookup are mismatches, worth about 2-5 ns of a 34 ns integer lookup, so 1-5% of `maps`, `word_freq` and `set_drain`; the slot format change touches `probe`, `index_put`, `find_key`, `reindex` and `copy_coll`.
- Scalar replacement of aggregates (SROA): a per-loop stream box is what a list stream still costs; after the map natives only `lexer` has per-item boxes (29,501 boxes of 16 bytes a run), so the payoff on today's benches is small; not measured as time.
- Re-inlining after the last GVN (an extra `inline, prune, sccp_changed` after `ranges`, measured and not kept): 70 more call sites over `bench/` in 8 of 37 programs, -3.7% `lexer` and -2.2% `arena_trees` dispatches, under 0.5% elsewhere, for +6.5% `compile_at`.
- Held shuttles with inline frames (design D5): a held shuttle's frame is a box (a `lexer` run makes 29,501 of 16 bytes); the payoff is not measured as time.
- Stream fusion for `flat_map`, `peekable` and `chunks` (they still run boxed steps), and a `collect_as` gate against a hand loop: no payoff measured, no gate yet (the fused ones sit at 2.0-6.5x of a hand loop in `std_counts`).
- Adjacent-opcode counters, then superinstructions (design D2's profile-driven step): the counters were not built, so the payoff is unknown.
- Profile-guided optimization of the `loom` binary: not tried; the payoff is unknown (the dispatch loop's layout alone moved benchmarks by up to 30%, design/vm.md section 9.1).
- Splitting the pool lock (spawn, start and end of a task still take it): spawn+join costs 0.3 us on one worker and 0.8-1.1 us on 8 against the 2 us gate, so at most that much; also `sarena.*`, the host natives and the scheduler's natives still take it.
- `loom for` over inclusive and open ranges and over general streams, and an adaptive grain: only exclusive integer ranges, lists, slices, arrays and their chunks chunk today.
- V4's bake: building the program from the job build's module, and baking maps, sets and sums (they are computed when read): the payoff is compile time only, unmeasured.
- `print` from many tasks beyond 3.7x on 8 workers: the formatting natives themselves scale about 2.5x here (8 workers on 8 physical cores); dropping the queue, skipping the write and 16x bigger buffers each changed 8-worker time by under 10%.
- Compile-time passes: the gvn root index, `tidy` and `range` repeats, codegen liveness and `Emitter::line`, pipeline consolidation (`compile_at` alone is 1.95-2.11x `-O0` against the design's 1.5x); also the deferred optimizer passes, specializing lambdas and self tail calls (unmeasured).
- `Instant.now()` at 8 workers is unmeasured.

**V9 (Treadle by default)**: verifier hardening before images are loaded from disk: a backward `Jmp` neither ticks nor is rejected, loads/stores through registers are unchecked outside the checked build (reword "verified" in the docs), header reads assume 8-aligned offsets, a waiter on a constant busy-spins.

**Std backlog (no compiler blocker; a stdlib session, any time)**
- Natives and plain Loom: `reverse_bits`, `swap_bytes`, `ilog(base)`, `cbrt`, `mul_add`, `lerp`, Float constants (`EPSILON`, `INF`, `NAN`), `parse_radix`, `escape`, `matches`, `String` `drain`/`split_off`/`to_bytes`, `env.vars`, `BufReader`/`BufWriter`, `process.run`/`Child` (own designs), `Deque` `for` loop (works over any `Iter`, so it is a weave away); `remove_entry` returning the map's stored key; the `Range.rev` message on an open range.
- Bugs filed in vm-review-v4 and not seen fixed in git history (verify, then fix): on the Lua backend a nested generator's `defer` doesn't run when the outer one is dropped; `parse[f32]` parses as `f64` and rounds, on both backends (change both together).
- Phase 7: `std.json` (`Json`, `parse`, derived `ToJson`/`FromJson` by reflection), `Args` and `Random` derives, `assert_eq`/`assert_ne`/`assert_close`/`assert_unravels`/`todo`, `bench`. None exist yet.
- Accepted differences, left as is: `char_indices` builds its list up front; `min_by_key` keeps the best in a one-item list; `sinh`/`cosh`/`tanh` lose precision for tiny arguments; `fs.exists`/`remove_file` on directories differ per platform; `fract(-0.0)` and `is_sign_negative(NaN)`; Lua `usize` is 63 bits, so `18446744073709551615` as a `usize` is T0102 on Lua and fits Treadle (`tests/fail/usize_literal.loom`); `Str.chars` on invalid UTF-8 crashes on Lua (U+FFFD on Treadle); indexing on `Deque`, `Grid` and `Duration` arithmetic stay methods only (D4).

## Standard library, mechanical group, chunks 2 and 3 (2026-10-20)

Done: `fs.metadata`/`is_dir`/`is_file`/`file_kind`/`file_size`, `env.current_dir`/`home_dir`/`temp_dir`, `term.columns`/`rows`/`is_tty`, `process.id`, `SystemTime`, `Rng.from_entropy`, `std.path` (`join`, `parent`, `file_name`, `stem`, `extension`, `with_extension`, `normalize`, `is_absolute`; `/` is the separator), `Str` `splitn`/`rsplit`/`split_by`, `String.from_utf8` with `Utf8Error`. `Int` `MAX`/`MIN` already work (`i32.MAX`). Compiler fix: a branch that gives an outer local by name (`if c { "x{y}" } else { body }`) moves it into the join; before, Treadle dropped it first and returned an empty string (`tests/run/if_moves_outer_local`).
Left from the group, each blocked on something the library can't do yet: `pow_checked`, `abs_diff`, `to_string_radix`, `total_cmp`, `to_bits` need a Loom body on `Int`/`Float` (their `self` is the warp, not the number: `Self` for gauge types is the obstacle) or an optional-returning native; `concat`/`join` on slices of slices need a `where T: [U]` bound; `env.vars` needs a list-of-pairs native; `process.run`/`Child` and `BufReader`/`BufWriter` are their own designs.

## Standard library, Default (2026-10-20)

`collect[C]` (2026-10-20): `Stream.collect_as[C]()`, spelled that way because the existing fused `collect()` (a list) is a builtin and stays. `C` comes from the explicit argument or from where the result goes (`let s: Set[i32] = xs.collect_as()`). `std.iter.FromStream` is a weft with an associated `Item` and a static `from_stream` (generic wefts aren't supported, so the item type is an associated type); woven for `List`, `Set`, `Map` (of pairs), `String` (of characters), `Deque`, `PriorityQueue`, and any user type (`tests/run/std_collect`, `tests/fail/collect_as`). Items of the wrong type are reported inside `collect_as` with the types named. Not done: the plan's count gate for `collect_as` against a hand loop, `BitSet`, `str` items for `String`.

Done (`tests/run/std_default`, `tests/fail/default_missing`): `std.default.Default` (in the prelude, like `Eq` and `Ord`, since it is derived from a header) with weaves for integers, floats, `bool`, `char`, `str`, `String`, `()`, `?T`, tuples of two and three, `List`, `Set`, `Map`; `std.default.of[T]()`; the derive. A warp that lists `Default` and has no `default` gets one made while names are resolved (`hir/lower/derive.rs`): fields without a default of their own take `std.default.of[FieldType]()`; a warp with cases takes its first case if it carries nothing. Static functions in wefts (`fun default() -> Self`) turned out to work once the checker's blanket "weft functions without `self`" refusal was lifted, and `T.default()` works in generic code. Not done: `Default` for arrays `[T; N]`, `Deque`, other tuple sizes; tuple warps (`warp Meters(f64)`) don't derive yet. The plan's `default_collect` fixture waits for `collect[C]`.

## Standard library, mechanical batch (2026-10-20)

Done (both backends, `tests/run/std_edit`, `std_fs_dirs`): `Slice` `starts_with`, `ends_with`, `count_of`, `rotate_left`, `rotate_right`; `List.extend` takes a stream (the native for slices is gone); `String` `truncate`, `insert`, `insert_str`, `remove`, `retain` (rebuild the text, so each is O(length)); `Int` `pow_wrap`, `pow_sat`; `fs` `create_dir_all`, `rename`, `copy`, `read_dir` (names in order; the in-memory file system of tests lists files, and has no empty directories).
Still open from the mechanical group: `splitn`/`rsplit`/`split_by`, `from_utf8`, `concat`/`join` on slices, `pow_checked`, `abs_diff`, `to_string_radix`, `Int` `MAX`/`MIN` constants, `total_cmp`, `std.path`, `fs.metadata`, `env`/`process`/`term` additions, `Rng.from_entropy`, `SystemTime`. `collect[C]` and `Default` are planned in `design/stdlib-collect-default.md`.

## Standard library review (2026-10-19)

Three Haiku agents mapped the library code, the Treadle natives and the Lua runtime; each finding was checked before acting. Fixed: `rfind`/`rsplit_once` unraveled on non-ASCII text (stepped one byte); `next_word` and `trim` on Treadle were quadratic (validated the whole text per call) and now decode one character at a time; `trim_end_matches` is one pass; Lua `lcm` of `i64::MIN`, `lcm` with zero, and `rem_euclid(MIN, -1)` now match Treadle; `hypot` is scaled so it neither overflows nor underflows (both backends); Lua text methods no longer crash on invalid UTF-8 (trimmed and cased as bytes, like Treadle); Lua ASCII `trim` no longer backtracks; `read_line` and `read_all` turn invalid UTF-8 into U+FFFD on both backends; `write_err` and `fs.write_text` write the bytes untouched on Treadle; `char.uni` rejects out-of-range code points; an unknown float method is an internal error rather than `is_nan`.
Known and left (cost or design): `char_indices` builds its list up front. `min_by_key` and friends keep the best in a one-item list. `sinh`, `tanh` and `cosh` lose precision for tiny arguments (both backends use the same formula). `fs.exists` and `remove_file` differ between backends for directories on some platforms. `fract(-0.0)` and `is_sign_negative(NaN)` differ in the sign of zero and NaN. Lua's `usize` is 63 bits and Treadle's 64, which shows in `count_zeros(0)` and `next_power_of_two` above 2^62. `Str.chars` on invalid UTF-8 crashes on Lua and gives U+FFFD on Treadle.

Follow-up (2026-10-20, Caio asked): map and set streams skip removed entries instead of compacting, and a seventh header word (`head`) remembers the first entry that may be live, so `Set.pop` and `Map.first` take constant time when used as a queue (draining 20 000 items: 3 s to 0.2 s; `bench/set_drain` is scale-gated). `Map.last` is still O(n). The claim that comparators copy their arguments was checked in Heddle and is false on Treadle: `sort_by`, `retain`, `dedup` and `PriorityQueue` pass the elements by reference (no `copy`/`str.new` in the loops; only the index lists allocate). `Range.rev` on a range with no end unravels (the message says "chunks of a range with no end").

## Standard library, phases 2 and 3 (2026-10-18/19)

Phase 3 done (both backends agree, `tests/run/std_math`): `Int` `signum`, `rem_euclid`, `div_euclid`, `div_floor`, `div_ceil`, `gcd`, `lcm`, `isqrt`, `ilog10`, `next_power_of_two`, `count_zeros` (one native `int.math`, exact on 128 bits, unravels when the result doesn't fit); `Float` trigonometry, `exp`, `exp2`, `ln`, `log2`, `log10`, `log`, `sinh`/`cosh`/`tanh` (written out from `exp` in both runtimes, since Lua has none), `hypot`, `atan2`, `fract`, `signum`, `copysign`, `to_degrees`, `to_radians`, `is_finite`, `is_infinite`, `is_sign_negative`; constants `TAU`, `SQRT_2`, `LN_2`, `LN_10`, `INV_PI`, `PHI`; `std.hash` `fnv1a` and `combine`. `clamp`/`min`/`max` come from `std.cmp`.
Phase 3 not done: `pow_wrap`/`pow_sat`/`pow_checked` (the operators `+%`, `+|` exist; a method family is unplanned work), `abs_diff`, `to_be_bytes`/`from_be_bytes`, `to_string_radix`, `Int` `MAX`/`MIN`/`BITS` constants, `reverse_bits`, `swap_bytes`, `ilog(base)`, `cbrt`, `mul_add`, `lerp`, `total_cmp`, `to_bits`/`from_bits`, `Float` constants (`EPSILON`, `INF`, `NAN`). Trig results are only checked for agreement on the fixture's values (Lua and Rust both call the platform libm).

Phase 2 done (`tests/run/std_text`, `char_text`): `Char` and `Str` follow Unicode in place (D2): `is_alphabetic`, `is_numeric`, `is_alphanumeric`, `is_whitespace`, `is_upper`, `is_lower`, `to_upper`, `to_lower`, `trim*`, `words`, `to_upper`/`to_lower` on text (letter by letter, `ß` to `SS`, no final-sigma rule); `_ascii` variants keep the old behaviour. The Lua runtime's tables are generated from Rust's own (`crates/loom_vm/tests/unicode_tables.rs`, `LOOM_BLESS=1` after a Rust upgrade), so both backends agree; core.lua grew by about 46 KB, parsed lazily on first non-ASCII use. New `Str` methods written in Loom: `split_once`, `rsplit_once`, `rfind`, `strip_prefix`, `strip_suffix`, `count_of`, `char_indices`, `char_at`, `trim_matches`, `trim_start_matches`, `trim_end_matches`, `eq_ignore_case`, `capitalize`, `pad_left`, `pad_right`, `is_ascii`; `Char.is_ascii`. They return views (`str`) as the plan wants: a Loom-bodied method on `Str` may return a slice of `self`.
Phase 2 not done: `splitn`/`rsplit`/`split_by`/`matches`, `String` `insert`/`remove`/`truncate`/`retain`/`drain`/`split_off`, `from_utf8`/`to_bytes`, `parse_radix`, `escape`, grapheme clusters and normalisation, the `Fmt` spec/`pad`/`debug_struct`/`dbg`. Gates not measured: ASCII `to_upper` on 1 MB (the ASCII path is unchanged on both backends), non-ASCII at most 4x ASCII, hello-world size growth (core.lua only).

## Standard library, phases 0, 1 and 4 (started 2026-10-18; design/stdlib-plan.md)

Built is roughly the plan's section 6 first slice for these phases, plus `PriorityQueue` and `BitSet`. Everything below is on both backends, with fixtures `tests/run/std_*`.

Done:
- Phase 0: `Ordering`, `cmp`/`min`/`max`/`clamp` (free functions in the prelude, so they work on every `Ord` type and on numbers through UFCS), `Reverse`; stable O(n log n) `sort_by`, `sort_by_key`, `sorted`, `sorted_by`, `sorted_by_key`, `is_sorted`, `lower_bound`, `upper_bound`, `binary_search`, `partition_point`; `Optional` `map`, `filter`, `zip`, `replace`, `take_if`, `ok_or`, `is_some`, `is_none`; `then`, `then_some`.
- Phase 1: `List` `retain`, `dedup`, `dedup_by_key`, `swap_remove`, `resize`, `split_off`, `append`, `drain`; `Stream` `rev`, `step_by`, `filter_map`, `flat_map`, `peekable`, `cycle`, `chunks`, `nth`, `is_empty`, `eq`, `min_by_key`, `max_by_key`, `min_by`, `max_by`, `partition`, `counts`, `group_by`; `Map` `of`, `get_or`, `retain`, `extend`, `remove_entry`, `update`, `merge`, `first`, `last`; `Set` algebra, `retain`, `extend`, `pop`; `Deque`, `PriorityQueue`, `BitSet`.
- Phase 4: `io.write`, `eprint`, `ewrite`, `read_line`, `read_all`, `lines`; `Instant`, `measure`, `Duration` methods; `Rng` `below`, `between`, `float`, `chance`, `shuffle`, `choose`, `fill`, `jump`.
- Compiler: `Self` in `Optional`/`Range` methods is `?T`/the range, so those can have Loom bodies; a Loom-bodied `Slice` method called on a list gets a view on the Lua backend; the reads of the clock and of stdin are refused in compile-time code (K0104).

Waiting on compiler or design work:
- Storable closures (V6, regions; Caio 2026-10-18: defer): today a lambda argument borrows what it captures and cannot be put in a warp field or returned (M0106), so `inspect`, `scan`, `from_fn`, `successors` and any adaptor that must call a user function later stay compiler builtins (not built). `filter_map` and `flat_map` are built from `map`/`filter` and a warp without a callback. Blocker today: the closure type has no owner that outlives the call; the checker rejects returning a value that borrows a lambda's captures (M0106) and `^(x => ...)` only boxes a lambda expression, not a borrowed parameter. Needs regions.
- `collect[C]` (a `FromStream` weft) and the `Default` derive need compiler support (`collect` is a fused builtin; derives are compiler-written).
- A hand-written `Show` on a warp declared in `std` is silently ignored (the debug form is used), which is why `Duration` has `to_string` and no `Show`. Unexplained; looks like a std-module special case in the weave recording.
- `mem.swap`/`replace`/`take`: moving out of a `mut` parameter is not allowed, so they need a language rule.

Done since: methods of `T ! E` (`Fallible`: `map`, `map_err`, `value`, `error`, `is_ok`); `inspect`, `filter_map` and `step_by` are stream steps on Treadle and closures on Lua (filter_map then sum 8.6x the hand loop, step_by 26x); `fs.write_text`, `exists`, `remove_file`; the 04-errors sentence about `unwrap` is gone. `scan`, `from_fn`, `successors` and fusing the new steps into pipelines (`fuse.rs`) are not done. `Instant.now()` costs about 33 ns a call on one worker (8 workers not measured).

**Deferred by Caio (2026-10-18) while phases 2 and 3 go first** (phases 0, 1, 4 are otherwise done): the leftover mechanical items below, plus `Rng.from_entropy`, `SystemTime`, `scan`/`from_fn`/`successors`, fusing the new steps into pipelines, `collect[C]`, the `Default` derive, `BufReader`/`BufWriter`, `process.run`/`Child`, `std.path`, and `Slice` `starts_with`/`ends_with`/`rotate_*`/`concat`/`join`/`count_of`.

Mechanical and not done yet: fs (`read_dir`, `create_dir_all`, `rename`, `copy`, `metadata`), env/process/term additions, the extra slice and text methods of phase 2, `List.extend(Stream)` and the other section-3 replacements (the `trim`/`to_upper` Unicode change is phase 2), removing the sentence about `unwrap()` and `expect()` from 04-errors.

API shapes that drifted from the plan (accepted): `List.drain(from, to)` instead of a `Range`; `Rng.below(n)` and `between(lo, hi)` (i64) instead of `range[T](Range)`; `Duration.to_string()` instead of `Show`; `BitSet.intersect_with`, `union_with`, `subtract` (`and`/`or` are keywords); `Ord.cmp` is a free function (a default method recursed through the derived `<`); `Rng.jump(mut self)` returns the stream at the current state and moves `self` 2^128 steps on; `Deque` has `get`/`set`/`to_list` and no indexing or `for` loop yet.

Decided (Caio 2026-10-18: D4 and the API drifts; the rest picked by Opus at his request): D4 methods only, no operator overloading (`Duration.plus` etc. are the API, `Grid` will use `get`/`set`); the drifted API shapes stay; D1 no prelude growth, library methods live in the type's own body; D2 Unicode in place with `_ascii` variants (Phase 2); D3 as built; D5 one-shot `fs` functions, no `once File` until there is a real handle; D6 postpone; D7 declared default per weft (V6); D8 done. Earlier text, kept for the record: D1 methods of a type's defining module are always callable (done for `cmp` etc. by putting them in the prelude, which is what the plan wanted to avoid); D2 Unicode in place (phase 2, not started); D3 `sort_by(less)` plus `sort_by_key`; D4 operator wefts first (blocked, above); D5 `File` as `once` (not started); D6 `try_lock` postponed; D7 declared defaults for `given` (V6); D8 phases 0 and 1 alongside the V5 fixes (done).

Gates: `tests/std_counts.rs` compares instruction counts of std methods with hand loops. Met (1.00x): `retain`, `dedup`, `swap_remove`. Missed, held by a regression guard: `resize` 2.2x (plan 1.1x), `counts` 2.6x (1.1x), `min_by_key` 8x and `filter_map.sum` 16x (1.25x), `step_by.sum` 32x (1.25x; the std version walks every item, the hand loop skips). `sort_by` costs 1.3x the native `sort`. Scaling (`std_collections_scale`): deque 10.0x for 10x input, list_ops 10.2x, heap under 13x. Std code written in Loom keeps about 95% of its bounds checks at `-O2` (index arithmetic on lists), so those benchmarks are left out of the check-elimination aggregate. Hello-world `loom check` takes 23 ms now; the baseline before the new modules was not measured. Treadle's `read_line` blocks on stdin while holding the native lock, which stalls other workers in a threaded program. `remove_entry` returns the key it was given, not the map's stored key.

## V5 close-out roadmap (proposed 2026-10-17)

State: V5 (threads) is done except two gates and one design gap (see AGENTS.md item 27, design/vm.md 11, bench/RESULTS.md).

Decision (Caio): the scaling gate counts logical cores (16) if that is worth it, otherwise stays at 8 physical. Settle by measuring (step 4).

1. **Baseline.** Run the commit gates and `bench/run.py` on the current tree; record par_mandelbrot (1/2/4/8/16 workers) and par_spawn in RESULTS.md. Add `par_mandelbrot` and `par_spawn` to `bench/run.py`. Gate: all commit gates green.
2. **Chunk `loom for`** (design/vm.md 11.3/11.4). One task per chunk, not per iteration; `#disjoint`/`chunks_mut` semantics unchanged. Gate: par_mandelbrot no slower at 1 worker, `tests/par` and `parallel.loom` outputs identical on Lua and Treadle at 1/2/4/8 workers.
3. **Per-worker deques with stealing** in `loom_vm::pool`, taking the scheduler off the single lock for spawn/join. Keep `LOOM_THREADS=1` as the exact reference schedule. Gate: spawn+join < 2 us on 8 workers; `par_stress` (1000 runs) and the 8-worker thread-fixture stress green, plain and checked.
4. **Scaling re-measure** at 8 and 16 workers. If 16 logical cores give a real speedup over 8, set the gate to 0.7x of 16 and make the pool default match; otherwise keep 0.7x of 8 physical. Write the decision here.
5. **Assert the gates. Done** (`#[ignore]`d perf gates, AGENTS.md; the compile-time ratio is 1.63 now, not the design's 1.5, and is asserted at 1.75): optimizer compile time (<= 1.5x -O0, slowest example <= 200 ms) and spawn+join, so a regression fails a test.
6. **Gaps in V5's design. Done.** `yield`/`pull` in a loom and `loom(given: ...)` wait for V6.
   - Per-worker heaps (vm.md 5.5, 11.4; `heap.rs`): allocating and freeing take no lock. `par_alloc` (new) on 8 workers: 0.852 s before (0.37x of one worker), 0.065 s now (4.9x). Single-thread benches within noise of the unchanged commit rebuilt in the same directory; `par_spawn_join` on 8 workers is 4-7% slower (0.203-0.210 s to 0.218), which is the price (RESULTS.md). The limit is exact with one worker and loose by two 16 KiB chunks per worker with several. Frees on another worker go on the freeing worker's list and spill to the shared store past 32 KiB net; the checked build keeps its object map behind a mutex.
   - P8 note on both backends (vm.md 11.2): `Outcome::Unravel.thread`. Assumed: the note is at the direct child's `thread` expression (overwritten at each nesting level); no note for exit, Ctrl-C, deadlock, cancel or the parent's own unravel. Lines agree on both backends; columns can differ (Lua: the `thread` expression, Treadle: its statement). Fixtures `threads_note*` (`thread line N` / `no thread` in `.unravel`).
   - Copied str/slice (vm.md 11.3): a lambda's parameters get the receiver's roots (list, slice, map, set or stream) when they are text, slices or owned values, so `names.map(n => thread n.len())` over a loom-local `names` is C0405. Not covered: a receiver that a call just made as a list (`xs.filter(f).map(...)`: a temporary in between).
7. **Doc clean-up. Done.** 05-concurrency.md (I/O as cancel point, `.join()` type, `loom(given:)`); status marker for V5 in vm.md 17; vm.md 11.4 deques wording; stale doc comment in `loom_compiler/src/lib.rs`.
8. **Flake. Done.** `an_interrupt_before_the_run_starts_is_not_lost` hung the Lua backend about 1 run in 300 (`LOOM_INTERRUPT_AFTER_MS=0`): the interrupt's hook fired while mlua's `Chunk::eval` was loading the bundle as an expression, which `eval` answers by loading it as statements, so the one-shot hook was spent and the program ran on. Now the bundle is loaded with `into_function`. `interrupt::watch` also registers before it reads the flag, so an interrupt can't fall between the two. 0 hangs in 3600 Lua and 1200 Treadle runs.

Then: V6 (spools, regions, `given`, and storable closures: see the standard library section above).

## Decisions

- 2026-10-17: keep `// SAFETY: ...` as the unsafe comment format (not `UNSAFE:`); existing sites are fine as long as documented.
- 2026-10-17: `ledger.log` removed; this file replaces it.
- 2026-10-17: Open design question Q8 (`once` values leaked when a thread unravels) deferred by Caio.

## Pending questions for Caio

- Q8: what happens to `once` values when a thread unravels? (deferred)
- Q9 (settled, Caio 2026-10-22): a private function spells its `given`s like a `pub` one; no inference.
- Q10 (done, T0107): arena pointers and mutation. A field behind an arena pointer is writable only when the target is plain data (option c); other targets are read-only through it.
- Q11: should `Pool.free` give R0102 for a use after free? Today a use after `free` compiles, and so does a copy of the pointer taken before `free`, because `^T@s` is `Copy`. Making `free` take by move does nothing, so the fix is either to make spool pointers move-only (a change to how they are copied everywhere) or to track slots per pool, which the region pass can't do precisely. Asked 2026-10-22.
- `projects/` (spindle) is untracked: commit it, ignore it, or leave it?

## V5 close-out result (2026-10-17)

- Step 4 decision (Caio): the scaling gate stays 0.7x of 8 physical cores (5.6x). 16 logical workers reach 8.2x against 6.1x on 8, so the pool keeps one worker per logical core, but the gate is not raised.
- Caveats: the optimizer compile-time ratio is 1.62-1.64x and `compile_time` asserts 1.75x (design gate 1.5x); `par_mandelbrot` measures 5.7-6.3x at 8 workers, a thin margin over 5.6x; par_spawn_join is ~1.1 us on 8 workers after per-worker heaps (gate 2 us).
- Only exclusive integer ranges chunk, with a fixed grain.
