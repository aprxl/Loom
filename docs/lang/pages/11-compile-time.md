title: Compile-time code
summary: Constants and expressions computed by ordinary code while the program compiles, `const assert`, and the limits of the sandbox they run in.
---
Some answers are known before a program ever runs: a table of squares, the checksum of a constant, whether a buffer's size is a power of two. You could compute them at startup, every time the program starts, or you could paste the answers in by hand and hope nobody edits the code that made them. Loom has a third way: ask the compiler to run the code and keep the answer.

There's no macro language and no second dialect for this. Code that runs while the program compiles is **ordinary Loom**, and the keyword `const` says where. This chapter covers the first half: constants, `const` expressions, `const assert`, and the sandbox the code runs in. The second half, [Const parameters and control flow](page:const-params), covers what decides which code exists.

## Constants

A constant's initializer runs while the program compiles. It can use loops, locals and any function you've written, so a lookup table is just a function body in braces.

@sample ct_table

- A constant is named in `SCREAMING_CASE` with two or more characters.
- Constants are lazy. Each is computed the first time something needs it, so the order they're written in doesn't matter.
- `const LIMIT = 15` with no type works like a literal: each use gets the type its context needs. `const LIMIT: u32 = 15` has one type everywhere.
- The result can't depend on anything outside the program. Compile-time code is deterministic: the same source always gives the same value.

@try Change the `7` in `SQUARES[7] == 49` to `6`. The build stops with a failed `const assert` before `main` ever starts. Then make the loop in `SQUARES` stop one short, with `0u32..7`, and see which line the compiler blames.

@compare
-- Zig
Zig runs the same ordinary code at compile time, and so does Loom; the difference is that Zig asks you to say where (`comptime`, or a container-level `const`, which is evaluated at compile time anyway). Loom's `const` is that marker.
```zig
const squares: [8]u32 = blk: {
    var table: [8]u32 = undefined;
    for (&table, 0..) |*slot, i| slot.* = @intCast(i * i);
    break :blk table;
};
```
-- Rust
In Rust a `const` initializer may only call `const fn`s, and a `const fn` can't use a `for` loop (iterators are trait calls), so you write `while` loops with a counter by hand.
```rust
const SQUARES: [u32; 8] = {
    let mut table = [0u32; 8];
    let mut i = 0;
    while i < 8 {
        table[i] = (i * i) as u32;
        i += 1;
    }
    table
};
```
-- C++
`constexpr` evaluates the initializer at compile time, and since C++17 a lambda can build the array. A `constexpr` variable of a type like `std::vector` isn't possible, though; Loom's constants can be lists and text.
```cpp
constexpr std::array<std::uint32_t, 8> squares = [] {
    std::array<std::uint32_t, 8> table{};
    for (std::uint32_t i = 0; i < 8; ++i) table[i] = i * i;
    return table;
}();
```
-- D
D's compile-time function execution (CTFE) is the closest match: an initializer is evaluated at compile time whenever it has to be constant, and nothing is marked.
```d
static immutable uint[8] squares = () {
    uint[8] table;
    foreach (uint i; 0 .. 8) table[i] = i * i;
    return table;
}();
```
@end

## No function is marked

Look again at `fib` in the sample. It's an ordinary function, and `main` could call it at run time too. Nothing says "this one may run at compile time"; the compiler runs whatever a constant calls, and checks that code on the way. If a function does something the sandbox doesn't allow (below), the error appears where a constant used it, and names the call.

That is a deliberate difference from Rust's `const fn` and C++'s `constexpr`, which put a mark on the function and then limit what it may contain. In Loom the limit is on the *run*, not on the declaration: a function that prints is perfectly good, until a constant calls it.

> The design documents reserve `const fun` for functions that compute *types*. That isn't built (see [Not supported yet](page:not-yet)), and you never need to write it for the functions in this chapter.

@compare
-- Rust
`const fn` marks a function as callable in constants, and the compiler then restricts its body. Loom marks nothing and restricts the run.
```rust
const fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}
const FIB_20: u64 = fib(20);
```
-- C++
`constexpr` functions *may* run at compile time; `consteval` functions *must*. Loom has neither word: any function runs wherever a constant needs it, and a `const` in front of an expression is the way to insist.
```cpp
constexpr std::uint64_t fib(std::uint64_t n) {
    return n < 2 ? n : fib(n - 1) + fib(n - 2);
}
constexpr auto fib_20 = fib(20);
```
-- D
D works the same way as Loom here: any function that CTFE can handle runs at compile time when a constant needs it. No marking.
```d
ulong fib(ulong n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
enum fib20 = fib(20);
```
-- Zig
Zig functions are unmarked too; `comptime` goes on the call (or the variable), like Loom's `const` in front of an expression.
```zig
fn fib(n: u64) u64 {
    return if (n < 2) n else fib(n - 1) + fib(n - 2);
}
const fib_20 = comptime fib(20);
```
@end

