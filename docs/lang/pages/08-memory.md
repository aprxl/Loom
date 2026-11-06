title: Pointers, spools and `given`
summary: Boxes on the heap, arenas that free everything at once, regions that keep pointers honest, and values passed down implicitly.
---
Most values in Loom live right where they're declared: in a variable, a field, a list. That covers a great deal. But some data has to be *linked*: a tree whose nodes hold their children, a graph, a cache. For those you need pointers, and in most languages pointers are where memory bugs live.

Loom's pointers are checked. This chapter has three parts. First, **boxes**: `^e` puts a value on the heap and gives you a pointer that owns it. Then **spools**: named allocators, such as arenas that free a whole batch at once, with a **region checker** that makes sure no pointer outlives the spool it points into. Finally **`given`**, the way a function says what it needs from its caller (an allocator, a logger, an output) without threading it through every call by hand.

## Boxes: `^e` and `^T`

`^e` puts the value of `e` in a box on the heap and gives you a pointer to it, whose type is `^T`. **A pointer owns its box.** It moves like any non-`Copy` value, and dropping it drops the value and frees the box, so there's nothing to free by hand and nothing to leak.

`p^` reads the box. Everything else looks through it: fields, methods, `==`, `<`, hashing, patterns and `{p}` in a template all see the value inside. A pointer in a `var` can be written through (`p^ += 1`, `p.x = 1`).

@sample mem_box

A `^T` is one machine word, and `?^T` (a pointer that might be absent) costs nothing extra, because a pointer is never null. A box costs about 10 ns to allocate, read and free.

@try Add `print(s)` at the end of `main`. Taking the `String` out of the box moved it, so the pointer `s` is spent ([M0101](diag:M0101)). Change `^"moved".to_string()` to `^"moved"` and it works again: a `str` is `Copy`, so reading it out leaves the box intact.

@compare
-- Rust
`^T` is `Box<T>`: an owning heap pointer, moved by default and freed when dropped. Loom's differences are small ones: `^e` is the whole constructor (no `Box::new`), and `p^` reads instead of `*p`.
```rust
let mut n = Box::new(41);
*n += 1;
println!("{}", *n);
```
-- C++
`std::unique_ptr` is the same idea with the same single owner. Loom's version can't be null and can't be used after a move, and both are checked at compile time.
```cpp
auto n = std::make_unique<int>(41);
*n += 1;
std::cout << *n;
```
-- Go
In Go every pointer is shared and the garbage collector decides when the memory goes. Loom has no collector, so a pointer is an owner: only one place holds a `^T`, and when that place is gone, so is the box.
```go
n := new(int)
*n = 41
*n++
```
@end

## Types that hold themselves

A warp can hold a pointer to its own kind: a `Tree` with `^Tree` fields has no fixed size otherwise. This is also how you build lists, expression trees and graphs.

@sample mem_tree

`insert` takes its tree with `take`, and the `match` takes it apart: a `take` pointer taken apart by `match` hands its parts to the arms **by value**, so the arms can move `l` and `r` into a new node without copying. `sum` and `depth` only read, so they use plain parameters, and `match`, methods and field access all see through the pointer.

- A warp that holds pointers (`Tree`) is non-`Copy`, so it moves.
- Moving out of a box piece by piece works: a pattern's by-value binding takes its part.
- Assigning through a pointer (`p.x = 1`, `p^ += 1`) needs a `var`.

@try Insert the values in order, `[1, 2, 3, 5, 8, 9]`, and print `depth`: the tree degenerates into a list and the depth is 6 instead of 3. The sum doesn't change.

@compare
-- Rust
In Rust you'd write the same type with `Box`. Loom's `match` on a `take` pointer feels like Rust's `match *t`, but moves parts out of the box directly.
```rust
enum Tree {
    Leaf,
    Node(Box<Tree>, i32, Box<Tree>),
}
```
-- C++
In C++ the tree is a struct with `unique_ptr` members and nulls for leaves. Loom has no null: the leaf is a case.
```cpp
struct Tree {
    std::unique_ptr<Tree> left, right;
    int value;
};
```
@end

## Spools: freeing everything at once

