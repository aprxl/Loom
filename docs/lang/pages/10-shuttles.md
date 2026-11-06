title: Shuttles
summary: Functions that pause: generators that yield values out, duplex shuttles that take values in, and projections that lend a place.
---
On a real loom the shuttle is the little boat that carries the weft thread back and forth across the warp, pausing at each edge. A Loom **shuttle** is a function that can do the same: run for a while, pause, and be resumed later with its locals intact.

Pausing is useful whenever the caller and the code want to take turns. A generator hands out values one at a time instead of building a whole list. A lexer wants characters pushed in as they arrive and tokens handed back as soon as they're complete. Writing either as an ordinary function means turning the logic inside out into a state machine; a shuttle lets it read from top to bottom.

## Four kinds of function

The return arrow says what kind a function is:

| Signature | Kind | Type |
|-----------|------|------|
| `fun f() -> T` | an ordinary function | |
| `fun f() ~> T` | a **generator**: yields `T`s | `Shuttle[T, ()]` |
| `fun f() ~> T <~ U` | a **duplex** shuttle: pulls in `U`s, yields `T`s | `Shuttle[T, U]` |
| `fun f() <~ U` | a **sink**: only pulls | `Shuttle[(), U]` |

Two expressions work only inside a shuttle. `yield v` sends `v` out and pauses. `pull` pauses until the driver pushes an input, and evaluates to `?U`: `none` once the shuttle is closed. `return` ends it. Using one the signature doesn't allow is [T0120](diag:T0120), and outside any shuttle [N0114](diag:N0114).

@sample shuttles_err_kind

@try Change `sink` to `fun sink() ~> i32 <~ i32`. It can both pull and yield now, and the first error goes away.

## Calling runs nothing

**Calling a shuttle function runs none of its body.** It builds a suspended shuttle value and returns it. The body starts when something drives it, and each drive runs it up to the next `yield` or `pull`.

@sample shuttles_lazy

"called: nothing has run yet" comes before "started". A shuttle is an ordinary value: you can keep it in a variable, pass it as `Shuttle[i32, ()]` (a `take` parameter, since `for` uses it up), and drive it later.

On Treadle, a shuttle's body is a step function plus a frame that holds what the shuttle keeps between pauses. On the Lua backend it's a coroutine.

@compare
-- Python
A Python generator is the same idea: calling it runs nothing until `next`. The difference is typing and cost. A Loom generator has a declared yield type and a frame whose size is known when compiling.
```python
def noisy():
    print("started")
    yield 1

g = noisy()      # nothing printed yet
next(g)          # started
```
-- JavaScript
`function*` works the same way and is driven with `next()`. Loom drives a generator with `for` or any stream method (`take`, `map`, `collect`, ...), because a generator is a stream.
```typescript
function* noisy() {
  console.log("started");
  yield 1;
}
const g = noisy();   // nothing yet
g.next();            // started
```
-- C#
C#'s `yield return` makes an `IEnumerable<T>`, driven by `foreach` or LINQ. The ideas match; Loom has no separate iterator class, and the same arrow syntax also covers pushing values in.
```csharp
static IEnumerable<int> Noisy() {
    Console.WriteLine("started");
    yield return 1;
}
```
@end

## Generators

A generator is a stream of what it yields: `for x in fib()`, `fib().take(10)`. It can be infinite, because only what you ask for is computed. The methods are the ones on any [stream](page:collections): `take`, `skip_while`, `find`, `collect`.

@sample shuttles_gen

Leaving a `for` loop over a generator early (a `break`, a `return`) **closes** it, and that runs its `defer`s, so cleanup written inside the generator happens however the loop ends. In the sample, "early closed" is printed as the loop breaks, and "whole closed" as the generator finishes.

@try Change `fib().take(10)` to `fib().take(20)`: only what you ask for is computed. Then change `countdown("whole", 2)` to start from `0`: the loop body never runs, but the `defer` still does.