## `const` expressions

A constant is a name. Sometimes you want the computed value without naming it: `const expr` runs `expr` while compiling, once, and every use reads the result. `const { ... }` is the block form, and its locals stay inside it. In a function body, a `const` expression is a constant of its own, so `const fib(20)` in a loop is computed once.

@sample ct_expr

A `const` expression can go anywhere an expression can, which includes default arguments and the default value of a field. A constant can also hold more than numbers: text, a record, a list, an optional or a tuple, as long as what's inside is data.

@sample ct_defaults

@try In `ct_expr`, change `const fib(25)` to `fib(25)`, so it runs when the program does, and run it again. The answer is the same; what changed is when it was worked out. Then try `const fib(100)` and see the budget run out ([K0102](diag:K0102), below).

## Constants are values

A constant is one value, and every use of it gets a value of its own. `var mine = PRIMES` copies the list, `Copy` or not, so changing `mine` leaves `PRIMES` alone. Reads that don't own, such as `PRIMES.len()` or `for x in PRIMES`, share it.

@sample ct_copies

That is why a constant can't hold something that changes as it's used, like a shuttle: see [K0108](diag:K0108) below.

## `const assert`

`const assert(condition, message)` is checked while compiling. It can go at the top of a module, in a warp body, or in a function body. A failed one is [K0101](diag:K0101), and it stops the build. In generic code it's checked once for each instance, and the error names the instance (see [Const parameters](page:const-params)).

The condition can call any function, as `crc32(...)` does in `examples/compile_time.loom`, which checks a known checksum on every build. The message can hold `{...}` holes.

@sample ct_err_assert

A plain `assert(...)` inside compile-time code is a different thing. It unravels like any other code, and the unravel is a compile error ([K0103](diag:K0103), below). Use `const assert` when you mean to check something while compiling. Together with [reflection](page:reflection), it also pins down what the compiler can see about a type: `const assert(Header.size == 8, "the header changed size")`.

@try Change the failing assertion's message to include a hole, `"expected 21, got {double(LIMIT)}"`, and see the number appear in the error.

@compare
-- C++
`static_assert` is the same idea, with the condition limited to a constant expression. Loom's can call any function.
```cpp
static_assert(sizeof(Header) == 8, "the header changed size");
```
-- Rust
Rust has no `static_assert`; the usual spelling is an unnamed constant whose initializer asserts. A failure is a compile error.
```rust
const _: () = assert!(std::mem::size_of::<Header>() == 8, "the header changed size");
```
-- Zig
Zig has no assertion keyword for this: a `comptime` block calls `@compileError`, which is also how a generic function rejects a bad type.
```zig
comptime {
    if (@sizeOf(Header) != 8) @compileError("the header changed size");
}
```
@end

## What ends up in the program

Compile-time work isn't only about checking. The values it computes are written into the program as data, so nothing computes them again at run time. A constant that is a number, text, a tuple, an array, a list, a record or an optional of those (not too big) is data in the program image, or a literal in the Lua bundle. `ct_table`'s `SQUARES` and `FIB_20` become, in the Lua that `loom build --emit lua` writes, two plain assignments:

```text
C.SQUARES_0 = {n = 8, [1] = 0, [2] = 1, [3] = 4, [4] = 9, [5] = 16, [6] = 25, [7] = 36, [8] = 49}
C.FIB_20_2 = 6765
```

A constant of another type (a map, a sum) is computed when it's first read, as it would be anyway. Building the same program twice gives the same bytes.

`loom build`, `run`, `test` and `check` all run the compile-time work, so a broken constant is found by `loom check` too.

## The sandbox

Compile-time code runs on Treadle, Loom's own VM, inside the compiler, whichever backend the program is for. It's a sandbox: it can compute, and it can't reach anything outside the compiler. Every limit is a compile error with a `K` code, and each message says which constant was being computed and where the trouble was.

