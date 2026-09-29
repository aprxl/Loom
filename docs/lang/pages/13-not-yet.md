title: Not supported yet
summary: What the compiler doesn't do, said plainly.
---
The design in `README.md` and `design/*.md` is bigger than what runs. This is the list of what doesn't run, from `design/lua-backend.md` and from trying it.

Anything not built stops with a `U` code, and most stop with [U0102](diag:U0102), "not implemented yet". The compiler doesn't guess and doesn't miscompile.

## Next in line

These are planned next, in this order: reflection, threads, spools.

### Reflection

`T.name`, `T.kind`, `T.fields`, `T.cases`, `T.weaves(W)`, `v.(f)` and `v is .(c)`. Reflection is what will let `Show`, `Eq`, `Ord` and `Hash` be ordinary default methods that inspect the type. Until then, std's defaults for them, and `Error.message`, are built into the compiler.

@sample nyet_reflect

### Threads

`loom { }` blocks, `thread`, `loom for`, `.join()`, and the types for sharing between threads, `Atomic[T]` and `Mutex[T]`. The two types are declared in [std.sync](std:sync) and can't be built yet.

@sample nyet_threads

### Spools

`spool` bindings, `Arena`, `^T` allocation, region pointers (`^T@s`), and the region checker that goes with them. [std.mem](std:mem) declares `Arena` and `Spool`.

@sample nyet_spools

## Compile-time code

The standard library's `std.sys` and reflection are not built:

@sample nyet_sys

Array lengths and gauge bounds must be something the compiler's own evaluator can fold. A call to one of your functions isn't. It's [U0102](diag:U0102) for an array length and [T0103](diag:T0103) for a gauge bound.

@sample nyet_types

Also missing:

- `const fn`s that return types (`const fn fixed(...) -> type`), and anonymous `warp { ... }` types ([U0101](diag:U0101)).
- `break` and `continue` out of a `const for`.
- `const` parameters that aren't integers or `bool`.
- `embed` and `embed_text`, and the `bolt.loom` manifest.

## Memory and safety

- `given` parameters, `ref warp`, and projections (`~> mut T`, the design's way to lend a place).
- `unsafe` blocks, raw pointers (`*T`), `extern "C"`, `Volatile`. `#layout(...)` is parsed but nothing acts on it.
- User-written `Spool` weaves.
- The region checker. A stored lambda simply works, without the escape check the design describes.

## Types and library

- Integers wider than 64 bits and `f16`.
- Generic wefts (`weft From[T]`).
- `xs.sorted_by(...)` and other list methods the design mentions that std doesn't declare. [std.list](std:list) and [std.slice](std:slice) list what exists.
- Reading standard input. [std.io](std:io) has `print` and nothing else.
- A `mut self` method of a scalar type can't be called through `any` yet, because the `any` holds its own copy.
- A union of errors doesn't convert to `any Error`.
- A warp that writes its own `eq` and `hash` is still keyed structurally in a `Map` or `Set`.

## Running programs

- Ctrl-C handling (exit status 130), `process.exit`, an instruction budget for `loom run`, and running a bundle in an outside Lua (`--lua PATH`).
- Async I/O and composing shuttles (`lexer().feed(parser())`) aren't in the design for this version.
- The emitted Lua isn't optimized: checks the compiler could prove unneeded are still emitted. Speed comes later, with Loom's own interpreter.

## The examples

`examples/tour.loom`, `examples/widgets.loom`, `examples/ledger.loom` and `examples/chip8.loom` compile and run. `examples/lexer.loom` fails only at one line, the `lexer.Frame.size` assert, which needs reflection. `examples/compile_time.loom` fails only at reflection and `std.sys`. `examples/calc.loom` needs spools, `given` and `ref warp`, and `examples/parallel.loom` needs threads.
