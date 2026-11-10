title: Threads
summary: Loom blocks, threads that can't outlive them, what they may share, and how the compiler keeps data races out.
---
Using every core is easy to ask for and hard to do safely. Two threads write the same variable, a thread outlives the data it borrowed, one failure leaves the others running with nobody watching. Most languages leave you to avoid these by discipline, a race detector, or a lot of ceremony around ownership.

Loom's answer is **structured**: threads live inside a **loom block**, and every one of them is done before the block ends. That single rule makes the rest simple. Because nothing outlives its loom, threads can borrow local variables directly, with no copies and no reference counting, and the type checker can see exactly which threads touch what. A data race is a type error, found before the program runs.

Threads run on several cores by default. The samples in this book run on one worker, in a fixed order, so their output is the same on every run (see [When threads run](page:threads#when-threads-run)).

## Loom blocks and `thread`

A **loom block** is a scope where threads run. `thread e` starts `e` as a thread of the innermost loom around it, and gives a `Spun[T]` for the `T` it computes. `h.join()` waits for the thread and gives the `T`. The block itself is an expression: its value is its last expression's.

@sample threads_basic

Both threads read `data`, which is declared outside the loom, and neither copies it. `data[..4]` and `data[4..]` are views of the same array.

- A thread nobody joined is joined where the block ends. The block's value is its last expression's: a `thread` there would be a handle, which can't leave the block ([C0404](diag:C0404)), so end the block with `()`, or join it.
- `thread` is only allowed inside a `loom` ([C0403](diag:C0403)), and the loom is the innermost one around it, also from inside a lambda or another thread.
- A `Spun` is joined once; a second `join` unravels (a `once` result makes the handle `once`, so it's [L0301](diag:L0301) to lose it).

@sample threads_err_handle

@try In `threads_basic`, add a third thread over `data[2..6]` and add its result to the total. Then delete the `.join()` of one thread and run it again: the block waits for it anyway, but its result is dropped.

@compare
-- Rust
`std::thread::scope` is the closest relative: threads started on a scope may borrow locals, and the scope joins them all at the end. The difference is how safety is checked. Rust relies on `Send`, `Sync` and borrow rules written into types and closures; Loom relies on the loom block alone.
```rust
let data = [3, 1, 4, 1, 5, 9, 2, 6];
let total = std::thread::scope(|s| {
    let left = s.spawn(|| sum(&data[..4]));
    let right = s.spawn(|| sum(&data[4..]));
    left.join().unwrap() + right.join().unwrap()
});
```
-- Go
Goroutines aren't scoped: `go f()` returns at once and nothing says when it ends, so you add a `WaitGroup` (or an `errgroup.Group`, which also collects the first error) to join them. Loom's join is the end of the block.
```go
var wg sync.WaitGroup
var left, right int
wg.Add(2)
go func() { defer wg.Done(); left = sum(data[:4]) }()
go func() { defer wg.Done(); right = sum(data[4:]) }()
wg.Wait()
total := left + right
```
-- Swift
Swift's structured concurrency has the same shape: a task group is a scope, child tasks can't outlive it, and the group is what you await. It's built on `async`/`await`, so functions that suspend are marked; Loom's threads run ordinary functions.
```swift
let total = await withTaskGroup(of: Int.self) { group in
    group.addTask { sum(data[..<4]) }
    group.addTask { sum(data[4...]) }
    return await group.reduce(0, +)
}
```
-- Java
Java's virtual threads are cheap enough to start one per task. An executor in a `try` block waits for its tasks when the block closes, which is the same structure, though nothing stops a task from escaping if it's started elsewhere. `StructuredTaskScope` (structured concurrency, introduced as a preview) makes the scope explicit.
```java
try (var executor = Executors.newVirtualThreadPerTaskExecutor()) {
    Future<Integer> left = executor.submit(() -> sum(data, 0, 4));
    Future<Integer> right = executor.submit(() -> sum(data, 4, 8));
    total = left.get() + right.get();
}
```
@end

## When threads run

A thread runs as soon as it's started, until it finishes or **parks**: waits in a `join`, or calls `sched_yield()`, which sends it to the back of the queue. Then another one runs, and the one that started it goes on. This is called the **reference schedule**, and it is what you get with one worker (`LOOM_THREADS=1`), which is what the Lua backend always does, and what this book's samples use. Nothing is preempted, so a program's output is the same on every run and on both backends.

@sample threads_order

`a` runs first, prints, and yields; `b` starts and does the same; then the parent, which started both, gets back control and prints `started`. The block's end waits for what's left.

With several workers, Treadle's default (one worker per core), threads run side by side. What a program prints from inside threads can come in any order: eight runs of `threads_order` on eight workers gave three different orders. Everything after the loom is the same, because the block waits. The rules in the next sections are what make a program's *results* the same however the threads interleave.

> Two things only matter with one worker, and the samples show them. A thread that spins on `load()` waiting for another one never lets it run, so put a `sched_yield()` in the loop. And `sched_yield()` is how you ask for the interleaving in `threads_order`; with many workers it means little.

@try Change `steps` from `2` to `3` in one of the two `thread worker(...)` calls, and read the interleaving. Then run it again with `LOOM_THREADS=8` on your machine (`loom run`) and see the order change.

## What a thread may share

Threads borrow local variables directly. The checker keeps that safe with two rules about where a variable was declared:

- What's declared **outside** the loom is shared. A thread may read it; if it changes it, no other thread may touch it, nor the parent, until it's joined ([C0401](diag:C0401), [C0402](diag:C0402)).
- What's declared **inside** the loom block (a loop variable, a local of the block, the parameter of a lambda around the `thread`) is **copied** into the thread when it starts: it may be gone by the time the thread runs. A thread can't change its copy ([C0405](diag:C0405)). A variable that owns what it holds (a `String`, a `List`) **moves** into the thread instead, and the parent can't use it after.

@sample threads_share

In the first loom, `count` is outside it and only one thread changes it, so that is allowed, and the parent reads it only after the join. In the second, `w` is a loop variable, so each thread gets its own copy.

### The race detector is the type checker

Break the rule and the compiler says which thread and which variable:

@sample threads_err

Two threads changing `hits` is [C0401](diag:C0401), and the help says what to do instead: `Atomic`, or a value per thread. The parent can't read a variable while a thread that changes it hasn't been joined either ([C0402](diag:C0402)):

@sample threads_err_join

And a thread can only change what outlives the loom. A variable declared inside the block is copied, so the change would be invisible to everybody, and the compiler says so ([C0405](diag:C0405)):

@sample threads_err_copy

@try In `threads_err`, wrap the first `thread { hits += 1 }` in a `let t = ` and call `t.join()` before the second thread starts. Nobody else touches `hits` while the first changes it, so it compiles.

@compare
-- Go
The same program compiles in Go and has a race. The detector (`go run -race`) finds it when the racy lines run; the compiler never does.
```go
hits := 0
var wg sync.WaitGroup
for i := 0; i < 2; i++ {
    wg.Add(1)
    go func() { defer wg.Done(); hits++ }()
}
wg.Wait()
```
-- Rust
Rust refuses it too, with borrow rules: two closures can't both hold a `&mut` to `hits`. The error is about aliasing, and it comes from the same machinery that governs the rest of the language. Loom's rule is specific to looms.
```rust
let mut hits = 0;
std::thread::scope(|s| {
    s.spawn(|| hits += 1);
    s.spawn(|| hits += 1); // error[E0499]: cannot borrow `hits` as mutable more than once
});
```
-- Swift
Swift 6 checks data-race safety at compile time too, with `Sendable` and actor isolation, and reports a mutation of a captured `var` in concurrently executing code. It does so across the whole language, not just within a task group.
```swift
var hits = 0
await withTaskGroup(of: Void.self) { group in
    group.addTask { hits += 1 } // error: mutation of captured var in concurrently-executing code
}
```
@end

## One thread for each item: `loom for`

`loom for x in items { … }` is a loom that starts a thread for each item. The parent goes through `items` (so it's a list, a range, a stream), and each iteration runs as a thread of its own. An item that owns memory moves into its thread; one that's a view of what outlives the loop is copied.

@sample threads_for

Each iteration is a thread that ends with its body, so nothing in it can leave the loop or the function: `break`, `return`, a `continue` of an outer loop, and a `?` with no `mend` are [C0406](diag:C0406). `continue` ends the iteration. A variable the iterations change is [C0401](diag:C0401), as for any threads: use an `Atomic`, or a `Mutex`.

@sample threads_err_break

How many threads actually run at once is decided by the workers, not by the loop. Over an exclusive integer range, a list, a slice, an array or their chunks, a task gets a run of items when there are several workers, so a hundred thousand iterations don't mean a hundred thousand tasks. Other iterators keep a task per item.

@try In `threads_for`, change `1..=5` to `1..=1000`. The sum of squares comes out as 333833500, and the program has run a thousand iterations as threads. Then add `if n % 2 == 0 { continue }` at the top of the loop and see that it ends only that iteration.

@compare
-- Rust (rayon)
rayon's parallel iterators are the nearest thing: a loop over items split across a pool, with the data-race rules enforced by closures' types. `reduce`/`sum` replace the `Atomic`.
```rust
use rayon::prelude::*;
let sum: u64 = (1..=5u64).into_par_iter().map(|n| n * n).sum();
```
-- C and C++ (OpenMP)
`#pragma omp parallel for` is the same idea written as an annotation on an ordinary loop. Nothing checks that iterations are independent: a race compiles and silently gives wrong answers, unless you write the `reduction` or `atomic` clause yourself.
```c
unsigned long sum = 0;
#pragma omp parallel for reduction(+:sum)
for (int n = 1; n <= 5; n++) sum += n * n;
```
-- Go
Go has no parallel loop. You start a goroutine per item (or a worker per core with a channel of items) and join with a `WaitGroup`, which is why libraries for it exist.
```go
var wg sync.WaitGroup
var sum atomic.Uint64
for n := uint64(1); n <= 5; n++ {
    wg.Add(1)
    go func() { defer wg.Done(); sum.Add(n * n) }()
}
wg.Wait()
```
@end

## Changing pieces of a list: `chunks_mut`

`xs.chunks_mut(n)` gives views of `n` elements at a time, which can be changed, and don't overlap. A `loom for` over them has each thread change its own piece, with no `Atomic` and no lock: the list is the parent's again when the loop ends. Pieces that might overlap (a view made another way) are [C0401](diag:C0401) when two threads change them.

@sample threads_rows

The compiler knows the pieces are disjoint because `chunks_mut` is declared that way in `std` (`#disjoint`); a hand-made view isn't.

@try Change `width` to `3`, and read the grid: four rows of three. Then change `line[col] =` to `line[col + 1] =`: the last column of each piece is out of bounds, and the bounds check catches it instead of writing into the next thread's piece.

@compare
-- Rust
Rust has the same method name: `chunks_mut` gives disjoint `&mut` slices, and rayon's `par_chunks_mut` hands them to threads. The disjointness is proved by the standard library's `unsafe` code and the borrow checker, not by a declaration.
```rust
grid.par_chunks_mut(width).enumerate().for_each(|(row, line)| {
    for col in 0..width {
        line[col] = row * 10 + col;
    }
});
```
-- Go
Go slices share memory freely. Handing each goroutine its own sub-slice (`grid[r*w:(r+1)*w]`) is safe by convention only: nothing stops two goroutines from being given overlapping slices.
```go
for r := 0; r < rows; r++ {
    wg.Add(1)
    go func() {
        defer wg.Done()
        line := grid[r*width : (r+1)*width]
        for col := range line { line[col] = r*10 + col }
    }()
}
```
@end

## Sharing what changes: `Atomic`

An `Atomic[T]` (`T` an integer of 8 to 64 bits, or `bool`) is changed through methods that only take `self` to read, so any number of threads can use one at once, with no [C0401](diag:C0401). Every operation is a whole step: `add` gives the value before, `compare_exchange` says whether it swapped.

@sample threads_atomic

The first loom takes turns, because of the `sched_yield()` on one worker; with several, the "saw" values are still all different (0 to 5, each once), but in any order. The second half is a tiny race with two racers: the first `compare_exchange(0, 1)` wins, the second finds a `1` and reports it.

With one worker a thread that spins on `load()` waiting for another one never lets it run: put a `sched_yield()` in the loop.

@try Change `Atomic[u32]` to `Atomic[u8]` and `0..2` to `0..100`. Three threads add 100 times each, which doesn't fit a `u8`: `add` unravels when the sum doesn't fit, leaving the value as it was, and the loom fails.

@compare
-- Rust
Rust's atomics take an explicit memory ordering on every call. Loom's are all sequentially consistent, which is the strongest and the simplest, and there is nothing to choose.
```rust
let hits = AtomicU32::new(0);
let before = hits.fetch_add(1, Ordering::SeqCst);
```
-- Go
Go's `sync/atomic` has typed values now (`atomic.Uint32`) with the same operations; `CompareAndSwap` reports whether it swapped.
```go
var hits atomic.Uint32
before := hits.Add(1) - 1
swapped := hits.CompareAndSwap(0, 1)
```
-- Java
`AtomicInteger` is the same: `getAndAdd` gives the value before, `compareAndSet` says whether it swapped. Its operations are sequentially consistent, as Loom's are.
```java
AtomicInteger hits = new AtomicInteger();
int before = hits.getAndAdd(1);
boolean swapped = hits.compareAndSet(0, 1);
```
@end

## Sharing what needs a lock: `Mutex`

A `Mutex[T]` holds a `T` that one thread at a time uses. `m.lock()` is the value, for the statement it's in: the lock is taken where it's evaluated, and given back when the statement ends, however it ends (an unravel too). Threads that find it taken wait for it, in the order they came.

@sample threads_mutex

There is no guard to hold on to, and no `unlock` to forget. The lock is taken after the right-hand side of an assignment is worked out, so a thread holds it for the shortest time. A thread that locks what it holds already unravels.

@try Replace `log.lock().push(pause(id))` with `let n = pause(id)` and then `log.lock().push(n)`. The result is the same, but the lock is no longer held while `pause` waits.

@compare
-- Rust
Rust's `Mutex<T>` hands out a guard. A guard made in a statement is dropped at its end, so `*total.lock().unwrap() += 10;` looks and behaves like Loom's version; in a `let`, the lock stays until the guard goes out of scope. In Loom there is no guard, so it can't be held across a statement.
```rust
let total = Mutex::new(0);
*total.lock().unwrap() += 10;
```
-- Go
Go's `sync.Mutex` doesn't hold data: it's a lock you take and give back around code that touches a variable, and `defer` is how you remember to give it back. Nothing connects the mutex to what it protects.
```go
var mu sync.Mutex
mu.Lock()
defer mu.Unlock()
total += 10
```
@end

## Passing items: `Pipe`

A `Pipe[T](capacity)` is a bounded queue. `send` puts an item in and waits while the pipe is full; `recv` takes the oldest and waits while it's empty, giving `none` once the pipe is closed and drained. `close` ends it: what's in it can still be received, and the threads waiting on it wake.

@sample threads_pipe

The producer sends four numbers into a pipe that holds two, so it waits when the pipe is full. The worker turns each into text and sends it on. The parent reads until the worker closes the results, which ends its `while`. Every method takes `self` to read, so any thread can use a pipe, with no `Mutex` around it.

A pipe nobody will fill, with a thread waiting to receive from it, is a deadlock, and it's reported instead of hanging (see below).

@try Change the capacity of `jobs` from `2` to `1`, then to `4`. The output doesn't change: capacity changes how often a thread waits, not what arrives.

@compare
-- Go
Channels are the model Pipe follows: bounded, `close` ends them, and a receive from a closed and empty channel says so (`ok` is false, Loom's `none`). Go's channels are first-class in the syntax (`<-`), and nothing stops goroutines being left blocked forever.
```go
jobs := make(chan int, 2)
go func() {
    for n := 1; n <= 4; n++ { jobs <- n }
    close(jobs)
}()
for n := range jobs { fmt.Println("job", n) }
```
-- Rust
`std::sync::mpsc::sync_channel` is the bounded channel; dropping all senders closes it, so a receiver ends when the last sender goes out of scope. Loom closes a pipe explicitly, and a pipe can be shared by any number of senders and receivers.
```rust
let (tx, rx) = std::sync::mpsc::sync_channel(2);
std::thread::spawn(move || {
    for n in 1..=4 { tx.send(n).unwrap(); }
});
for n in rx { println!("job {n}"); }
```
@end

## Results and errors

`join` doesn't throw. It gives back exactly what the thread's expression computed: if that is an `i32 ! Odd`, `join` gives the `i32 ! Odd`, for you to match, or pass on with `?` where there's a `mend`. An *error* is a value, and a thread that returns one has done nothing wrong.

@sample threads_results

## When something goes wrong

A thread that **unravels** is different: it fails its loom. The other threads are **cancelled**, and the parent unravels with the thread's message where it sees it, at the `join` or where the block ends, after everything has stopped.

@sample threads_cancel

The second thread's bad index fails the loom. The first thread, an endless loop, is cancelled at its next `sched_yield()`: it unwinds from there, and its `defer` runs (`slow: cleaned up`). Then the parent unravels, with the message and where the failing thread was started. `not reached` never prints.

A cancelled thread unwinds from where it waits (`join`, `sched_yield`, a `yield` or `pull`, a `Mutex` or `Pipe` wait), running its `defer`s, and nobody sees the cancel: `mend` doesn't catch it. The same goes for leaving the block early with `?`, `return` or `break`, for `process.exit` in a thread, and for Ctrl-C. A thread started in a loom that has already failed never runs. Reading a file or the terminal isn't a cancel point yet: a thread stuck in one finishes its call before it can be cancelled.

If every thread is waiting and none can wake another, the loom is **deadlocked**, and the parent unravels where it waits instead of hanging:

@sample threads_deadlock

@compare
-- Swift
Swift task groups cancel their siblings when one child throws, and the error surfaces at the end of the group, which is the same contract. Cancellation is cooperative there too: a task has to check, or wait at a suspension point.
```swift
try await withThrowingTaskGroup(of: Void.self) { group in
    group.addTask { try await slowWork() }
    group.addTask { throw Failure.bad }   // cancels the first one
    try await group.waitForAll()
}
```
-- Java
`StructuredTaskScope.ShutdownOnFailure`, from the JDK 21 to 24 previews of structured concurrency, is the same shape: the first failure interrupts the other subtasks, and `join` then throws. Ordinary executors don't do this; a failed task just sits in its `Future` until someone asks.
```java
try (var scope = new StructuredTaskScope.ShutdownOnFailure()) {
    var a = scope.fork(() -> slowWork());
    var b = scope.fork(() -> { throw new IllegalStateException(); });
    scope.join().throwIfFailed();
}
```
-- Go
A panic in a goroutine takes down the whole process, and nothing cancels its siblings: for that you pass a `context.Context` around and check it, or use `errgroup.WithContext`.
```go
g, ctx := errgroup.WithContext(context.Background())
g.Go(func() error { return slowWork(ctx) })
g.Go(func() error { return errors.New("bad") })  // cancels ctx
err := g.Wait()
```
@end

## How many run at once

A loom schedules its threads on Treadle's worker pool: one worker per core by default, `LOOM_THREADS=n` to change it. A loom can say it wants fewer, with a `Crew` (from [std.sync](std:sync)) given to it:

@sample threads_crew

With a `Crew` of two, no more than two of the eight threads run at the same moment. A thread that's waiting (in a `join`, a lock, a pipe) holds no worker, so the others go on. A crew of no workers unravels at the `loom`.

@try Change `workers: 2` to `workers: 0` and run it: the unravel names the problem. Then remove the `(given: …)` and run with the default.

## Other rules

- A loom block's own `defer`s run when its block ends, before it waits for its threads.
- Results nobody joined are dropped where the loom ends.
- A shuttle can `yield` and `pull` inside a loom block: its threads keep running while it waits, and closing it early cancels them and waits for them.
- Compile-time code can't start threads ([K0104](diag:K0104)).
- `Atomic` takes integers of 8 to 64 bits and `bool`. A `Pipe` of a `once` type isn't built yet ([U0102](diag:U0102)).

## Where next

Threads are the last feature of the language itself. [How programs run](page:running) follows a program from its file to Treadle, and says what `loom build` writes. After it, [Not supported yet](page:not-yet) says plainly what isn't built: async I/O, for one, isn't part of this version, and neither are thread pools you make yourself with their own queues.
