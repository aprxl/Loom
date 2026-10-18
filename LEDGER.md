# LEDGER

Running log of milestone work and decisions. Newest first. Replaces `ledger.log`.

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

Then: V6 (spools, regions, `given`).

## Decisions

- 2026-10-17: keep `// SAFETY: ...` as the unsafe comment format (not `UNSAFE:`); existing sites are fine as long as documented.
- 2026-10-17: `ledger.log` removed; this file replaces it.
- 2026-10-17: Open design question Q8 (`once` values leaked when a thread unravels) deferred by Caio.

## Pending questions for Caio

- Q8: what happens to `once` values when a thread unravels? (deferred)
- `projects/` (spindle) is untracked: commit it, ignore it, or leave it?

## V5 close-out result (2026-10-17)

- Step 4 decision (Caio): the scaling gate stays 0.7x of 8 physical cores (5.6x). 16 logical workers reach 8.2x against 6.1x on 8, so the pool keeps one worker per logical core, but the gate is not raised.
- Caveats: the optimizer compile-time ratio is 1.62-1.64x and `compile_time` asserts 1.75x (design gate 1.5x); `par_mandelbrot` measures 5.7-6.3x at 8 workers, a thin margin over 5.6x; par_spawn_join is ~1.1 us on 8 workers after per-worker heaps (gate 2 us).
- Only exclusive integer ranges chunk, with a fixed grain.
