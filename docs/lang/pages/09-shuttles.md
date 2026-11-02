title: Shuttles
summary: Functions that pause, yielding values out and pulling values in.
---
A **shuttle** is a function that can pause and resume. The return arrow says what kind it is:

| Signature | Kind | Type |
|-----------|------|------|
| `fun f() -> T` | an ordinary function | |
| `fun f() ~> T` | a **generator**: yields `T`s | `Shuttle[T, ()]` |
| `fun f() ~> T <~ U` | a **duplex** shuttle: pulls in `U`s, yields `T`s | `Shuttle[T, U]` |
| `fun f() <~ U` | a **sink**: only pulls | `Shuttle[(), U]` |

Two expressions work only inside a shuttle. `yield v` sends `v` out and pauses. `pull` pauses until the driver pushes an input, and evaluates to `?U`: `none` once the shuttle is closed. `return` ends it.

**Calling a shuttle function runs none of its body.** It builds a suspended shuttle value. On the Lua backend that's a coroutine; on Treadle, a frame holding what the shuttle keeps between pauses, and the function that runs it from one pause to the next.

## Generators

A generator is a stream of what it yields: `for x in fib()`, `fib().take(10)`. Leaving a `for` loop over a generator early closes it, and that runs its `defer`s.

@sample shuttles_gen

## Pushing values in

`sh.push(u)` resumes the shuttle with `u` as the value of its pending `pull`. The first push starts it. It returns a **`Burst`**: what the shuttle yields before it next pulls. A burst is lazy, and going through it is what runs the shuttle. `sh.close()` makes pending and later `pull`s give `none`, and returns the final burst. Pushing into a closed shuttle unravels.

@sample shuttles_duplex

A `Burst` is a `once` warp, so the outputs of a push can't be dropped by accident. Go through it with `for` or a stream method, or call `.drain()` to throw them away on purpose:

@sample shuttles_err

This push model suits lexers, protocol state machines and stream decoders: the caller keeps control of input, and the logic reads from top to bottom. `examples/lexer.loom` is one.

## Rules

- `yield` in a shuttle that doesn't yield, or `pull` in one that doesn't pull, is [T0120](diag:T0120). Neither can be inside a `defer` ([T0118](diag:T0118)).
- A shuttle that calls itself is [T0121](diag:T0121): its frame would contain itself.
- A shuttle keeps its parameters as long as it lives. For now it only takes ones it owns: `take` parameters, and `Copy` ones, which it copies when it's made. A borrowed parameter is [U0102](diag:U0102).
- Leaving a `for` loop over a generator early closes it: its `defer`s that it has reached run. One of them that unravels unravels in the loop.
- A shuttle dropped part-way any other way (after `.take(3)`, say) runs the `defer`s it has reached on Treadle; on the Lua backend it's left to the garbage collector, and they don't run.
- An unravel inside a shuttle closes it (running its `defer`s) and is raised again where it was driven.

## Frames

What a shuttle keeps between pauses is its **frame**, and its size is known while compiling: `fib.Frame.size` is how many bytes a shuttle made by `fib` keeps (24: two `u64`s and a state word), and `fib.Frame.align` its alignment. Locals that are never kept at the same pause share space. `examples/lexer.loom` checks its lexer's with `const assert(lexer.Frame.size <= 64)`.

## Projections

A **projection** is a shuttle that lends a place instead of yielding a value: `~> mut T` lends one to change, `~> ref T` one to read, with `yield mut place` (or `yield ref`). A call of one is that place, for the statement it's in:

@sample shuttles_lend

When the statement ends, the projection resumes, so the code after its `yield` (and its `defer`s) runs. One that ends without lending a place unravels, and so does one that lends a second. A projection can borrow its parameters (`mut self` here), and what it borrows stays borrowed until the statement ends ([M0104](diag:M0104)). What it lends can be read or changed, but not moved out ([M0103](diag:M0103)). A `for` loop over a projection that lends many places isn't supported yet.

## Where next

[Compile-time code](page:compile-time) runs ordinary Loom while the program compiles.
