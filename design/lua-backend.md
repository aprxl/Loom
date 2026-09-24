# Lua backend: decision log

How Loom maps onto Lua 5.4, and why. The emitted Lua stays inside the
**5.4 ∩ 5.5 subset**, so the bundled VM can move to 5.5 with a feature flag.
Each decision here should eventually have a fixture that pins it down.

**Guiding rule:** where Loom and Lua agree, lower 1:1. Where Loom promises more
than Lua gives, enforce it at compile time if possible and at runtime (in Lua)
otherwise. Everything the type system proves (borrows, regions, `once`,
exclusivity) is **erased**: Lua never sees it.

---

## Pipeline

```
SourceDb → lex → Tokens → parse → AST → resolve/desugar → HIR
  → typeck(Instance) → THIR ⇄ const eval (Rust fold | sandboxed Lua)
  → flow::Cfg → analyses → FlowFacts
  → lower → LIR → lir_verify → luagen → Bundle { lua, source map }
```

- Generics are type-checked **per instance** (Zig-style), because untaken
  `const if` branches aren't checked and `const for` bodies are checked per
  iteration.
- Flow analyses run on a CFG built only for analysis. Code is generated from
  the typed tree, because Lua needs structured control flow and `<close>`
  scoping.
- `lir_verify` enforces Lua's own rules (goto/label scoping, 200 active locals,
  255 upvalues, multi-value call positions, no writes to for-loop control
  variables or `<close>` variables) before any Lua text exists.

## Value representation

| Loom | Lua |
|------|-----|
| `bool` | boolean |
| `iN`/`uN`, N ≤ 64 | integer, kept normalized to the type's range |
| `usize`/`isize` | treated as `u63`/`i63` (`target.pointer_bits == 63`) |
| `f64` | float, always emitted with `.0` or an exponent |
| `f32` | float, rounded through `string.pack("f")` after each operation |
| `char` | integer code point |
| `str`, `String` | Lua string (immutable; mutation goes through copy-out) |
| `()` | `nil` |
| tuples | `{a, b}` |
| records | `{x = …, y = …}` |
| sums with payloads | `{tag = k, payload…, shared fields…}`, a fresh table per construction |
| unit-only enums | integers `1..n` (never shared singleton tables) |
| `?T` | `nil` when T's representation never uses nil; `{v}` box otherwise |
| `T ! E` | two slots `(ok, v_or_e)` in registers and returns; `{ok, v}` when stored |
| `A \| B` | `{tid, v}` |
| `any W` | `{v, vt}` (vtable per type × weft set; `vt.tid` for type tests) |
| `[T; N]` | table indexed `1..N` (every index is emitted `+1`) |
| `[T]` | view `{t, o, n}` |
| `^T` | the table itself for table-backed T; `{v}` box for scalars |
| Map / Set | insertion-ordered: `{keys, vals, idx, n}` |

## Calling convention

- **read** and **take** parameters pass the value or reference unchanged.
- **`mut`** parameters:
  - table-backed: pass the reference; whole-value assignment uses a
    generated `overwrite_T(dst, fresh)`;
  - scalar-backed: copy-in/copy-out via extra return values, always through
    temporaries (`local r, t = f(p.x); p.x = t`), because Lua's multiple
    assignment has no defined evaluation order.
- Fallible functions return `true, v` or `false, e`, followed by copy-outs.
- Multi-value calls only appear in `local … = call` and `return call`.

## Copy points

Invariant: every non-place expression produces a fresh table. A copy is
emitted only when a place of a table-backed `Copy` type flows into an owning
slot (`var`, field/element store, `take` argument, return, `^`, escaping
capture, yield, value-form dot-block) while its source stays live. Last use
turns the copy into a move.

Today the copies are made without last-use analysis: every place of a
table-backed `Copy` type that flows into an owning slot is copied (in
`lower/warp.rs`, `lower_owned`). `mut` parameters and `mut self` share the
caller's table instead, and a whole-value assignment to one goes through
`rt.overwrite`. `mut` parameters of scalar types (numbers, strings, enums)
need copy-in/copy-out and aren't implemented yet.

## Generics

Each instance of a generic function is its own Lua function, named with its
instance number (`F.largest_4_12`). So are a generic warp's methods, one per
list of arguments it's used with. A generic warp's runtime descriptor is per
type too (`Pair[i32, str]`), since its fields' descriptors differ.

