# Treadle: Loom's own VM

> **Status: V3 is built** (2026-10-11): the
> optimizer runs at `-O2` (`-O0` turns it off), takes away 87% of the
> bounds checks and 62% of the overflow checks a run executes, and costs
> 1.5× of `-O0` to compile; Treadle is 2.74× as fast as PUC Lua 5.4 on
> `bench/` (geomean) against the gate's 2.5× (design/vm-review-v3.md).
> The decisions in
> [section 3](#3-decisions) were settled on 2026-09-29.

Loom runs today by compiling to Lua 5.4 (see [lua-backend](lua-backend.md)).
That backend exists to get the whole language running with the right
semantics. It isn't fast and was never meant to be. This document designs
the other way Loom runs: **Treadle**, a virtual machine of its own, fed by
**Heddle**, a typed mid-level IR that an optimizer works on. On a loom, the
treadle is the pedal that drives it, and the heddles lift the warp threads
for each pass of the shuttle.

The Lua backend stays. It becomes the oracle Treadle is tested against, and
the way to run Loom inside a Lua host.

---

## Contents

1. Goals
2. Where the speed comes from
3. Decisions
4. Architecture
5. Values and memory
6. Heddle
7. The optimizer
8. Treadle bytecode
9. The interpreter
10. The language on Treadle, feature by feature
11. Threads
12. Spools and `given`
13. Lua interop
14. Semantics the VM pins down
15. Testing
16. Performance plan
17. Milestones
18. Switching the default
19. Risks
- Appendix A: one loop, from Loom to bytecode
- Appendix B: the lexer's frame
- Appendix C: from the Lua backend to Treadle

---

## 1. Goals

**Speed.** Treadle is an interpreter with no JIT, and it should be the
fastest one around for the code Loom is for. On the benchmark suite (§16),
with each program written idiomatically for each language and run on the
same machine:

| Compared with | Target (geometric mean over the suite) |
|---|---|
| PUC Lua 5.4 | at least 4× faster, and no benchmark under 2× |
| CPython 3.13+ without its JIT | at least 8× faster |
| Ruby 3.x without YJIT, QuickJS | faster on every benchmark |
| LuaJIT 2.1 with the JIT off (`-joff`) | at least even: the stretch goal |

LuaJIT's interpreter is hand-written assembly and the fastest
dynamic-language interpreter there is, so tying it from Rust is the
ambitious part. The others are within reach because of §2.

**The same language.** Everything that runs on Lua runs on Treadle, with the
same output, the same exit codes and the same unravel reports. The few places
where the VM has to decide something the Lua backend left loose are listed
in §14, each with a fixture.

**The whole language.** Shuttles with fixed-size frames, threads that really
run in parallel, spools, compile-time code, reflection on layouts (`T.size`,
`Frame.size`), and calling Lua. What the Lua backend fakes or can't do
(inline frames, parallelism, `T.size`), Treadle does for real.

**Safe to run.** A Loom program can't corrupt the VM. A compiler bug shows
up in the tests as an internal error with a Loom location (exit 70), through
the checked build (§5.6), not as silent memory corruption.

Not goals:

- **A JIT.** Heddle and the bytecode are shaped so one could be added later (a
  copy-and-patch JIT can be generated from the same op table), but nothing
  here depends on it.
- **Native code.** Heddle is low-level enough to feed Cranelift or C later.
  That would be a separate backend.
- **Running bytecode from untrusted sources.** Images come from our
  compiler. The verifier catches our bugs, not attacks.

---

## 2. Where the speed comes from

Most interpreters are slow for reasons Loom doesn't have. The compiler
already knows what they find out at run time:

| Other interpreters pay at run time for | Why Treadle doesn't |
|---|---|
| checking operand types on every `+`, `<`, `[i]` | types are static: `add.i32` only ever sees `i32`s |
| boxing numbers, tagging values | registers are untagged 64-bit slots |
| looking fields up by name (hash tables, shapes, inline caches) | every field is at a fixed offset: one load |
| looking methods up | every call is static except through `any`, which is one vtable load |
| a garbage collector | every value has one owner and is freed when it goes (§5.4); `Copy` values are flat bytes; arenas free in bulk |
| a heap object per record or tuple | records are laid out inline: in registers, in frames, in arrays of structs |
| assuming anything may alias anything | `mut` is exclusive, so a function can keep a `mut` parameter in a register and hoist loads out of loops |
| overflow and bounds checks everywhere | ranges and gauges prove most of them away (§7) |
| many small instructions | a register VM with fused instructions (compare-and-branch, loads at offsets, counted loops), after inlining |
| a stack per coroutine | a shuttle's frame is a value sized at compile time, and resuming it costs a call (§10.14) |

On top of that, the whole program is compiled at once and monomorphized, and
the parts of the standard library that call back into user code are written
in Loom. So `xs.map(f)` specializes and inlines like a C++ template.

What's left is the price of interpreting: a dispatch per instruction. The
design attacks it three ways: fewer instructions (the optimizer and fused
ops), cheap dispatch (§9.1), and no work inside an instruction that the
compiler could have done.

---

## 3. Decisions

Settled on 2026-09-29: every row as recommended, with D1 widened and D7
narrowed as below.

| # | Decision | Decided | By |
|---|---|---|---|
| D1 | `unsafe` Rust | Used wherever it makes sense, in any crate, not only the VM: for speed (unchecked access the verifier or an invariant already guarantees) or for what safe Rust can't express. Every block gets a `// SAFETY:` comment, and the VM's `checked` build turns its raw accesses into checked ones (§5.6). The all-safe alternative was an estimated 15–30% slower and made shared-memory threads awkward | V0 |
| D2 | How the interpreter dispatches | A `match` loop on stable Rust now. In V8, try tail-call threading: Rust's `become` once it's stable, or a C interpreter generated from the same op table and built with clang-cl like the Lua VM. Keep it only if it wins at least 15% | V0, V8 |
| D3 | `usize` on Treadle | 64 bits (`target.pointer_bits == 64`), like a native target. The Lua target keeps 63 | V0 |
| D4 | Field order in memory | Largest alignment first, declaration order among equals. `#layout(C)` keeps declaration order. It's only visible through `T.size` and `f.offset` | V1 |
| D5 | What `Shuttle[T, U]` and `Stream[T]` are | Existentials. A value made by a known function has that function's own type, sized and stored inline. Where the maker isn't known (a field, a list, two makers meeting in an `if`), the value is boxed like a large `any`. This needs a paragraph in 05-concurrency.md. V2 boxes every held shuttle and builds a generator a `for` loop calls in place (§10.14); inline frames move to V8 | V2, V8 |
| D6 | A `mut` argument when the callee unravels | Its writes are visible, as if it were passed by reference: a `defer` in the caller sees what the callee left. The Lua backend only copies back on return, and that's documented as a deviation | V1 |
| D7 | Dropping a suspended shuttle | Runs its pending `defer`s, the same as closing it: only the ones whose `defer` statement the shuttle has already run, in scopes it's still inside (§10.14). The Lua backend leaves them to the garbage collector | V2 |
| D8 | `x.hash()` values | Deterministic, but each backend's own. Nothing may depend on them matching | V1 |
| D9 | Where `#builtin` bodies come from | Heddle the builder writes, one function per instance, for anything that calls back into Loom or is generic (`push`, `sort`, `map`, showing a list); Rust natives for leaves (UTF-8, number formatting, I/O, moving bytes). A native never calls Loom code. Builtin bodies and glue have no source locations, so an unravel in one is reported at its caller, as Lua's `error(msg, level)` does | V1 |
| D10 | Where programs run | Inside `loom`'s own process: no child process, no temp file. Ctrl-C is a flag the VM checks (§9.5) | V1 |
| D11 | Names | Heddle for the IR, Treadle for the VM, `.treadle` for image files | now |

---

## 4. Architecture

```
            ┌────────────────── shared with the Lua backend ──────────────────┐
 source → lex → parse → resolve (HIR) → typeck, per instance (Results, Bodies)
                                                   │
                ┌──────────────────────────────────┴──────────────┐
                ▼                                                 ▼
        lower → LIR → Lua bundle                     build Heddle (typed SSA)
        (lua-backend.md)                                          │
                                                verify → optimize → verify
                                                                  │
                                                select → allocate registers
                                                                  │
                                             bytecode → verify → image
                                                                  │
                                                          Treadle runs it
```

| Crate | What |
|---|---|
| `loom_compiler` | adds `ty::layout` (sizes, alignments, offsets and niches for a target), `heddle/` (the IR, its builder, verifier, printer and passes) and `codegen/` (instruction selection, register allocation, emission) |
| `loom_isa` (new) | the op table, the instruction encoding, the image format, the bytecode verifier and the disassembler. Nothing but std, so the compiler and the VM share it |
| `loom_vm` (new) | the interpreter, memory and the allocator, natives, unwinding, the scheduler (V5), spools (V6), and the bridge to Lua (V7, a feature) |
| `loom_sys` (new) | host services for both backends: the terminal, files (real or in memory), the environment, the clock. Moved out of `loom_host` |
| `loom_host` | the Lua VM, as now |
| `loom` | `run`, `test` and `build` get `--backend vm\|lua` (§18) |

The frontend doesn't change shape. Heddle is built from what the Lua lowering
reads: the HIR, and each instance's `Body` of types and decisions (`calls`,
`coercions`, `tries`, `streams`, `moves`, `const_values`, …). What a flow
analysis needs (moves, drops, liveness at suspension points) is worked out
again on Heddle's control-flow graph, where it's exact.

---

## 5. Values and memory

Treadle lays values out in memory the way a native compiler would. Every type
has a size, an alignment and field offsets, all fixed at compile time. That
one choice pays off everywhere:

- copying a `Copy` value is copying bytes;
- a `mut` parameter is an address;
- `T.size` has an answer;
- a shuttle's frame fits inline in the shuttle;
- threads share one address space;
- a constant is bytes in the image.

### 5.1 Target facts

| | Treadle | Lua target |
|---|---|---|
| `target.pointer_bits` | 64 | 63 |
| `usize`, `isize` | `u64`, `i64` | `u63`, `i63` |
| `target.endian` | the host's | the host's |
| `target.os`, `target.arch` | the build host's | the build host's |

