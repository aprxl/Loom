# V6: pointers, spools, regions and `given`

Status: plan (2026-10-21); slice 1 done (below). Specs: [03-memory](03-memory.md), [vm.md](vm.md)
§12 and §17 (V6). This file records how the pieces are built and in what order.

## Where things stand

The parser and HIR cover every surface form: `spool x = e`, `given [x =] e`,
`given x: T` in signatures, `given: arg`, `^T`, `^T@r`, `^e`, `p^`,
`ref warp`, region parameters. The type checker refuses each with U0102, and
nothing past it exists: no `Ptr` type, no layout, no region codes (R0101 and
R0102 are not in `codes.rs`), no arena in the VM.

## Model

**A pointer is a type.** `TyKind::Ptr(elem, region)`; 8 bytes, never null (so
`?^T` costs nothing). `region` is a `Ty` of kind `TyKind::Region(..)`, so
regions travel through the existing generic machinery (`subst`, unify,
per-instance checking).

**Two views of a region.** The checker needs the precise region; code
generation needs only the spool *kind*. They are kept apart because a
`LocalId` is per body: local #3 of two functions would intern as the same
type, and a caller's local means nothing inside a callee.

- *Checker types* (`TyKind::Region(Region)`): `Heap`, `Static`,
  `Param(def)` (a region parameter, or a `given` spool of a signature), and
  `Spool(body, local)` (a `spool` binding, scoped to its body, never leaving
  it). At a call, regions in the callee's signature are substituted by the
  caller's: `Param(out)` becomes the caller's `Spool(body, small)`.
- *Instance keys, layout, glue and Heddle* see only the kind: `Heap`
  (owning pointer: move-only, drop frees) or `Spool` (any non-heap spool:
  `Copy`, drop does nothing). An instance is made per kind, not per spool
  binding, so `fold` is one instance for `raw` and `small`, and `Expr@raw`
  and `Expr@small` share layout and glue. (Per-kind: `Arena` bumps inline,
  `heap` calls the allocator; 03-memory says to monomorphize per spool type.)

**Warps follow their region.** A warp that allocates (a `^` field, or a
`String`/`List`/`Map` field, directly or through another such warp) has one
hidden region parameter: `Expr` is `Adt(Expr, [r])`. It is a real
`GenericParamKind::Region`, synthesized in HIR lowering (as `derive.rs` adds
`Default`) and defaulting to `@heap`; `display` omits `@heap`, so existing
programs, diagnostics, `tests/run` output and `bench/counts.txt` don't move
(checked when it lands). Written without an argument, `Expr` is `Expr@heap` in
return, field and `take` positions, and "any region" (a fresh region
parameter on the function) in read and `mut` parameter positions
(03-memory). Slice 1 has no hidden parameter yet: only `Heap` exists.

**Ambient spool.** `^e`, `.copy()`, templates that make a `String` and
collection constructors allocate in the ambient spool: the innermost `given`
of type `Spool` in scope, `heap` when there is none. A `given sp: Spool` is a
hidden parameter and a region parameter of its own (`@sp`).

**Region checking reuses `moves.rs`.** M0106 (a borrow outlives what it
borrows) and M0105 (a borrow used after what it borrows changed) already
track "roots" and `check_escape`. A value whose type mentions region `s`
gets `s` as a root:

- R0101 is M0106 with the spool local as the root: the value leaves the
  scope of `s` (returned, stored in something that outlives it, captured by
  an escaping lambda).
