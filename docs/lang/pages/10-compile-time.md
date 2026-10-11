title: Compile-time code
summary: Constants computed by ordinary code, `const` expressions, `const assert`, and the sandbox they run in.
---
Loom has no macro language. Code that runs while the program compiles is ordinary Loom. The keyword `const` marks where: a constant's initializer, an expression, an assertion, a parameter, a condition, a loop. This page covers the first three and the limits. [Const parameters and control flow](page:const-params) covers the rest.

> `const fn` means "compile-time only" in Loom, as in Zig's `comptime`. It is not Rust's `const fn`, which means "may also run at compile time". You don't mark functions: the compiler checks each function when a compile-time expression calls it.

## Constants

A constant's initializer runs while the program compiles. It can use loops, locals and any function, so a lookup table is just code.

@sample ct_table

- A constant is named in `SCREAMING_CASE` with two or more characters.
- Constants are lazy. Each is computed the first time something needs it, so the order they're written in doesn't matter.
- `const LIMIT = 15` with no type works like a literal: each use gets the type its context needs. `const LIMIT: u32 = 15` has one type everywhere.
- The result can't depend on anything outside the program. Compile-time code is deterministic: the same source always gives the same value.

## `const` expressions

`const expr` runs `expr` while compiling, once, and every use reads the result. `const { ... }` is the block form, and its locals stay inside it. A `const` expression can go anywhere an expression can, including default arguments and field defaults. In a function body it's a constant of its own, so `const fib(20)` in a loop is computed once.

@sample ct_expr

## Constants are values

A constant is one value, and every use of it gets a value of its own. `var mine = PRIMES` copies the list, `Copy` or not, so changing `mine` leaves `PRIMES` alone. Reads that don't own, such as `PRIMES.len()` or `for x in PRIMES`, share it.

@sample ct_copies

## `const assert`

`const assert(condition, message)` is checked while compiling. It can go at the top of a module, in a warp body, or in a function body. A failed one is [K0101](diag:K0101), and it stops the build. In generic code it's checked once for each instance, and the error names the instance (see [Const parameters](page:const-params)).

The condition can call any function, as `crc32(...)` in `examples/compile_time.loom` does. The message can hold `{...}` holes.

@sample ct_err_assert

A plain `assert(...)` inside compile-time code is a different thing. It unravels like any other code, and the unravel is a compile error ([K0103](diag:K0103), below). Use `const assert` when you mean to check something while compiling.

## The sandbox and its limits

Compile-time code runs in a sandbox: on Treadle, Loom's own VM, inside the compiler, whichever backend the program is for. Every limit is a compile error with a `K` code.

### Budget: K0102

All compile-time code shares one budget: 100 million steps (loop iterations and calls), 1 GiB of memory, and 20 seconds. A loop that never ends stops with [K0102](diag:K0102) instead of hanging the compiler.

@sample ct_err_budget

### Unravelling: K0103

If compile-time code unravels (overflow, a bad index, `unravel`, a failed `as`), the constant it was computing fails with [K0103](diag:K0103), and the message says where it unravelled. Things that needed that constant are skipped, so one failure isn't reported many times.

@sample ct_err_unravel

### No I/O: K0104

The sandbox has no way out. Everything in `std.io`, `std.env`, `std.fs`, `std.time` and `std.term` fails with [K0104](diag:K0104), and the note says which call, and where it was called from.

@sample ct_err_io

### No locals from outside: K0105

Compile-time code runs before the program does, so a local variable of the surrounding function has no value yet. Constants, functions and the code's own locals are fine.

@sample ct_err_local

### No leaving: K0106

`return`, `break`, `continue`, `?`, `yield` and `pull` can't leave compile-time code, exactly as in a `defer`. Handle the case inside: with `mend`, or an `if`.

@sample ct_err_flow

### No cycles: K0107

A constant that needs its own value, through any number of other constants and functions, is [K0107](diag:K0107).

@sample ct_err_cycle

### A constant can't change: K0108

A constant is one value that every use copies, so it can't hold something that changes as it's used: a shuttle, a stream or a `once` value.

@sample ct_err_holds

### The constant folder

Two things are decided by a small evaluator in the compiler itself, not by the sandbox: what a `const` argument, a `const if` condition or a `const for` sequence is, and array lengths and gauge bounds. It handles literals, constants, `const` parameters, arithmetic, comparisons, `and`/`or`/`not`, an array's `.len()`, a few integer methods, and [reflection](page:reflection). A top-level `const assert` that it can decide is reported without running the sandbox. When it can't work something out, you get [K0109](diag:K0109) or [K0110](diag:K0110) (see the next page), or [U0102](diag:U0102) for an array length or a type.

## How it runs

The compiler builds the "jobs": the constants, `const` expressions and `const assert`s. Only what they reach is built, as a program for Treadle, which it runs in its own process. Then it builds the real program with what the jobs computed written into it: a constant that is a number, text, a tuple, an array, a list, a record or an optional of those (not too big) is data in the program, or a literal in the Lua bundle, and nothing computes it when the program runs. A constant of another type (a map, a sum) is computed when it's first read, as it would be anyway.

`loom build`, `run`, `test` and `check` all run the compile-time jobs.

## Not supported yet

Types computed by `const fn`s are not built ([reflection](page:reflection) is). `embed` and `embed_text` don't exist, and neither does `bolt.loom`. See [Not supported yet](page:not-yet).