A box is freed one at a time. When many values live and die together (the nodes of a tree you're about to throw away, everything a game frame allocates) that's wasteful. A **spool** is an allocator you can name. The simplest is `Arena`, which hands out memory by bumping a pointer and takes it all back at once.

`spool frame = Arena(1.kib())` makes one. `frame.put(v)` moves a value into it and gives a pointer. `frame.reset()` frees everything in the spool, and the spool itself ends with its scope.

@sample mem_arena

Look at what the pointers are. `p` and `q` point at the *same* particle, so writing through `q` changes what `p` sees. A pointer into a spool is `Copy`, and **owns nothing**: it's a plain address, and dropping it does nothing. The spool owns the memory.

- A spool can't be moved ([M0107](diag:M0107)): a pointer into it would lose its home. Lend it instead, with `given: frame` (below).
- Heap pointers (`^T`) own their box. Spool pointers (`^T@frame`) are copies of an address.
- Allocating in an arena is a compare and an add, so it's much cheaper than a box.
- `Arena(cap)` takes a capacity in bytes and grows past it when it needs to. `1.kib()` and `1.mib()` (from `std.mem`) spell sizes.

@sample mem_err_move

@try Raise the loop in `mem_arena` to `0..1000`. That's far more than 1 KiB of particles, and the arena simply grows. Then add `print(p)` after `frame.reset()`: a pointer into a reset arena is exactly what the region checker refuses ([R0102](diag:R0102)).

@compare
-- Zig
Zig passes an allocator to every function that allocates, which is explicit and flexible. An arena is `std.heap.ArenaAllocator`. Loom's spools are the same tool with a checker on top: nothing stops Zig code from using a pointer after `reset`.
```zig
var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
defer arena.deinit();
const p = try arena.allocator().create(Particle);
p.* = .{ .x = 1, .y = 2 };
```
-- Rust
`bumpalo` is Rust's arena. The pointers it gives out are ordinary references tied to the arena by a lifetime, which is the same guarantee Loom's regions give, but in the type system's own vocabulary.
```rust
let bump = bumpalo::Bump::new();
let p = bump.alloc(Particle { x: 1, y: 2 });
```
-- C
In C an arena is a struct you write yourself, and `arena_reset` leaves every pointer dangling. That is the bug Loom's region checker exists to prevent.
```c
Arena a = arena_new(1024);
Particle *p = arena_alloc(&a, sizeof *p);
arena_reset(&a);   /* p now dangles; nothing checks */
```
@end

## Pointers into a spool

Because a spool pointer is `Copy`, two copies see one value. That is a feature (a graph is full of shared nodes), but it needs one rule so that sharing stays safe: **a field behind a spool pointer can be written only when the warp is plain data**, meaning numbers and the like, with no text, lists or cases that own anything. Otherwise it is read-only ([T0107](diag:T0107)).

@sample mem_err_write

Why? Writing `n.name = ...` would drop the old `String` while another copy of the pointer might still read it. Plain data has nothing to drop, so aliased writes can't hurt.

Text can live in a spool too: `.copy()` on a `String` or `str` allocates the copy in the spool the call is given, so an arena's tree can hold names and still go all at once. What a spool can't hold yet is a *collection*: a warp in a spool can't have a list, map, box or stream field ([U0102](diag:U0102)), because nothing would free it when the arena resets. See [Not supported yet](page:not-yet).

@try Delete the `name` field from `Named`, and its argument in `frame.put(Named(0))`. With only a number left, it's plain data and the write is allowed.

## Naming the spool: `^T@s`

How does a function say which spool its result lives in? With a **region**. `^T@s` is a pointer into the spool `s`, and a function can take the spool as a parameter: `fun tree(depth: i32) -> ^Node@sp given sp: Spool` builds in *whichever spool the caller gives* (`tree(4, given: frame)`), or on the heap if the call gives none. Don't worry about `given` yet; the last sections explain it. For now read it as "the caller picks the spool".

@sample mem_spools

- A parameter written `^Node` takes a tree from any spool, or from the heap. That's why `count` works on all three trees.
- A warp that holds pointers (`Node`) is `Node@s` for the spool its nodes are in. The region is a hidden parameter, and you never write it unless you want to.
- With no region, a `^T` in a return type, a field or a `take` parameter means the heap.
- After `frame.reset()` the same arena builds a second tree, and nothing from the first is left.

@try Change `tree(4, given: frame)` to `tree(4)`. Now the tree is on the heap, and everything still runs: `count` and the program don't care. That's the point of leaving the spool to the caller.

@compare
-- Rust
Lifetimes are Rust's regions: `Bump` hands out `&'a` references and the borrow checker proves none outlives it. Loom puts the same guarantee in a pointer's type (`^T@s`), but a region is named by the spool *variable*, so there's no lifetime algebra to learn.
```rust
fn parse<'a>(bump: &'a Bump, n: f64) -> &'a Expr<'a> {
    bump.alloc(Expr::Neg(bump.alloc(Expr::Num(n))))
}
```
@end

## The region checker

Three things can go wrong with a pointer into a spool, and the checker catches each.

**It outlives the spool.** A value that points into a spool can't leave the block the spool ends with ([R0101](diag:R0101)). Here `tmp` ends when `dangling` returns, and the tree it holds would end with it:

@sample mem_err_escape

The return type `^Expr` means a heap pointer, so the compiler offers the fix: take the spool as a `given` and return `^Expr@sp`, as `parse` did.

**It's used after a reset.** `frame.reset()` takes every allocation back, so a pointer into `frame` can't be used after it ([R0102](diag:R0102)). The check goes through loops, so a value from one round used after the next round's reset is caught too.

@sample mem_err_reset

**It's in the wrong spool.** A parameter whose type names `@sp` needs a pointer that is *in* the spool the call gives ([R0104](diag:R0104)):

@sample mem_err_mixed

`x` lives in `a`, and `same` was told `sp` is `b`. Without the check it would have returned a pointer typed as `b`'s that really points into `a`.

@try In `mem_err_reset`, move `frame.reset()` below `count(t)`. The pointer is used before the reset, so it's fine. Or keep the reset where it is and call `tree` again after it, to see a fresh pointer is fine too.

## Pools and shared arenas

Two more spools come with `std.mem`. `Pool[T](n)` holds `n` slots of one type: `put` takes a free slot, `free` gives it back, and each costs a few instructions. It holds plain data only. A freed slot is the next one handed out, which is why a loop that puts and frees never runs the pool dry.

@sample mem_pool

`SharedArena(cap)` is an arena that threads may allocate in, and its pointers may cross to them. A pointer into any *other* spool can't ([R0103](diag:R0103)).

@sample mem_shared

@sample mem_err_thread

`put` on a `SharedArena` takes the VM's lock, so it's slower than a plain `Arena`. The rule keeps a thread from reading memory another thread is about to reset.

@try Change `SharedArena(1024)` in `mem_shared` to `Arena(1024)`. The threads now reach into an arena that isn't shared, and the compiler stops it with R0103 and suggests `SharedArena`.

## `given`: what a function needs

Some values need to reach deep into the call graph: allocators, loggers, a clock, random numbers. Passing them through every function by hand is tedious, and hiding them in globals makes them invisible. `given` sits between the two.

A function says what it needs with a `given` parameter of a weft: `fun welcome(who: str) given g: Greeter`. It isn't in the argument list. The call supplies it from context.

@sample mem_given

- **By type.** The innermost `given` of the right type in scope supplies it. The name (`g`) matters only inside the function.
- **From a block.** `given name = value` holds from there to the end of the block.
- **At one call.** `welcome("eve", given: Plain())` gives one to a single call, and wins over the ones in scope.
- **A default.** A weft marked `#given_default(Type)` has one when nothing is in scope (a spool's default is the heap). A program that never says `given` still runs.
- **Cost.** A function is checked for the type it is given, so an unused `given` costs nothing: no dispatch, no hidden argument.