- R0102 is M0105 where `s.reset()` counts as the change to `s`.
- R0103: a pointer to a non-shared spool crosses a `thread`/`loom` boundary
  (05-concurrency; extends C0404/C0405's rule).

No new pass. The codes are added to `codes.rs` with the slice that uses them.

**Aliased mutation through arena pointers** (open question 1). Decided,
pending Caio's veto: option (c). Fields behind an arena pointer can be
written only when the target type is plain data (no cases, no owned fields);
other targets are read-only through an arena pointer. Heap pointers own
their target, so they are unrestricted.

**Both backends.** The Lua backend erases regions. `^T` for a table-backed
`T` is the table itself (moved, not copied); for a scalar `T` a one-slot
cell. `p^` reads (a copy for `Copy` types). `Arena.reset` is a no-op on Lua.

**Treadle runtime.** `heap` is spool 0. An `Arena(cap)` is a chunk list and a
bump pointer: allocation is a compare and an add, a new op in `loom_isa`
(verifier rule, listing, `LOOM_BLESS` of the listings); `reset` rewinds to the
first chunk; the scope's end frees the chunks; the checked build bumps a
generation on reset and poisons freed memory. Collections grow in their spool
via the spool number in the capacity word's top 16 bits, reserved since V1:
slice 4 first checks that the list, string and map growth paths mask them
today, and measures what the extra branch in `FreeBuf` costs every list drop
(benches re-blessed, cost recorded in `bench/RESULTS.md`).

## Slices (each lands with fixtures on both backends and an asserted gate)

`loom check examples/calc.loom` today reports seven U0102s: `^Expr` types
(3), `ref warp`, two `given` signatures, and `spool raw = Arena(..)`. Slice 2
also needs what the example uses without a refusal today: auto-deref through
`^` in method calls and UFCS (`x.fold()`), in `is`/`match` patterns, and in
templates and `Show` (`"{x}"`).

1. **`^T` on the heap, no spools. Done.** `TyKind::Ptr(elem, region)` with
   only `Region::Heap`; auto-deref is a `Coercion::Deref` on the base,
   receiver, scrutinee or template value (typeck `pointer.rs`), patterns
   record `pat_derefs`; a pointer is one word in memory on Treadle (a zero
   word is a moved-from pointer) and a one-slot table `{v}` on Lua (the
   descriptor `rt.ptr`, patched after the fact when a type holds a pointer
   to itself). Moving out of a box piece by piece works (a pattern's
   by-value binding zeroes its part). Left for later slices: `.copy()` on
   pointers, `once` behind a pointer (Q8), `^(…)`, moving the whole target
   out (`let t = p^` copies for `Copy` and moves otherwise, but is untested
   with big `once` values), the hidden region parameter on warps. Measured:
   alloc, read and free of a box 10 ns; `ptr_trees` 0.87 s against
   `binary_trees` 0.68 s (twice the allocations: leaves are boxes too).
   Original plan: `Ptr(T, Heap)`: `^e`, `p^`, auto-deref
   (fields, methods, UFCS, patterns, templates, `Show`/`Eq`/`Ord`/`Hash`),
   drop frees the box, recursive warps (`case Node(^Tree, ^Tree)`), moves,
   `mut` through an owning pointer, `?^T` niche; `^(…)` closures stay
   slice 5. Gates: `bench/counts.txt` rows for a pointer micro-bench
   (`^e` is one alloc plus its stores, `p^` one load); alloc+free of a small
   box at most 15 ns in `perf_gates.rs` (`#[ignore]`d, asserted); a
   `binary_trees` heap version in `bench/`.
2. **Spools and regions.** `Spool` weft, `Arena`, `spool x`, `heap`, `^T@s`,
   `@s` on types, hidden region parameters on warps, region-polymorphic read
   parameters, `ref warp`, `given sp: Spool` and `given:` arguments for
   spools, the ambient spool, R0101/R0102/R0103, checked-build generations,
   the inline bump op. Passing: `examples/calc.loom` runs on both backends;
   its two "rejected" snippets are fixtures. Updated with it:
   `docs/lang/samples/nyet_spools.loom` and its `.out`,
   `docs/lang/pages/15-not-yet.md`, `python docs/gen.py --check`, and calc's
   entry in `every_example_fails_only_with_not_implemented_yet`. Gates: `Arena`
   alloc at least 10x `heap` (counts and wall); `binary_trees` at least 4x
   PUC Lua; the region parameter moves no existing `tests/run` output nor
   `bench/counts.txt`.
3. **`given` in general.** Declared default per weft (D7), inference for private
   functions (fixpoint), explicit at `pub`; the ambient `Writer`, `Rng`,
   `Clock`, `Logger`, `Pool`; `loom(given: pool)`; `yield`/`pull` in a loom.
   Gate: an unused `given` costs 0 dispatches (counts).
4. **Spool-aware library.** Collection constructors and `.copy()` in the
   ambient spool, `Pool[T]` (alloc/free at most 5 dispatches), `SharedArena`,
   `Deque`/`SortedMap` nodes in a spool. Gate: list drop cost with the spool
   check (see above).
5. **Storable closures and moves out of `mut`.** `scan`, `from_fn`,
   `successors` and the adaptors that keep a callback (`^(…)` boxes the
   captures), `mem.swap/replace/take/drop`, I/O and `time.sleep` as cancel
   points.

V6.5 (language gaps: `Self` for numbers, `where T: [U]`, generic wefts,
`Show` on std warps, more `Default`, `collect_as` call-site errors, `Fmt`)
follows, or interleaves where it is cheaper to do a gap before the slice
that needs it.

## Questions for Caio

- Q8 (`once` values when a thread unravels): ask at slice 5, where `once`
  values start living behind pointers.
