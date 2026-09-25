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
| `any W` | `{v, vt}` (vtable per type × weft set; `vt.d` is the type's descriptor, for type tests) |
| `[T; N]`, `List[T]` | `{n = len, [1]…[n]}`: the same, so a literal can be either (every index is emitted `+1`; never Lua's `#`) |
| `[T]` | view `{t = elements, o = offset, n = len}` |
| `A => B` | a Lua function (a lambda is a closure over the enclosing locals) |
| `Stream[T]` | a Lua function: each call gives `true, item`, or nothing once done |
| ranges (as values) | `{lo, hi, inclusive}` |
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
  *For now* a result is always the table `{true, v}` / `{false, e}`, in
  returns too: two slots are an optimization for later.
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
are copied back: every `return` of the function also returns their final
values, and the caller assigns them to where they came from
(`local r, t = f(x); x = t`). A `mut self` method of a scalar type can't be
called through `any` yet, since the `any` holds its own copy. A place
converted to `any` or a union is copied too: the box owns its value.

Copying only `Copy` places is sound because moves are checked
(`typeck/moves.rs`): using a variable after its value moved is M0101,
moving out of a borrow (a read or `mut` parameter, a loop's item over a
list, a lambda's parameter or captured variable) is M0102, and moving out of
a list's element is M0103. A `mut` argument that overlaps another argument of
the same call (`swap(mut a, mut a)`, or a table read alongside) is M0104;
distinct fields are fine. The check follows branches and goes through loop
bodies twice. Two simplifications: a binding a `match` or `is` makes from a
place moves the whole variable when it moves, and distinct fields of a
variable move separately (`let a = p.first; let b = p.second`).

## Generics

Each instance of a generic function is its own Lua function, named with its
instance number (`F.largest_4_12`). So are a generic warp's methods, one per
list of arguments it's used with. A generic warp's runtime descriptor is per
type too (`Pair[i32, str]`), since its fields' descriptors differ.

A generic function nobody calls isn't checked or emitted. An error in a
generic body is reported once, with a note naming the instance it came from,
even when several instances hit it.

## Sequences

`xs[i]` checks `i` against the length (`rt.idx`, which also makes it
1-based), unless the index's type can't reach past the end of an array: a
`u4` into `[u8; 16]` is never out of bounds. `xs[a..b]` makes a view
(`rt.subview`), and so does passing an array or a list where a `[T]` is
expected (`rt.view`).

`List` and `Slice` are `#builtin` warps in std; their methods are in
`rt.ext` and get the element type's descriptor first, so `contains`, `sort`
and printing work for any element type. Arrays and lists have every slice
method, passed as they are. `for mut x in xs` makes `x` stand for `xs[i]`.

## Lua's limits

Lua allows 200 active locals in a function. When one block of an emitted
function would go over (a long body, with the temporaries that `if` and
`match` values need), its locals become the fields of a table made at the
start of the block (`local _L1 = {}`, `_L1.x_3`); see `lower/limits.rs`.

## Lambdas and streams

A lambda is a Lua closure. It sees the enclosing locals as upvalues, so
`xs.each(x => total += x)` changes `total` itself, and each loop iteration's
locals are fresh. Its body is lowered as a function of its own: `return`
leaves the lambda, and a `?` in it leaves the lambda too (labels don't cross
into a Lua function). A function's name used as a value is its `F` entry.
Escape checking (the `^(…)` rule for stored closures) comes with the region
checker; for now a stored lambda simply works.

A `Stream[T]` is a Lua function returning `true, item` (`true` first, since
an item can be `nil`), so `for x in s` is Lua's generic `for _, x in s do`.
Its methods live in `rt.ext` like `List`'s. Streams are lazy and read once.
Each item is its own value: a stream of a list's elements gives copies
(`rt.seq_stream` copies as it goes), so a loop over one may keep or move its
items. The methods that keep items copy them too, which is a second copy for
a list's stream but never shares a table.

Arrays, lists, slices, ranges and types that weave `Iter` all have
`Stream`'s methods (method lookup rule 3, see 02-types): the receiver becomes
`rt.seq_stream(xs)`, `rt.range_stream(d, r)` or `rt.iter_stream(F.next, it,
boxed, copy_out)`, and the same happens where a `Stream` argument is
expected (`a.zip(1..10)`) and in `for` over an `Iter`. An `Iter`'s stream
works on a copy of a `Copy` place, since `next` changes it. `map` and
`filter` on a slice are eager instead, and make lists.

When an unravel happens inside the runtime (a stream's step, say), the host
reports the first frame of the Lua traceback that maps to Loom code.

## Gauges

A gauge's value is its base's: a Lua number, with its base's descriptor. The
type checker keeps gauges apart (`TyKind::Gauge`); lowering sees their base
types. Each gauge checked at run time gets a function, `F.Cents_12(v)`, of
comparisons against its range or a call of its predicate (plus its base's
range, for values from a wider type). `x as G` calls `rt.into_gauge`, which
unravels; `as?` calls `rt.gauge_opt`; `as|` converts to the base saturating
and then `rt.clamp`s to the range (NaN goes to the low end). Arithmetic
intervals, literal checks and `is` narrowing are all at compile time. A
narrowing only happens for a local that can't change and already has the
gauge's base type.

Captured runs (the test harness) see an empty environment through the
host's `getenv`, so `env.get` gives the same answer on every machine.

## Text and primitive methods

`str` and `String` are both Lua strings. `String`'s `mut self` methods
(`push`, `push_str`) return the new string after their result, like any
scalar `mut` parameter, and the caller stores it back. `s[a..b]` slices by
bytes and unravels when either end is inside a character.

The methods of `str`, `char`, integers and floats come from std's builtin
warps `Str`, `Char`, `Int[T]` and `Float[T]` (a `String` has `Str`'s too).
The value is passed as it is; `Int` and `Float` methods get the type's
descriptor first, so `trailing_zeros` and `abs` know the width.

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

An `any W` value is `{v, vt}`. The vtable holds the type's descriptor
(`vt.d`) and, keyed by the weft method's number, a function for each weft
method the program calls through `any`: `w.draw(at, mut c)` is
`rt.dyn(w, 31, at, c)`. Vtables are filled in after type checking, since a
call anywhere can add a method to every one. A type test (`w is b: Button`)
compares `vt.d` with `Button`'s descriptor, which is unique per type. An
`any` of more wefts is already an `any` of fewer: nothing is converted.

`Fmt` is a Lua array of strings: `f.put(x)` appends `x`'s shown form, and
`rt.show` concatenates the array after calling a hand-written `show`.

## Control flow shapes

- `?` / `mend`: result locals declared before a `do … end`, failures `goto mN_fail`,
  success `goto mN_done`. Labels are unique per function. The handler is its
  own `do … end`, so no `goto` jumps into a local's scope (see
  `lower/fallible.rs`). A `?` with no `mend` around it returns `nil` (an
  optional) or `{false, e}`, with `e` converted to the function's error type.
- Union values are boxed like `any` values, `{v, vt}`, with a vtable that only
  holds the type's descriptor: type tests and printing work the same way.
- `continue`: `while c do do BODY end ::cN:: end`.
- Labeled break: `goto bN` with the label right after the outer loop.
- Never `repeat … until` (its condition sees body locals).
- `defer`: `local d <close> = rt.defer(function() … end)`, so Lua runs it
  however the block ends: its end, `return`, `break`, a `goto` out (`?`,
  `continue`), or an error (an unravel). A value being returned is worked
  out before the defers run. `defer x => …` adds a flag, `local f = true`,
  that each move of `x` sets to false (the move checker records where they
  are) and a new value of `x` sets back; the body runs only while it's true.
  So a value moved into a call is the call's to clean up, even if that call
  then unravels. The type checker keeps `return`, `fail`, `?` and
  `break`/`continue` from leaving a defer (T0118); the move checker keeps a
  defer's variables from moving while it still needs them (M0102).
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

`main ! E` shows its error (`error: …` on stderr) and exits with 1.

Implemented so far: 0, 1, 2, 70 and 101. Still to come: Ctrl-C
(130), `process.exit`, an instruction budget, and `--lua PATH` to run a bundle
in an external interpreter.

## Not supported in v1

Raw pointers, `extern "C"`, `#layout(bits)` I/O, f16, integers wider than 64
bits, user-written `Spool` weaves, anonymous `warp {}` types from `const fn`s (U0101),
and `bolt.loom`. Each fails with a `U` code.
