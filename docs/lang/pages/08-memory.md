title: Pointers, spools and `given`
summary: Boxes on the heap, arenas that free everything at once, and values passed down implicitly.
---
## Pointers

`^e` puts a value in a box on the heap and gives a pointer to it, written `^T`. A pointer owns its box: it moves like any non-`Copy` value, and dropping it drops the value and frees the box. That is how a type holds itself (`^Tree` inside `Tree`). `p^` reads the box. Fields, methods, patterns and `{p}` see through a pointer, so `t.value` and `match t { .Node(l, x, r) => ... }` work on a `^Tree`. A pointer in a `var` can be written through (`p.x = 1`, `p^ += 1`).

@sample own_pointers

A `take` pointer that a `match` takes apart hands its parts to the arms by value, so the arms can move them into a new node.

## Spools

A pointer into the heap owns its box and frees it. When many values live and die together (the nodes of a tree, a frame of a game) a **spool** is cheaper: `spool frame = Arena(1.kib())` makes one, `frame.put(v)` moves a value into it, and `^e` allocates in the spool a function is `given`. A pointer into a spool is `Copy` and owns nothing; `frame.reset()` takes every allocation back at once, and the spool ends with its scope.

`^T@s` is a pointer into the spool `s`, and `fun tree() -> ^Node@sp given sp: Spool` is a function that builds in whichever spool the caller gives (`tree(4, given: frame)`), or the heap if the call gives none. A parameter written `^Node` takes a tree from any spool. A warp that holds pointers (`Node`) is `Node@s` for the spool its nodes are in.

@sample own_spools

The region checker keeps pointers from outliving their spool: a value that points into a spool can't be returned out of the block the spool ends with ([R0101](diag:R0101)), used after the spool was reset ([R0102](diag:R0102)), or passed where a parameter names another spool ([R0104](diag:R0104)). A spool can't be moved ([M0107](diag:M0107)).

@sample own_err_region

Text copied with `.copy()` goes in the spool the call is given, so an arena's tree can hold names and still go at once. A list, map or box can't be held by a warp that is in a spool yet ([U0102](diag:U0102)): nothing would free it.

A pointer into a spool is `Copy`, so two copies see the same value: a field behind one can be written only when the warp is plain data (no text, lists or cases that own anything); otherwise it is read-only ([T0107](diag:T0107)).

`Pool[T](n)` is a spool of `n` slots of one type: `put` takes a free slot and `free` gives it back, a few instructions each. It holds plain data only. `SharedArena(cap)` is an arena threads may allocate in, and its pointers may cross to them; a pointer into any other spool can't ([R0103](diag:R0103)).

## `given`

A function can say what it needs from its caller with a `given` parameter of a weft: `fun welcome(who: str) given g: Greeter`. It is passed on by the call, not in the argument list: the innermost `given` in scope supplies it, `given name = value` states one from there to the end of the block, and `given: x` gives one to a single call. A weft marked `#given_default(Type)` has one when nothing is in scope (a spool's is the heap), so a program that never says `given` still runs. A function is checked for the type it is given, so an unused `given` costs nothing. Every function spells its own `given`s: a private one doesn't infer them from what it calls yet.

@sample own_given

## Not supported yet

Projections (`~> mut T`), which lend a place, work: see [Shuttles](page:shuttles). See [Not supported yet](page:not-yet).

## Where next

[Gauges](page:gauges) restrict a number to a range.
