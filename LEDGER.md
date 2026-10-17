# LEDGER

Running log of milestone work and decisions. Newest first. Replaces `ledger.log`.

## V5 close-out roadmap (proposed 2026-10-17)

State: V5 (threads) is done except two gates and one design gap (see AGENTS.md item 27, design/vm.md 11, bench/RESULTS.md).

Decision (Caio): the scaling gate counts logical cores (16) if that is worth it, otherwise stays at 8 physical. Settle by measuring (step 4).

1. **Baseline.** Run the commit gates and `bench/run.py` on the current tree; record par_mandelbrot (1/2/4/8/16 workers) and par_spawn in RESULTS.md. Add `par_mandelbrot` and `par_spawn` to `bench/run.py`. Gate: all commit gates green.
2. **Chunk `loom for`** (design/vm.md 11.3/11.4). One task per chunk, not per iteration; `#disjoint`/`chunks_mut` semantics unchanged. Gate: par_mandelbrot no slower at 1 worker, `tests/par` and `parallel.loom` outputs identical on Lua and Treadle at 1/2/4/8 workers.
3. **Per-worker deques with stealing** in `loom_vm::pool`, taking the scheduler off the single lock for spawn/join. Keep `LOOM_THREADS=1` as the exact reference schedule. Gate: spawn+join < 2 us on 8 workers; `par_stress` (1000 runs) and the 8-worker thread-fixture stress green, plain and checked.
4. **Scaling re-measure** at 8 and 16 workers. If 16 logical cores give a real speedup over 8, set the gate to 0.7x of 16 and make the pool default match; otherwise keep 0.7x of 8 physical. Write the decision here.
5. **Assert the gates** that are only printed: optimizer compile time (<= 1.5x -O0, slowest example <= 200 ms) and spawn+join, so a regression fails a test.
6. **Gaps in V5's design.** Per-worker allocator caches (11.4 "As built"); P8 note on both backends; copied str/slice tracked against the loom (11.3). `yield`/`pull` in a loom and `loom(given: ...)` wait for V6.
7. **Doc clean-up.** 05-concurrency.md (I/O as cancel point, `.join()` type, `loom(given:)`); status marker for V5 in vm.md 17; vm.md 11.4 deques wording; stale doc comment in `loom_compiler/src/lib.rs`.
8. **Flake. Done.** `an_interrupt_before_the_run_starts_is_not_lost` hung the Lua backend about 1 run in 300 (`LOOM_INTERRUPT_AFTER_MS=0`): the interrupt's hook fired while mlua's `Chunk::eval` was loading the bundle as an expression, which `eval` answers by loading it as statements, so the one-shot hook was spent and the program ran on. Now the bundle is loaded with `into_function`. `interrupt::watch` also registers before it reads the flag, so an interrupt can't fall between the two. 0 hangs in 3600 Lua and 1200 Treadle runs.

Then: V6 (spools, regions, `given`).

## Decisions

- 2026-10-17: keep `// SAFETY: ...` as the unsafe comment format (not `UNSAFE:`); existing sites are fine as long as documented.
- 2026-10-17: `ledger.log` removed; this file replaces it.
- 2026-10-17: Open design question Q8 (`once` values leaked when a thread unravels) deferred by Caio.

## Pending questions for Caio

- Q8: what happens to `once` values when a thread unravels? (deferred)
- `projects/` (spindle) is untracked: commit it, ignore it, or leave it?