Every function spells out its own `given`s. A private function doesn't pick them up from what it calls (yet), so a helper that calls `stamp` has to say that it needs a `Clock` too, or the compiler stops it with [T0104](diag:T0104):

@sample mem_err_given

@try Add `given c: Clock` to `log_line`'s signature, right after `(msg: str)`, and run it: `[42] started`. Then move `given c = Fixed(42)` below the `log_line` call in `main` and see T0104 again, because a `given` only reaches the code after it.

@compare
-- Scala
`given` and `using` in Scala 3 are the closest relative: the compiler finds a value of the right type in scope and passes it. Loom's `given` also works for allocators, which is why `^e` can allocate in the spool in scope.
```scala
trait Clock { def now: Long }
def stamp(msg: String)(using c: Clock): String = s"[${c.now}] $msg"

given Clock = FixedClock(42)
println(stamp("started"))
```
-- Odin
Odin's `context` is an implicit parameter of every procedure, carrying an allocator and a logger. It's one fixed struct; Loom lets any weft be given and checks for each use.
```odin
context.allocator = mem.arena_allocator(&arena)
data := make([]int, 100)   // uses context.allocator
```
-- Go
Go passes a `context.Context` as the first parameter of every function that needs one. It's explicit, which is clear, and also the very threading that `given` leaves out.
```go
func stamp(ctx context.Context, msg string) string {
    return fmt.Sprintf("[%d] %s", clockFrom(ctx).Now(), msg)
}
```
@end

## The ambient spool, and `print`

`given` is also how allocation finds its spool. **The ambient spool** is the innermost `given` of type `Spool` in scope, or the heap when there is none. `^e`, `.copy()` and the constructors that allocate all use it. A `given sp = frame` states one for the rest of the block:

@sample mem_ambient

`on_heap` was made before the `given`, so the reset doesn't touch it; `a` and `b` were made after, in the arena, and are gone.

`print` is another `given` in disguise. It writes to whichever `Writer` is given, and standard output is its default, so a program that never mentions it still prints. Give a `Buffer` and the output is captured:

@sample mem_writer

The function `report` has to say `given out: Writer` because it prints; `main` doesn't, because it states its own. This is how tests can check what a program prints without touching the program.

@try In `mem_writer`, add `print("early")` just above `given out = Buffer()`. It prints straight away, because the `given` doesn't reach back before the line it's on.

## Not supported yet

Some of this is designed and doesn't run yet:

- Lists, maps and sets allocated *in* a spool (`List[i32]@frame`), and a warp in a spool that holds one ([U0102](diag:U0102)).
- `given` of private functions inferred from what they call.
- `loom(given: pool)`, and `yield` / `pull` inside a `loom` body.
- User-written spools (`weave Spool`).
- A `for` loop over a projection that lends many places; plain projections work, and the [Shuttles](page:shuttles) chapter covers them.

The full list is in [Not supported yet](page:not-yet).

## Where next

[Gauges](page:gauges) restrict a number to a range, and let the compiler prove things about it.