@compare
-- Rust
Rust splits the work in two. Constant evaluation is a restricted subset (no I/O, no heap allocation) and a panic is a compile error. Anything that needs more goes into a `build.rs`, an ordinary program run before the build, which can read files and call tools. Loom has only the first kind.
```rust
// build.rs: a separate program, free to do anything
fn main() {
    let text = std::fs::read_to_string("table.txt").unwrap();
    println!("cargo:rerun-if-changed=table.txt");
}
```
-- Zig
Zig's `comptime` is also pure computation, with the same restriction of no I/O, and a failing `@compileError` or an overflow stops the build. The exception is `@embedFile`, which Loom's design has (as `embed`) but doesn't build yet.
```zig
const data = @embedFile("table.txt");
```
-- C++
In a constant expression, undefined behavior (overflow, a bad index) is not a constant expression, so it's rejected. That is the same rule as [K0103](diag:K0103). C++20 also lets a `constexpr` function allocate, but only if it frees everything before returning, so no `std::vector` survives into the program.
```cpp
constexpr int bad = [] { int a[2]{}; return a[5]; }();  // error: not a constant expression
```
-- D
D's CTFE can't do I/O either (a string import, `import("file")`, is the one door out, and it needs a compiler flag) and reports an error with a compile-time stack trace for what would unravel at run time.
```d
enum text = import("table.txt");   // needs -J. on the command line
```
@end

### Budget: K0102

All compile-time code shares one budget: 100 million steps (loop iterations and calls), 1 GiB of memory, and 20 seconds. A loop that never ends stops with [K0102](diag:K0102) instead of hanging the compiler.

@sample ct_err_budget

@try Change `% 5` to `% 20`. Now `n` can reach `10`, the loop ends, and the program compiles.

### Unravelling: K0103

If compile-time code unravels (overflow, a bad index, `unravel`, a failed `as`), the constant it was computing fails with [K0103](diag:K0103), and the message says where it unravelled. Things that needed that constant are skipped, so one failure isn't reported many times.

@sample ct_err_unravel

### No I/O: K0104

The sandbox has no way out. Everything in `std.io`, `std.env`, `std.fs`, `std.time` and `std.term` fails with [K0104](diag:K0104), and the note says which call, and where it was called from. Starting a [thread](page:threads) is refused the same way.

@sample ct_err_io

### No locals from outside: K0105

Compile-time code runs before the program does, so a local variable of the surrounding function has no value yet. Constants, functions and the code's own locals are fine.

@sample ct_err_local

@try Change `twice(n)` to `twice(4)` and the error goes away: `4` is known now. Or make the whole function generic over a `const` parameter, which is the next chapter.

### No leaving: K0106

`return`, `break`, `continue`, `?`, `yield` and `pull` can't leave compile-time code, exactly as in a `defer`. Handle the case inside: with `mend`, or an `if`.

@sample ct_err_flow

### No cycles: K0107

A constant that needs its own value, through any number of other constants and functions, is [K0107](diag:K0107).

@sample ct_err_cycle

### A constant can't change: K0108

A constant is one value that every use copies, so it can't hold something that changes as it's used: a shuttle, a stream or a `once` value.

@sample ct_err_holds

### All together

| Code | What happened | What to do |
|------|---------------|------------|
| [K0101](diag:K0101) | a `const assert` was false | fix the code or the assertion |
| [K0102](diag:K0102) | the budget ran out | find the loop that doesn't end, or compute less |
| [K0103](diag:K0103) | the code unravelled | whatever would unravel at run time, fix it |
| [K0104](diag:K0104) | the code called I/O, or started a thread | move the I/O to `main` |
| [K0105](diag:K0105) | the code read a run-time local | pass it as a `const`, or compute it at run time |
| [K0106](diag:K0106) | control flow tried to leave | handle it inside |
| [K0107](diag:K0107) | a constant needs itself | break the cycle |
| [K0108](diag:K0108) | a constant holds something that changes | make it in a function |

## The constant folder

Two things are decided by a small evaluator in the compiler itself, not by the sandbox: what a `const` argument, a `const if` condition or a `const for` sequence is, and array lengths and gauge bounds. It handles literals, constants, `const` parameters, arithmetic, comparisons, `and`/`or`/`not`, an array's `.len()`, a few integer methods, and [reflection](page:reflection). A top-level `const assert` that it can decide is reported without running the sandbox.

When it can't work something out, you get [K0109](diag:K0109) or [K0110](diag:K0110) (see the next chapter), or [U0102](diag:U0102) for an array length or a type. The difference matters in practice: a *constant* can call your functions, but the *length of an array* can't yet. [Not supported yet](page:not-yet) shows it.

## Where next

[Const parameters and control flow](page:const-params) uses compile-time values to specialize functions and choose code. Types computed by functions, `embed` and `bolt.loom` aren't built; [Not supported yet](page:not-yet) lists them.
