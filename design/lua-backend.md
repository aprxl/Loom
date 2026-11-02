# Lua backend: decision log

How Loom maps onto Lua 5.4, and why. The emitted Lua stays inside the
**5.4 ∩ 5.5 subset**, so the bundled VM can move to 5.5 with a feature flag.
Each decision here should eventually have a fixture that pins it down.

**Guiding rule:** where Loom and Lua agree, lower 1:1. Where Loom promises more
than Lua gives, enforce it at compile time if possible and at runtime (in Lua)
otherwise. Everything the type system proves (borrows, regions, `once`,
exclusivity) is **erased**: Lua never sees it.

**Priorities:** correctness and the whole language first, speed later. The
Lua backend's job is to run every Loom feature with the right semantics,
checked at run time wherever the compiler can't prove it yet; it isn't
optimized (no dropping of proven-unneeded checks, no copy elision beyond
what semantics need). Once the language is complete on Lua, the plan is to
move to Loom's own interpreter, and that's where optimization happens. Until
then, a change that makes emitted code faster at the cost of clarity or of
a runtime check doesn't go in.

---

## Pipeline

```
SourceDb → lex → Tokens → parse → AST → resolve/desugar → HIR
  → typeck(Instance) → THIR ⇄ const eval (Rust fold | Treadle)
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
| Map / Set | insertion-ordered: `{keys, vals, hks, idx, n}` (a set has no `vals`) |

## Calling convention

- **read** and **take** parameters pass the value or reference unchanged.
- **`mut`** parameters:
  - table-backed: pass the reference; whole-value assignment uses a
    generated `overwrite_T(dst, fresh)`;
  - scalar-backed: copy-in/copy-out via extra return values, always through
    temporaries (`local r, t = f(p.x); p.x = t`), because Lua's multiple
    assignment has no defined evaluation order;
  - `?T`, even when `T` is table-backed: copy-in/copy-out too, since
    `none` is `nil` and nothing can be written into it.
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
(`local r, t = f(x); x = t`). A `return` reads them before the function's
`defer`s run, so a function with both runs its body in an inner function
and returns their values once that has returned. A tuple assignment writes
into each `mut` parameter among its targets, after copying every part that's
a table, since one may be another target's old value (`(a, b) = (b, a)`).
A `mut self` method of a scalar type can't be
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
instance number after an `i` (`F.largest_4_i12`), which no other function's
name can end with. So are a generic warp's methods, one per
list of arguments it's used with. A generic warp's runtime descriptor is per
type too (`Pair[i32, str]`), since its fields' descriptors differ.

A generic function nobody calls isn't checked or emitted. An error in a
generic body is reported once, with a note naming the instance it came from,
even when several instances hit it.

An instance that leads to another, and so on, more than 100 in a row
(`inf(n + 1)`), or one made for a type of more than 10 000 parts
(`f[(T, T)]` calling itself doubles `T`), is T0122 rather than a checker that
never stops.

**`const` parameters** are generics whose arguments are values, of an
integer type or `bool` for now. An instance's arguments are types, so a
value is a type too (`TyKind::Value`): `Ring[u32, 4]` and `Ring[u32, 8]` are
different types, and `zeros(4)` and `zeros(5)` different instances. An
array whose length is a `const` parameter is `TyKind::ArrayOf` until the
length is known (in a template, or at a call before the argument says),
which is also how `fun sum[const N: usize](xs: [i32; N])` infers `N`.

- A `const` argument must fold (the constant folder: literals, constants,
  other `const` parameters, arithmetic, comparisons, `log2`,
  `is_power_of_two`, `min`, `max`, `pow`); anything else is K0109. It's
  checked against the parameter's type (T0102).
- A `const` function parameter (`fun zeros(const n: usize)`) is still
  passed at run time like any other; inside the instance, and in
  compile-time code, it's its value.
- `uint(n)` and `int(n)` are integer types of `n` bits. A generic warp's
  field types are worked out again for each list of arguments, since
  `type Idx = uint(N.log2())` can't be in the template.
- A warp's `const assert`s run for each concrete instance of it, with a
  note naming the instance (`in Ring[u32, 100]`); so do those in a
  function's body, for each instance of the function.

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

Lua also has 255 registers per function, and each pending call and operand
of an expression takes one, so a value whose Lua expression nests more than
24 deep goes into a temporary (a sum of 200 terms works), and a template's
pieces are joined in groups of 64. Parameters and `defer`s have to be real
locals: a function with more than 100 parameters, or more than 150 `defer`s,
is U0102.

## Lambdas and streams

A lambda is a Lua closure. It sees the enclosing locals as upvalues, so
`xs.each(x => total += x)` changes `total` itself, and each loop iteration's
locals are fresh. Its body is lowered as a function of its own: `return`
leaves the lambda, and a `?` in it leaves the lambda too (labels don't cross
into a Lua function). A function's name used as a value is its `F` entry.
Escape checking (the `^(…)` rule for stored closures) comes with the region
checker; for now a stored lambda simply works. Until then, a stored lambda
can change a list, map or set while a loop goes over it (the move checker
only stops the loop's body itself), so every loop over one checks, each
time around, that its length hasn't changed, and unravels if it has
(`rt.same_len`).

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

## Shuttles

Calling a shuttle function makes `rt.shuttle(body)`: a table holding a
coroutine that runs the body when driven. `yield v` is
`coroutine.yield(true, v)`; `pull` is `coroutine.yield(false)`, which the
driver resumes with `true, input` for a push or `false` once closed (`none`).
A `Burst` is `{sh, input, closing}`: going through it resumes the shuttle,
hands its input to the first `pull`, and ends at the next `pull` or when the
shuttle does. A generator's stream is the same, with no input to hand over.
Both are `Stream` sources (`IterSource::Shuttle`, `IterSource::Burst`), so
their items go through `for` and every `Stream` method.

- A new `push` runs what's left of the previous burst first, throwing its
  items away, and so does `close`, before it marks the shuttle closed: that
  burst's `pull`s still wait for input. Pushing into a closed shuttle
  unravels.
- An unravel inside a shuttle closes its coroutine (running its pending
  `defer`s) and is raised again, unchanged, where it was driven, so it keeps
  the shuttle's Loom location.
- A `for` loop directly over a generator (`for x in fib()`) passes Lua a
  closing value: leaving the loop early closes the coroutine, which runs
  the generator's `defer`s; one of those that unravels unravels there, in
  the loop, unless the loop is already unwinding from another (design/vm.md
  §14, P3). A shuttle dropped part-way through any other way (after
  `.take(3)`, say) is left to the garbage collector, and its `defer`s don't
  run (P2: Treadle runs them).
- A shuttle keeps its parameters as long as it lives. Until the region
  checker can follow a borrow that long, it only takes ones it owns:
  `take` parameters, and `Copy` ones, which it copies when it's made (U0102
  otherwise).
- `yield`/`pull` in a shuttle that doesn't yield/pull is T0120; a shuttle
  calling itself is T0121 (its frame would contain itself); neither can be
  inside a `defer` (T0118). A burst is `once`, so dropping one is L0301.
- A projection (`~> mut T`, `~> ref T`) yields its place as `{table, key}`:
  a call of one is `rt.lend(…)`, which runs it to that `yield`, and the place
  is `t[k]` until the statement ends, when `rt.lend_end` runs it to its end
  (it unravels if it lends a second place, and so does one that ends without
  lending any). The place has to be in a table: a projection's own local
  can't be lent (U0102). One whose statement unravels is left to the garbage
  collector (P2).
- `f.Frame.size` and `f.Frame.align` are about Treadle's frame (vm.md
  §10.14): to answer them, the program is checked for Treadle too and the
  shuttle built for it. They're never folded while type checking, so a
  `const assert` on one runs in the sandbox.

## `once` values and files

`once` is checked entirely at compile time and leaves nothing at run time:
the move checker keeps, along each path, the locals whose `once` value may
not be used up yet, and reports one still there when its scope ends or a
`return`, `fail`, `?` or `break` leaves it (L0301), unless a fallback defer
ties it off. A warp holding a `once` value is `once` itself, and never
`Copy`.

A `File` is `{path}`: a write appends to the file, which stays open (at most
64 at a time) until the state ends, however the program ends; Lua's are line
buffered and Treadle's unbuffered, so every line is written at once and a
killed run keeps them. Reading a path flushes it first. Sandboxed runs get an
in-memory file system from the host.

## Maps and sets

`keys[i]` and `vals[i]` are the i-th pair in insertion order, `hks[i]` the
key's hash key and `idx[hk]` its position. A key that's a Lua number,
boolean or string is its own hash key; any other (a tuple, a record) is its
debug text, the same for equal values. (So a warp that writes its own `eq`
and `hash` is still keyed structurally, for now.) Removing an entry marks
it (its `hks[i]` becomes a sentinel) and leaves it in place; the later ones
move down, keeping the order, once more than half the `top` entries made are
removed ones, and before anything walks the entries (`n` counts the live
ones). `m[k]` is `rt.map_at` (it unravels
when the key is missing) and is a place: `m[k].count += 1` changes the value
in the map, and `m[k] op= v` becomes `rt.map_set`. A scalar value in a map
can't be lent as `mut`, since there's no Lua place to write it back to.
Maps and sets compare equal with the same contents in any order, and derive
`hash` as FNV-1a of the debug text.

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

Sandboxed runs (the fixture tests) see an empty environment through the
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
- Shuttles: see below.

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
- A format spec's width and precision are at most 65535, as in Rust (T0117
  past that); a precision above 99 is U0102, since `string.format` stops
  there.
- Every type has a builtin structural Debug in v1.
- A program may use 8 GiB of memory (the VM's limit); past that it unravels
  with `out of memory`, rather than the process aborting.
- An unravel inside a defer during an unravel replaces the first one.
- `target.os` is the build host's OS; `target.pointer_bits` is 63.
- A new `push` while the previous Burst is unfinished drains it silently.
- The RNG is xoshiro256** written in Loom (`std.rand`), never `math.random`.

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
| 70 | internal error (a raw Lua error, or a panic of the compiler or Treadle: a Loom bug) |
| 101 | unravel |
| 130 | interrupted (Ctrl-C) |
| n | `process.exit(n)`, any `n`: `1`, `101` and `130` look like the codes above |

`main ! E` shows its error (`error: …` on stderr) and exits with 1.

Implemented so far: all of them. `process.exit(n)` and Ctrl-C unwind the
program as an unravel does, so its `defer`s run, before it exits; Ctrl-C
sets a flag and, from the handler's thread, installs an instruction hook
(none is installed until then, which would slow every instruction down) that
raises the interrupt and takes itself away, so the unwinding runs `defer`s
to the end. A hook belongs to one Lua state, so the host also keeps the
address of each shuttle's coroutine (the runtime tells it, and holds each
until it's done) and the hook goes on every one: a shuttle that never yields
is interrupted too. A second Ctrl-C while the program unwinds exits at once. Still to come: an
instruction budget, and `--lua PATH` to run a bundle in an external
interpreter.

The host gives the program `env.args()` (the Loom file's path, then the
arguments after `--`), `time.sleep`, and the terminal: `term.raw_mode` and
`term.poll_keys` go through crossterm. Where the terminal reports key
releases (the Windows console does), a key is held from its press to its
release; elsewhere a key counts as held for the one poll after it's pressed.
In raw mode Ctrl-C arrives as a key, so `poll_keys` unravels with
"interrupted (Ctrl-C)" and the program's `defer`s restore the terminal. The
host turns raw mode off, too, if the program ends with it on.

### Tests

`loom test` lowers the same program with `lower_tests`: test bodies are
lowered only then, and the bundle returns `{{name, run}, …}` for the root
module's tests instead of `main`. `loom lua-host --test` calls each `run` in
one VM, collecting its output separately, and prints a report like cargo's.
A test that returns `! E` is wrapped like `main`: `run` returns the error's
text when it fails. `loom test` exits with 0 when every test passed, 1 when
one failed, and 70 when one hit a Loom bug.

### Docs

`loom docs` opens the documentation site, like `zig std`. `crates/loom/build.rs`
builds every page and stylesheet under `docs/` into the binary, so the docs
work offline and match that `loom`. They're unpacked into
`%TEMP%/loom-docs-<version>-<hash>/`, a folder per build's docs (the hash
covers every file), and the landing page is opened with the system's
default program (`explorer.exe`, `open`, or `xdg-open`). `--no-open` only
prints its path. A test checks that every relative link in the site points
at a page that's built in.

### Sandboxed runs

The fixture tests run programs sandboxed (`Host { sandbox: true }`): no
environment variables (`env.get` gives `none`), files in memory starting
empty, no terminal (`poll_keys` gives 0) and no waiting (`sleep` returns at
once). What a sandboxed run prints doesn't depend on the machine.

## Compile-time code

Compile-time code (see `06-compile-time.md`) doesn't run on Lua at all: it
runs on Treadle, in the compiler's own process, whichever backend the program
is for (`consteval.rs`, `heddle/build/jobs.rs`; design/vm.md §17, V4):

- **Jobs.** Typeck records which bodies have a `const` expression. The
  Heddle builder walks those bodies (and the typed constants, read or not)
  for *jobs*: one per typed constant, per `const` expression, and per
  `const assert` (at the top of a module, in a warp, or in a body). Only
  what the jobs reach is built. `loom build`, `run`, `test` and `check` all
  run them; `test` and `check` include the tests' own. A Lua-target program
  is checked with 63-bit `usize`, and its jobs run with it.
- **Constants are lazy.** A constant's flag word in the VM's data says
  whether it's computed, being computed, or failed. Reading one while it's
  being computed is a cycle, K0107. A `const` expression in a function body
  is a constant of its own, so `const fib(20)` in a loop is computed once.
  The Lua bundle does the same with `C` and `rt.lazy`.
- **The sandbox** has no host: std's `io`, `env`, `fs`, `time` and `term`
  natives end a job with K0104. Unravelling is K0103. All jobs share a
  budget of 100M ticks (loop iterations and calls), 1 GiB of memory and 20
  seconds: K0102. After a job fails, the constants it was computing are
  marked failed, and jobs that need them stop quietly rather than reporting
  the same failure again.
- **Values are written into the bundle** (`lower/baked.rs`). After the jobs
  run, each constant they computed is read back from the VM by its type
  (`baked.rs`): numbers, `bool`, `char`, `()`, `str`, `String`, tuples,
  arrays, `List`, records, tuple warps and `?T`, when they're no deeper
  than 32, no more than 64k values and 16 MiB. Such a constant is `C.name =
  <literal>` rather than a function that computes it when read, and a
  number is written where it's read. The literal has the shape the
  program's own code makes of that type: strings are Lua strings, arrays
  and lists `{n = N, [1] = …}`, records keyed by field name, tuple warps by
  position, `none` is `nil` (and `C` has no entry), and `some(v)` is `v`
  (`{v}` for `()` and optionals). Maps, sets, sums, slices and the like are
  still computed when read.
- **Checked while type-checking:** compile-time code can't read the locals
  around it (K0105), and `return`, `break`, `continue`, `?`, `yield` and
  `pull` can't leave it (K0106), exactly like a `defer`'s body.
- **The constant folder** (Rust, `typeck/gauge.rs`) still decides a
  top-level `const assert` whose condition folds from literals, constants,
  arithmetic, comparisons, `and`/`or`/`not` and an array's `.len()`, so the
  type checker reports it without the sandbox. Array lengths and gauge
  bounds must fold: a type can't depend on the sandbox yet (U0102).

**`const if` and `const for`** are decided while type-checking
(`typeck/unroll.rs`), so their conditions and sequences must fold (K0110):
the sandbox runs after type checking, and what's in a branch decides what
gets checked. A `const if` checks, move-checks and lowers only the branch it
takes, reporting that branch's name-resolution errors; the other is never
looked at. A `const for` goes over a range, an array literal (or a constant
holding one) or `.indexed()` of those, at most 10 000 items, and 50 000
bodies for all of a program's `const for`s, nested ones multiplying (K0102); its
body is checked once per item in a `Body` of its own, with the loop
variable standing for the item like a `const` parameter, and lowered once
per item in a `do … end`. `break` and `continue` can't target a `const for`
yet (U0102).

`target` is a `std.target.Target` record: `os`, `arch` and `endian` of the
machine doing the build, and `pointer_bits` 63. `std.sys` (the platform's
own APIs) is U0102 for now.

**Reflection** (`typeck/reflect.rs`) is worked out by the constant folder
while type-checking, like a `const if` condition. `T.name` (a warp's own
name, without arguments), `T.kind` (a case of `std.meta.Kind`), `T.fields`
and `T.cases` (lists of `std.meta.Field` and `Case`, which `const for` goes
over), `f.name`, `f.is_pub`, `c.name`, `c.fields`, and `.len()` and
`.is_empty()` of the lists. Every expression that reads reflection and folds
is recorded in `Body::const_values`, so lowering writes the value itself
(`"x"`, `true`, a unit case's tag); a `Field`, a `Case` or a list of them
needed at run time is K0111. At run time only two things are left:
`v.(f)`, which is `v`'s field `f` (a place, assignable like `v.x`), and
`v is .(c)`, a test of `v`'s tag. `v.(c).(f)` reads a payload field through
`rt.as_case`, which unravels unless `v` is case `c` (it's meant to be used
inside `if v is .(c)`). `T.size`, `T.align` and `f.offset` fold to the
layout Treadle gives values (`ty::layout`): numbers no Lua code depends
on. `T.bits`, `f.ty` and `T.weaves(W)` are U0102 for now, and the builtin
structural `Show`, `Eq`
and `Ord` haven't been replaced by default methods written with reflection
yet.

A constant is one table in `C`, and every use gets a value of its own:
`var t = TABLE` (or `CONFIG.names`, or `TABLE[i]`) copies what it reads into
an owning slot, `Copy` or not, so changing `t` leaves `TABLE` alone. Reads
that don't own (`TABLE.len()`, `for x in TABLE`) share it.

## Not supported in v1

Raw pointers, `extern "C"`, `#layout(bits)` I/O, f16, integers wider than 64
bits, user-written `Spool` weaves, anonymous `warp {}` types from `const fun`s (U0101),
and `bolt.loom`. Each fails with a `U` code.
