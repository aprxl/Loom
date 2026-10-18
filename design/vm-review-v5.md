# V5 review: compile time and runtime findings

Static review (nothing built or benchmarked except one `LOOM_PASS_TIMES` run on
`examples/chip8.loom`, release). Ranks are estimates until profiled. No code was
changed by this pass.

Implementation follow-up: resource wakes now survive cancellation of the
selected mutex/pipe waiter; normal task results are dropped even when a
cancellation was requested; interrupt delivery is tracked per task across
worker migration; an empty retained stack is handled on a subsequent VM
call; lazy constant flags use acquire loads and release stores. Deterministic
scheduler regressions cover the wake handoff, result ownership and interrupt
claim, alongside repeated threaded calls on one VM.

## Measured

chip8, `-O2`: optimize 6.6 ms, codegen 3.3 ms. By pass: gvn 2.5 ms (two runs),
sccp 1.5 ms (four runs), ranges 0.6, inline 0.5, cells 0.17, licm 0.23,
cleanup 0.32.

The 1.63x gate is a ratio, so speeding up codegen (shared by `-O0` and `-O2`)
raises the ratio while lowering both times. Work that lowers the ratio is in
the optimizer passes below.

## Compile time, ranked (optimizer first)

1. `opt/gvn.rs:805-807, 433-490` every store/clobber walks all of `mem` and
   `stable` and clones the dropped keys: O(writes x known memory). Index known
   memory by alias root, or keep `Zero` keys apart (only they matter to
   `zero_left`).
2. `opt/gvn.rs:728-756` per instruction: `inst.kind.clone()` (with its arg
   Vec) even for `Role::Other`, a `tys` Vec, and two or three key clones
   hashed as `(InstKind, Vec<VTy>)`. Check the role on the reference first;
   use a small inline vec for types.
3. `opt/tidy.rs` runs about 10 times per function (each sccp, gvn, inline,
   licm, cleanup ends with it); each run is a whole-function fixpoint that
   rebuilds use counts. Run it only when the pass changed something, and share
   use counts.
4. `opt/tidy.rs:303-322` (`carry_last`) is O(n^2) per round: for each result it
   rescans every instruction for readers. Build a use count once per block.
5. `opt/range.rs:520-544` materializes each block's full dominator chain
   (blocks x depth); `range.rs:352-372` rescans predecessors per parameter per
   round.
6. `opt/gvn.rs:289-303` `writes_before` redoes a DFS and `writes_of` for every
   join block.
7. `opt/modref.rs:51-70` summaries fixpoint runs per gvn (twice), in function
   order; reverse-call order converges in fewer rounds, and the result can be
   kept between the two gvn runs when nothing was inlined.
8. `opt/alias.rs:65-68` clones every single-result `InstKind`; `Alias::new` is
   built in four passes. `opt/cfg.rs:40` dedups preds with `Vec::contains`;
   `Cfg` is rebuilt in licm (twice), cells, gvn, ranges.
9. `opt/sccp.rs:151-163` allocates `Vec<Vec<User>>` per run (4 runs).

Codegen (lowers both levels equally; good for absolute time, bad for the ratio):

- `codegen.rs:1305-1316` `line()` allocates a `String` and does a string-keyed
  map lookup per instruction with a span. Cache by `span.file` (a `Vec<u32>`).
- `codegen.rs:946-1000` liveness uses `BTreeSet` clones per fixpoint round and
  `allocate` runs up to 4 times per function; `codegen.rs:549` clones the whole
  `Func` for any function with a constant in a loop.
- `frame.rs:731-760` `update_ssa` is O(reloads x blocks); `frame.rs` and
  `heddle/verify.rs` still use std `HashMap` (SipHash).

Compiler options already in place (fat LTO, one codegen unit, abort) are right.
SIMD does not apply to the passes (pointer-chasing, small sets); `-C
target-cpu=native` is not safe for a distributed compiler.

## Runtime: correctness