A generic function nobody calls isn't checked or emitted. An error in a
generic body is reported once, with a note naming the instance it came from,
even when several instances hit it.

## Weaves

Weave methods are static calls, like any other method. A weft's default
method becomes one Lua function per type that uses it (`F.name_12_7`, with
the instance number), checked with `Self` as that type.

A method forwarded by `through` is a call on the field: `b.size()` is
`F.size_8(b.frame)`. When it stands in for a weft method (`Widget.size(b)`),
a one-line function per type does that call.

`{x}`, `print`, `==` and `<` work through a type's runtime descriptor.

- A warp with a hand-written `show`, `eq` or `lt` has it stored in the
  descriptor (`T[n].show = F.show_9`). `rt.show`, `rt.eq` and `rt.cmp` call
  it, at any depth: inside a tuple, an optional, or another warp's field.
- Without one they work field by field. That's also how the compiler-made
  (`#builtin`) defaults run.

`Fmt` is a Lua array of strings: `f.put(x)` appends `x`'s shown form, and
`rt.show` concatenates the array after calling a hand-written `show`.

## Control flow shapes

- `?` / `mend`: result locals declared before a `do … end`, failures `goto mN_fail`,
  success `goto mN_done`. Labels are unique per function.
- `continue`: `while c do do BODY end ::cN:: end`.
- Labeled break: `goto bN` with the label right after the outer loop.
- Never `repeat … until` (its condition sees body locals).
- `defer`: `local d <close> = rt.defer(fn)`. Fallback defers add a liveness
  flag, cleared right before the consuming call.
- Shuttles: `coroutine.create` with tagged yields `(OUT, v)` / `(PULL)`.
  A coroutine that dies by error is `coroutine.close`d before re-raising.

## Defaults (decided; change here first)

- `str.len()` counts bytes.
- Integer `/` and `%` truncate (Lua's floor). Zero divisors and
  `mininteger / -1` unravel.
- `<<` truncates to the width; a shift amount ≥ width unravels.
- float → int `as` truncates and unravels on NaN or out of range; `as|`
  saturates (NaN → 0); `as%` on floats is rejected.
- Evaluation order is left to right, everywhere.
- Map and Set preserve insertion order.
- f64 Display is Rust-like: `17`, `-3`, `1.5`, `inf`, `NaN`. Debug adds `.0`.
  Never Lua's `tostring`.
- Every type has a builtin structural Debug in v1.
- An unravel inside a defer during an unravel replaces the first one.
- `target.os` is the build host's OS; `target.pointer_bits` is 63.
- A new `push` while the previous Burst is unfinished drains it silently.
- The RNG is xoshiro in the runtime, never `math.random`.

## Bundles and run mode

A bundle is one self-contained Lua chunk: the runtime (`runtime/rt/core.lua`)
inlined at the top, then type descriptors (`T`), the std externs it calls
(`EXT`), every reachable function (`F`, a table because Lua allows at most 200
locals), constant initializers in dependency order (`C`), and finally
`return F.main_N`. Next to it, `NAME.lua.map` maps each Lua line that starts a
statement to its Loom file, line and column.

`loom build` writes both files. `loom run` compiles in-process, writes them to
a temp file, and spawns `loom lua-host <bundle> -- args` with stdin, stdout and
stderr inherited. The host loads the chunk, gives it `__LOOM_HOST.write` (so
output reaches the Windows console as UTF-8), and calls `main`. Anything that
unravels raises a string starting with `rt.MARK` at the Loom code's position
(`error`'s level argument), and the host turns `bundle:LINE:` into a Loom
location through the map and prints a rustc-style report.

| Exit code | Meaning |
|-----------|---------|
| 0 | success |
| 1 | `main` returned an error |
| 2 | compile or usage error |
| 70 | internal error (a raw Lua error, meaning a Loom bug) |
| 101 | unravel |
| 130 | interrupted (Ctrl-C) |
| n | `process.exit(n)` |

Implemented so far: 0, 2, 70 and 101. Still to come: `main ! E` (1), Ctrl-C
(130), `process.exit`, an instruction budget, and `--lua PATH` to run a bundle
in an external interpreter.

## Not supported in v1

Raw pointers, `extern "C"`, `#layout(bits)` I/O, f16, integers wider than 64
bits, user-written `Spool` weaves, anonymous `warp {}` types from `const fn`s (U0101),
and `bolt.loom`. Each fails with a `U` code.