@compare
-- Python
Python's generator is closest. `yield` suspends and `for` drives it. Python's `try/finally` inside a generator plays the role of `defer`, and runs when the generator is closed or garbage collected; Loom's `defer` runs when the shuttle is closed or dropped.
```python
def fib():
    a, b = 0, 1
    while True:
        yield a
        a, b = b, a + b

print(list(itertools.islice(fib(), 10)))
```
-- Kotlin
Kotlin's `sequence { yield(x) }` builds a lazy `Sequence`. It's a library over coroutines; in Loom `~>` is part of the function's signature.
```kotlin
val fib = sequence {
    var (a, b) = 0L to 1L
    while (true) { yield(a); a = b.also { b += a } }
}
println(fib.take(10).toList())
```
-- Lua
Lua's coroutines are the model the Lua backend uses: `coroutine.wrap` plus `coroutine.yield`. They are untyped and general; a shuttle is a typed coroutine with a fixed shape.
```lua
local fib = coroutine.wrap(function()
  local a, b = 0, 1
  while true do coroutine.yield(a); a, b = b, a + b end
end)
```
@end

## When a shuttle is dropped

A shuttle can be dropped part-way: after `.take(3)`, at the end of a `break`, or because it goes out of scope. On Treadle that **runs the `defer`s it has reached**, not ones further down its body and not ones in a scope it already left.

@sample shuttles_drop

- Leaving a `for` loop over a generator early closes it; a `defer` in it that unravels unravels in the loop.
- A shuttle dropped any other way runs its reached `defer`s on Treadle. On the Lua backend it is left to the garbage collector and they don't run.
- An unravel inside a shuttle closes it (running its `defer`s) and is raised again where it was driven.
- One dropped before it started runs none.

## Pushing values in

A generator only yields. A **duplex** shuttle (`~> T <~ U`) also takes input, with `pull`: the caller pushes a value and the shuttle goes on from where it paused.

`sh.push(u)` resumes the shuttle with `u` as the value of its pending `pull`. The first push starts it. It returns a **`Burst`**: what the shuttle yields before it next pulls. A burst is lazy, and going through it is what runs the shuttle. `sh.close()` makes pending and later `pull`s give `none`, and returns the final burst. Pushing into a closed shuttle unravels. A **sink** (`<~ U` alone) only pulls, and its bursts are always empty.

@sample shuttles_duplex

`pairs` yields a tuple for every two numbers it's given, and the odd one out when it's closed (`pull` is `none`). `summer` is a sink: it adds up whatever it's pushed and reports at the end, in a `defer` that runs when the shuttle is closed.

@try Push a sixth number in `pairs`' loop (`[1, 2, 3, 4, 5, 6]`) and the `closed:` line goes away: every number found a partner. Then change `s.close().drain()` to a plain `s.close()` and see what the compiler says (next section).

@compare
-- Python
Python's `send()` is the same push: `x = yield` receives what `send` gives, and the generator's `GeneratorExit` is its close. Python needs a priming `next()` first; Loom's first push starts the shuttle.
```python
def summer():
    total = 0
    try:
        while True:
            total += yield
    except GeneratorExit:
        print("sum:", total)

s = summer(); next(s)
s.send(1); s.send(2); s.close()
```
-- JavaScript
`gen.next(value)` sends a value in and `gen.return()` closes it, like `push` and `close`. As in Python, the first `next` is spent starting the generator.
```typescript
function* summer() {
  let total = 0;
  try { while (true) total += yield; }
  finally { console.log("sum:", total); }
}
const s = summer(); s.next();
s.next(1); s.next(2); s.return();
```
-- Go
Go reaches for goroutines and channels to do two-way conversations. That's concurrency; a shuttle is a plain, deterministic pause inside one thread, with no scheduler and no channel to close.
```go
in := make(chan int)
go func() {
    total := 0
    for n := range in { total += n }
    fmt.Println("sum:", total)
}()
in <- 1; in <- 2; close(in)
```
@end

