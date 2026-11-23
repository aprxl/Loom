# Optimizer passes (HIR, Heddle, codegen): a map for review

Audited at `fb58422` (main, 2026-11-23). This is a read-only audit. "Read"
means checked in the code. "Measured" means a number in `bench/RESULTS.md`,
`LEDGER.md`, or a commit message. "Unmeasured" means nothing I found backs the
pass's gain in isolation. Items marked *inferred* come from reading comments
and call sites, not from running anything.

HIR has no optimization passes. The optimizer is Heddle-level (`heddle/opt/`),
with one HIR-level transform (stream fusion) and codegen-time rewrites. The
boundary section says where each one sits.

## The boundary

- **HIR has no optimization passes.** `crates/loom_compiler/src/hir/` resolves
  and types the program; nothing in it rewrites code for speed (read).
- **One transform reads HIR directly:** stream fusion, `heddle/build/fuse.rs`.
  It is a `Builder` method set over `hir::Expr` pipelines, run while the typed
  program becomes Heddle (read).
- **`const if` / `const for` unrolling** (`typeck/unroll.rs`) is semantics, not
  an optimization. It runs at type-check time (read).
- **Everything else runs on Heddle**, the SSA IR in `heddle/opt/`. `opt::optimize`
  (`heddle/opt/mod.rs:74`) runs `PASSES` at `-O2`, the default. `-O0` runs none
  (read).
- **Codegen does peephole-style work on Heddle** while emitting Treadle bytecode:
  compare-and-branch fusion, immediates, constant hoisting, element access
  fusion (`crates/loom_compiler/src/codegen.rs:538, 647, 674, 707, 802`). This
  is not HIR and not in `PASSES`.
- **The Lua backend** (`crates/loom_compiler/src/lower/`) lowers HIR to Lua with
  no pass module (read).

`design/vm.md` §7 describes thirteen passes, several of them deferred. The code
runs 15 entries in `PASSES`: the same passes, with `sccp`, `gvn` and `inline`
run a second time, and `sccp` and `gvn` in their `_changed` forms on the
later rounds. The §7 table is older (see Risk 9).

## Pass table (-O2 passes, in code order)

