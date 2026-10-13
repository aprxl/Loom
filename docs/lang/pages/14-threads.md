title: Threads
summary: Loom blocks, threads that can't outlive them, and what they may share.
---
A **loom block** is a scope where threads run, and every one of them is done before the block ends. `thread e` starts `e` as a thread of the innermost loom around it, and gives a `Spun[T]` for the `T` it computes. `h.join()` waits for it and gives the `T`.

@sample threads_basic

## What a thread runs

A thread runs as soon as it's started, until it finishes or **parks**: waits in a `join`, or calls `sched_yield()`, which sends it to the back of the queue. Then another one runs, and the one that started it goes on. Nothing is preempted, so a program's output is the same on every run, and on both backends. (This is the schedule with one worker. Several workers at once come later.)

A thread nobody joined is joined where the block ends. The block's value is its last expression's: a `thread` there would be a handle, which can't leave the block ([C0404](diag:C0404)), so end the block with `()`, or join it.

## What a thread may share

Threads borrow local variables directly, with no copies and no `Arc`, because nothing outlives its loom. The checker keeps that safe:

- What's declared **outside** the loom is shared. A thread may read it; if it changes it, no other thread may touch it, nor the parent, until it's joined ([C0401](diag:C0401), [C0402](diag:C0402)).
- What's declared **inside** the loom block (a loop variable, a local of the block, the parameter of a lambda around the `thread`) is **copied** into the thread when it starts: it may be gone by the time the thread runs. A thread can't change its copy ([C0405](diag:C0405)), and a variable that isn't `Copy` can't be given to it that way.
- `thread` is only allowed inside a `loom` ([C0403](diag:C0403)), and the loom is the innermost one around it, also from inside a lambda or another thread.

@sample threads_share

The race detector is the type checker:

@sample threads_err

## Sharing what changes: `Atomic`

An `Atomic[T]` (`T` an integer of 8 to 64 bits, or `bool`) is changed through methods that only take `self` to read, so any number of threads can use one at once, with no [C0401](diag:C0401). Every operation is a whole step: `add` gives the value before, `compare_exchange` says whether it swapped.

@sample threads_atomic

A thread that spins on `load()` waiting for another doesn't let it run (nothing is preempted yet): put a `sched_yield()` in the loop.

## When something goes wrong

A thread that unravels fails its loom: the other threads are **cancelled**, and the parent unravels with the thread's message where it sees it, at the `join` or where the block ends, after everything has stopped. A cancelled thread unwinds from where it waits (`join`, `sched_yield`, a `yield` or `pull`), running its `defer`s, and nobody sees the cancel: `mend` doesn't catch it. The same goes for leaving the block early with `?`, `return` or `break`, for `process.exit` in a thread, and for Ctrl-C.

A thread started in a loom that has already failed never runs.

## Rules

- A loom block's own `defer`s run when its block ends, before it waits for its threads.
- Results nobody joined are dropped where the loom ends.
- A `Spun` is joined once; a second `join` unravels (a `once` result makes the handle `once`, so it's [L0301](diag:L0301) to lose it).
- `yield` and `pull` can't be inside a loom block yet ([U0102](diag:U0102)).
- Compile-time code can't start threads ([K0104](diag:K0104)).