## Bursts are `once`

A `Burst` is a `once` warp, so what a push yields can't be dropped by accident. Go through it with `for` or a stream method, or call `.drain()` to throw it away on purpose:

@sample shuttles_err

The loose-end error ([L0301](diag:L0301), from [Moves, `once` and `defer`](page:ownership)) tells you to do exactly that. The contract is simple: the output of each push is either used or explicitly discarded.

## A push-style state machine

This push model suits lexers, protocol state machines and stream decoders: the caller keeps control of input, and the logic reads from top to bottom, with its state held in ordinary local variables.

@sample shuttles_lexer

`words` keeps the word it's building in a local between pushes. There is no `state` enum and no `match state`, which is how the same thing looks in an ordinary function. `examples/lexer.loom` in the repository is a bigger one: a full lexer written this way.

@try Put two spaces in the text: `"weave  the warp"`. The empty word between them is skipped because of the `is_empty` check; replace both checks with `true` and empty words come out.

## Frames

What a shuttle keeps between pauses is its **frame**, and its size is known while compiling. `fib.Frame.size` is how many bytes a shuttle made by `fib` keeps (24: two `u64`s and a state word), and `fib.Frame.align` its alignment. Locals that are never kept at the same pause share space. A program can pin that down: `examples/lexer.loom` checks its lexer's with `const assert(lexer.Frame.size <= 64)`.

@sample shuttles_frame

A shuttle's frame is stored inside the shuttle value, so a shuttle can't contain itself ([T0121](diag:T0121)):

@sample shuttles_err_recurse

- A shuttle keeps its parameters as long as it lives. For now it only takes ones it owns: `take` parameters, and `Copy` ones, which it copies when it's made. A borrowed parameter such as a `List` or `String` is [U0102](diag:U0102).
- On Treadle, a generator a `for` loop calls directly has no separate frame: its body runs in place. A shuttle held in a variable keeps its frame in a box.

@try Change every `u64` in `fib` to `u32`: the frame shrinks from 24 bytes to 12, and its alignment from 8 to 4.

## Projections

A **projection** is a shuttle that lends a place instead of yielding a value: `~> mut T` lends one to change, `~> ref T` one to read, with `yield mut place` (or `yield ref`). A call of one is that place, for the statement it's in:

@sample shuttles_lend

When the statement ends, the projection resumes, so the code after its `yield` (and its `defer`s) runs. That's how a function can hand out a place without returning a borrow, which Loom otherwise forbids (see [Moves](page:ownership#borrows-are-second-class)).

- One that ends without lending a place unravels, and so does one that lends a second.
- A projection can borrow its parameters (`mut self` here), and what it borrows stays borrowed until the statement ends ([M0104](diag:M0104)).
- What it lends can be read or changed, but not moved out ([M0103](diag:M0103)).
- A `for` loop over a projection that lends many places isn't supported yet: see [Not supported yet](page:not-yet).

@try Add a second `yield mut .cells[0]` after the first one in `at`. The program unravels with "a projection lent more than one place".

@compare
-- Swift
Swift's `_modify` accessors and Hylo's subscripts are the relatives: they yield a place and resume afterwards. Loom reuses the shuttle machinery for it instead of adding a special form.
```swift
var cells: [Int] = [0, 0]
var first: Int {
    _read { yield cells[0] }
    _modify { yield &cells[0] }
}
```
-- Rust
Rust has no equivalent. `IndexMut` returns a `&mut`, which ties the result to the lifetime of the borrow. A projection's cleanup after the `yield` has no counterpart there.
```rust
impl IndexMut<usize> for Grid {
    fn index_mut(&mut self, i: usize) -> &mut i32 { &mut self.cells[i] }
}
```
@end

## Where next

[Compile-time code](page:compile-time) runs ordinary Loom while the program compiles.
