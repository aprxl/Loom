title: Not supported yet
summary: What the compiler doesn't do, said plainly, chapter by chapter.
kind: appendix
---
The design in `README.md` and `design/*.md` is bigger than what runs. This is the list of what doesn't, checked by trying each one against the compiler, and grouped by the chapter it belongs to. If something isn't here and doesn't work, that's a bug in this list: tell us.

The rule behind it: **anything not built stops the build with a `U` code, and never compiles wrongly.** Most stop with [U0102](diag:U0102), "not implemented yet", and the message names the feature. A program that goes over one of Treadle's own limits stops with [U0103](diag:U0103).

Two things that earlier versions of this page listed work now, and are worth knowing: several worker threads (see [Threads](page:threads)), and the Treadle VM, which is what `loom run` uses; `--backend lua` selects the older Lua backend. Reading standard input (`std.io.read_line`, `read_all`, `lines`), `process.exit` and Ctrl-C handling work too.

## Compile-time code

These belong to [Compile-time code](page:compile-time), [Const parameters and control flow](page:const-params) and [Reflection](page:reflection).

The standard library's `std.sys`, the platform's own APIs, is not built:

@sample nyet_sys

Array lengths and gauge bounds must be something the compiler's own evaluator can fold. A call to one of your functions isn't. It's [U0102](diag:U0102) for an array length and [T0103](diag:T0103) for a gauge bound. (A *constant* may call your functions; it's the type that can't wait for them.)

@sample nyet_types

Also missing:

- **Types computed by functions.** `const fun fixed(...) -> type` is [U0102](diag:U0102), and so are anonymous `warp { ... }` types. `uint(n)` and `int(n)` are the built-in cases. The keyword `const fun` itself parses, but today it doesn't change what the function may do: the "compile-time only" meaning in `design/06-compile-time.md` isn't enforced.
- **`break` and `continue` in a `const for`** ([U0102](diag:U0102)). A `const for` also goes only over numbers and `bool`s the folder can work out, not over strings ([K0110](diag:K0110)).
- **`const` parameters that aren't integers or `bool`**: a `str`, a `char` or a warp is [T0101](diag:T0101).
- **`embed`, `embed_text` and the `bolt.loom` manifest.** There's no way to bring a file into a constant yet.
- **Reflection:** `T.bits` (which needs `#layout(bits)`), `f.ty` and `T.weaves(W)`.
- **Derives written in Loom.** `Show`, `Eq`, `Ord` and `Hash` for warps are still built into the compiler instead of being default methods that reflect; [Reflection](page:reflection) shows how you'd write your own.
- **`#layout(...)`** is parsed, and the compiler reports it as not implemented ([U0102](diag:U0102)).

## Tests

For [Tests](page:tests): `assert` is the only check. There is no `assert_eq`, no way to select one test by name, no setup and teardown, no way to mark a test as expected to fail, and no way to assert that code unravels.

## Threads

For [Threads](page:threads), the design is built apart from these:

- **I/O is not a cancel point.** A thread that is inside a file or terminal call isn't interrupted when its loom fails; it finishes the call first.
- **Async I/O** and an event loop (`std.reel`) aren't in this version. A [shuttle](page:shuttles) can already suspend, but nothing yet drives shuttles that wait on I/O.
- **Pools of your own.** `loom(given: Crew(workers: n))` limits how many workers a loom uses; there isn't a way to make a pool with its own queue.
- **A `Pipe` of a `once` type** ([U0102](diag:U0102)), and an `Atomic` of anything but integers of 8 to 64 bits and `bool`.
- **Composing shuttles** (`lexer().feed(parser())`) isn't built.
- `loom for` hands out runs of items only for exclusive integer ranges, lists, slices, arrays and their chunks, at a fixed grain. There is no adaptive grain.

## Memory and safety

For [Pointers, spools and `given`](page:memory):

- **`unsafe` blocks, raw pointers (`*T`), `extern "C"` calls and `Volatile`.** `unsafe` and `*T` are [U0102](diag:U0102); an `extern "C"` function can be declared but not called, on either backend; `Volatile` doesn't exist.
- **Spools:** a list, a map or a box can't be held by a warp that lives in a spool ([U0102](diag:U0102)), and `Deque` and `SortedMap` nodes can't be in one either. You can't write a spool of your own: `spool s = MySpool()` for a type that weaves `Spool` is [T0101](diag:T0101).
- **`given`:** every function spells its own `given`s; a private function doesn't infer them from what it calls.
- **Treadle images:** `loom build --image` writes a file Treadle can run, but the loader doesn't verify one well enough to trust a file from someone else. Run only images you built.

## Types and library

- **Integers wider than 64 bits and `f16`**, **generic wefts** (`weft From[T]`), **generic type aliases**, **slice patterns**, **inferred error sets** (`! _`), **explicit type arguments on a function used as a value** (`let f = id[i32]`), and **`main` with parameters** (it takes none; read `env.args()`):

@sample nyet_limits

- A `mut self` method of a scalar type can't be called through `any`, because the `any` holds its own copy ([U0102](diag:U0102)).
- A union of errors doesn't convert to `any Error`: `?` on an `A | B` in a function that returns `! any Error` is [T0108](diag:T0108), and the help says to widen the function's errors.
- A warp that writes its own `eq` and `hash` is still keyed structurally in a `Map` or `Set`: two keys your `eq` calls equal are two entries.
- A `Fmt` with specs, padding and a debug builder, `Default` for arrays and big tuples, `BitSet`, `std.json`, `process.run`, buffered readers and a few number methods are on the library's to-do list. [The standard library](std:) shows what exists.
- `for` over a range of `char`s (`'a'..'e'`) runs on the Lua backend and not on Treadle ([U0102](diag:U0102)).

## The Lua backend

`--backend lua` runs the same language on Lua 5.4. It is kept correct, and it is what the fixtures compare Treadle against, but it isn't optimized: checks the compiler could prove unneeded are still emitted. Its `usize` is 63 bits (Treadle's is 64), and `target.pointer_bits` says so. Programs that run threads run them as coroutines, one at a time.

## The examples

`examples/tour.loom`, `widgets.loom`, `ledger.loom`, `lexer.loom`, `chip8.loom`, `calc.loom` and `parallel.loom` compile and run. `examples/compile_time.loom` fails only at `std.sys`.
