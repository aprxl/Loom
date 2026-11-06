title: Const parameters and control flow
summary: `const` parameters that become part of a type, `uint(n)`, `const if`, `const for`, `const unravel` and `target`.
---
[Compile-time code](page:compile-time) computed values and left them in the program. This chapter goes one step further: a value known while compiling can shape the *code* and the *types* around it. An array's length becomes a parameter, a bit width follows a number, a branch is chosen before it exists, and a loop is written out by hand, so to speak, by the compiler.

The same keyword does all of it. A `const` parameter takes a value that's known at compile time and specializes the function or warp for it. `const if` and `const for` decide what code exists at all.

## `const` parameters

A `const` parameter must be known at compile time. Each distinct value makes its own instance of the function, and the value can appear in types: the length in `[f32; n]` is the parameter. A `const` generic parameter (`const N: usize`) can also be worked out from the arguments, here from an array's length.

@sample cp_zeros

- The parameter's type is an integer type or `bool` for now. An argument is checked against it ([T0102](diag:T0102) when it doesn't fit).
- A `const` function parameter (`fun zeros(const n: usize)`) is written as an ordinary argument at the call. Inside the instance, and in compile-time code, it's its value.
- `zeros(4)` and `zeros(5)` are different instances, and `Ring[u32, 4]` and `Ring[u32, 8]` are different types.
- A generic `const N: usize` is usually inferred (`sum([1, 2, 3])`), and can be given: `sum[2]([10, 20])`.

@try Change `sum([1, 2, 3])` to `sum([0; 0])`, an array with no items. The `const assert` is checked for each instance, and the error says `N = 0`.

@compare
-- Rust
Rust's const generics are the nearest cousin. A const parameter is a generic argument (turbofish), not a function argument, and on stable Rust it can't appear in arithmetic inside a type: `[f32; N * 2]` needs an unstable feature. Loom's `[f32; n]`, `uint(N.log2())` and friends are ordinary expressions.
```rust
fn zeros<const N: usize>() -> [f32; N] {
    [0.0; N]
}

fn sum<const N: usize>(xs: [i32; N]) -> i32 {
    xs.iter().sum()
}

let z = zeros::<3>();
```
-- C++
Non-type template parameters are the same mechanism, written in a template header. C++ can compute with them freely; a mistake tends to surface as a trail of template instantiation notes, where Loom shows the `const assert` message you wrote.
```cpp
template <std::size_t N>
std::array<float, N> zeros() { return {}; }

template <std::size_t N>
int sum(const std::array<int, N>& xs) {
    static_assert(N > 0, "summing nothing");
    return std::accumulate(xs.begin(), xs.end(), 0);
}
```
-- Zig
Zig's `comptime` parameters are written like Loom's: a normal parameter with a marker, and the value can be used in the return type.
```zig
fn zeros(comptime n: usize) [n]f32 {
    return [_]f32{0} ** n;
}
```
@end

### What an argument may be

