title: Overview
summary: What the Loom compiler runs today, and what it doesn't.
---
Loom is a statically typed language with no garbage collector. Data and behavior are separate threads, and you weave them together: a **warp** declares data, a **weft** declares behavior, and a **weave** joins the two.

The compiler is written in Rust. It checks a program, erases everything the type system proved, and emits Lua 5.4, which a bundled VM runs. The whole language is designed, but only part of it works yet. These pages describe the part that does.

> Every code sample on these pages is a real file in `docs/lang/samples/`. `python docs/gen.py --check` runs each one and compares what happens with what the page says. If a sample doesn't behave as shown, the check fails.

The design itself is in the repository: `README.md` and `design/*.md`. Where the two differ, the design says where Loom is going and these pages say where it is.

## What works today

| Area | State | Page |
|------|-------|------|
| Numbers of every width up to 64 bits, overflow operators, casts | works | [Values and control flow](page:values) |
| Strings, templates, characters, `parse` | works | [Values and control flow](page:values) |
| Functions, lambdas, `match`, twill patterns | works | [Values and control flow](page:values) |
| Warps: records, cases, methods, the leading dot, dot-blocks | works | [Warps, wefts and weaves](page:types) |
| Wefts, weaves, `through`, derived `Show`, `Eq`, `Ord`, `Hash` | works | [Warps, wefts and weaves](page:types) |
| Generics, `any W`, wefts as parameter types | works | [Generics and `any`](page:generics) |
| Arrays, lists, slices, maps, sets, lazy streams | works | [Collections and streams](page:collections) |
| Optionals, results, `?`, `mend`, `fail`, error unions, `unravel` | works | [Optionals and errors](page:errors) |
| Moves, `mut` and `take` parameters, `once` warps, `defer` | works | [Moves, `once` and `defer`](page:ownership) |
| Gauges: number types limited to a range | works | [Gauges](page:gauges) |
| Shuttles: generators, duplex shuttles, sinks | works | [Shuttles](page:shuttles) |
| Constants computed by ordinary code, `const` expressions, `const assert` | works | [Compile-time code](page:compile-time) |
| `const` parameters, `const if`, `const for`, `target` | works | [Const parameters and control flow](page:const-params) |
| Reflection, and default methods that derive from a type's shape | works | [Reflection](page:reflection) |
| Tests and `loom test` | works | [Tests](page:tests) |
| Threads: `loom`, `thread`, `join`, `sched_yield`, `loom for`, `chunks_mut`, `Atomic`, `Mutex`, `Pipe`, cancel and failure, one at a time | works | [Threads](page:threads) |
| several worker threads at once | not yet | [Not supported yet](page:not-yet) |
| Spools, region pointers, `given`, `ref warp` | not yet | [Not supported yet](page:not-yet) |
| `unsafe`, raw pointers, `extern "C"`, `std.sys`, reading standard input | not yet | [Not supported yet](page:not-yet) |

## How to read a sample

Each sample shows its source, a tag saying what happens to it, and what it prints or what the compiler reports. A sample that fails to compile shows the exact diagnostic, and the tag links the code to the [diagnostic codes](page:diagnostics) table.

Anything the compiler doesn't do yet stops with a `U` code (usually [U0102](diag:U0102), "not implemented yet"). It never compiles it wrongly.

## The standard library

Names like `print` come from `std`. The [standard library spec](std:) lists every module and every item. Only the [prelude](std:prelude) is in scope without a `use`.