1. Lost wakeup (`sched.rs:662-671, 727-766, 382-393`): a waiter handed a mutex
   or pipe wake and then cancelled before it runs never passes the wake on, so a
   second waiter stays parked on a free mutex and the run reports a false
   deadlock.
2. Leak (`sched.rs:562`): `leftover` skips any task with `cancelled` set, but
   that flag is also set on tasks that then finish normally (parked in
   `loom.leave`, or not started), so their results leak.
3. Ctrl-C twice (`exec.rs:1764-1773`): `stopping` is per worker, so a task
   already unwinding from Ctrl-C can raise again on another worker and skip the
   rest of its landing pad.
4. `Vm::call` after a threaded run (`exec.rs:358`, `stack.rs:111`): the host
   stack can be left as `Stack::empty()`, and `first_holds` indexes
   `segments[0]` and panics. Narrow reach (a second call on one `Vm`).
5. Constants (`natives.rs:499`, `consts.rs:111-183`): flag and value are written
   with plain stores and read lock-free; the SAFETY comment claims the lock
   covers it. Needs Release/Acquire on the flag.
6. Verifier gaps (not reachable from compiler output): backward `Jmp` is
   accepted and does not tick (`verify.rs:190`, `exec.rs:1256`), which bypasses
   the const-eval budget on a hand-built image; loads/stores through registers
   are unchecked outside the checked build, so "verified" means "from our
   compiler" (reword the docs); header reads use aligned `read` on offsets the
   verifier doesn't require to be 8-aligned.
7. Minor: `Ctx::exit` rescans the partial output line for `\n` on every locked
   native (`natives.rs:251`, quadratic on long lines without a newline); a
   waiter on a constant busy-spins and blocks deadlock detection
   (`exec.rs:1998`); `MIN % -1` gives 0 while vm.md:396 can be read as a trap.

Checked and sound: heap size classes, spill lists and the credit accounting;
stack segments; pool idle/sleeper accounting and the lost-wakeup-free spin
path; integer ops (div/rem edge cases, overflow policies, shifts, casts);
frame/linkage reads against the verifier.

## Runtime: speed

1. Natives take the global lock (`exec.rs:1352-1366`) except the heap ones:
   `float.math`, `int.bits`, `str.*`, `map.find_*`, `char.digit` could use a
   lock-free table like `heap_natives`. Largest parallel win (up to ~6-8x on
   8 workers for kernels calling them per iteration).
2. `time.sleep` sleeps under the pool lock (`host.rs:81`): eight threads
   sleeping 1 s take 8 s, and later sleepers ignore Ctrl-C.
3. Atomics and uncontended mutex lock/unlock go through the lock; real atomics
   or a CAS fast path would be 5-10x on contended ops.
4. Spawn zeroes a 16 KiB stack under the lock (`sched.rs:854`); a free list of
   first segments saves about 0.2-0.3 us of the ~0.9 us spawn+join.
5. `wake_one` notifies one sleeper per op even when an op readies several.
6. Dispatch: `CallN` and `Ret` link words; bounds-checked `natives[b]`;
   narrow-width checked add/sub as two steps; `Copy`/`Zero` of 32/40/48/64
   bytes fall back to memmove/memset; `tick!` on every call. Each roughly 1-3%.

## Merged from the second review (static; items marked "checked" I re-read)

New findings:

1. **Task ops take the pool lock twice** (checked, `exec.rs:1347-1366`, `run_task_op`
   at `exec.rs:1843`): `CallN` locks for any non-heap native, a task native
   returns `Trap::Task`, and `run_task_op` locks again. The cancellation check
   between chunk items (`thread.rs:428`) pays the first lock even when nothing
   was ever cancelled. Route task natives to their own dispatch, with the
   `cancel_seen` fast path ahead of any lock. Likely the cheapest win for
   `loom for` and spawn/join.
