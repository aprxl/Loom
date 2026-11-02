# 05 · Shuttles & looms

On a real loom, the shuttle carries the weft thread back and forth across the
warp. In Loom, a **shuttle** is a function that can pause and resume, yielding
values out and pulling values in. A **loom block** is a scope where threads
run side by side, and every one of them has finished before the scope ends.

---

## Shuttles

### Declaring one

The return arrow tells you what kind of function it is:

| Signature | Kind | Type |
|-----------|------|------|
| `fun f() -> T` | ordinary function | |
| `fun f() ~> T` | **generator**: yields `T`s | `Shuttle[T, ()]` |
| `fun f() ~> T <~ U` | **duplex**: pulls in `U`s, yields `T`s | `Shuttle[T, U]` |
| `fun f() <~ U` | **sink**: only pulls | `Shuttle[(), U]` |
| `fun f(mut self) ~> mut T` | **projection**: lends places (see [03](03-memory.md#projections)) | |

Two expressions only work inside a shuttle:

- **`yield v`** sends `v` out and suspends until the driver asks for more.
- **`pull`** suspends until the driver pushes an input. It evaluates to
  `?U`, which is `none` once the input has been closed.

`return` ends the shuttle. Calling a shuttle function doesn't run any of its
body. It just builds the suspended shuttle value.

```loom
fun fib() ~> u64 {
    var (a, b) = (0, 1)
    loop {
        yield a
        (a, b) = (b, a + b)
    }
}

fun running_mean() ~> f64 <~ f64 {
    var (sum, n) = (0.0, 0)
    while pull is some(x) {
        sum += x
        n += 1
        yield sum / n as f64
    }
}
```

### Driving one

- **Generators are iterators:** `for x in fib().take(10)`.
- **`sh.push(u)`** resumes the shuttle with `u` as the value of its pending
  `pull` (on the first push, it starts the shuttle). It returns a **`Burst`**,
  an iterator over what the shuttle yields before it reaches its next
  `pull`. The burst is lazy: iterating it is what actually runs the shuttle.
- **`sh.close()`** makes pending and future `pull`s return `none`, and returns
  the final burst. Pushing into a closed shuttle unravels.
- `Burst` is a **`once`** type. You can't drop the outputs a push produced by
  accident. If you really don't care, call `.drain()`.

```loom
var lex = lexer()
for ch in source.chars() {
    for tok in lex.push(ch) { handle(tok) }
}
for tok in lex.close() { handle(tok) }
```

This push model is why shuttles work well for parsers, protocol state
machines, and stream decoders: the caller keeps control of I/O, and the logic
reads top to bottom.

### Frames

- A shuttle's frame (what it keeps across its `yield`s and `pull`s) has a
  **size known at compile time**: `lexer.Frame.size` gives it, and
  `lexer.Frame.align` its alignment. Locals never live at the same
  suspension point share space.
- What calling a shuttle function makes has that function's frame in it. A
  value whose maker is known can keep its frame **inline**; one stored where
  makers can differ (a field, a list, two makers meeting in an `if`) is a
  plain `Shuttle[T, U]`, its frame boxed like a large `any`. On Treadle
  today, a generator a `for` loop calls directly has no frame at all (its
  body runs in place, in the loop), and every shuttle held in a value has
  its frame in a box: one allocation when it's made.
- Because the frame is part of the shuttle, a shuttle can't recurse into
  itself directly. To nest one, allocate it: `^inner()`.
- If a shuttle takes **borrowed** parameters, it's second-class, like a
  `ref warp`: it can't be returned or stored. If it only takes owned (`take`)
  parameters, it's an ordinary value and can be stored, moved, or sent to
  another thread.

---

## Loom blocks

```loom
let (a, b) = loom {
    let x = thread slow_sum(left)       // starts running now
    let y = thread slow_sum(right)
    (x.join(), y.join())                // the block's value
}
```

- **`thread expr`** evaluates `expr` on the loom's worker pool and returns a
  **`Spun[T]`** handle, where `T` is the type of `expr` (`return` leaves the
  thread, and a `?` in it can't reach a `mend` outside it). **`.join()`**
  (`fun join(take self) -> T`) waits for the thread and gives its value as it
  is: if `expr` is a `U ! E`, `join` gives the `U ! E` for the caller to
  handle, it doesn't propagate it. A handle is joined once (a second `join`
  unravels), and a `Spun[T]` is `once` when `T` is. If a thread unravelled,
  `join` unravels in the joiner with a note of where the thread started.
- `thread` only works lexically inside a `loom` block, and it belongs to the
  innermost one. The block acts as a region: a handle's type is
  `Spun[T]@<that loom>`. Like arena pointers, handles can go into a `List`
  inside the block, but the region checker won't let one escape it.
- **Every thread started in a loom has finished before the block ends.**
  Threads nobody joined are joined when the block exits. If their results are
  `once`, that's a compile error, because you'd lose them.

### Why threads can borrow

Because nothing outlives its loom, threads can borrow local data directly,
with no `Arc` and no `'static` bound:

| Captured as | Allowed |
|-------------|---------|
| read | by any number of threads at once |
| `mut` | by **one** thread, and the parent can't touch it until that thread is joined |
| `take` | moved into the thread |

Shared mutation needs a type designed for it: `Atomic[T]`, `Mutex[T]`,
`Pipe[T]` (a bounded channel). Their read-only methods do the synchronizing
internally. Arena pointers (`^T@frame`) can't cross threads unless the arena
is a `SharedArena`. So data races become type errors:

```
error[C0401]: `hits` is mutated by more than one thread
  --> race.loom:2:37
  |
2 |     loom for x in data { if x > 0 { hits += 1 } }
  |                                     ^^^^ every iteration writes `hits`
   = help: use `Atomic[u32]`, or produce a value per iteration and `.sum()` them
```

### Leaving early

If the block exits early through `?`, `return`, `break`, or a thread that
unraveled, the loom:

1. **cancels** the unfinished threads, cooperatively, at their next
   cancellation point: a `yield`, `pull`, `join`, `sched_yield()`, a `time.sleep` (on Treadle), a wait on
   a `Mutex` (`lock`) or a `Pipe` (`send`, `recv`), or the wait for a
   constant another thread is computing. **I/O is not a cancel point yet**:
   a thread that is inside a file or terminal call isn't interrupted, and
   one that never reaches a cancellation point can't be cancelled (the
   block waits for it). Making I/O calls cancel points is planned with
   `std.reel` (see below).
2. **waits** for all of them
3. carries on with the exit: returning, propagating the error, or unravelling
   again in the parent

The error is only seen after every thread is done, so structured concurrency
still holds when things go wrong.

### `loom for`

`loom for` is shorthand for "one thread per iteration, joined at the end". The
pool decides the actual parallelism.

```loom
loom for (row, mut line) in pixels.chunks_mut(width).indexed() {
    for col in 0..width { line[col] = shade(col, row) }
}
```

The iterations have to be independent. Each one may hold `mut` places only if
the iterator guarantees they're **disjoint**. `chunks_mut` is a projection
marked `#disjoint` in `std`.

### Where threads run

A loom schedules threads on the ambient `given pool: Pool`. The default is a
global pool with one worker per core (`LOOM_THREADS` changes the count; with
one worker threads run in the reference schedule: a thread runs at once until
it parks). For a dedicated pool, `loom(given: my_pool) { ... }` is planned,
but **not implemented yet**: it waits for V6, which brings `given`, and a
`loom` with arguments is rejected today with a `U` diagnostic. There is no
`Pool` type in `std` yet either. Loom has no hidden async runtime.

---

## Not in v0.1

- **async I/O.** Shuttles already give you suspendable functions. The plan is
  a `std.reel` event loop that drives shuttles blocked on I/O, with no
  function coloring, since a shuttle's type already says it suspends. See
  [open questions](open-questions.md).
- **Shuttle composition.** Something like `lexer().feed(parser())` would be
  nice to have.
