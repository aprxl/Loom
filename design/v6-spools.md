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

**Types carry the region kind, not the spool.** `Region` is `Heap` (an
owning pointer: move-only, dropping frees), `Spool` (any arena: `Copy`, owns
nothing, drop does nothing) or `Any` (the region of a read or `mut`
parameter written without one: of unknown kind, only ever borrowed, so
never dropped and never allocated into). Which spool a value points into is
not in its type: a `LocalId` is per body, and a caller's local means nothing
inside a callee. Layout is the same for all three. Passing `Heap` or `Spool`
where `Any` is expected is a free coercion, so `eval(e: ^Expr)` is one
instance for every tree.

**`given sp: Spool` is a hidden region parameter and a hidden value.** The
kind it is instantiated with (`Heap`, or `Spool` for an `Arena`) is a
generic argument of the instance, so `parse` has an instance for heap trees
and one for arena trees: the `Heap` one frees nodes one at a time, the
`Spool` one bumps a pointer. At a call, the argument (`given: raw`) or the
innermost `given` in scope (else `heap`) picks the kind. `@sp` in the
function's types is that kind.

**Warps follow their region.** A warp with a `^` field, directly or through
another such warp, has one hidden region parameter: `Expr` is
`Adt(Expr, [kind])`, where kind is `Heap`, `Spool` or `Any`. A `String` or
`List` field does not need it: the buffer's spool number lives in its
capacity word, so its drop is right in any region. The parameter is a real
`GenericParamKind::Region` made while names are resolved (as `derive.rs`
adds `Default`) and shown only when it is not `@heap`, so existing programs
and diagnostics don't change. A pointer and its target share a region:
`^Expr` is `Ptr(Expr[r], r)`. Written without a region, `Expr` is `@heap` in
return, field and `take` positions, and in a read parameter it is `Any`
(03-memory).

**Which spool is the region checker's job, not the type's** (below).

**Ambient spool.** `^e`, `.copy()`, templates that make a `String` and
collection constructors allocate in the ambient spool: the innermost `given`
of type `Spool` in scope, `heap` when there is none. A `given sp: Spool` is a
hidden parameter and a region parameter of its own (`@sp`).

**Region checking is a pass over typed HIR** (`typeck/regions.rs`, in the
style of `moves.rs`, sharing its idea of roots). Each local of a type with a
`Spool` region has a set of root spools, worked out from where it came from:
`^e` in the ambient spool `X` is rooted in `X`; a call returning `@out` is
rooted in whatever the call's `given: out` was; a pattern binding or a field
read keeps the roots of what it came from. Then:

- R0101: a value rooted in a spool `X` leaves `X`'s scope: returned, stored
  in something that outlives it, or captured by an escaping lambda.
- R0102: `X.reset()` while a value rooted in `X` is used later.
- R0103: a pointer into a non-shared spool crosses a `thread`/`loom`
  boundary (05-concurrency; extends C0404/C0405's rule).

Types don't say that two pointers share a spool, so the pass enforces it:

- *Arguments meet the signature's names.* An argument whose parameter type
  names `@x` must be rooted in the spool passed or implied for `x`. (Else
  `fun left(e: ^Expr@sp) -> ^Expr@sp given sp` called with `given: small`
  on a `raw` tree returns something rooted in `raw` and typed as `small`'s.)
- *Stores stay in one region.* Storing a value rooted in `Y` into a place
  rooted in `X` is an error unless `X` is `Y`, or the value is heap or static
  (03-memory: each value lives in one region).
- *Loop bodies are gone through twice*, so `loop { render(given: frame);
  frame.reset() }` catches a value from one round used after the next
  round's reset.

`Any` is for read parameters only and is never `Copy` or droppable; the free
coercion `Heap | Spool -> Any` applies to read-parameter arguments. An
omitted region on a `mut` parameter is a real region parameter inferred from
the argument (else `p = ^Node(..)` in `fun f(mut p: ^Node)` would write a
heap pointer into an arena slot).

Moves: a move out of a pointer's target (path element `^`) is allowed when
the pointer is `Heap`, and is M0103 when it is `Spool` or `Any` (arena
pointers are `Copy`: two owners of one `String` otherwise).

The codes are added to `codes.rs` with the slice that uses them.

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
2. **Spools and regions. Done** (functionally; the arena's inline bump op and
   its gates are below). Model as built: `Region::{Heap, Spool, Any}` in
   types; `hir/lower/regions.rs` adds the hidden parameter to warps (and
   weaves) that hold pointers; `given` params are implicit generics (the
   weft's machinery) lowered first so types can name them; `typeck/regions.rs`
   is the roots pass with a reset epoch per spool; a `String` in an arena has
   capacity 0 and is moved to the heap if it grows. Left: R0103, `given` of
   non-spools, collections in a spool.
   Original plan: (A `given` is passed on implicitly at calls:
   the innermost one in scope. Inferring them for private functions is
   slice 3.) `str.copy()` and `String.copy()` allocate in the ambient spool. `Spool` weft, `Arena`, `spool x`, `heap`, `^T@s`,
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

V6.5 (language gaps: `Self` for numbers, `where T: [U]`, generic wefts (done,
except `any W[A]`, inferring their arguments from the result type and
associated types through them), `Show` on std warps, more `Default`, `collect_as` call-site errors, `Fmt`)
follows, or interleaves where it is cheaper to do a gap before the slice
that needs it.

## Questions for Caio

- Q8 (`once` values when a thread unravels): ask at slice 5, where `once`
  values start living behind pointers.