2. **`Sched::leftover` is quadratic** (checked, `sched.rs:551-571`): each call
   rescans `group.tasks` from the start past tasks already taken, and
   `emit_loom_end` calls it once per result. N unjoined results with drop glue
   cost 1+2+...+N probes under the lock. `unfinished` (`sched.rs:493`) restarts
   from the start on every retried `leave`. Keep a cleanup cursor per group.
   Cursor must keep result-drop order and tasks appended while active.
3. **Atomics**: `Atomic.add/sub` must trap on overflow without changing the
   value, so use a checked compare-exchange loop, not `fetch_add` plus a check.
   Pairs with finding 3 of the speed list above.
4. **Native split**: removing the lock while still handing out `&mut Ctx` would
   be unsound; split program data (read-only), worker-local scratch and shared
   services into separate interfaces, then move scalar leaves, then read-only
   text/map natives, then allocating text natives.
5. **Map maintenance** (`build/map.rs:255, 377`): `reindex` is an interpreted
   loop and `compact` calls `mem.move` per live entry. A native index rebuild
   over stored hashes and run-wise compaction need no user hash/eq/drop calls.
   Fingerprints in the index are the larger follow-up.
6. **Held shuttles**: V2 records 1.91 s held versus 0.21 s in place for 50 M
   items; inline frames and direct step calls for known makers have real
   headroom. Large piece of work.
7. **Pass schedule**: the last gvn devirtualizes after the last inliner, so the
   calls it exposes are never inlined; a worklist inline of just those calls.
8. **Measurement gap**: no adjacent-opcode pair counters in the `vm-stats` path
   although the design describes them; add before choosing superinstructions or
   a new dispatch engine (the design's 15% acceptance bar stands).
9. **Instrument first**: lock acquisitions by native category, scheduler scan
   lengths, task-stack allocations, native-call counts.

Assessments of the lower-confidence ideas: SIMD for text scanning and map
probing is plausible only after reading the generated code (search already uses
memchr/memmem; char counting auto-vectorizes) and a map index redesign; SIMD
numeric kernels need callback-free loops and exact overflow and trap order;
translating a leaf to C gains nothing unless the batch amortizes the call;
macros only keep specializations consistent; PGO measured about 1.5% before and
`target-cpu=native` kept nothing. Compile-time list: the second review's
ranking matches this one (gvn stores, gvn per-instruction clones, `carry_last`,
codegen liveness, range and modref repeats, `Emitter::line`).

## Implementation follow-up (2026-10-18)

The correctness fixes above landed in `b6ffd49`. Task natives now dispatch
directly to task handling; an uncancelled chunk boundary acquires no lock.
Atomic operations use exact-width Rust atomics, with checked CAS loops for
add/sub. Scalar leaves take only the worker heap and argument window.
Sleep checks host access and flushes under a short lock, then blocks its
worker outside the lock, polling interrupt/shutdown. It remains outside
ordinary cooperative cancellation points. Text/map natives still use the
shared context and lock.

Groups retain cleanup and completed-prefix cursors. `LOOM_STATS=3` reports
native calls and lock acquisitions by category, scheduler probes and stack
allocations; worker-local counters merge on shutdown. Adjacent-opcode
counters remain a prerequisite for future dispatch-engine work.

The review also found an exclusive scheduler reference held across a pool
wait in `choose`. It now carries a raw pointer across waits and creates
short exclusive borrows only while the guard is held. Output processing
examines only the suffix appended by the current native invocation.
Measurements, including the spawn/join regression, are in
`bench/RESULTS.md` under “V5 review”.

The compiler follow-up retains a flat GVN fact table, with a secondary
root index for private-slot facts and zeroed regions. Indexing every fact
regressed map-heavy compilation, so broad writes still use a complete
alias scan. Instruction/type-key allocation reductions, indexed readers
in `carry_last`, and dirty-gated SCCP/GVN cleanup are implemented. The
pass schedule and call-summary recomputation are unchanged. The full
root index, compact Alias provenance, codegen work and re-inlining after
the last GVN remain separate follow-ups; no dispatch/SIMD change is made.
