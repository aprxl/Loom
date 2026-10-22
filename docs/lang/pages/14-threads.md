title: Threads
summary: Loom blocks, threads that can't outlive them, and what they may share.
---
A **loom block** is a scope where threads run, and every one of them is done before the block ends. `thread e` starts `e` as a thread of the innermost loom around it, and gives a `Spun[T]` for the `T` it computes. `h.join()` waits for it and gives the `T`.

@sample threads_basic

## What a thread runs

A thread runs as soon as it's started, until it finishes or **parks**: waits in a `join`, or calls `sched_yield()`, which sends it to the back of the queue. Then another one runs, and the one that started it goes on. Nothing is preempted, so a program's output is the same on every run, and on both backends. (This is the schedule with one worker, which `LOOM_THREADS=1` asks Treadle for and the Lua backend always runs. With several workers, Treadle's default, threads run side by side: what a program prints from inside threads can come in any order, and the rules below are what make its results the same.)

A thread nobody joined is joined where the block ends. The block's value is its last expression's: a `thread` there would be a handle, which can't leave the block ([C0404](diag:C0404)), so end the block with `()`, or join it.

## What a thread may share

Threads borrow local variables directly, with no copies and no `Arc`, because nothing outlives its loom. The checker keeps that safe:

- What's declared **outside** the loom is shared. A thread may read it; if it changes it, no other thread may touch it, nor the parent, until it's joined ([C0401](diag:C0401), [C0402](diag:C0402)).
- What's declared **inside** the loom block (a loop variable, a local of the block, the parameter of a lambda around the `thread`) is **copied** into the thread when it starts: it may be gone by the time the thread runs. A thread can't change its copy ([C0405](diag:C0405)). A variable that owns what it holds (a `String`, a `List`) **moves** into the thread instead, and the parent can't use it after.
- `thread` is only allowed inside a `loom` ([C0403](diag:C0403)), and the loom is the innermost one around it, also from inside a lambda or another thread.

@sample threads_share

The race detector is the type checker:

@sample threads_err

## One thread for each item: `loom for`

`loom for x in items { … }` is a loom that starts a thread for each item. The parent goes through `items` (so it's a list, a range, a stream), and each iteration runs as a thread of its own. An item that owns memory moves into its thread; one that's a view of what outlives the loop is copied.

@sample threads_for

Each iteration is a thread that ends with its body, so nothing in it can leave the loop or the function: `break`, `return`, a `continue` of an outer loop, and a `?` with no `mend` are [C0406](diag:C0406). `continue` ends the iteration. A variable the iterations change is [C0401](diag:C0401), as for any threads: use an `Atomic`, or a `Mutex`.

## Changing pieces of a list: `chunks_mut`

`xs.chunks_mut(n)` gives views of `n` elements at a time, which can be changed, and don't overlap. A `loom for` over them has each thread change its own piece, with no `Atomic` and no lock: the list is the parent's again when the loop ends. Pieces that might overlap (a view made another way) are [C0401](diag:C0401) when two threads change them.

@sample threads_rows

## Sharing what changes: `Atomic`

An `Atomic[T]` (`T` an integer of 8 to 64 bits, or `bool`) is changed through methods that only take `self` to read, so any number of threads can use one at once, with no [C0401](diag:C0401). Every operation is a whole step: `add` gives the value before, `compare_exchange` says whether it swapped.

@sample threads_atomic

A thread that spins on `load()` waiting for another doesn't let it run (nothing is preempted yet): put a `sched_yield()` in the loop.

## Sharing what needs a lock: `Mutex`

A `Mutex[T]` holds a `T` that one thread at a time uses. `m.lock()` is the value, for the statement it's in: the lock is taken where it's evaluated, and given back when the statement ends, however it ends (an unravel too). Threads that find it taken wait for it, in the order they came.

@sample threads_mutex

The lock is taken after the right-hand side of an assignment is worked out, so a thread holds it for the shortest time. A thread that locks what it holds already unravels.

## Passing items: `Pipe`

A `Pipe[T](capacity)` is a bounded queue. `send` puts an item in and waits while the pipe is full; `recv` takes the oldest and waits while it's empty, giving `none` once the pipe is closed and drained. `close` ends it: what's in it can still be received, and the threads waiting on it wake. A thread that waits for a pipe nobody will fill is a deadlock, which unravels the parent where it waits (after cancelling what's left).

@sample threads_pipe

## When something goes wrong

A thread that unravels fails its loom: the other threads are **cancelled**, and the parent unravels with the thread's message where it sees it, at the `join` or where the block ends, after everything has stopped. A cancelled thread unwinds from where it waits (`join`, `sched_yield`, a `yield` or `pull`), running its `defer`s, and nobody sees the cancel: `mend` doesn't catch it. The same goes for leaving the block early with `?`, `return` or `break`, for `process.exit` in a thread, and for Ctrl-C.

A thread started in a loom that has already failed never runs.

## Rules

- A loom block's own `defer`s run when its block ends, before it waits for its threads.
- Results nobody joined are dropped where the loom ends.
- A `Spun` is joined once; a second `join` unravels (a `once` result makes the handle `once`, so it's [L0301](diag:L0301) to lose it).
- A shuttle can `yield` and `pull` inside a loom block: its threads keep running while it waits, and closing it early cancels them and waits for them.
- Compile-time code can't start threads ([K0104](diag:K0104)).
