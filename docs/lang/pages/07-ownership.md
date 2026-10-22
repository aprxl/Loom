title: Moves, `once` and `defer`
summary: Who owns a value, what a parameter can do with it, and what runs when a scope ends.
---
## Values first

Every variable owns its value. There is no type "reference to T", so there are no lifetimes to write. Assigning a value or passing it to a `take` parameter **copies** it if its type is `Copy` and **moves** it otherwise. A warp whose fields are all `Copy` is `Copy` by itself, unless it's a `once` warp.

A function's parameters say what it may do:

| Declared | Call site | The callee |
|----------|-----------|------------|
| `x: T` | `f(x)` | reads `x`; the caller keeps it |
| `mut x: T` | `f(mut x)` | edits `x` in place; the caller keeps it, changed |
| `take x: T` | `f(x)` | owns `x`; the caller loses it unless `T` is `Copy` |

`mut` is written at the call site as well as the declaration. Method receivers are the exception: `scores.sort()` is already clear.

@sample own_params

## Moves are checked

Using a variable after it moved is an error, and so is moving a value out of a borrow (a read parameter, a loop's item, a lambda's captured variable) or out of a list's element. The check follows branches and goes through loop bodies twice.

@sample own_err_moved

While a variable is lent as `mut`, nothing else in the call may touch it. The check works per field, so `swap(mut p.x, mut p.y)` is fine.

@sample own_err_overlap

A borrow lasts as long as the call that received it, so a function can't return one or store one. A local can hold a value that borrows another local (a slice of a list, a stream over one, a lambda), and it sees that local as it is now. When the local changes, a borrow made earlier can't be used:

@sample own_err_borrow

Nor can a borrow outlive what it borrows. A lambda that uses variables around it is passed as an argument or kept in a local, and that's all: returning it, storing it in a warp or a list, or passing it to a `take` parameter is an error, and so is keeping it in a variable declared outside the block of a variable it uses. The same goes for a slice of a local list, and a stream made from one or from such a lambda. (A lambda that uses nothing around it is a plain function value, and goes anywhere.)

@sample own_err_escape

The codes are [M0101](diag:M0101) (use after move), [M0102](diag:M0102) (move out of a borrow), [M0103](diag:M0103) (move out of an element), [M0104](diag:M0104) (overlapping `mut`), [M0105](diag:M0105) (a stale borrow) and [M0106](diag:M0106) (a borrow that outlives what it borrows).

## `once` warps

A `once warp` describes values that must be used up exactly once. They can't be copied, and they can't just go out of scope. The ways to use one up are to destructure it (`let Tx(moves) = self`, which only code that can see its fields may do) or to pass it to a `take` parameter that does. A warp that holds a `once` value is `once` itself, and so is `List[Tx]`.

@sample own_once

The compiler tracks each path. A `once` value that might still be loose when its scope ends, or when a `return`, `fail`, `?` or `break` leaves, is a **loose end**:

@sample own_err_loose

Nothing of this exists at run time: `once` is checked entirely at compile time.

## `defer`

`defer` runs code when its scope ends, however it ends: normally, or through `return`, `fail`, `?`, `break`, `continue` or an unravel. Defers run in reverse order of declaration. A `return` value is worked out before the defers run. A defer can't change control flow: `return`, `fail`, `?`, `break` and `continue` in one are [T0118](diag:T0118).

@sample own_defer

A **fallback defer**, `defer tx => tx.rollback()`, names a `once` value and runs only on exits where that value is still live. It's how the `transfer` function above rolls back on an early exit without doing it after a commit. Giving `tx` a new value re-arms it.

## Pointers

`^e` puts a value in a box on the heap and gives a pointer to it, written `^T`. A pointer owns its box: it moves like any non-`Copy` value, and dropping it drops the value and frees the box. That is how a type holds itself (`^Tree` inside `Tree`). `p^` reads the box. Fields, methods, patterns and `{p}` see through a pointer, so `t.value` and `match t { .Node(l, x, r) => ... }` work on a `^Tree`. A pointer in a `var` can be written through (`p.x = 1`, `p^ += 1`).

@sample own_pointers

A `take` pointer that a `match` takes apart hands its parts to the arms by value, so the arms can move them into a new node.

## Spools

A pointer into the heap owns its box and frees it. When many values live and die together (the nodes of a tree, a frame of a game) a **spool** is cheaper: `spool frame = Arena(1.kib())` makes one, `frame.put(v)` moves a value into it, and `^e` allocates in the spool a function is `given`. A pointer into a spool is `Copy` and owns nothing; `frame.reset()` takes every allocation back at once, and the spool ends with its scope.

`^T@s` is a pointer into the spool `s`, and `fn tree() -> ^Node@sp given sp: Spool` is a function that builds in whichever spool the caller gives (`tree(4, given: frame)`), or the heap if the call gives none. A parameter written `^Node` takes a tree from any spool. A warp that holds pointers (`Node`) is `Node@s` for the spool its nodes are in.

@sample own_spools

The region checker keeps pointers from outliving their spool: a value that points into a spool can't be returned out of the block the spool ends with ([R0101](diag:R0101)), used after the spool was reset ([R0102](diag:R0102)), or passed where a parameter names another spool ([R0104](diag:R0104)). A spool can't be moved ([M0107](diag:M0107)).

@sample own_err_region

Text copied with `.copy()` goes in the spool the call is given, so an arena's tree can hold names and still go at once. A list, map or box can't be held by a warp that is in a spool yet ([U0102](diag:U0102)): nothing would free it.

A pointer into a spool is `Copy`, so two copies see the same value: a field behind one can be written only when the warp is plain data (no text, lists or cases that own anything); otherwise it is read-only ([T0107](diag:T0107)).

`Pool[T](n)` is a spool of `n` slots of one type: `put` takes a free slot and `free` gives it back, a few instructions each. It holds plain data only. `SharedArena(cap)` is an arena threads may allocate in, and its pointers may cross to them; a pointer into any other spool can't ([R0103](diag:R0103)).

## `given`

A function can say what it needs from its caller with a `given` parameter of a weft: `fn welcome(who: str) given g: Greeter`. It is passed on by the call, not in the argument list: the innermost `given` in scope supplies it, `given name = value` states one from there to the end of the block, and `given: x` gives one to a single call. A weft marked `#given_default(Type)` has one when nothing is in scope (a spool's is the heap), so a program that never says `given` still runs. A function is checked for the type it is given, so an unused `given` costs nothing. Every function spells its own `given`s: a private one doesn't infer them from what it calls yet.

@sample own_given

## Not supported yet

Projections (`~> mut T`), which lend a place, work: see [Shuttles](page:shuttles). See [Not supported yet](page:not-yet).

## Where next

[Gauges](page:gauges) restrict a number to a range.
