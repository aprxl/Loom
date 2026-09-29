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

The codes are [M0101](diag:M0101) (use after move), [M0102](diag:M0102) (move out of a borrow), [M0103](diag:M0103) (move out of an element), [M0104](diag:M0104) (overlapping `mut`) and [M0105](diag:M0105) (a stale borrow).

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

## Not supported yet

`ref warp`, projections (`~> mut T`), spools and region pointers, and `given` all belong to this part of the design and stop with [U0102](diag:U0102). See [Not supported yet](page:not-yet).

## Where next

[Gauges](page:gauges) restrict a number to a range.
