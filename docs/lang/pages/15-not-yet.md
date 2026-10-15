title: Not supported yet
summary: What the compiler doesn't do, said plainly.
---
The design in `README.md` and `design/*.md` is bigger than what runs. This is the list of what doesn't run, from `design/lua-backend.md` and from trying it.

Anything not built stops with a `U` code, and most stop with [U0102](diag:U0102), "not implemented yet". The compiler doesn't guess and doesn't miscompile.

## Next in line

These are planned next, in this order: the rest of threads, then spools and the rest of Loom's own VM, Treadle (designed in `design/vm.md`). Treadle already runs everything: `loom run --backend vm file.loom`. A program over one of its limits (a frame of more than 65 535 registers, say) stops with [U0103](diag:U0103).

### Threads

[Threads](page:threads) run, one at a time: `loom { }`, `thread`, `.join()` and `sched_yield()`. Not yet: several worker threads at once, `loom(given: pool)`, and `yield` or `pull` inside a `loom` block ([U0102](diag:U0102)).

@sample nyet_threads

### Spools

`spool` bindings, `Arena`, `^T` allocation, region pointers (`^T@s`), and the region checker that goes with them. [std.mem](std:mem) declares `Arena` and `Spool`.

@sample nyet_spools

## Compile-time code

The standard library's `std.sys` is not built, and neither are `T.bits` (which needs `#layout(bits)`), `f.ty` or `T.weaves(W)`:

@sample nyet_sys

Array lengths and gauge bounds must be something the compiler's own evaluator can fold. A call to one of your functions isn't. It's [U0102](diag:U0102) for an array length and [T0103](diag:T0103) for a gauge bound.

@sample nyet_types

Also missing:

- `const fn`s that return types (`const fn fixed(...) -> type`), and anonymous `warp { ... }` types ([U0101](diag:U0101)).
- `break` and `continue` out of a `const for`.
- `const` parameters that aren't integers or `bool`.
- `embed` and `embed_text`, and the `bolt.loom` manifest.

## Memory and safety

- `given` parameters, `ref warp`, a `for` loop over a projection that lends many places, and `xs[i]` through an `Index` weave.
- `unsafe` blocks, raw pointers (`*T`), `extern "C"`, `Volatile`. `#layout(...)` is parsed but nothing acts on it.
- User-written `Spool` weaves.
- The region checker. Lambdas, slices and streams can't outlive the locals they borrow ([M0106](diag:M0106)), but nothing checks region pointers yet.

## Types and library

- Integers wider than 64 bits and `f16`.
- Generic wefts (`weft From[T]`).
- `xs.sorted_by(...)` on a list or an array (it takes a stream: `xs.take(n).sorted_by(...)`, or `map.pairs().sorted_by(...)`), and other list methods the design mentions that std doesn't declare. [std.list](std:list) and [std.slice](std:slice) list what exists.
- Reading standard input. [std.io](std:io) has `print` and nothing else.
- A `mut self` method of a scalar type can't be called through `any` yet, because the `any` holds its own copy.
- A union of errors doesn't convert to `any Error`.
- A warp that writes its own `eq` and `hash` is still keyed structurally in a `Map` or `Set`.

## Running programs

- Ctrl-C handling (exit status 130), `process.exit`, an instruction budget for `loom run`, and running a bundle in an outside Lua (`--lua PATH`).
- Async I/O and composing shuttles (`lexer().feed(parser())`) aren't in the design for this version.
- The emitted Lua isn't optimized: checks the compiler could prove unneeded are still emitted. Speed comes later, with Loom's own interpreter.

## The examples

`examples/tour.loom`, `examples/widgets.loom`, `examples/ledger.loom`, `examples/lexer.loom` and `examples/chip8.loom` compile and run. `examples/compile_time.loom` fails only at `std.sys`. `examples/calc.loom` needs spools, `given` and `ref warp`, and `examples/parallel.loom` needs threads.
