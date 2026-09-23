# Open questions

These are the parts of Loom that aren't settled, roughly ordered from "could
break soundness" to "just taste". They're the most interesting places to
experiment, or to steal ideas from for another compiler.

---

### 1. Aliased mutation through arena pointers

Arena pointers are `Copy`, so two `^Node@s` can point at the same node. If
you mutate through one while a borrow taken through the other is still live,
exclusivity breaks. The worst case is changing an enum's case while someone
holds its old payload.

Options:
- **(a)** Arena pointers give read-only access. Mutation goes through `Cell`-like fields.
- **(b)** Enforce exclusivity dynamically when a `^T@arena` is borrowed `mut`
  (Swift's approach), with the check removed when the compiler can prove it's unneeded.
- **(c)** Allow mutation only for plain data (no cases, no owned fields) and require (a) for the rest.

(c) is the current favorite. It covers the common game and AST cases and
keeps ordinary code free of `Cell`.

### 2. Warps with more than one region

"Each value lives in one region" covers most data structures. Explicit
`warp Link[@a, @b]` is the escape hatch, but region parameters then show up in
signatures much like Rust lifetimes. How often does that really happen? One
experiment: allow region parameters only on `ref warp`s.

### 3. Place vs. value in dot-blocks

`p.{ ... }` edits in place when `p` is a mutable place, and gives back an
edited copy otherwise. The rule is simple, but `let q = p.{...}` on a `var`
is a trap, even with a good error message. The alternative is two syntaxes:
`p.{ }` always gives an edited copy, and `with mut p { }` edits in place.
That's more explicit but noisier. It needs real code to decide.

### 4. The continuation rule and tools

The rule that a line starting with `.` continues the previous statement only
when it's indented deeper makes whitespace matter in one narrow place. Editors
and formatters have to get it right. Is that worth it compared to requiring a
trailing `.`, or banning the leading-dot shorthand at the start of a line?

### 5. Inferring `given` across modules

Private functions infer their `given`s. That's easy within a module and gets
interesting with mutual recursion (you need a fixpoint). Should inferred givens
also be allowed to cross module boundaries within a bolt, and only be required
at `pub` boundaries?

### 6. Twill: signed captures and scattered fields

- Real ISAs like RISC-V scatter immediates across the instruction word, and
  often sign-extend them. A possible syntax is `0b iiiiiii_..._iiiii` plus a
  `: i12` annotation on the capture, meaning "sign-extend". Or allow uppercase
  for signed captures? No, uppercase is already hex digits.
- Should captures be able to reorder bits (`i[11|4:0]`)? That's probably a
  library helper, not syntax.

### 7. Shuttles that fail, and async

What does `?` mean inside a shuttle? Right now it ends the shuttle, and the
error only shows up in `sh.result()`. Should the `Burst` produce `T ! E`?

Async I/O: the plan is to have a reel (event loop) drive shuttles that are
blocked on I/O handles. The open question is whether a `Shuttle` that awaits
I/O needs its own type (`~> T` plus `waits Io`), or whether that's
visible anyway from the `given reel: Reel` it needs.

### 8. `once` and `unravel`

When a thread unravels, `once` values without a fallback defer get leaked. Is
that acceptable (like `mem::forget`), or should `once` types require a
panic-time handler (`on unravel => ...`)?

### 9. Arithmetic on gauges

Interval tracking for `+ - *` on range gauges works well until it produces a
surprising type. `Percent * Percent` has interval `0..=1`, so it's still a
`Percent`. `Percent + Percent` has interval `0..=2`, so it quietly becomes a
plain `f32`. Should results that don't fit fall back to the base type
silently, as they do now, or cause a warning?

### 10. How big `any W` is inline

Values of two words or less are stored inline and larger ones are boxed in the
ambient spool. So whether `List[any Widget]` allocates depends on the size of
each concrete type. Should boxing be explicit (`any ^Widget`) so allocation is
always visible?

### 11. Hole lambdas and nested calls

`xs.map(f(g(_)))`: the innermost argument that *properly* contains the `_` is
`g(_)` as `f`'s argument, which gives `xs.map(f(x => g(x)))`. Is that
intuitive? Scala users would expect `x => f(g(x))`. We could make bare
function-call arguments "transparent" to holes, at the cost of a slightly more
complicated rule.

### 12. Names

Settled: the keywords you type every day are plain words (`defer`, `const`,
`unsafe`, `yield`, `once`), and the weaving vocabulary names the things Loom
is built from (`warp`, `weft`, `weave`, `spool`, `shuttle`, twill, gauge).
Still open: `mend` versus Zig-style `catch`, and whether `unravel` should just
be `panic`. `sched_yield()` was renamed from `yield_now()` so it can't be
confused with the `yield` keyword.