An argument must be something the [constant folder](page:compile-time#the-constant-folder) can work out: literals, constants, other `const` parameters, arithmetic, comparisons, and the integer methods `log2`, `is_power_of_two`, `min`, `max` and `pow`. Anything else is [K0109](diag:K0109). A local variable is the usual culprit, and a `bool` parameter shows it:

@sample cp_err_known

A `const` parameter that isn't an integer or a `bool`, such as a `str` or a `char`, is [T0101](diag:T0101):

@sample cp_err_type

And an argument that doesn't fit the parameter's type is [T0102](diag:T0102), as `300` doesn't fit a `u8`:

@sample cp_err_range

@try In `cp_err_known`, move the `3` out of `main` into `const LIMIT = 3` and call `pick(LIMIT > 2)`. A constant is known while compiling, so the call is accepted.

## Types from numbers

`uint(n)` and `int(n)` are the integer types with `n` bits, for `n` from 1 to 64. `u12` is `uint(12)`. The argument must be known at compile time ([K0109](diag:K0109) if it isn't). Together with `const` parameters, this lets a type follow a number:

@sample cp_types

@try Change `let small: uint(3) = 7` to `= 8`. A `uint(3)` holds 0 to 7, and the compiler refuses the literal.

@compare
-- Zig
Zig has integers of any width (`u12`) and builds one from a number with the standard library. It's the same idea; in Zig the function that makes the type is ordinary code. Loom's design lets a function return a type too, but that isn't built yet.
```zig
const Idx = std.meta.Int(.unsigned, 12);   // u12
```
-- C
C23 added `_BitInt(N)`, a bit-precise integer whose width is a literal in the source. It can't follow a computed number.
```c
unsigned _BitInt(12) idx = 4095;
```
-- Rust
Rust has the fixed widths (`u8`, `u16`, `u32`...) and nothing in between. A 12-bit index is a `u16` plus a convention, or a crate.
```rust
struct Idx(u16); // 0..4096, by convention
```
@end

## Warps with `const` parameters

A generic warp's field types are worked out again for each list of arguments, since `type Idx = uint(N.log2())` can't be resolved in the template. A `const assert` in a warp's body runs for each instance of it, and the error names the instance.

The ring buffer below uses both: its cursors are exactly as wide as its capacity needs, so `+%= 1` wraps at `N` by itself, with no `%` and no bounds check.

@sample cp_ring

An instance that breaks the rule doesn't compile, and the error says which one:

@sample cp_err_assert

@try Change `Ring[u32, 4]` in `cp_ring` to `Ring[u32, 8]`. The cursors become `u3`, and since six items now fit, nothing is dropped. Then try `Ring[u32, 6]` and read the error.

@compare
-- Rust
A const-generic struct is the same shape, but the cursor type can't be computed from `N` on stable Rust, so the usual ring stores `usize` and masks it with `N - 1`, with a check somewhere that `N` is a power of two.
```rust
struct Ring<T, const N: usize> {
    slots: [Option<T>; N],
    head: usize,
    tail: usize,
}
```
-- C++
A class template with a `static_assert` is the equivalent, and the cursor type can be computed with a metafunction, at the price of a helper template.
```cpp
template <class T, std::size_t N>
struct Ring {
    static_assert((N & (N - 1)) == 0, "Ring capacity must be a power of two");
    std::array<std::optional<T>, N> slots{};
    std::size_t head = 0, tail = 0;
};
```
-- Zig
Zig has no generic types as such: a function takes the parameters at compile time and *returns a type*. That is the model Loom's design follows for computed types (not built yet), while `warp Ring[T, const N: usize]` covers the common case.
```zig
fn Ring(comptime T: type, comptime n: usize) type {
    return struct {
        slots: [n]?T = [_]?T{null} ** n,
        head: usize = 0,
        tail: usize = 0,
    };
}
```
@end

## `const if`

`const if cond { ... } else { ... }` is decided while compiling, and the whole `else if` chain with it. The compiler checks, and emits, only the branch it takes. **The others are parsed but never type-checked**, so they can name things that don't exist on this target.

@sample ci_basic

- The condition must be something the constant folder can work out, like a `const` argument: literals, constants, `const` parameters, `target`, arithmetic and comparisons. Otherwise it's [K0110](diag:K0110), or [K0105](diag:K0105) if it reads a local variable.
- It can be an expression. Its branches then have to give a value.
- A condition can use [reflection](page:reflection) too: `const if T.kind == .Record { ... }`.

The skipped branch is where `const if` earns its keep. This function has a fast path and a fallback that calls something that doesn't exist, and it compiles, because only one branch is ever looked at:

@sample ci_skipped

@try Change `FAST` to `false` and run it again. Now the fast branch is skipped, the other one is checked, and the compiler reports the missing name.

@compare
-- C++
`if constexpr` discards a branch, but only inside a template: outside one, the discarded statement is still fully checked. Loom checks only the taken branch everywhere.
```cpp
template <std::size_t N>
constexpr const char* describe() {
    if constexpr (N > 2) return "long";
    else return "short";
}
```
-- Rust
Rust splits it in two. `#[cfg(...)]` removes an item before it's checked (but it must still parse), and `if cfg!(...)` is an ordinary `if`, where *both* branches are checked.
```rust
#[cfg(windows)]
fn separator() -> &'static str { "\\" }
#[cfg(not(windows))]
fn separator() -> &'static str { "/" }
```
-- D
D's `static if` is the closest match: a compile-time condition, and the branch not taken is not compiled.
```d
string describe(size_t N)() {
    static if (N > 2) return "long";
    else return "short";
}
```
-- Zig
Zig doesn't need a keyword: an ordinary `if` on a `comptime`-known condition skips the other branch. Loom spells it out so the reader can see it's decided early.
```zig
fn describe(comptime n: usize) []const u8 {
    if (n > 2) return "long" else return "short";
}
```
@end

### What a condition may be

A `const if` condition is read by the folder, so it can't call a function of yours, and it can't read a local:

@sample ci_err_known

@sample ci_err_fold

### Saying no: `const unravel`

A `const if` chain is also the place to refuse the cases you don't handle. `const unravel("message")` stops the build with [K0103](diag:K0103), with the instance named in a note. If the branch is never taken, it costs nothing.

@sample ci_unravel

@try Change `lanes[64]()` to `lanes[16]()`. Now it takes the second branch, and the program compiles.

@compare
-- Zig
`@compileError` is the same tool, and is how a Zig function rejects a type it doesn't support.
```zig
fn lanes(comptime n: usize) usize {
    if (n <= 4) return 1;
    if (n <= 16) return 2;
    @compileError("no lane width for this size");
}
```
-- Rust
`compile_error!` is a macro that fails the build whenever the compiler sees it, so it is only useful inside a `#[cfg]` that is off; a `const` panic is the closest to Loom's version.
```rust
const fn lanes(n: usize) -> usize {
    if n <= 4 { 1 } else if n <= 16 { 2 } else { panic!("no lane width") }
}
const LANES: usize = lanes(64); // error: evaluation of constant value failed
```
@end

## `const for`

`const for` is unrolled while compiling. Its body is checked once for each item, with the loop variable known at compile time, so each copy can use it in a type (`[0u8; i + 1]` below). It goes over a range, an array literal of numbers, a constant that holds such an array, or `.indexed()` of one of those. The items must be things the folder can work out, so integers and `bool`s; an array of strings is [K0110](diag:K0110).

@sample ci_for

- It can run at most 10 000 times ([K0102](diag:K0102)).
- `break` and `continue` can't target a `const for` yet ([U0102](diag:U0102)).
- Each copy of the body is a block of its own, checked on its own: a `const if` on the loop variable can pick different code for each.

A `const for` over `0..N` turns a loop into straight-line code for each instance:

@sample ci_dot

Past the limit, the compiler says so, and suggests a plain `for`:

@sample ci_err_limit

@try In `ci_for`, change `0..3` to `0..5`. The output grows, and `[0u8; i + 1]` is checked for each of the five lengths.

@compare
-- Zig
`inline for` is the same thing: the loop is unrolled at compile time, and each iteration can have a different type.
```zig
fn dot(comptime N: usize, a: [N]i32, b: [N]i32) i32 {
    var sum: i32 = 0;
    inline for (0..N) |i| sum += a[i] * b[i];
    return sum;
}
```
-- D
`static foreach` unrolls a loop at compile time, and can also generate declarations.
```d
int dot(size_t N)(int[N] a, int[N] b) {
    int sum = 0;
    static foreach (i; 0 .. N) sum += a[i] * b[i];
    return sum;
}
```
-- C++
C++ has no compile-time loop. The idiom is a parameter pack of indices, expanded by a fold expression.
```cpp
template <std::size_t N, std::size_t... I>
constexpr int dot_impl(const std::array<int, N>& a, const std::array<int, N>& b,
                       std::index_sequence<I...>) {
    return ((a[I] * b[I]) + ...);
}
template <std::size_t N>
constexpr int dot(const std::array<int, N>& a, const std::array<int, N>& b) {
    return dot_impl(a, b, std::make_index_sequence<N>{});
}
```
@end

## `target`

`target` is a `std.target.Target`, always in scope. It has `os`, `arch`, `endian` and `pointer_bits`, and is known at compile time, so it works in `const if`. It describes the machine doing the build, and `pointer_bits` is 64 on Treadle (the Lua backend reports 63). See [std.target](std:target).

@sample ci_target

@try Add `print(target.arch)` to `main`, and see which case your machine is.

@compare
-- Rust
Rust asks the same question with `cfg!` or an attribute, or reads the strings in `std::env::consts`.
```rust
let sep = if cfg!(windows) { "\\" } else { "/" };
let os = std::env::consts::OS; // "windows", "linux", ...
```
-- Zig
Zig's `@import("builtin")` has a `target` with the same fields, and a comptime `if` on it drops the other branch.
```zig
const builtin = @import("builtin");
const sep = if (builtin.target.os.tag == .windows) "\\" else "/";
```
-- Go
Go decides at the file level: build tags and `_windows.go` file suffixes pick which files exist, and `runtime.GOOS` is a run-time string.
```go
// separator_windows.go
//go:build windows

package main

const separator = `\`
```
@end

## Where next

[Reflection](page:reflection) asks a type about its fields and cases, and uses `const for` to write code for each one. `std.sys`, `const` parameters that aren't integers or `bool`, and `break` in a `const for` are listed in [Not supported yet](page:not-yet).
