title: Optionals and errors
summary: `?T`, `T ! E`, and the `?` and `mend` pair.
---
Loom has two kinds of "didn't work": **absence** (`?T`) and **failure** (`T ! E`). One operator leaves either, and one keyword recovers from either.

| | Syntax |
|---|--------|
| optional | `?T`, with values `some(x)` and `none` |
| result | `T ! E`, with values `ok(v)` and `err(e)`; `fn f() ! E` when there's no success value |
| raise | `fail e` |
| bail out | postfix `?` |
| recover | `expr mend fallback`, `expr mend e => handler` |
| give up | `unravel("message")` |

## Optionals

A `T` converts to `?T` by itself, so a function that returns `?i32` can end with a plain number. `?` unwraps an optional. If there's no value, it jumps to the nearest `mend` around it, or leaves the function with `none` if there isn't one. That one rule covers early return, optional chaining and try/catch.

@sample errors_optional

- `mend` has the lowest precedence of any binary operator, so put parentheses around it in arithmetic: `(counts.get(k) mend 0) + 1`.
- A `?` has to sit inside the left side of the `mend` it jumps to. It can cross loops and blocks, but not a lambda or a shuttle.
- `mend` chains left to right: `a() mend b() mend 8080`.
- In `mend e => ...`, `e` is the union of everything that can reach the `mend`.

## Results and error types

An error is a warp that weaves `Error`, usually with cases. `Error` gives it a `message` that shows the case and its payload. `fail .Empty` leaves the function with an error, and the leading dot picks the case from the declared error type.

A `?` that leaves a function converts the error. A `from` case absorbs another error type: `case Parse(from ParseError)` means a `?` on a `ParseError` wraps it as `.Parse(...)`. At most one case can absorb a given type.

@sample errors_results

## Several error types

`A | B` is a union type, mostly used for errors. Each member converts to the union, and a `match` takes the members apart with type patterns. `any Error` accepts an error of any one type, which is handy for `main` and scripts.

@sample errors_union

> A union of errors doesn't convert to `any Error` yet: `?` on a `! (A | B)` inside a `! any Error` function is [T0108](diag:T0108). Widen the function's errors to the union instead.

## `main`

`fn main() ! E` lets `?` leave `main`. An error that reaches the end is printed as `error: ...` on stderr, and the program exits with status 1.

@sample errors_main

## `unravel`

`unravel(message)` is for bugs, not for expected failures. Its type is `never`. It unwinds the current thread and runs `defer`s on the way out. Plain arithmetic overflow, a failed `as`, an out-of-range index and a failed `assert` all unravel. The program exits with status 101.

@sample errors_unravel

`assert(condition, message)` unravels unless the condition holds. In compile-time code a failed assertion is a compile error: see [Compile-time code](page:compile-time).

## Where next

[Moves, `once` and `defer`](page:ownership) is about who owns a value and what runs when a scope ends.