`target` becomes a parameter of type checking (today it's fixed), so
compile-time code sees the target being built. A Lua-target build does its
`usize` arithmetic in 63 bits even when Treadle is the evaluator (V4).

### 5.2 Layout

Primitives:

| Type | Size | Alignment | Spare bit patterns (niches) |
|---|---|---|---|
| `bool` | 1 | 1 | 2–255 |
| `char` | 4 | 4 | 0x110000 and up |
| `iN`, `uN` | 1, 2, 4 or 8: the smallest that holds N bits | its size | the patterns past N bits (a `u12` has 61 440) |
| `f32`, `f64` | 4, 8 | its size | none |
| `()`, `never`, unit warps | 0 | 1 | none |

Everything else:

| Type | Layout |
|---|---|
| tuple, record, tuple warp | fields ordered by alignment (D4), padded, with the size rounded up to the alignment |
| sum (a warp with cases) | the shared fields, then a tag (`u8`, or `u16` past 256 cases), then the cases' payloads overlapping, each laid out like a record. The tag's unused values are niches |
| unit-only sum | the tag alone |
| `?T` | `T` itself when `T` has a niche, with `none` as the niche; otherwise `T` followed by a `bool` |
| `T ! E` | a tag, then `T` and `E` overlapping |
| `A \| B` | a tag (the member's position in the sorted member list), then the members overlapping |
| `any W` | 24 bytes: a 16-byte payload and a vtable pointer. A value bigger than 16 bytes or aligned past 8 is boxed in the ambient spool, and the payload holds the box. The vtable says which |
| `[T; N]` | N elements, inline |
| `[T]`, `str` | pointer and length (16 bytes) |
| `List[T]`, `String` | pointer, length and capacity (24 bytes). The capacity's top 16 bits are reserved for the spool the buffer lives in (§12) |
| `Map[K, V]`, `Set[T]` | a list of entries (hash, key, value) in insertion order, and an index table of entry numbers (48 bytes) |
| range | low end, high end, and which ends are open |
| `A => B` | a function number and an environment pointer (16 bytes) |
| `^T` | a pointer (8 bytes), never null, so `?^T` is 8 bytes too |
| a shuttle made by `f` | `f.Frame`: a state word and what `f` keeps across its suspensions (Appendix B) |
| `Shuttle[T, U]` or `Stream[T]` whose maker isn't known | a box and a vtable, like `any` (D5) |

Some consequences: `?String` is 24 bytes (a null pointer is `none`), `?char`
is 4, `?Shape` costs nothing because the tag has room, and `?Point` for two
`f64`s is 24.

```
warp Point { x: f64, y: f64 }            size 16, align 8
warp Token { span: Span, case Ident(String), case Int(u64), case Eof }
    span 0..8, tag 8, payload 16..40     size 40, align 8
?Token                                   size 40: tag value 3 means none
```

### 5.3 Where values live

- **Registers**: untagged 64-bit slots in the current frame. Scalars live
  here (numbers, `bool`, `char`, pointers, function numbers), and so do the
  parts of small aggregates once the optimizer splits them (SROA): a `Point`
  local becomes two float registers, a `str` two registers.
- **Frame memory**: aggregates that stay aggregates (arrays, big records, and
  anything whose address is taken for a `mut` argument or a lambda) have a
  fixed place in their function's frame.
- **The heap**: the buffers of lists, strings and maps, boxed `any`s, `^T` on
  the heap, the environments of escaping closures, boxed shuttles and
  streams.
- **The image**: string literals, constants (from V4), vtables and type
  descriptors. It's read-only and never freed. That's `@static`.

### 5.4 Ownership: moves, copies and drops

Every value has exactly one owner, which the move checker already proves.
Treadle turns that into memory management with no collector.

- **Copying a `Copy` value is copying its bytes.** A `Copy` type can't own
  anything: its fields are all `Copy`, and lists, strings and maps aren't. So
  there's never a deep copy to do. That's the whole cost of value semantics
  for plain data, and after SROA it's usually a register move.
- **Moving is copying the bytes and forgetting the source.** Nothing is
  freed.
- **Dropping** runs the type's *drop glue*, generated per type. A `List[T]`
  drops its elements (when `T` has glue) and frees its buffer. A sum drops
  its case's payload. An `any` calls its vtable's drop. A `^T` on the heap
  drops what it points at and frees it. A type with nothing to free has no
  glue, and dropping it costs nothing.
- **Drop elaboration**, a Heddle pass, puts drops on every way out of a
  scope, unwinding included (§9.4). Where a value is moved on some paths and
  not others, a one-byte drop flag decides. Everywhere else the decision is
  static.
- **`.copy()`** on a non-`Copy` value runs generated deep-copy glue.
- **`once`** leaves nothing at run time, as on Lua. A `once` value that an
  unravel leaves behind without a fallback `defer` is leaked, with whatever it
  owns, as 03-memory.md says.

**The leak invariant:** a program that ends without unravelling has freed
everything it allocated, except the image. The checked build (§5.6) checks
this at exit, for every fixture. Leaks and double frees are how ownership
bugs in the compiler show up, and this catches them.

This makes aliasing bugs visible that Lua's garbage collector hid. The
review's V4 (`first()` and `get()` hand out a list's non-`Copy` element
without copying it) is a double free on Treadle, so it's fixed before V1
(§17).

### 5.5 The heap

`heap` is a size-class allocator on top of Rust's global allocator:

- Sizes up to 32 KiB go in classes about 25% apart, carved out of 64 KiB
  pages. Each class has its own free list. An object's class comes from its
  page, so small objects have no header.
- Bigger ones come straight from the global allocator.
- The limit is 8 GiB, as on Lua. Past it, allocation unravels with
  `out of memory`.
- From V5 each worker thread has its own cache, and a buffer freed on
  another thread goes back to the page it came from.

The fast path (pop a free list) is a handful of machine instructions, inside
the ops that allocate.

### 5.6 Safety

This is D1. Treadle's memory is raw: registers are slots found from a frame
pointer, heap objects are real addresses, and the interpreter reads and
writes them without checking. That's sound when both of these hold:

1. **The bytecode verifier** (§8.6) has proven that every register number,
   jump and call is in range, so the interpreter's own bookkeeping can't go
   wrong.
2. **The frontend's guarantees** hold: no use after a move, exclusive `mut`,
   no borrow outliving what it borrows, no region pointer outliving its
   spool.

The second is trust in our own compiler, and the compiler has known gaps
(the region checker doesn't exist yet). So:

- **`unsafe` is used where it makes sense**, in any crate. In the VM that's
  mostly `loom_vm::mem` (loads, stores, the allocator) and `loom_vm::interp`
  (register access, the dispatch loop); elsewhere it's whatever a profile
  or an API calls for. The workspace lint goes from `unsafe_code = "forbid"`
  to `"warn"` when the first `unsafe` lands, so every use is deliberate
  (`#[allow(unsafe_code)]` on the module or item). Every block says why it's
  sound in a `// SAFETY:` comment, the way Loom's own `unsafe "…"` blocks
  have to.
- **The checked build** (`--features checked`): every access goes through a
  shadow map of live allocations and frames. An access outside them, a use
  after free, a double free, or a leak at exit is an internal error (exit
  70) with the Loom location. The whole test corpus runs checked as well as
  unchecked.
- **Dynamic guards stay until the frontend proves the property.** A loop over
  a list checks that the list's buffer and length haven't changed (what
  `rt.same_len` does on Lua) wherever the body calls something that might
  reach the list, until the escape and region checks make that impossible.
- **Miri** would run the VM's unit tests when a nightly toolchain is installed; it hasn't been run yet (the interpreter keeps addresses in `u64` registers, which Miri's permissive provenance mode accepts).
  It isn't a gate.

---

## 6. Heddle

### 6.1 Its shape

- One Heddle function per instance, as on Lua.
- A control-flow graph of blocks. Blocks take parameters instead of having
  phi nodes, as in Cranelift. Values are in SSA form, with machine types:
  `int` (an integer of any width, a `bool` or a `char`, normalized in 64
  bits, so widening one is free), `f32`, `f64` and `ptr`. A Loom value of a
  small compound type (a tuple, a `str`) is several values, one per part.
- Integer instructions carry the Loom integer type (`u12`, `i64`) and the
  overflow policy, so checks are exact and passes know the ranges.
- Aggregates are memory: a `slot` (size, alignment) in the frame, `load` and
  `store` at offsets, `copy` of n bytes. SROA turns slots back into SSA
  values when their address doesn't escape.
- **Checks are instructions**: `bounds`, `gauge_check`, and arithmetic with
  the `trap` policy. Passes remove them when they're proven not to fire.
- **Unwind edges are explicit.** An instruction that can unravel (a call, a
  checked operation) names the block to go to if it does, when there's
  anything to clean up (like LLVM's `invoke`). Drops and `defer`s on those
  edges are ordinary code. The bytecode turns the edges into tables, so the
  normal path pays nothing for them (§9.4).

### 6.2 Its instructions

| Group | Instructions |
|---|---|
| constants | `iconst`, `fconst`, `data` (an address in the image), `fnref` |
| integers | `iadd`, `isub`, `imul` with a policy: `trap`, `wrap`, `sat`, or `checked`, which gives (ok, value); `idiv` and `irem` (trap on zero, and on `MIN / -1`); shifts (trap on an amount ≥ the width); `and`, `or`, `xor`, `not`, `neg`; `icmp` |
| floats | `fadd`, `fsub`, `fmul`, `fdiv`, `fneg`, `fabs`, `fsqrt`, …; `fcmp`, ordered or unordered explicitly |
| conversions | `widen` (free), `narrow` with a policy, integer ↔ float with a policy, `f32` ↔ `f64`, `gauge_check` |
| memory | `slot`, `load`, `store`, `copy`, `zero` |
| ownership | `drop T`, `copy_deep T`. Drop flags are ordinary `i8` slots |
| calls | `call f`, `call_fn` (a function value), `call_vt` (a vtable slot), `call_native n` |
| control | `jump`, `brif`, `switch` (dense tables), `ret`, `unravel`, `unreachable`, `safepoint` |
| shuttles | `suspend yield v` and `suspend pull` until the shuttle pass splits them (§10.14) into `frame_addr`, `state_switch` and `ret_suspended k` |
| threads (V5) | `spawn`, `join`, `loom_enter`, `loom_exit`, atomic operations |
| spools (V6) | `alloc` (spool, size, alignment), `arena_reset` |

### 6.3 Building it

| From | To |
|---|---|
| `let`, `var` | SSA variables, using Braun et al.'s on-the-fly SSA construction. A variable whose address is taken gets a slot |
| `if`, `match`, blocks as values | blocks with parameters. There's nothing like Lua's statement lifting to do |
| `match` | a decision tree (Maranget's), with `switch` for dense tags and integers. Twill patterns become mask-and-compare, or a `switch` on the fixed bits when that's denser: chip8's `0x8xy?` family is one switch on the low nibble |
| `?`, `mend` | a branch to the mend's block, with the error as a block parameter. `fail` is a return of the error. A `?` that leaves the function converts the error and returns |
| `defer` | code on each way out, unwind edges included. A small body is copied onto each edge; a big one is a shared block with a selector saying where to go next |
| `defer x => …` | put only on the ways out where `x` is still live, from the move facts. There's no flag at run time: this is the native backend 03-memory.md describes |
| loops, labels, `break` with a value | blocks and jumps |
| `for`, per `IterSource` | `Seq`: a counted loop over the buffer, the item an address or a loaded scalar, copied only if it's kept. `Range`: a counted loop. `Woven`: calls to `next`. `MapPairs`, `SetItems`: the entry list. `Shuttle`, `Burst`: steps (§10.14) |
| templates | writes into the destination, piece by piece (the standard output buffer, a `Fmt`, a `String`), never a temporary string when the destination can take the pieces |
| lambdas | a function and an environment: the enclosing frame's captured slots for a lambda that can't escape, or a heap box for `^(…)` |
| constants | V1: a data slot computed on first read, as `rt.lazy` does. From V4: data in the image |
| reflection | `v.(f)` is a field offset and `v is .(c)` a tag compare. Everything else was folded by the type checker |
| coercions | `ToAny` builds the 24-byte `any`, `ToUnion` writes the tag, `ToSlice` makes a pointer and a length, `ToStream` makes a stream state (§10.13) |

### 6.4 Verifying and printing it

- `heddle::verify` runs after building, and after every pass in debug builds.
  It checks operand types, that every definition dominates its uses, block
  parameters, that only instructions that can unwind have unwind edges, and
  that every path ends in a terminator.
- `loom build --emit heddle` prints it. `tests/heddle/` holds snapshot tests,
  text in and text out, for the builder and for each pass.

---

## 7. The optimizer

The rules:

- **Semantics first.** A pass may remove a check only when it proves the
  check can't fire.
- **Traps are observable.** A pass never moves one across another trap, or
  across anything the program can see: I/O, a write to memory that someone
  else can read, or a call that might do either. When two things in a
  statement could unravel, the first in evaluation order does (§14, P8).
- **Floats aren't reassociated.** `x * x` computed twice is the same value,
  so it's computed once, but `(a + b) + c` stays as written.
- **Levels:** `-O0` builds, verifies, elaborates drops, splits shuttles and
  generates code (the fastest compile and exact stack traces). `-O2` is the
  default for `loom run`. The corpus is tested at both.
- **Compile time has a budget:** `-O2` costs at most 1.5× `-O0` on the test
  corpus (a V3 gate).

The passes, in order:

| Pass | What it does | Why it works so well in Loom |
|---|---|---|
| 1. inline | inlines small functions (up to about 40 instructions), functions called once, and std's leaves (`len`, `push`'s fast path, indexing). Bottom-up over the call graph, with a budget of 2× growth per function | calls are static, and monomorphization gave every callee concrete types |
| 2. specialize lambdas | a function called with a known lambda (`xs.map(x => x * 2)`, `sort_by`, `each`, `fold`) gets a copy with the lambda built in, which pass 1 can then inline | a lambda passed as an argument can't escape, so the copy is private |
| 3. SROA | aggregates whose address doesn't escape become SSA values | |
| 4. SCCP | propagates and folds constants, and drops unreachable blocks | folding goes through the `ty::int` oracle, so it matches run time exactly |
| 5. GVN | shares pure computations, and loads nothing could have changed | a local or a `mut` parameter can only change through its own name, so alias analysis is mostly syntactic |
| 6. LICM | hoists invariant loads and computations out of loops: a list's pointer and length, a bounds check against a length that doesn't change | a loop's body can't change what the loop goes over |
| 7. ranges | interval analysis over SSA (widening at loop heads), seeded by types, gauges, `for` ranges and conditions. Removes the overflow traps, bounds checks and gauge checks that can't fire | a `u16` counter under `i < limit` can't overflow, and `for i in 0..xs.len()` can't index past the end |
| 8. streams | a stream built and used up in one place (`xs.filter(p).map(f).sum()`, `for x in (1..n).filter(p)`) becomes one loop | a stream is read once and can't be shared (reusing one is a move error) |
| 9. projections and generators | a projection used by one statement (`counts.entry(k, default: 0) += 1`) is inlined around the place's use. A generator a `for` goes over directly may be inlined into the loop | projections last one statement, and the generator's frame belongs to the loop |
| 10. copy → move | a value's last use moves it where it would have been copied | the move facts come from the same analysis as the drops |
| 11. drops | elaboration, then removing drops of values moved on every path, and merging frees | |
| 12. cleanup | dead code, merging blocks, threading jumps | |
| 13. self tail calls | a function that calls itself in tail position, with nothing to clean up, loops instead | |

Code generation adds switch lowering (jump tables, bit tests, binary search)
and superinstructions (§8.4).

**What V3 built.** `opt/` runs, in order: `inline`, `prune`, `sccp`,
`cells`, `gvn`, `sccp`, `inline`, `prune`, `gvn`, `sccp`, `ipcp`, `ranges`,
`sccp`, `licm`, `cleanup` (`LOOM_PASSES=a,b` runs only those;
`LOOM_PASS_TIMES=1` times each). Besides the table's:

- **`prune`**: a function nothing reaches (what inlining used up the last
  call of) has its body replaced by a stub, so later passes and code
  generation skip it. Functions are numbered, so they stay in place.
- **`cells`** (with GVN): a loop that counts `p` from 0 below a bound and
  adds one to a frame word each trip (a list's length, pushed to once a
  trip) leaves the word `bound` more than it was; the pass says so with a
  store where the loop exits, which GVN keeps. `mem.grow` writes words 0 and
  2 of the header it's given and nothing else, so the header doesn't escape.
- **`ranges`** takes a passed `bounds` as a fact from there on, and narrows
  again what widening overshot.
- **GVN** keeps what's known across a join no path to it writes in.
- **`ipcp`** (a parameter every call gives one constant is that constant).

Built where the typed program becomes Heddle, not as passes of their own:
projections used by one statement and generators a `for` goes over (9), and
the cleanups of moved values (10, 11, `build/cleanup.rs`).

**Deferred**, with what covers them for now: *specialize lambdas* (2:
GVN turns a call through a known function value into a call, which inlining
then copies in), *SROA* (3: GVN forwards through frame slots, and the slowest
benchmarks' time is in hashing, strings and natives; not measured to be
what's missing)
*self tail calls* (13), and merging frees (11).

Every pass has snapshot tests, and the program generator (§15) compares
`-O0` with `-O2`.

---

## 8. Treadle bytecode

### 8.1 Encoding

An instruction is one 64-bit word, sometimes followed by extension words:

```
 63         48 47         32 31         16 15          0
┌─────────────┬─────────────┬─────────────┬─────────────┐
│      c      │      b      │      a      │   opcode    │
└─────────────┴─────────────┴─────────────┴─────────────┘
```

- `a`, `b` and `c` are register numbers (0–65535, counted in 8-byte slots
  from the frame pointer), or a 16-bit immediate. Together, `b:c` can be a
  32-bit immediate or jump offset.
- A frame is at most 65 535 words, since registers number them, and frame
  slots (memory for values that live in memory, arrays on the stack) are in
  it too; so a slot over 64 KiB (1 KiB, if the frame is still too big)
  lives on the heap: the frame holds its address (and size), the prologue
  allocates it, and a return and every landing pad's end free it.
- A 64-bit constant (an `i64`, an `f64`, an address) takes an extension word.
- Why 64-bit words: 16-bit register numbers mean no 200-local or
  255-register limit (lua-backend.md's biggest headache), three operands
  fit, and every word is aligned. The code is a little bigger than Lua's
  32-bit words, and it has fewer instructions.

### 8.2 Ops

Families, specialized along the dimensions that matter:

- **integer arithmetic**: add, sub and mul × policy (trap, wrap, sat) × 8,
  16, 32 or 64 bits, signed or not; a generic form that takes the width as an
  operand (`u12`, `i24`); and immediate forms for the common widths
  (`addk.i64 r, r, imm16`, `subk`, `add%k`). The `checked` policy writes two
  registers.
- **division and remainder** (the trapping forms), shifts (trapping on the
  amount), bitwise operations.
- **floats**, `f32` and `f64` separately.
- **compare-and-branch**: `jlt.s a, b, off`, `jnle.f64 a, b, off`, and
  against a 16-bit constant, `jltk.s a, k, off`. The negated float forms are
  how NaN goes the right way. A plain compare that writes a `bool` is for
  when the result is stored.
- **conversions**: narrowing with a policy, integer ↔ float. Widening is a
  `mov`, and usually not even that.
- **memory**: loads and stores of 1, 2, 4 or 8 bytes (sign- or
  zero-extending) at a register plus a 16-bit offset; frame-relative forms
  for aggregates in the frame; `copy` specialized for 8, 16, 24 and 32 bytes,
  and a general one.
- **sequences**: `ldx.w dst, base, idx` checks the bounds and loads in one
  instruction (`base` names two registers, pointer and length), and so does
  `stx.w`.
- **calls**: `call f, base`, `call_fn`, `call_vt slot`, `call_native n`,
  `ret`, `ret1`, `ret0`.
- **control**: `jmp`, `jz`, `jnz`, `switch` (a jump table), and `loop off`: a
  back-edge that counts a tick (§9.5). A conditional jump that goes
  backwards counts one too: a loop whose back edge repeats the head's test
  (it's *rotated*, §8.4) branches straight back into its body.
- **counted loops**: `forrange i, end, off` increments, compares and
  branches in one instruction; `foreach.w` walks a buffer.
- **traps**: `trap kind`, `unravel`.
- Later milestones add shuttle, thread, atomic and spool ops.

That's around 300 to 450 opcodes. A `match` over a dense `u16` compiles to a
jump table.

### 8.3 One op table

Every op is declared once, in `loom_isa/src/op.rs`, with its mnemonic and
its operands' form:

```
AddTI32 = "add.i32", ABC;      // a, b, c: registers
AddTKI32 = "addk.i32", ABK;    // a, b: registers; c: a 16-bit constant
JLtSK = "jltk.s", AKJ;         // a: a register; b: a constant; c: an offset
```

From that table come the `Op` enum, the verifier's operand rules and the
disassembler. The interpreter matches on `Op`, so Rust checks that every op
has an arm; the arms of the width-specialized families are written by a
generator once and stay one line each, calling the integer semantics in
`loom_vm::ints` with a constant width, which folds away. If D2 goes to a C
interpreter, its handlers are generated from the same table.

### 8.4 Selection and superinstructions

Code generation picks the fused form whenever the pattern is there. A compare
whose only use is a branch becomes a compare-and-branch. A small constant
operand becomes an immediate. A field's offset folds into the load. `i += 1`,
a compare and the back-edge become `forrange`. A loop whose head is a short
test is *rotated*: its back edge runs the head's instructions again and
branches back into the body when the test holds, so an iteration costs no
`loop`.

V8 adds more from measurements. The stats build counts pairs of adjacent ops
over the benchmark suite, and the most frequent pairs that can fuse become
ops. An op is kept only if it wins on the suite.

### 8.5 Registers and frames

- Allocation over SSA live ranges with holes (a value dead between two
  uses leaves its register to others there), after coalescing each block
  parameter with what its edges pass it where their ranges don't clash
  (loops' back edges first), with hints: a value only
  passed to a call is made in the call's window, a call's result stays where
  the call left it (when no other call comes before its last use), a value
  `ret` gives back is made in its result register, and a block parameter
  shares a register with the values its edges pass.
- **Calls use a window**, as in Lua. The caller puts the arguments in
  consecutive registers at the top of its frame, and the callee's frame
  starts there, so its parameters are already in `r0, r1, …`. Results come
  back the same way. No arguments are copied.
- **The linkage** (the return address, the caller's frame, and where the
  results go) is in the three registers just below the window, which the
  caller leaves free; the verifier checks that it does. So nothing is kept
  past a frame, where the frames of the calls it makes are laid.
- **Aggregates returned by value** are written straight into the caller's
  destination, whose address the caller passes. `let p = make_point()` builds
  `p` in place.
- **Parameters**:
  - a read parameter of 16 bytes or less goes by value, a bigger one by
    address (03-memory.md: the compiler decides by size);
  - a `take` parameter goes by value, or by the address of a temporary the
    callee then owns;
  - a `mut` parameter goes by address. The callee keeps a scalar one in a
    register and writes it back on every way out, unwinding included. That's
    as fast as copy-in/copy-out and means the same as by reference (D6).
- **A frame** is its values' registers, a temporary (for swapping values on
  an edge), scratch (natives' arguments; calls' linkage), then the window;
  from V1, its aggregate area and drop flags. Its size is in the function's
  header, so a call checks for room once.

### 8.6 The verifier

It runs on every function before the function can run, whether it was just
compiled or loaded from an image:

- every register number is less than the frame size, every jump lands on the
  start of an instruction in the same function, and the last instruction
  can't fall off the end;
- every function, native, vtable slot and data reference exists, and each
  call site's window fits the callee's parameters;
- the unwind table's ranges nest and point at landing pads;
- extension words are where their ops say.

It doesn't check what a register's value *means*: whether a pointer really
points somewhere. That's the compiler's job, and the checked build's (§5.6).

### 8.7 Images

An image holds:

- a header: a magic number, the format version, and the compiler's build
  hash;
- functions: bytecode, frame size, name;
- data: literals, constants, vtables and type descriptors, with relocations
  for the pointers inside them;
- the natives it uses, by name, resolved when it loads, so a missing native is
  a load error and not a crash;
- unwind tables;
- debug info: a line table per function, what was inlined where, file names.

`loom build --backend vm` writes one and `loom run x.treadle` runs one.
Images aren't portable across compiler versions, and the header says which
compiler made them. A standalone executable (the `loom` binary with an image
appended) comes in V9.

---

## 9. The interpreter

### 9.1 The loop

In outline:

```rust
loop {
    let w = fetch(pc);              // unchecked: the verifier proved pc in range
    pc = pc.add(1);
    match Op::of(w) {
        Op::AddI32Trap => {
            let (x, y) = (fp.i32(b(w)), fp.i32(c(w)));
            match x.checked_add(y) {
                Some(r) => fp.set_i32(a(w), r),
                None => pc = trap(task, pc, Trap::Overflow)?,   // cold, out of line
            }
        }
        Op::JltI64 => {
            if fp.i64(a(w)) < fp.i64(b(w)) {
                pc = pc.offset(c16(w));
            }
        }
        // …
    }
}
```

- Only `pc` and `fp` stay in locals, so the compiler keeps them in machine
  registers. The rest (the end of the stack segment, the tick counter, the
  interrupt state) is in one `Env` behind `black_box`, read from memory where
  needed: as locals they took `pc` and `fp`'s registers, and every
  instruction paid (the loop swung by 30% with unrelated changes).
- An instruction either goes on (`continue 'run`) or leaves its labeled block
  with a trap, which one place after the `match` unwinds: handled inline at
  each instruction, the loop got too big to keep `pc` and `fp` in registers.
- Each op gets its own indirect jump only because LLVM is told to duplicate
  the dispatch (`-C llvm-args=-tail-dup-pred-size=1000` and `-succ-size`, in
  `.cargo/config.toml`, for every target), and each op's code starts on a
  32-byte boundary (`-align-all-nofallthru-blocks=5`): without it, where the
  code landed moved a benchmark by 24% when only the compiler changed. A
  `RUSTFLAGS` environment variable would replace them, which a test in
  `loom_vm` checks.
- The dispatch loop is the first code in the binary, then the natives and
  the allocator it calls most (the `loom` binary's build script hands the
  linker an `/ORDER` file of their names): their addresses moved with the
  size of everything else, and `fib` ran 30% slower, `word_freq` 13%, with
  the same bytecode. What else the loop calls out of line (`memcpy`, the
  other natives) still moves, by a few percent at most. Only MSVC targets
  (lld-link) take the file; elsewhere the code lands where it lands.
- Everything rare is `#[cold]` and out of line: traps, the allocator's slow
  path, stack segment switches, unwinding.
- Handlers never allocate Rust values or format strings on their fast path.

Rust has no computed goto. The `match` compiles to one indirect jump shared by
every op, which modern branch predictors handle better than folklore says,
but not perfectly (D2). What V8 measures:

- tail-call threading with `become`, once explicit tail calls are stable;
- a C interpreter generated from the op table, with `[[clang::musttail]]`
  and the `preserve_none` calling convention, built with clang-cl as the Lua
  VM already is;
- profile-guided optimization of the `loom` binary, which typically gives an
  interpreter 10–20% on its own.

### 9.2 Calls and the stack

- A **task** (the main program, or a thread from V5) has a **value stack** of
  8-byte slots. Frames and their linkage (§8.5) are all in it: there's no
  separate control stack, and a function is found from its return address
  when an unravel needs it.
- **The value stack is segmented.** When a frame doesn't fit in the current
  segment, the call moves to a new one, copying the arguments, and the return
  copies the results back. Segments never move, so a pointer into a frame (a
  `mut` argument, a lambda's environment, a thread's borrowed capture) stays
  valid. A freed segment is kept for next time, so a call right at the edge
  in a loop doesn't allocate every time.
- **A call** checks for room (one compare against the segment's end), writes
  the linkage below the window, moves `fp` up to it and jumps. **A return**
  reads the linkage back. The target is under 10 ns for a call and its
  return: `fib(32)` makes about 7 million calls, so under 70 ms for those
  (V0 runs the whole program in about 30 ms).
- **Stack overflow**: past the limit (256 MiB of value stack for the main task
  by default), a call unravels with `stack overflow`. Loom code never
  recurses on the Rust stack, so only memory limits the depth.

### 9.3 Natives

A native is a Rust function over the argument window,
`fn(&mut Ctx, Args) -> Result<(), Trap>`, for leaf work: UTF-8, formatting
and parsing numbers, searching strings, hashing bytes, copying, I/O.

**A native never calls Loom code.** Anything that needs to (sorting with a
Loom `<`, `map`, showing a list of user types) is written in Loom (D9), where
it specializes and inlines. So the Rust stack never interleaves with Loom
frames, unwinding is one mechanism, and a thread can be parked anywhere.

Natives that touch the host (`io`, `fs`, `env`, `time`, `term`) are marked as
such, so the compile-time sandbox and sandboxed test runs can refuse or fake
them.

### 9.4 Unwinding

Four things unwind a task:

- an **unravel**, with its message and location;
- an **interrupt** (Ctrl-C);
- a **cancel**, when a loom block leaves early (V5);
- a **close**, when a shuttle is closed or dropped part-way through.

All four run `defer`s and drops the same way. Only an unravel is an error.

- Each function has a table from pc ranges to landing pads, built from
  Heddle's unwind edges, innermost first.
- To unwind: note the reason on the task, find the landing pad for the
  current pc, and jump there. A landing pad is ordinary bytecode (drops,
  `defer` bodies) ending in `resume`, which goes on to the next enclosing pad,
  or out to the caller, where the search starts again at the call's pc.
- The normal path pays nothing: no `pcall` per frame, no `<close>` objects,
  no flags.
- An unravel inside a landing pad replaces the one in flight, as on Lua.
- A fallback `defer x => …` is only in the pads of the ranges where `x` is
  live.
- At the bottom of the main task, an unravel prints its report and exits with
  101, and an interrupt exits with 130.
- `unravel = .abort` (a `bolt.loom` setting, later) skips the tables and
  exits at once.

### 9.5 Safepoints and ticks

Back-edges (`loop`, `forrange`, and any conditional jump backwards) and
calls count down a tick counter kept in `Env`. When it runs out, one cold function checks everything that can
interrupt a program:

- Ctrl-C, a flag the signal handler sets;
- the compile-time step budget (K0102, from V4);
- the time limit;
- preemption between threads (V5).

Then it resets the counter: every 65 536 ticks in a normal run, so Ctrl-C is
noticed within about a millisecond. The cost is a read, a decrement and a
write of the counter in `Env` and one predicted branch per loop iteration.
Natives that wait (`time.sleep`) check the same flag themselves, in slices.

Cancellation (V5) is *not* checked at ticks. 05-concurrency.md says a thread is
cancelled at its next `yield`, `pull`, `join`, I/O call or `sched_yield()`,
and those are exactly where Treadle checks.

### 9.6 Reports

- The line table maps a pc to a Loom file, line and column. Inline info
  recovers the frames the optimizer inlined.
- An unravel prints the same report as on Lua (`--> file:line:col`, the
  source line, the message). With `LOOM_BACKTRACE=1` it lists every Loom
  frame, inlined ones included. The Lua backend only knows the first.
- The exit codes are the Lua backend's: 0, 1 (`main` returned an error), 2,
  70 (an internal error: a verifier failure, a checked-build violation, a trap
  that can't happen), 101, 130, and `process.exit(n)`.

### 9.7 The host

`loom_sys`, shared with the Lua host, provides the terminal (raw mode and
keys, through crossterm), files (real, or in memory for sandboxed runs), the
environment, the clock and sleeping.

Standard output is buffered. It's flushed at each newline when it's a
terminal and when it's full otherwise, and also before reading input, before
terminal operations, and at exit however the program ends. Sandboxed runs
behave exactly as on Lua: no environment, files in memory, no terminal, no
waiting.

---

## 10. The language on Treadle, feature by feature

### 10.1 Integers

- Registers hold every integer normalized, sign- or zero-extended from its
  width, as on Lua. So widening is free and comparing is a plain 64-bit
  compare.
- **trap**: a 64-bit operation, then a check that the result fits the width.
  Below 64 bits the operation itself can't overflow. **wrap**: sign-extend
  or mask. **sat**: clamp. **checked**: a flag and a value.
- `u64` uses unsigned machine operations: nothing is emulated (Lua needed
  `math.ult` and friends). Division truncates natively, which is Loom's rule,
  with the traps for zero and `MIN / -1`. Shifts trap at an amount ≥ the
  width.
- Casts: widening is free and narrowing follows its policy. Float → integer
  truncates and traps on NaN or out of range; `as|` saturates, with NaN → 0.
- Integers wider than 64 bits and `f16` can come later on Treadle (two
  registers, and software `f16`). They stay U0102 until then.

### 10.2 Floats and formatting

`f64` and `f32` are native IEEE arithmetic. `f32` math is really `f32` math,
with no `string.pack` rounding.

Display is Rust's shortest round-trip form, which the Lua runtime imitates.
So Treadle becomes the reference, and the review's float items (F2–F4: `f32`
digits, `inf` and `NaN` padding, fill characters) are pinned by one table of
cases that both backends pass (§14, P5).

### 10.3 Text

- `str` is a view and `String` owns its bytes, both UTF-8. Literals are in
  the image.
- A template writes piece by piece into its destination. `print("{a} {b}")`
  writes into standard output's buffer; a template stored in a `String` sizes
  the buffer once. Its operands are all worked out first, left to right, and
  only then is anything written, so an operand that prints does so before
  the template, as on Lua.
- `for c in s.chars()` decodes UTF-8 in the loop, with a fused op and no
  stream object.
- String methods are natives over bytes. Keeping short strings inline (SSO) is
  a V8 experiment.

### 10.4 Warps

- Construction writes the fields in place, or into registers after SROA.
  Reading a field is a load at an offset.
- A dot-block edits in place or edits a copy, whichever the type checker
  decided.
- Shared fields sit at the same offset in every case, so reading one needs no
  `match`. Testing a case compares the tag.
- Methods and UFCS are static calls. A method forwarded by `through` is a call
  on the field, which inlines.

### 10.5 Optionals and results

- Niches make most `?T` free: `?String`, `?char`, `?Token`, `?^T`.
- In registers, a `?T` is two values (present, value), or one with a niche. A
  `T ! E` is a tag and a value, returned in two registers and never boxed. The
  Lua backend's `{true, v}` tables go away.
- `?` and `mend` are branches, and `fail` is a return.

### 10.6 Unions and `any`

- A union is a tag and a payload, inline. There's no box. Widening into a
  bigger union renumbers the tag through a small table fixed at compile time,
  and a type pattern compares tags.
- `any W` is 24 bytes (§5.2). There's a vtable per type and set of wefts. It
  holds the type's descriptor (unique per type, so `w is b: Button` compares
  a pointer), the size, whether the value is boxed, drop and copy glue,
  `show`, `eq` and `hash` for printing and comparing, and a slot for each weft
  method called through an `any`.
- Slots are numbered across the whole program, one number per weft method
  called through `any`, so every vtable has the same shape. An `any` of more
  wefts is already an `any` of fewer, as on Lua, and a call through `any` is
  one load and an indirect call.

### 10.7 `match` and twill

Decision trees, with jump tables for dense tags and integers. chip8's opcode
`match` is one switch on the top nibble and then the rest. A twill pattern's
fixed bits are an `and` and a compare, its captures a shift and a mask.
Exhaustiveness stays the type checker's job.

### 10.8 Loops

`for` over a range or a sequence is a counted loop (`forrange`, `foreach`).
The language forbids the body from changing what the loop goes over, so the
sequence's pointer and length are loaded once. `for mut x in xs` makes `x` the
element's address. Labels, `break` with a value and `continue` are plain
jumps.

### 10.9 Lambdas and function values

- A function value is a function number and an environment.
- A lambda passed as an argument points its environment at the captured
  slots in the enclosing frame. There's no allocation, and `total += x`
  inside it writes `total` itself.
- `^(…)` copies or moves its captures into a box in the ambient spool (the
  heap until V6), and owns the box.
- Calling a function value is an indirect call, and the optimizer removes
  most of them (§7, pass 2).
- **The frontend's escape rule is needed first**: a lambda that captures can
  only be passed as an argument or kept in a local, unless it's `^(…)`. On
  Treadle, a stored lambda that outlived its frame would read freed memory.
  It's a V1 prerequisite (§17).

### 10.10 Generics, weaves and wefts

Instances come from the type checker, and each is a function. A weft's
default method is one function per type, as on Lua. Instances that compile to
the same bytecode (methods of `Pair[i32]` and `Pair[u32]` that only move
bytes) are merged by hash, to keep code small.

### 10.11 Gauges

A gauge is its base type in memory. `as G` is a range check (inline compares,
or a call to the predicate), `as?` makes an optional and `as|` clamps. The
ranges pass removes the checks it can prove, which is what 02-types.md
promised gauges would let an optimizer do.

### 10.12 `defer` and `once`

`defer` is §9.4. `once` leaves nothing at run time.

### 10.13 Maps, sets and streams

- **Maps** keep insertion order, like on Lua: a list of entries (a key's
  hash, the key, the value), plus an index table with open addressing whose
  slots hold entry numbers, kept at most half full. Removing is O(1): the
  entry's hash becomes 0 (a live one's never is) and it stays until more than
  half the entries made are removed ones, when the live ones move down, in
  order, and the index is built again; a loop, `show`, `==`, `copy` or
  `sorted` moves them down first. The header has `len` (the live entries) and
  `used` (the entries made). Keys hash and compare through their `Hash`
  and `Eq` (monomorphized), with native fast paths for strings and integers.
  A hand-written `eq` or `hash` is used (on Lua, keys are still compared
  structurally).
- `m[k]` and `entry` give the value's address, so `m[k].count += 1` and
  `counts.entry(w, default: 0) += 1` change it in place.
- `x.hash()` is D8.
- **Streams**: a local chain is fused into its loop (§7, pass 8). A stream
  that has to be a value (passed to a `Stream[T]` parameter, or returned) is
  a state machine: its adaptors' states in one block (in the frame when it
  doesn't escape, in a box when it does) and a step function that `next`
  calls.

### 10.14 Shuttles

**The frame.** What a shuttle made by `f` keeps across its suspension points
(its `yield`s and `pull`s) is its *frame*: a state word, then each value
that's live across one of them. Values never live at the same suspension
point share space. `f.Frame.size` is its size (Appendix B), and
`f.Frame.align` its alignment. It's worked out on Heddle before anything is
optimized (`heddle/frame.rs`), so it doesn't change with `-O` levels, and it's
the same number on both backends: the Lua backend builds the shuttle for
Treadle to answer it.

- A value in registers is kept at its own width (a `u32` local takes 4
  bytes) and is in the frame only while the shuttle is suspended. A local in
  memory (a `String`, an array) lives in the frame from where its value
  starts until its block ends, since something may hold its address until
  then. Constants and addresses in the frame aren't kept: they're made again
  after the resume.
- Items go in order of alignment, largest first, then of where they first
  appear, each at the lowest offset where it doesn't overlap one that's live
  at the same time. The state word is first, 4 bytes, with the "closed" bit.
- An item's temporaries are dropped before the `yield` suspends, when the
  item can't borrow from them: they'd be dropped at the end of the statement
  anyway, and nothing could tell.

**Running it.** `f`'s body is compiled as its *step function*,
`step(op, frame, out, input) -> status`. A step:

1. reads the state and marks the shuttle finished (a `return` or an unravel
   leaves it so);
2. resuming (`op` 0), goes on after the suspension point the state names;
3. runs until the next `yield` or `pull`, stores what's live there and the
   new state, and returns `Yielded` (the item is at `out`) or `Pulled`; or
   returns `Finished`.

A resume costs about a call: the body runs on the ordinary value stack, and
the calls it makes are ordinary calls. Calling `f` makes the shuttle: a box
on the heap with a header (who holds it, the latest burst, the input being
handed over), then the frame, with `f`'s arguments in it. A
`Shuttle[T, U]` is the box's address and the step: every shuttle is the
erased form D5 describes (see below).

- **Generators**: `for x in g(…)`, where `g` is a generator the program
  writes, called right there, doesn't make a shuttle at all: `g`'s body is
  built in place, each `yield` running the loop's body with the item. An
  item costs what a `while` loop's iteration does (`bench/` has the
  numbers). `break`, `return` and unravels leave through `g`'s pending
  `defer`s, as closing it would; `continue` goes on after the `yield`.
  Anywhere else (a generator held in a variable, `fib().take(10)`), the
  shuttle is a stream whose step resumes it.
- **Bursts**: a `Burst[T]` is the box and the number of the push that made
  it. `push` puts the input in the box; going through the burst steps the
  shuttle, handing the input to its first `pull` and stopping at the next.
  Pushing (or closing) again first runs what's left of the burst before,
  throwing its items away. `close` sets the closed bit, and from then on
  the driver hands `none` to every `pull`. Pushing into a closed shuttle
  unravels.
- **Bursts share the box** with the shuttle: each holds it, and the last to
  go drops it. A burst kept after its shuttle is dropped still works.
- **An unravel in a shuttle** unwinds its frame (defers, drops), leaves it
  finished, and carries on in the driver with the shuttle's location, as on
  Lua.
- **Closing or dropping a suspended shuttle** enters the step in close mode.
  It runs the cleanups of the suspension point it's at, the pending
  `defer`s and drops (D7), as a `return` there would. A `defer` that
  unravels there unravels in whoever closed it (P3).
  - **Only `defer`s the shuttle has reached run**: a `defer` whose statement
    ran before the current suspension point, in a scope the shuttle is still
    inside. One further down the body, or in a scope it already left, doesn't
    run. A `defer` inside a loop runs once, for the iteration it's suspended
    in. That's the same rule as for an unravel: each suspension point's
    cleanups are exactly those in scope there.
  - A shuttle that never started runs no `defer`s; it only drops what it
    owns (its `take` parameters). A finished one runs nothing.
- **Pointers into the frame** stay valid: the frame is in its box, which
  doesn't move when the shuttle value does.
- **D5, inline frames.** The decided design stores a shuttle made by a
  known function inline in its holder, with its frame's size, and boxes it
  only where the maker isn't known; pointers into the frame would then be
  rebased after a move. V2 boxes every held shuttle (one allocation when
  it's made) and builds the common case, a generator in a `for` loop, in
  place with no frame at all. Inline frames come with V8, measured against
  the allocation they save.
- **Projections** (`~> mut T`, `~> ref T`) are shuttles that yield the
  address of a place. A call of one makes it and steps it once: the place it
  lends is what the call is, for the rest of the statement (read, assigned,
  changed in place, called a `mut` method on). When the statement ends, it's
  stepped to its end, so the code after its `yield` runs; one that ends
  without lending a place, or lends a second, unravels. Unwinding, it's
  closed instead. A projection may borrow its parameters (it lives for one
  statement of its caller's), so the frame keeps a borrowed one's address,
  and what it borrows stays borrowed until the statement ends (M0104). The
  optimizer inlines them where it can (§7, pass 9). A `for` loop over one
  that lends many places, and `xs[i]` through an `Index` weave, come later.

### 10.15 Tests

`loom test --backend vm` runs each test as its own task in one VM, and
captures each test's output separately. In the checked build, each test must
also end with nothing leaked.

### 10.16 Compile-time code and reflection

V1 keeps the Lua sandbox, and V4 moves compile-time code onto Treadle (§17).
Everything the type checker folds stays folded. `T.size`, `T.align` and
`f.offset` get their answers from `ty::layout`, on both targets. On the Lua
target that's the layout Treadle would use: a number no Lua code depends on.
`f.ty` and `T.weaves(W)` are frontend work, independent of the VM.

### 10.17 The rest

`main ! E` shows the error and exits with 1. `env.args` and `process.exit`
work as on Lua: `process.exit(n)` unwinds as an unravel does, running
`defer`s, then exits with `n`. Raw memory makes `*T`, `#layout(C)` and `#layout(bits)`
natural on Treadle later. `extern "C"` needs a foreign function interface,
which isn't in this plan.

---

## 11. Threads

**Frontend work comes first**, because the type checker has none of it:
`loom` blocks, `thread`, `Spun[T]`, `join`, `loom for`, `sched_yield`, the
capture rules and C04xx, `#disjoint` projections (`chunks_mut`), and
`Atomic`, `Mutex` and `Pipe`. The Lua backend gets the eager lowering its
plan describes: a `thread` runs to the end as soon as it's spawned. That's
exactly Treadle's schedule with one worker, so the two backends stay
comparable.

On Treadle:

- **A Loom thread is a task**: its own value stack and control stack, in VM
  memory. Workers are OS threads, one per core by default (`LOOM_THREADS`
  changes it), each with a work-stealing deque. A task's whole state is data,
  so parking one means saving three words, and its worker picks up another.
  These are green threads with no OS stack switching.
- **`thread e`** makes a task that runs `e` as a closure whose environment
  points into the parent's frame. That's safe for the reason 05-concurrency.md
  gives: the loom block waits for every thread before its frame can go away,
  and stack segments never move.
- **`join`** takes the result if the thread has finished, and parks until it
  has otherwise. A thread that unravelled makes `join` unravel in the joiner,
  with a note giving the thread's location.
- **Leaving a loom block**, by any path:
  1. cancel the unfinished threads (a flag they see at their next
     cancellation point, where they unwind with a *cancel* and run their
     `defer`s);
  2. wait for all of them;
  3. carry on: return, propagate the error, or unravel again.
- **`loom for`** splits its iterations into chunks adaptively (halving on a
  steal, like Rayon), a task per chunk. `#disjoint` projections give each
  iteration its own `mut` place.
- **`Atomic[T]`** has `T`'s layout, with Rust's atomic operations on its
  address.
- **`Mutex[T]`** is a lock word and a `T`. Contention parks the task, not the
  worker. A suggested API is `fn lock(self) ~> mut T`, a projection, so the
  lock is held for exactly one statement: `total.lock() += n`.
- **`Pipe[T]`** is a bounded queue. `send` when it's full and `recv` when
  it's empty park the task.
- **Memory**: per-worker allocator caches, and frees from other threads go
  back to the page's owner (§5.5).
- **Determinism**: with one worker, a spawned task runs to its end first and
  there's no preemption: the same order as the Lua backend's eager threads.
  The fixture harness uses one worker for any fixture that prints from
  threads. Dedicated tests run with many workers and check only what's
  deterministic.

---

## 12. Spools and `given`

**Frontend work comes first**: `spool` bindings, `^T`, `^expr`, `p^`,
reading fields through a pointer, `T@s`, `given` (resolved by type, inferred
for private functions with a fixpoint, explicit at `pub`), `ref warp`, and
the region checker (R0101, R0102, arena pointers crossing threads). The Lua
backend erases spools, as it planned to.

On Treadle:

- **A spool is an object at run time.** `heap` is the global allocator (§5.5)
  and spool number 0. `Arena(cap)` is a list of chunks with a bump pointer:
  allocating is a compare and an add, inline. `reset()` rewinds to the first
  chunk, and the end of its scope frees the chunks. `Pool[T]` (later) is
  fixed slots with a free list.
- **`given sp: Spool`** is a hidden parameter, monomorphized per spool type
  (static dispatch, as 03-memory.md says). Code allocating in an `Arena`
  bumps a pointer inline, and code for `heap` calls the allocator.
- **`^expr`** allocates in the ambient spool. A `^T@heap` owns what it points
  at (dropping it frees it). Arena pointers are `Copy` and own nothing.
- **Collections in a spool** (`List[i32]@frame`) grow in that spool. The
  capacity word's top 16 bits, reserved since V1, are the spool's number in a
  registry, so only growing (a cold path) looks it up.
- **`.copy()`** makes a deep copy in the ambient spool (calc's semispace
  trick).
- **The checked build** gives each arena a generation, which `reset` bumps,
  and poisons the freed memory, so a use after `reset` (a region-checker bug)
  is caught.
- **`SharedArena`**, for threads, bumps atomically.

---

## 13. Lua interop

- `extern "lua"` in user code, guarded as the Lua plan's M7 describes. On
  Treadle it calls into an embedded Lua 5.4 (the vendored mlua), started on
  first use.
- **Values cross by conversion, checked at the boundary**: integers
  (range-checked into the declared type), floats, `bool`, `str` and `String`
  (copied), arrays and lists (to and from sequence tables, copied), records
  (to and from tables by field name, copied), and optionals (`nil`). A value
  that doesn't fit unravels at the extern declaration's location.
- **Lua calling Loom**: functions marked `#export` can be handed to Lua as
  callbacks. Calling one re-enters the interpreter on the same task, the one
  place the Rust stack nests, with a depth limit.
- **The `lua` feature** of `loom_vm`: without it, `extern "lua"` in user code
  is a U code and the VM doesn't link Lua at all.

---

## 14. Semantics the VM pins down

| # | What | Treadle | Lua backend |
|---|---|---|---|
| P1 | writes to a `mut` argument when the callee unravels (D6) | visible | lost: copy-out happens on return only. Allowed to differ |
| P2 | dropping a suspended shuttle (D7), or a projection whose statement unravels | runs the `defer`s it has reached | left to the garbage collector. Allowed to differ |
| P3 | a `defer` that unravels while a generator is being closed (the review's V5) | unravels in whoever closed it | fixed to match |
| P4 | `x.hash()` values (D8) | Treadle's own hash | FNV-1a of the debug text. Allowed to differ |
| P5 | formatting numbers (the review's F2–F4) | one table of cases, Rust's formatting where the spec is silent | fixed to match |
| P6 | `usize` (D3) | 64 bits | 63 bits |
| P7 | how deep recursion goes before `stack overflow` | memory-bound (256 MiB of stack by default) | Lua's limits |
| P8 | which unravel wins when two things in a statement could unravel | the first in evaluation order | the same |
| P9 | a `Map` or `Set` key with a hand-written `eq` or `hash` | uses them | structural. Fixed to match where it can be |

Every row has a fixture. Where the backends are allowed to differ, the
fixture has a `NAME.vm.out` next to its `.out`, and a comment naming the row.

---

## 15. Testing

1. **The gates**, as always, plus `cargo test -p loom_vm --features checked`.
2. **Differential fixtures.** `crates/loom/tests/fixtures.rs` runs every
   `tests/run`, `tests/test` and `tests/unravel` fixture on Lua, on Treadle,
   and on Treadle's checked build. Standard output, the report on standard
   error and the exit code must match. `tests/vm_skip.txt` lists the fixtures
   Treadle can't run yet, one per line with the milestone that fixes each.
   The harness fails when a listed fixture passes (so the list only shrinks)
   or an unlisted one fails.
3. **Oracles.** `vm_int.rs` checks every integer operation, at every width,
   signedness and policy, against `ty::int`: the oracle `rt_int.rs` already
   uses for the Lua runtime. Floats are checked against Rust.
4. **Heddle snapshots** for the builder and every pass.
5. **The verifier** is given hand-built broken functions it must reject.
6. **A program generator.** `tests/gen` makes random, well-typed programs from
   a seed: integers of odd widths under every policy, floats, records, sums,
   `match`, loops, lists, `?` and `mend`, `defer`, and the values that own
   memory: `String`s, lists of them, maps of `String`s to integers and the
   other way, and lambdas that capture locals. `cargo test` runs 300
   seeds, comparing Lua, `-O0`, `-O2` and the checked build. A longer run
   (`--ignored`) does 20 000. A failing seed is shrunk and kept as a fixture.
7. **Leaks and bad accesses**: every fixture, in the checked build.
8. **CLI tests** for images, exit codes, backtraces, and Ctrl-C (through the
   flag the handler sets).

---

## 16. Performance plan

**The suite** lives in `bench/`. Each benchmark is written in Loom, in
idiomatic Lua 5.4, and in Python:

| Benchmark | What it stresses |
|---|---|
| `fib` | calls and returns |
| `sieve` | arrays, bounds checks, integer loops |
| `mandelbrot` | float arithmetic in tight loops (on one thread) |
| `nbody` | records of floats, arrays of structs |
| `spectral_norm` | small functions called in float loops |
| `fannkuch` | small-integer arrays, permutations |
| `binary_trees` | allocating and freeing (with arenas from V6) |
| `richards` | sums, `match`, records, `any` dispatch |
| `lexer` | chars, strings and a shuttle (lexer.loom, scaled up) |
| `word_freq` | `String`, `Map[String, u32]`, templates |
| `sort` | `List.sort` on records, comparing through `Ord` |
| `streams` | iterator chains: `filter`, `map`, `sum`, `take_while` |
| `chip8` | twill `match`, `u8` and `u16` wrapping arithmetic, an interpreter loop |

**Measuring.** `bench/run.py` runs every interpreter it finds on `PATH`
(Lua 5.4 is always there: `loom lua-host --script` runs a plain Lua file on
the bundled VM) and takes the best of 5 wall-clock times. The results, with
versions and the machine, go in `bench/RESULTS.md`.

Wall-clock time is too noisy to gate `cargo test` on, so there are two kinds
of gate:

- **A deterministic one**: the stats build counts the instructions each
  benchmark dispatches (at a small size). `bench/counts.txt` holds the
  numbers, and a test fails when one moves by more than 2%. A commit that
  means to change them updates the file (`LOOM_BLESS=1`).
- **Milestone gates**, as wall-time ratios, measured and recorded in
  RESULTS.md when each milestone ends:

| Milestone | Gate (compared with PUC Lua 5.4 running idiomatic Lua) |
|---|---|
| V0 | `fib` and `mandelbrot` at least 2× faster than the Lua backend's output of the same program |
| V1 | geomean at least 1.5×, and nothing below 0.8× |
| V3 | geomean at least 2.5× (3× was the first aim: what the optimizer has left to win is mostly V8's, see RESULTS.md) |
| V6 | `binary_trees` at least 4× |
| V8 | geomean at least 4×, and nothing below 2×. Stretch: at least even with LuaJIT 2.1 `-joff` |

**Micro targets** in `bench/micro` (reported, not gated): about 1 ns to
dispatch a simple op, under 10 ns for a call and its return, under 15 ns to
allocate and free a small object, under 15 ns for a generator's resume and
yield, under 2 µs to spawn and join a thread.

---

## 17. Milestones

Every milestone ends with the gates green, the docs updated (this file's
status, AGENTS.md, the README, the docs site), and RESULTS.md updated. Sizes
are rough, in lines of Rust (or Loom).

### V0: the skeleton, end to end (about 6k lines) — done

**Goal:** prove the whole pipeline on the simplest programs, and measure it.

**Result** (2026-09-29): every condition below holds. 14 fixtures of
`tests/run` run on Treadle with the Lua backend's output (`scalars` was
written for this); the other 60 are in `tests/vm_skip.txt`, 57 waiting for
V1 and 3 for V2. `fib(32)` runs 15× faster than the Lua backend's output
and 2.2× faster than idiomatic Lua 5.4; the mandelbrot loop, 10× and 1.6×
(`bench/RESULTS.md`).

In:

- `ty::layout` for every type, though only scalars and tuples are used yet,
  and `target` as a parameter of type checking (D3).
- Heddle: the IR, its printer and verifier, and a builder for literals,
  integers of every width and policy, floats, `bool`, `char`, `let` and
  `var`, `if`, `while`, `loop`, `break`, `continue` and labels, `match` on
  literals, ranges and tuples, functions (named and default arguments,
  recursion), tuples, templates passed to `print`, `unravel` and `assert`.
- Code generation: selection with compare-and-branch and immediates,
  linear-scan register allocation, the call window.
- `loom_isa`: the op table (about 240 ops, since the width-specialized
  families and the immediate forms came in V0), encoding, the disassembler,
  the verifier, images in memory.
- `loom_vm`: the loop, calls, segmented stacks, traps, reporting an unravel
  (no landing pads yet), the natives `print` needs, and the checked build.
  (A clock waits for V1: `std.time` has none yet, and `sleep` takes a
  `Duration` warp.)
- The CLI: `loom run --backend vm`, `--emit heddle|bytecode`. The fixture
  harness's backend axis, with `tests/vm_skip.txt`. `bench/` with `fib` and
  the mandelbrot inner loop, and `bench/counts.txt`.

Passing:

1. The gates, and the checked build's tests.
2. Every fixture that only uses V0's features gives identical output and exit
   code on Treadle. The rest are in `vm_skip.txt`.
3. `vm_int.rs`: every width from 1 to 64, signed and unsigned, every operator
   and policy, and casts, on the boundary values plus 1000 random pairs each,
   with no mismatch against `ty::int`.
4. `f32` and `f64` operations and conversions match Rust bit for bit (NaN
   payloads aside).
5. The verifier rejects at least 12 different kinds of broken function.
6. The unravel fixtures (overflow, division by zero, a failed `assert`) print
   the same report as on Lua and exit with 101.
7. A recursion 1 000 000 calls deep works. An endless one unravels with
   `stack overflow` and never crashes the process.
8. Speed: the V0 row of §16.

### V1: the sequential language (about 12k) — done

**Goal:** everything that runs on Lua today, except shuttles and
compile-time code, runs on Treadle with the same results and nothing leaked.

Frontend first, each a commit of its own with fail fixtures:

- the lambda escape rule (§10.9);
- the review's V4: `first()`, `last()`, `get()` and `Map.get` stop sharing a
  non-`Copy` element (they return copies, or need `T: Copy`);
- the review's V10: reusing a `Stream` is a move error;
- `T.size`, `T.align` and `f.offset`, from `ty::layout`.

In:

- Every layout in §5.2, with niches.
- Warps, sums, dot-blocks, patterns and decision trees, twill; `?`, `mend`,
  `fail` and `main ! E`; unions, `any` and vtables; gauges.
- Arrays, lists, slices, strings, chars, templates, `Fmt`, `Show` and the
  debug form, maps and sets.
- Ownership: drop glue, drop elaboration, drop flags, `.copy()`. Landing
  pads, `defer` and fallback `defer`.
- Lambdas and function values (frame environments, and `^(…)` on the heap),
  generics, weaves, `through`, default methods, `Iter`, and `Stream` as state
  machines (not fused yet).
- Constants computed on first read, as on Lua. The run-time parts of
  reflection.
- the builder's `#builtin` bodies and the natives (D9); `loom_sys`, shared with the Lua
  host; `loom test --backend vm`; buffered output, backtraces, Ctrl-C (130),
  `process.exit`.

Passing:

1. The gates, and the checked build.
2. `vm_skip.txt` names only shuttle fixtures and fixtures that need
   compile-time code, each tagged V2 or V4.
3. `tour` (with `PORT` unset and with `PORT=9000`), `widgets` and `ledger`
   give identical output on both backends. `loom test --backend vm
   examples/chip8.loom` gives 3 passed. chip8 plays a ROM (checked by hand,
   and noted in RESULTS.md).
4. The checked build over the whole corpus: no bad access, and nothing leaked
   at a normal exit.
5. The only `.vm.out` files are for rows P1, P4 and P6, each with its
   comment (P5's fixture agrees on both backends).
6. Unravel reports are identical, and a backtrace fixture shows every frame.
7. Speed: the V1 row of §16.
8. The program generator passes 300 seeds (the subset V1 covers).

Met on 2026-10-02. `vm_skip.txt` lists four fixtures, all V2. The speed is
in `bench/RESULTS.md`; what got it there: the dispatch loop keeps only `pc`
and `fp` in registers and gives each op its own dispatch, ops with constant
operands, list elements loaded and stored by one instruction, constants
hoisted out of loops, natives' arguments made in place, size classes in the
heap, and small transparent functions (std's and glue) inlined. The program
generator (`crates/loom/tests/gen.rs`) also passes 20 000 seeds; it found
unravels reported at stale locations on both backends, and Lua's tail calls
losing a function's frame (fixtures `unravel_after_block`,
`unravel_tail_call`). chip8 plays `examples/digits.ch8` the same on both
backends. `process.exit(n)` and Ctrl-C unwind the program (its `defer`s
run) before it exits with `n` or 130.

### V2: shuttles (about 3k) — done

Frontend first: D5's typing (calling a shuttle function gives its frame type,
erased where it has to be), `f.Frame.size`, and projections (`~> mut T`,
`~> ref T`, `yield mut place`).

In: all of §10.14. Frames and `Frame.size`, generators, duplex shuttles and
sinks, bursts, `close`, dropping runs `defer`s, rebasing pointers into the
frame, erased shuttles.

Passing:

1. The gates, and the checked build.
2. Every shuttle fixture is identical on both backends (with `.vm.out` only
   for P2, and P3 fixed on Lua).
3. `examples/lexer.loom` runs on both backends, its `const
   assert(lexer.Frame.size <= 64)` included, and gets
   `tests/examples/lexer.out`.
4. A fixture pins `Frame.size` exactly: 40 for the lexer (Appendix B), 24 for
   `fib`, and a few more.
5. New fixtures: a shuttle moved while suspended in the middle of a loop over
   an array in its frame resumes correctly; a dropped generator runs the
   `defer`s it has reached and none of the others (P2): not one further down
   its body, not one in a scope it already left, and one inside a loop only
   once; a generator dropped before it starts runs none; an unravel inside a
   generator reports the generator's line;
   pushing after `close` unravels; the next `push` drains an unfinished burst.
6. The checked build: nothing leaked with shuttles that were dropped, closed
   or finished.
7. Speed: `for x in gen()` costs at most 2× `for x in 0..n` per item on a
   trivial body, and the `lexer` benchmark is recorded.

Met on 2026-10-03, but for D5's inline frames, which move to V8 (§10.14):
every held shuttle has its frame in a box, so no pointer into one needs
rebasing. `vm_skip.txt` is empty. Shuttle bodies are step functions laid
out by the frame pass, a generator a `for` loop calls is built in place
(0.21 s for 50M items, where `for x in 0..n` takes 0.33 s), and the lexer
benchmark runs 5.9× as fast as PUC Lua, with Lua's coroutine per char
(`bench/RESULTS.md`). The program generator writes generators and duplex
shuttles, and 20 000 seeds agree on the checked build. On the way: a loop's
back edge lost its landing pad when an `if` ended it (Ctrl-C skipped the
`defer`s), a constant slice viewed its init's temporaries, the move checker
bound a `for` loop's own items once for all iterations, and on Lua a
`defer` that unravelled while a loop closed its generator was swallowed (P3)
and `close` handed `none` to the burst before it.

### V3: the optimizer (about 8k) — built

Where it stands (2026-10-11): items 1–7 hold; 7 is 2.74× against 2.5×.
`crates/loom/tests/check_counts.rs` holds item 4, `tests/compile_time.rs`
(ignored, `--release`) measures item 6, and RESULTS.md has the numbers.

In: the passes of §7, the `-O` levels, inline info for backtraces, the rest of
the benchmark suite, `bench/run.py`, and RESULTS.md with every interpreter
available.

Passing:

1. The gates, and the checked build at `-O2`.
2. The corpus gives identical results at `-O0` and `-O2`. The program
   generator passes 300 seeds comparing Lua, `-O0` and `-O2`.
3. Every pass has snapshot tests.
4. Checks go away: over the suite, at least 80% of the bounds checks and 60%
   of the overflow checks executed at `-O0` aren't executed at `-O2` (the
   stats build counts them).
5. Backtraces at `-O2` show inlined frames correctly (a fixture).
6. At `-O2`, compiling the corpus takes at most 1.5× as long as at `-O0`, and
   no example takes more than 200 ms.
7. Speed: the V3 row of §16.

### V4: compile-time code on Treadle (about 2k)

In:

- Compile-time jobs run on Treadle, in-process, with host natives refused
  (K0104).
- The budgets go through ticks: steps, 1 GiB of memory and 20 seconds
  (K0102). K0103, K0107 and K0108 as before.
- Constants are baked in: data with relocations in the image, and literals in
  the Lua bundle instead of initializers.
- `target` facts come from the target being built.

**Plan** (decided 2026-10-11, in commit order; each commit passes the gates).

What the code looks like today: `lower::compile_time` runs every constant,
`const` expression and `const assert` as a job in the Lua sandbox
(`loom_host::consteval::run_jobs`) and gets back only pass, fail or a
category; the compiler's own values for const generics, `const if`/`for` and
array sizes come from the Rust folder (`typeck/gauge.rs`, `unroll.rs`), not
from jobs. On Treadle a constant is a lazy data slot (a flag word, then the
value; `heddle/build/consts.rs`) computed on first read. So V4 has two
halves that share a readback: *running* jobs, and *baking* their values.

0. **Baseline first.** Before anything is removed, record the sandbox's
   compile times for `const_eval`, `const_params`, `derive_json`,
   `examples/compile_time.loom` and `endless_work` in RESULTS.md. Pass 5
   compares against them, and they can't be measured afterwards.
1. **VM: budgets, refusal, readback** (`loom_vm`, `loom_isa`).
   - The tick check (§9.5) already runs every 65 536 backward jumps and
     calls. Add a step budget and a deadline to it, and a heap cap through
     `Trap::OutOfMemory`; no hot-path cost. A new `Trap::Budget`.
     Ticks count calls and back edges, not instructions, so K0102's step
     number is re-calibrated: `const_budget.loom` and `endless_work.loom` must
     still fail, and every `tests/run` constant must pass with 10× headroom.
   - A host native reached under `Host { sandbox: true }` traps with
     `Trap::HostRefused(name)`, carrying the native's name, so the compiler
     can report K0104 with it. Open: `NativeSig` has no host flag today
     (the host natives are a group in `loom_vm/host.rs`); add one to the
     table so the refusal is data, not a list of names.
   - Readback: `Vm::data()`, `data_addr()`, `rodata_range()` and an
     `unsafe read(addr, len)`, so a caller can walk a value by its layout.
   - *Built* (2026-10-11): `Config { limits: Limits { steps, time, memory },
     refuse_host }`; `Outcome::OverBudget { budget, location }` and
     `Outcome::Refused { native, location }`. The checks sit in the tick
     counter's cold path, which now persists across `Vm::call`s (a
     budget is for all the jobs, and many short jobs would otherwise never
     reach a check). The first budget trap unwinds, so what the program
     owns is freed; if the program is still running a chunk of ticks
     later (a `defer` that never ends), the next check aborts it. The
     dispatch loop is unchanged: an A/B of `bench/` is within noise.
2. **Move `srcmap` and `RT_CORE` out of `loom_host`**, as one mechanical
   commit. They are the other reasons `loom_compiler` depends on it. The
   `lua54`/`lua55` features move with them (the `lua55` gate must still
   pass).
3. **Run jobs on Treadle, constants still lazy.** One image holds every job
   (`Module.roots` is the job init functions, so only what they reach is
   built), one `Vm`, a `Vm::call` per job; `Vm::new` verifies the whole
   image once. A job that exhausts the budget skips the rest, as today.
   The job build takes a `Target`: a Lua-target build evaluates `usize` in 63
   bits (§5.1). `OS`, `ARCH` and `ENDIAN` stay the build host's, as §5.1
   says; `pointer_bits` already comes from `Target`. `-O0` or `-O2` for job
   images is picked by measuring against step 0. Then `lower/consteval.rs`
   and `loom_host/src/consteval.rs` go, and `loom_compiler` stops depending
   on `loom_host`. Passing 1, 2 and 5 are checked here.
4. **Cycles and the checked build.** A constant's flag word becomes
   three-state: 0 untouched, 2 in progress, 1 done. Reading a 2 unravels with
   the cycle message (K0107), as `rt.lazy`'s CYCLE marker does on Lua, rather
   than overflowing the stack. After a job VM's results are read, run
   `ConstExit` on it so `vm-checked` doesn't report every constant as a leak.
5. **Bake.** After the jobs run, read each baked constant back *by type*
   (`ty::layout`) into canonical bytes in a fixed order. A raw heap address
   is never copied (ASLR would break pass 4): a pointer becomes an offset
   into the data plus an entry in a relocation list, which `Machine::load`
   patches once at load.
   - *Closed set*: scalars, `str`/`String`, arrays, lists, tuples,
     optionals, and warps and cases of those. Everything else (maps and
     sets until their hashing and iteration order are shown to match,
     `any`, lambdas, shuttles) keeps its lazy slot on both backends. A
     constant that isn't baked is never wrong, only slower.
   - *Ownership*: baked data is never freed. `ConstExit` and drop glue skip
     it, `.copy()` still allocates a heap copy, and the checked build's
     shadow map marks the data region valid. Open: what `read_const` hands
     out for a non-`Copy` baked value (it copies today, `consts.rs:92`),
     and whether a baked list can be read without a copy.
   - *Heddle*: a baked scalar is an immediate, so SCCP, ranges and GVN fold
     it; a baked aggregate is a `Global` address with no flag test.
   - *Lua*: a baked value is a literal in the bundle. Pitfalls, each a
     fixture: `-9223372036854775808` (write `math.mininteger`), `-0.0`,
     NaN and infinities, `1` against `1.0` and exact float round-trip,
     byte strings as `\ddd`, a `nil` constant (not a table field), and
     nesting deep enough to hit the parser's C-stack limit (such a
     constant stays lazy). Everything stays inside 5.4 ∩ 5.5.
   *Built* (2026-10-11, steps 2–4): the jobs are found from what typeck
   recorded (`Results::const_exprs`, the exact `(body, expression)` pairs it
   checked, so a branch `const if` doesn't take has none) and from the
   reachable typed constants, read or not; `heddle::build_jobs` builds only
   what they reach, as one module whose roots are the jobs; `consteval.rs`
   runs it on a VM with `refuse_host` and a 100 M tick, 1 GiB and 20 s
   budget. Cycles and failures go through the flag word's four states and
   marker unravels. A job image is `-O0`: `-O2` wasn't faster. The program is
   still built separately afterwards, so its image is what it was; reusing
   the job module is left to step 5, where baked values make it matter.
   `loom_compiler` depends on `loom_vm`, and on `loom_host` only for its
   tests. The numbers are in `bench/RESULTS.md`: 239 ms against 495 ms over
   the corpus, and the heaviest file uses 5.8 M of the 100 M ticks.
   Open for step 5: the job VM never runs `ConstExit`, so what constants
   allocate (big objects) is only freed with the process.
6. **Docs**: this section, §5.1, §10.16, `design/lua-backend.md`'s
   constants, `docs/`, and AGENTS.md's status.

Verification beyond the passing list:

- Pass 4 is checked across two separate `loom build` processes, comparing
  bundle bytes and an image listing (`Image` has no serialization; add a
  deterministic listing if `loom_isa`'s doesn't cover the data). One process
  comparing with `PartialEq` can't see address leakage.
- `gen.rs` gets a differential mode: each program emits a constant and the
  same expression in a run-time function, and the two must agree, on Lua,
  `-O0` and `-O2`. With Lua no longer computing its own constants, this is
  the check on baking.
- Fixtures: a constant cycle, K0104 naming the native, a budget that passes
  just under, a baked `i64::MIN`, `-0.0`, NaN, a string with every byte value,
  a constant of constants, and one of each type outside the closed set.

Passing:

1. Every K fixture gives the same diagnostics as before.
2. `loom_compiler` no longer depends on `loom_host`, so building or checking a
   program never starts a Lua VM.
3. A constant table (a CRC table, a list of strings) is data in the image and
   a literal in the Lua bundle. Neither computes it at run time, and the
   output doesn't change.
4. Building twice gives byte-identical images and bundles.
5. Compile-time-heavy fixtures compile at least as fast as with the Lua
   sandbox.

### V5: threads (frontend about 3k, VM about 4k)

In: all of §11.

Passing:

1. The gates, and the checked build (its shadow map made thread-safe).
2. `examples/parallel.loom` gives its golden output (`primes below 200k:
   17984`), on one worker and on many.
3. Every thread fixture is identical to the Lua backend's output on one
   worker.
4. Fixtures for: a thread that unravels (the parent unravels with a note, and
   the siblings are cancelled with their `defer`s run), `?` out of a loom block
   (cancel, wait, then propagate), unjoined threads joined at the end, and
   C0401 and the other C04xx errors.
5. Stress: 1000 runs of the thread fixtures on 8 workers, with no failure and
   no hang (`--ignored`).
6. Speed: mandelbrot with `loom for` scales to at least 0.7× the number of
   cores, and spawning and joining a thread takes under 2 µs.

### V6: spools and `given` (frontend about 4k, VM about 2k)

In: all of §12.

Passing:

1. `examples/calc.loom` prints its three lines on both backends (spools erased
   on Lua).
2. The R0101 and R0102 fixtures match calc's comments and 03-memory.md's
   example.
3. The checked build catches a use after `reset` in a hand-built VM test.
4. Nothing leaked in the checked build, arenas included.
5. Speed: the V6 row of §16.

### V7: Lua interop (about 2k)

In: all of §13.

Passing:

1. Fixtures call `string.rep` and a user Lua module.
2. Values convert both ways for each supported type.
3. A value that fails its guard unravels at the extern's line.
4. An `#export`ed Loom function is called back from Lua.
5. `loom_vm` builds without the `lua` feature, and `extern "lua"` in user code
   is then a U code.

### V8: speed (about 3k)

In: profile-driven superinstructions, the dispatch experiments (D2),
profile-guided optimization of the `loom` binary, inline short strings, fast
paths for maps and strings. Each change is kept only if it wins at least 3% on
the suite's geomean (15% for a new dispatch strategy), with before and after
numbers in RESULTS.md.

Passing: the V8 row of §16, and a scoreboard in RESULTS.md against every
interpreter available.

### V9: Treadle by default

In: `loom run`, `test` and `build` use Treadle unless told `--backend lua`;
image files; `loom build --exe`; docs-site pages on how programs run; this
document's status.

Passing: §18.

---

## 18. Switching the default

Treadle becomes the default once:

1. `vm_skip.txt` is empty;
2. every example that runs on Lua runs on Treadle with the same output, and
   every `.vm.out` is explained by a row of §14;
3. the checked build is clean over the corpus, and 20 000 generator seeds
   pass;
4. the V3 speed gate holds;
5. nothing is known to be wrong on Treadle that's right on Lua.

V9 is the latest this happens. The Lua backend stays in the test suite as the
oracle, and stays the way to run Loom inside a Lua host.

---

## 19. Risks

| Risk | What keeps it in check |
|---|---|
| a memory bug in `unsafe` code | a `// SAFETY:` comment on every block, the lint that makes each use deliberate, the verifier, the checked build on every fixture, the program generator, Miri (not run yet) |
| frontend gaps turning into memory bugs (Lua's collector hid them) | V1's frontend prerequisites, dynamic guards until the checkers exist, the leak invariant |
| a `match` loop in Rust leaves speed on the table | the op table makes the dispatch strategy replaceable (D2), and profile-guided optimization |
| monomorphization and inlining make code big | the inlining budget, merging identical instances, and RESULTS.md, where cache misses show up as time |
| the two backends drift apart | differential fixtures on every commit, and every allowed difference is a row of §14 |
| the optimizer makes compiling slow | the V3 gate, and `-O0` wherever a program's speed doesn't matter |
| ten milestones is a lot | each ends with something that runs and is measured, and none depends on a later one |

---

## Appendix A: one loop, from Loom to bytecode

`escape` from `examples/parallel.loom`:

```loom
fn escape(cx: f64, cy: f64, limit: u16) -> u16 {
    var (x, y, i) = (0.0, 0.0, 0u16)
    while x * x + y * y <= 4.0 and i < limit {
        (x, y) = (x * x - y * y + cx, 2.0 * x * y + cy)
        i += 1
    }
    i
}
```

Heddle after `-O2`:

```
fn escape(v0: f64, v1: f64, v2: u16) -> u16 {
b0:
    jump b1(0.0, 0.0, 0)
b1(v3: f64, v4: f64, v5: u16):       ; x, y, i
    v6 = fmul v3, v3
    v7 = fmul v4, v4
    v8 = fadd v6, v7
    brif fle v8, 4.0, b2, b4
b2:
    brif ult v5, v2, b3, b4
b3:
    v9  = fsub v6, v7                ; GVN: x * x and y * y from the condition
    v10 = fadd v9, v0
    v11 = fmul 2.0, v3
    v12 = fmul v11, v4
    v13 = fadd v12, v1
    v14 = iadd.u16.wrap v5, 1        ; was .trap: v5 < v2 <= 65535, so it fits
    safepoint
    jump b1(v10, v13, v14)
b4:
    ret v5
}
```

Two passes did the interesting work. GVN noticed that `x * x` and `y * y` in
the body are the values the condition already computed. Floats aren't
reassociated, so everything else stays as written. The ranges pass knows
`i < limit` in `b3` and `limit` is a `u16`, so `i + 1` can't overflow, and the
trap goes.

The bytecode, with `cx`, `cy` and `limit` in `r0`–`r2`, `x`, `y` and `i` in
`r3`–`r5`, and a frame of 9 registers:

```
 0  ldk      r3, 0.0
 1  ldk      r4, 0.0
 2  ldi      r5, 0
 3  fmul     r6, r3, r3        ; loop head
 4  fmul     r7, r4, r4
 5  fadd     r8, r6, r7
 6  jnle.fk  r8, 4.0, →16      ; not (r8 <= 4.0): NaN leaves the loop too
 7  jge.u    r5, r2, →16
 8  fsub     r8, r6, r7
 9  fadd     r8, r8, r0
10  fmul.k   r6, r3, 2.0
11  fmul     r6, r6, r4
12  fadd     r4, r6, r1
13  mov      r3, r8
14  addk     r5, r5, 1
15  loop     →3                ; counts a tick
16  ret1     r5
```

Thirteen dispatches per iteration, with no type checks and no allocation. The
same loop in idiomatic Lua 5.4 is around twenty instructions, each checking
its operands' types.

## Appendix B: the lexer's frame

`lexer()` in `examples/lexer.loom` suspends at 13 places: its `pull`s and its
`yield`s. What's live across at least one of them:

| Local | Type | Live across | Size |
|---|---|---|---|
| `pos` | `u32` | all of them | 4 |
| `next` | `?char` | the `yield`s (the `pull`s assign it) | 4, the niche |
| `c` | `char` | the `pull` in the `_` arm | 4 |
| `start` | `u32` | the `pull`s inside the arms | 4 |
| `n` | `u64` | the `pull` in the digit loop | 8 |
| `name` | `String` | the `pull`s in the name loop | 24 |
| `text` | `String` | the `pull`s in the string loop | 24 |

`d` isn't there: each loop uses it before it pulls. The token being yielded
isn't either: it moves out to the driver.

Locals go in order of alignment, then declaration, each at the lowest offset
where it doesn't overlap a local that's live at the same suspension point.
`pos` is live everywhere, so it overlaps nothing; `start` is live with the
arms' locals; the rest never meet:

```
 0..4    state (with the "closed" bit)
 4..8    pos
 8..16   n       ┐
 8..32   name    │
 8..32   text    │ never live at the same suspension point,
 8..12   next    │ so they share space
 8..12   c       ┘
32..36   start
36..40   padding, to the alignment of 8
```

`lexer.Frame.size` is 40, inside the example's `<= 64`. `fib()` (`a` and `b`,
two `u64`s) is 24: the state, padding, then 8 bytes each. The example prints
the lexer's (`tests/examples/lexer.out`), and `tests/run/frame_size` pins
`fib`'s and a few more, on both backends.

## Appendix C: from the Lua backend to Treadle

| On Lua | On Treadle |
|---|---|
| a table per record, tuple and case | fields at offsets, usually in registers |
| `rt.idx` for every index | `ldx`: bounds check and load in one, and often no check |
| deep copies of table-backed `Copy` values | copying bytes, or nothing after SROA |
| scalar `mut` arguments copied back through extra returns | an address, kept in a register by the callee and written back on every way out |
| `{true, v}` for results | two registers |
| `<close>` variables for `defer` | landing pads |
| `rt.lazy` constants | data in the image (V4) |
| coroutines | frames inside values |
| descriptors walked at run time for `==` and `{x}` | generated glue, and vtables for `any` |
| `rt.same_len` on every loop over a list | a pointer and a length loaded once, guarded only where the checker can't prove it |
| a garbage collector | ownership, drops and arenas |