| # | Pass | Where | IR | What it does | Cost or pattern targeted | Gain evidence |
|---|---|---|---|---|---|---|
| 1 | `inline` | `heddle/opt/inline.rs:inline` | Heddle | Copies a callee of at most 40 instructions into callers, or a callee with one call site of at most 400. Bottom-up over call-graph SCCs; growth limited to 2x or +80 instructions. | Call, return, argument moves; lets later passes see through calls | V3 geomean 2.74x PUC Lua (`bench/RESULTS.md`, "V3, after the run-time review"), no per-pass ablation. A second inline round was measured and not kept: -3.7% `lexer`, -2.2% `arena_trees`, +6.5% `compile_at` (`LEDGER.md` line 104). Unmeasured in isolation otherwise. |
| 2 | `prune` | `heddle/opt/mod.rs:prune` | Heddle | Replaces functions nothing reaches (from the entry, `roots`, a call, a `FuncRef`, or a data word) with a crash stub. | Later passes and codegen time | Unmeasured at run time. About 5.6 us per compile (`RESULTS` V8 table). |
| 3 | `sccp` | `heddle/opt/sccp.rs:sccp`, folding in `opt/fold.rs:fold` | Heddle | Sparse conditional constant propagation over block parameters, folded through the `ty::int` oracle. Also identities (`x+0`, `x*1`, `not not x`). Drops unreachable blocks. | Constant branches and dead blocks | Unmeasured in isolation. About 71 us per compile. `fold` is checked against the Rust oracle by `crates/loom_compiler/tests/rt_int.rs` (AGENTS.md). |
| 4 | `cells` | `heddle/opt/cells.rs:cells` | Heddle | A counted loop that adds one to a frame word per trip (a list's length after `for i in 0..n { xs.push(i) }`) leaves the word `bound` higher. It writes that as a store at the loop's exit for GVN to keep. | Length and index checks after a push loop | Unmeasured in isolation. Commit `55c1e39` has no numbers. About 30 us per compile. |
| 5 | `gvn` | `heddle/opt/gvn.rs:gvn`, `devirtualize` at `gvn.rs:74` | Heddle | Global value numbering over the dominator tree. Reuses computations and loads nothing has written since (through `alias` and `modref`), map lookups (`map.find_str`, `map.find_int`), element addresses and bounds checks. Turns indirect calls on a known `FuncRef` into direct calls. | Redundant work, checks, indirect calls | `std_counts`: `min_by_key` 8.00x to 6.50x, `counts` 2.53x to 2.18x (commit `311d5cf`, `RESULTS` V8). Run only on changed functions after the first round: -7.2% total pass time, `compile_at` 552 to 532 ms (`a72212a`). GVN compile time before and after the V5 review: 3 to 15% faster on six programs (`RESULTS`, "V5 review: compiler passes"). |
| 6 | `sccp_changed` | `opt/sccp.rs:sccp_changed` | Heddle | `sccp`, only on functions flagged by `Module::changed`. | Compile time | Compile time only (`a72212a`). |
| 7 | `inline` (round 2), `prune` | as 1 and 2 | Heddle | Inlines what GVN's devirtualization exposed. | As 1 | As 1. Round 2 has no separate measurement; it is inside the whole-pipeline numbers. |
| 8 | `gvn_changed` | `opt/gvn.rs:gvn_changed` | Heddle | `gvn`, only on flagged functions. | Compile time | `9ef9735`: counts unchanged, compile time only. |
| 9 | `sccp_changed` | as 6 | Heddle | As 6, after round 2. | As 3 | Compile time only. |
| 10 | `ipcp` | `heddle/opt/ipcp.rs:ipcp` | Heddle | A parameter that every in-module `Call` passes as the same `Iconst`/`Fconst` becomes that constant inside the function. Excludes the entry, functions used as values (`FuncRef`), and functions with a `Suspend`. | Makes parameters constant for `ranges` and `sccp` | Unmeasured in isolation. Motivation in its header: `times_both` gets 400-float lists. About 9 us per compile. `tests/opt/ipcp`. |
| 11 | `ranges` | `heddle/opt/range.rs:ranges` | Heddle | Interval analysis over integers: reverse postorder, widening at loop heads, then three narrowing rounds if anything widened (`range.rs:417`). Narrows by dominating tests and passed bounds checks. Removes overflow, bounds, cast and comparison checks that can't fire. | Overflow and bounds checks | Bounds checks removed: 87.4% of 721 048; overflow checks: 62.4% of 1 434 750 (`check_counts.rs`, `RESULTS` V3). This is the combined effect of `gvn`, `cells`, and `ranges`. Per pass unmeasured. About 111 to 120 us per compile. |
| 12 | `sccp_changed` | as 6 | Heddle | As 6, after `ranges`. | As 3 | Compile time only. |
| 13 | `licm` | `heddle/opt/licm.rs:licm` | Heddle | Adds preheaders. Moves invariant, non-trapping, effect-free instructions and loads that nothing in the loop writes (through `alias`) to the preheader. | Loop-invariant work, list header loads | Unmeasured in isolation. Commit `b48a2d6`. About 48 us per compile. `tests/opt/licm`. |
| 14 | `cleanup` | `heddle/opt/tidy.rs:cleanup`, with `tidy` at `tidy.rs:tidy` | Heddle | `tidy`: merges single-predecessor blocks, removes dead values by use count (`07c905e`) and unreachable blocks. `cleanup`: sinks element addresses to their loads and stores (`sink_elems`, `sink_indexes`, `place_indexes`), `carry_last`. | Dead code; bounds check fused with the access (`ldelf`/`stelf`) | Unmeasured in isolation. About 71 us per compile. Also `96ef588`: std bounds checks that are fused into checked loads and stores save almost no dispatch. |

Analyses shared by the passes, not passes themselves: `alias.rs` (which
accesses may overlap), `modref.rs` (what a function may write through a
parameter), `cfg.rs` (blocks, dominators, loops), `fold.rs` (constant folding).

### Build-time and codegen transforms (not in `PASSES`)

| Transform | Where | IR | Gain evidence |
|---|---|---|---|
| Stream fusion (`fuse`) | `heddle/build/fuse.rs` | HIR, at Heddle build | `step_by`.sum over a hand loop 25.75x to 2.00x; `filter_map`.sum 8.60x to 2.00x (`LEDGER.md` line 88; commits `4900bb0`, `d0d8532`, `def0e51`). Counting instead of streaming for a range source: `f25295e`. `flat_map`, `peekable`, `chunks` are not fused; payoff unmeasured (`LEDGER.md` line 106). |
| Projections and generators used once | `heddle/build/*` (`design/vm.md` §7, row 9) | HIR, at build | Unmeasured in isolation. |
| Tail-local hand-off to a join | `heddle/build/*` (`RESULTS` V8, "a block's tail local") | HIR, at build | A 10M-iteration microbenchmark 6.6% faster; `list_ops` dispatches -26.2% (`RESULTS`, same section). |
| Compare-and-branch fusion, immediates, constant hoisting, element access fusion, fused counted-loop increment | `codegen.rs:538, 647, 674, 707, 802`; `8441ae0` | Heddle, at codegen | Unmeasured in isolation. The increment fusion is part of the V3 review changes (`RESULTS`, "V3, after the run-time review"), whose total is +11.9% against the build before the review. |

## Order and dependencies (-O2)

Order, from `PASSES` in `heddle/opt/mod.rs:38`:

`inline, prune, sccp, cells, gvn, sccp_changed, inline, prune, gvn_changed, sccp_changed, ipcp, ranges, sccp_changed, licm, cleanup`

Dependencies, read from code and comments:

1. `inline` before `prune`: `prune` removes what inlining used up the last call of.
2. `cells` before `gvn`: `cells` emits the store that GVN keeps. `cells` also uses GVN's
   `PURE_NATIVES` and `QUIET_NATIVES` tables (`cells.rs:22`, `cells.rs:266`), so a change to
   those tables changes what `cells` treats as safe.
3. `gvn` before `sccp_changed`: loads GVN forwards can decide a branch.
4. `gvn` before `inline` (round 2): GVN's devirtualized calls are what round 2 copies in.
5. `gvn` before `ipcp`: `ipcp` only counts `Call` instructions, and devirtualization makes
   more of them.
6. `ipcp` before `ranges`: constant parameters feed the interval analysis.
7. `ranges` before `licm`: `licm` takes the loads GVN placed at loop heads.
8. `module.changed` is the contract between the passes: `sccp_changed` and
   `gvn_changed` trust that unflagged functions are already optimal (see Risk 2).
9. `alias`, `modref`, `cfg` are shared. `modref` feeds `gvn` and, through it, `cells`.

## Risks and suspicious spots

Each item says whether it was read, run, or inferred.

1. **`ipcp` ignores exported roots (read; not run; the host side is confirmed).** `ipcp` bars only the
   entry (`ipcp.rs:21`) and functions taken by `FuncRef` or with a `Suspend`.
   `Module::roots` (`heddle/mod.rs:517`) is not checked. The host can call any
   of them with any arguments: `Program::call` takes an argument list and checks
   only its length (`crates/loom_embed/src/lib.rs:889-897`). `build_exports_with`
   (`heddle/build/mod.rs:452-481`) makes every `pub`, non-generic function with a
   body a root, and `loom_embed` calls those with host arguments; `compile_exports`
   (`treadle.rs:129-131`) runs the optimizer on that module at the default level.

   Suppose an export has parameters, is called inside the module, every in-module
   call passes the same constant for a parameter, and the export is not inlined
   (more than 40 instructions with two or more calls, or more than 400 with one call).
   Then `ipcp` fixes that parameter, and a host call with a different value gets the
   folded result. Fix: bar every root in `ipcp`. Reproducing it needs a `loom_embed`
   host, which I did not build or run.

2. **`ipcp` leaves `module.changed` alone (read).** `ipcp.rs` never touches
   `changed`. The `sccp_changed` after `ranges` skips unflagged functions, so
   constants that `ipcp` introduced may not fold branches there. This costs
   speed, not correctness. A fix is to flag the functions `ipcp` edits.

3. **Vtable dispatch is safe through a thunk (read, plus a probe).** A vtable
   slot points at a per-type thunk (`heddle/build/any.rs:240-265`), which passes
   its own block parameters to the method. `ipcp` therefore never sees a constant
   there. A probe (a method called directly with 2 and through `any` with 5)
   printed 42, 42, 365 on both Treadle and the Lua backend. The probe does not
   show that `ipcp` fired, so this is evidence the result is right, not proof of
   the mechanism. If a slot ever points at the method itself, `ipcp` becomes
   unsound.

4. **`cells` depends on `mem.grow`'s writes (read).** `cells.rs` assumes `mem.grow`
   writes only words 0 and 2 of the header it's given. That is a contract with a
   native, and I found no gate that ties the two together.

5. **`alias` assumes landing pads don't keep slot addresses (read).** The
   non-escape rule for frame slots (`alias.rs` header) is used by `gvn`, `licm`,
   `cells`, and `modref`. Landing pads are assumed to only load, store, copy and
   zero slots.

6. **`ranges`: does widening reach a post-fixpoint first? (read; not verified).** Narrowing
   runs three fixed rounds after widening (`range.rs:417`), not to a fixpoint. Standard
   narrowing is sound when it starts from a post-fixpoint and the transfer function is
   monotone. The open question is whether the widening loop always reaches one before
   narrowing begins, and whether every transfer is monotone. A reviewer should check both
   against `range.rs`.

7. **Stream fusion's rules are comments (read).** `fuse.rs` is the only
   transform that reads HIR and skips the normal lowering. Its rules (owned items
   excluded; `take`, `take_while`, `inspect` ordering) are stated in its header.
   The checks are `tests/run` fixtures at both optimization levels and the
   generator (`crates/loom/tests/gen.rs`), not a fusion-specific oracle.

8. **No per-pass runtime gate (read).** `tests/opt` has 44 snapshot files, with
   none for `fold`, `alias`, or `modref` directly. The count gates (`bench_counts.rs`,
   `check_counts.rs`, `std_counts.rs`) catch changes in pass output. The wall-clock
   perf gates are `#[ignore]`d (AGENTS.md).

9. **Doc drift (read).** `design/vm.md` §7's list has `sccp` where the code has
   `sccp_changed`, and it lists passes that aren't built: specialize lambdas, SROA,
   self tail calls, and merging frees. Update §7 in the same commit as any change to
   `PASSES`.

10. **Compile-time budget (measured).** `design/vm.md` §7 says `-O2` costs at most
    1.5x `-O0`. `RESULTS` measures 1.62 to 1.64x, and `compile_time.rs` asserts 1.75x.

## Not built, and what covers them

Specialize lambdas, SROA, self tail calls, and merging frees (`design/vm.md` §7,
rows 2, 3, 13, 11). The design says GVN's devirtualization stands in for
specializing lambdas, and that the slowest benchmarks' time is in hashing and
natives, so SROA isn't known to be what's missing (`design/vm.md` §7, "Deferred").
Not measured.
