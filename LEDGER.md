# LEDGER

Running log of milestone work and decisions. Newest first. Replaces `ledger.log`.

## Standard library, phases 0, 1 and 4 (started 2026-10-18; design/stdlib-plan.md)

Built is roughly the plan's section 6 first slice for these phases, plus `PriorityQueue` and `BitSet`. Everything below is on both backends, with fixtures `tests/run/std_*`.

Done:
- Phase 0: `Ordering`, `cmp`/`min`/`max`/`clamp` (free functions in the prelude, so they work on every `Ord` type and on numbers through UFCS), `Reverse`; stable O(n log n) `sort_by`, `sort_by_key`, `sorted`, `sorted_by`, `sorted_by_key`, `is_sorted`, `lower_bound`, `upper_bound`, `binary_search`, `partition_point`; `Optional` `map`, `filter`, `zip`, `replace`, `take_if`, `ok_or`, `is_some`, `is_none`; `then`, `then_some`.
- Phase 1: `List` `retain`, `dedup`, `dedup_by_key`, `swap_remove`, `resize`, `split_off`, `append`, `drain`; `Stream` `rev`, `step_by`, `filter_map`, `flat_map`, `peekable`, `cycle`, `chunks`, `nth`, `is_empty`, `eq`, `min_by_key`, `max_by_key`, `min_by`, `max_by`, `partition`, `counts`, `group_by`; `Map` `of`, `get_or`, `retain`, `extend`, `remove_entry`, `update`, `merge`, `first`, `last`; `Set` algebra, `retain`, `extend`, `pop`; `Deque`, `PriorityQueue`, `BitSet`.
- Phase 4: `io.write`, `eprint`, `ewrite`, `read_line`, `read_all`, `lines`; `Instant`, `measure`, `Duration` methods; `Rng` `below`, `between`, `float`, `chance`, `shuffle`, `choose`, `fill`, `jump`.
- Compiler: `Self` in `Optional`/`Range` methods is `?T`/the range, so those can have Loom bodies; a Loom-bodied `Slice` method called on a list gets a view on the Lua backend; the reads of the clock and of stdin are refused in compile-time code (K0104).

Waiting on compiler or design work:
- Operator wefts (`Add`, `Sub`, `Mul`, `Index`; D4): not implemented, so `Duration` has `plus`/`minus`/`times`/`divide`. Under the no-aliases rule those methods go when `+` works on warps.
- Storable closures (V6, regions; Caio 2026-10-18: defer): today a lambda argument borrows what it captures and cannot be put in a warp field or returned (M0106), so `inspect`, `scan`, `from_fn`, `successors` and any adaptor that must call a user function later stay compiler builtins (not built). `filter_map` and `flat_map` are built from `map`/`filter` and a warp without a callback. Blocker today: the closure type has no owner that outlives the call; the checker rejects returning a value that borrows a lambda's captures (M0106) and `^(x => ...)` only boxes a lambda expression, not a borrowed parameter. Needs regions.
- `collect[C]` (a `FromStream` weft) and the `Default` derive need compiler support (`collect` is a fused builtin; derives are compiler-written).
- Methods of `T ! E` (`map`, `map_err`, `value`, `error`): there is no result warp to hang them on yet.
- A hand-written `Show` on a warp declared in `std` is silently ignored (the debug form is used), which is why `Duration` has `to_string` and no `Show`. Unexplained; looks like a std-module special case in the weave recording.
- `mem.swap`/`replace`/`take`: moving out of a `mut` parameter is not allowed, so they need a language rule.

Mechanical and not done yet: fs (`write_text`, `exists`, `read_dir`, `File` as `once`), env/process/term additions, the extra slice and text methods of phase 2, `List.extend(Stream)` and the other section-3 replacements (the `trim`/`to_upper` Unicode change is phase 2), removing the sentence about `unwrap()` and `expect()` from 04-errors.

API shapes that drifted from the plan, for Caio to veto: `List.drain(from, to)` instead of a `Range`; `Rng.below(n)` and `between(lo, hi)` (i64) instead of `range[T](Range)`; `Duration.to_string()` instead of `Show`; `BitSet.intersect_with`, `union_with`, `subtract` (`and`/`or` are keywords); `Ord.cmp` is a free function (a default method recursed through the derived `<`); `Rng.jump(mut self)` returns the stream at the current state and moves `self` 2^128 steps on; `Deque` has `get`/`set`/`to_list` and no indexing or `for` loop yet.

Open decisions (defaults taken, Caio to override): D1 methods of a type's defining module are always callable (done for `cmp` etc. by putting them in the prelude, which is what the plan wanted to avoid); D2 Unicode in place (phase 2, not started); D3 `sort_by(less)` plus `sort_by_key`; D4 operator wefts first (blocked, above); D5 `File` as `once` (not started); D6 `try_lock` postponed; D7 declared defaults for `given` (V6); D8 phases 0 and 1 alongside the V5 fixes (done).

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
- Standard library: the open decisions and drifted API shapes in the section above.
- `projects/` (spindle) is untracked: commit it, ignore it, or leave it?

## V5 close-out result (2026-10-17)

- Step 4 decision (Caio): the scaling gate stays 0.7x of 8 physical cores (5.6x). 16 logical workers reach 8.2x against 6.1x on 8, so the pool keeps one worker per logical core, but the gate is not raised.
- Caveats: the optimizer compile-time ratio is 1.62-1.64x and `compile_time` asserts 1.75x (design gate 1.5x); `par_mandelbrot` measures 5.7-6.3x at 8 workers, a thin margin over 5.6x; par_spawn_join is ~1.1 us on 8 workers after per-worker heaps (gate 2 us).
- Only exclusive integer ranges chunk, with a fixed grain.
