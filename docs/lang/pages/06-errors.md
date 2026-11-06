title: Optionals and errors
summary: `?T` for a value that may be missing, `T ! E` for work that may fail, and the `?` and `mend` pair that deals with both.
---
Sooner or later a function can't give you what you asked for. Maybe there was nothing to find, maybe the text wasn't a number, maybe the file wasn't there. Loom keeps two kinds of "didn't work" apart, and gives each its own type:

- **Absence** is `?T`: there may be a `T`, or there may be nothing. Nothing went *wrong*; the map simply doesn't have that key.
- **Failure** is `T ! E`: there's a `T`, or an error of type `E` that says what went wrong.

Both are ordinary values, checked by the compiler, and there are no exceptions. One postfix operator, `?`, leaves a function (or a block) when either one comes up empty, and one keyword, `mend`, patches the hole.

| | Syntax |
|---|--------|
| optional | `?T`, with values `some(x)` and `none` |
| result | `T ! E`, with values `ok(v)` and `err(e)`; `fun f() ! E` when there's no success value |
| raise | `fail e` |
| bail out | postfix `?` |
| recover | `expr mend fallback`, `expr mend e => handler` |
| give up | `unravel("message")` |

The last row is a different animal. `unravel` is for *bugs*, the things that should never happen, and it's the only thing in this chapter that unwinds the stack. It gets its own section at the end.

## Absence: `?T`

`?T` is either `some(x)` or `none`. A `T` converts to `?T` on its own, so a function that returns `?i32` can end with a plain number and only write `none` for the empty case. The standard library uses optionals wherever "not there" is an ordinary answer: [`Map.get`](std:map), `List.first`, [`read_line`](std:io) at the end of the input.

@sample errors_optional

You take an optional apart in three ways: `match` with `some(..)` and `none` arms, `is` (which binds the value for the rest of the condition, so `and` can use it), and the methods on `?T`: `map`, `filter`, `zip`, `is_some`, `is_none`, `ok_or` and a few more, all in [std.option](std:option). The `{x:?}` in a template is the debug form, which shows `some(..)` and `none` rather than failing to format.

- Nesting is real: `??i32` is a different type from `?i32`, so `some(none)` and `none` stay distinct. A plain `?i32` converts to `??i32` as `some(..)`.
- A bare `none` has no type of its own. Where the compiler can't tell, it asks for one: `let x: ?i32 = none` ([T0110](diag:T0110)).

@try Change `halve(10)` to `halve(7)` in the `map` line and run it again: every `some` becomes `none`, and nothing crashes. Then ask `find(xs, 9)` for `k > 20` and see the `if` body skipped.

@compare
-- Swift
Swift's `T?` is the same idea, and `if let` is close to Loom's `is some(x)`. Swift spells the empty value `nil` and promotes a plain value to an optional automatically, as Loom does.
```swift
func halve(_ n: Int) -> Int? {
    n % 2 == 0 ? n / 2 : nil
}
if let h = halve(10), h > 4 { print(h) }
```
-- Kotlin
Kotlin's `Int?` is a nullable type rather than a wrapper: `Int??` collapses to `Int?`, and there is no `Some`. In Loom `?T` is a real two-case type, which is why `??T` means something. The surface looks alike all the same: `?.` and `?:` in Kotlin are `?` and `mend` here.
```kotlin
fun halve(n: Int): Int? = if (n % 2 == 0) n / 2 else null
val bigger = halve(10)?.plus(1)
```
-- Rust
Rust's `Option<T>` is the same type under another name. The difference is convenience: Loom converts `T` to `?T` for you, so you never write `Some(x)` just to return a value.
```rust
fn halve(n: i32) -> Option<i32> {
    if n % 2 == 0 { Some(n / 2) } else { None }
}
```
@end

## Leaving early: `?`

Postfix `?` unwraps an optional. If there is a value, you get it. If there isn't, `?` **jumps to the nearest `mend` around it**; and when there is no `mend`, it leaves the function, returning `none`. That one rule covers early return, optional chaining and the try block of other languages, which is why Loom has no separate `?.`: `owners.get(pet)?` followed by a method call is already a chain.

@sample errors_question

Here each `?` stops `zip_of` at the first lookup that comes up empty. The function's own type, `?u32`, is what makes that legal: `?` only leaves a function that can say "nothing" or "failed" back to its caller.

@sample errors_err_question

- `?` on a result (`T ! E`) works the same way, and then it *propagates the error*: it leaves with `err(e)`. You'll see that two sections down.
- A `?` has to sit inside the **left** side of the `mend` it jumps to. It can cross loops and blocks, but not a lambda or a shuttle, because those are separate functions.
- A `?` that finds no `mend` and sits in a function that can't fail is [T0101](diag:T0101).

@try In `errors_question`, change the return type of `zip_of` from `?u32` to `u32`. The `?`s are now in a function that can't hand back `none`, and the compiler says so.

@compare
-- Rust
Loom's `?` is Rust's `?` with one extra target. In Rust `?` always returns from the function (or the closure, or the unstable `try` block); in Loom it first looks for a `mend`. The `Option` and `Result` cases are one operator here, and the same function can use `?` on both when its error type has room for `Missing`.
```rust
fn zip_of(pet: &str, owners: &HashMap<&str, &str>, zips: &HashMap<&str, u32>) -> Option<u32> {
    let owner = owners.get(pet)?;
    zips.get(owner).copied()
}
```
-- Swift
Swift writes optional chaining as `?.` and error propagation as `try`; the two never share a symbol. Loom has one operator for both, and a call after `?` is already a chain.
```swift
let zip = owners[pet].flatMap { zips[$0] }
let city = user?.address?.city ?? "unknown"
```
-- Zig
Zig's `try x` is the closest relative: unwrap, or return the error from the function. Zig keeps optionals (`orelse`) and error unions (`catch`) apart; Loom lets one `mend` handle both.
```zig
fn load(path: []const u8) !Config {
    const text = try readFile(path);
    return parse(text);
}
```
@end

## Recovering: `mend`

`expr mend fallback` evaluates `expr`; if it came up empty or failed, the result is `fallback` instead. The name is a weaver's: you've found a dropped thread and tied it back in. A `mend` works on optionals and results alike, and takes a few shapes:

@sample errors_mend

- **A value.** `s.parse[u16]() mend 8080`.
- **A chain.** `a() mend b() mend 8080` reads left to right, and the first that works wins.
- **A handler.** `mend e => { ... }` runs a block that sees the error. `e` has the type of everything that could reach the `mend`: if a `?` on an optional can reach it, `Missing` is part of that type.
- **A way out.** If the right-hand side can't produce a value (`continue`, `break`, `return`, `fail`, `unravel`), that's fine: the `mend` never returns normally. `mend unravel("...")` is the `expect` of other languages.
- A block on the left makes a try block: `{ let a = f()?; let b = g()?; a + b } mend 0`. The `?`s jump to the `mend` after the brace.
- **Precedence.** `mend` binds more loosely than any other binary operator. In arithmetic, parenthesize it: `(counts.get(k) mend 0) + 1`.
- Whatever the left side would have produced, the right side must have the same type, unless it diverges.

@try Remove the parentheses around `counts.get("a") mend 0` in the arithmetic line. The `+ 1` is now part of the fallback, so the key that *is* there prints 2 instead of 3.

@compare
-- Kotlin
Kotlin's elvis operator `?:` covers the fallback, and `try`/`catch` is an *expression* there too. Loom has one construct for both: `mend` after a value is `?:`, and `mend e => ...` after a block is `catch`.
```kotlin
val port = text.toIntOrNull() ?: 8080
val sum = try { a() + b() } catch (e: Exception) { 0 }
```
-- Swift
Swift's `??` supplies a fallback for an optional, and failing calls need `do`/`catch` (or `try?`, which turns an error into `nil`). `mend` is `??` and `try?` and `catch` in one.
```swift
let port = Int(text) ?? 8080
let n = (try? parse(text)) ?? 0
```
-- Rust
Rust spells the fallbacks as methods: `unwrap_or`, `unwrap_or_else`, `expect`. `mend` is the operator version, and because the handler is just an expression it can `continue` or `return` without a closure getting in the way.
```rust
let port = text.parse::<u16>().unwrap_or(8080);
let n = text.parse::<i32>().unwrap_or_else(|e| {
    eprintln!("could not parse: {e}");
    -1
});
```
@end

## Failure: `T ! E`

A function that can fail says so in its return type: `-> u8 ! AgeError` returns a `u8` or an `AgeError`. Its values are `ok(v)` and `err(e)`, and a plain `u8` converts to `ok(v)` as a plain value converts to `some(v)`. A function with no success value drops the `-> T`: `fun greet(age: i32) ! AgeError`.

An error is a warp that weaves `Error`, usually with a few cases. `Error` gives it a `message` that shows the case and its payload, so you write no code to print one. `fail e` leaves the function with an error, and the leading dot picks the case from the declared error type: `fail .Negative`.

@sample errors_basic

Results have methods much as optionals do (`map`, `map_err`, `value`, `error`, `is_ok`; see [std.error](std:error)), and `match` takes one apart with patterns inside `err(..)`.

- `err(e)` builds an error value without leaving the function; `fail e` builds it *and* leaves.
- `fail` always leaves the function, even from inside the left of a `mend`. Only `?` jumps to a `mend`.
- `age as? u8 mend fail .TooOld(age: age)` is a common shape: try a conversion that may not fit, and turn a miss into your own error.
- Inside the function, `?` on a result with the same error type passes the error straight out. The next section is about what happens when the types differ.

@try Change the `err(e)` arm to `err(.TooOld(a))` and print `a`. Then delete the `err(.Negative)` arm and see that the `match` still compiles: the last arm catches the rest.

@compare
-- Go
Go returns the error beside the value and leaves checking it to you, which is clear and a little repetitive. In Loom the result is one value, you can't use the success side without handling the error side, and `?` replaces the `if err != nil` block.
```go
n, err := strconv.Atoi(text)
if err != nil {
    return 0, err
}
```
-- Rust
`T ! E` is `Result<T, E>`, `ok` and `err` are `Ok` and `Err`, and `fail e` is `return Err(e)`. The difference is mostly spelling and conversions, which the next section covers.
```rust
fn check(age: i32) -> Result<u8, AgeError> {
    if age < 0 { return Err(AgeError::Negative) }
    u8::try_from(age).map_err(|_| AgeError::TooOld(age))
}
```
-- Java
Java's checked exceptions put the possible errors in the signature, like `T ! E`, but a thrown exception unwinds the stack and can be caught far away. A Loom error is a returned value: you see it at the call, and nothing unwinds.
```java
static int check(int age) throws AgeException {
    if (age < 0) throw new AgeException("negative");
    return age;
}
```
@end

## Converting errors

When a `?` leaves a function, the error has to fit the function's declared error type. If the types differ, the compiler converts for you where it can. The standard way is a **`from` case**: `case Parse(from ParseError)` says "this case absorbs a `ParseError`", and a `?` on a `ParseError` wraps it as `.Parse(...)`. Give at most one case per absorbed type, so it's clear where an error goes.

@sample errors_from

The output shows the wrapping: `error: Parse(Empty)` is an `AppError.Parse` holding a `ParseError.Empty`. This replaces the `From` impls and the `#[from]` attributes of other languages with one word in the case.

@try Change `load` to return `u8 ! ParseError`. The `?` on `small(n)` now has a `RangeError` and nowhere to put it: [T0108](diag:T0108) says the error can't become a `ParseError`, and suggests two ways out, widening the type or adding a `from` case.

@compare
-- Rust
In Rust you write `impl From<ParseError> for AppError` (or `#[from]` with the `thiserror` crate) so that `?` can convert. In Loom the conversion is declared once, in the case that carries it.
```rust
#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("parse")] Parse(#[from] ParseError),
    #[error("range")] Range(#[from] RangeError),
}
```
-- Zig
Zig infers error sets: a function lists nothing and the compiler works out the union of everything its `try`s can return. Loom asks you to say it. (A `! _` that infers the set is designed but [not supported yet](page:not-yet).)
```zig
fn load(text: []const u8) !u8 {
    const n = try parse(text);
    return try small(n);
}
```
@end

## Several error types

Sometimes the right answer is not to wrap but to *list*. `A | B` is a **union type**, mostly used for errors: each member converts into the union, and a `match` takes the members apart with type patterns (`err(e: IoProblem)`). `any Error` is wider still: it accepts an error of any type, which is handy in `main` and scripts where you only want to report it.

@sample errors_union

- A `?` inside a function declared `! (A | B)` accepts an error of either member.
- A union of errors doesn't convert to `any Error` yet: `?` on a `! (A | B)` inside a `! any Error` function is [T0108](diag:T0108). Name the union itself as the function's error type instead.

@try Narrow `load` to `! IoProblem` and run it. The `?` on `decode` has a `BadText` that no longer fits ([T0108](diag:T0108)), and the compiler suggests widening the union.

@compare
-- Java
A Java `throws IoException, ParseException` is a union too, and so is `catch (A | B e)`. The difference is that Loom's union is a plain return type, so it sits in a signature, a variable or a `match` like any other.
```java
int load(String path) throws IOException, ParseException { /* ... */ }
```
-- Swift
Swift has typed `throws` (`throws(ParseError)`) and the untyped `any Error`. Loom has both too, but `any Error` is a type like any other, not an escape hatch from the checker.
```swift
func run(_ path: String) throws {   // throws any Error
    let v = try decode(read(path))
}
```
@end

## `main` can fail

`fun main() ! E` lets `?` leave `main`. An error that reaches the end is printed as `error: ...` on standard error, and the program exits with status 1. That makes a small script as honest as a library: no `try`/`catch` at the top, just a type.

@sample errors_main

The "starting" line is printed; the second print never is. Status 1 means "`main` returned an error"; see the exit codes in [Getting started](page:start#the-loom-command).

## Giving up: `unravel`

`unravel(message)` is for bugs, not for expected failures. Its type is `never`, because it doesn't return. It **unwinds the current thread**, running each `defer` on the way out, and the program then exits with status 101. Arithmetic overflow, a failed `as`, an out-of-range index and a failed `assert` all unravel.

@sample errors_unravel

`assert(condition, message)` unravels unless the condition holds. In [compile-time code](page:compile-time) a failed assertion is a compile error instead.

Expected failures, like a missing file or a number typed wrong, belong in `T ! E`. A caller can decide what to do about them. An unravel is the program saying it has reached a state it was written never to reach.

@try Replace the call to `unravel` with `assert(1 + 1 == 3, "arithmetic is broken")`. The report changes, the `defer`s still run, and the exit status is still 101.

@compare
-- Rust
`unravel` is Rust's `panic!`: it unwinds, runs destructors (Loom's `defer`s) and ends with a failure status. Rust's `Result` and Loom's `T ! E` are for what you expect to go wrong.
```rust
fn inner() {
    let _guard = Guard;           // dropped while unwinding
    panic!("something that should never happen");
}
```
-- Go
Go's `panic` also runs deferred calls on the way out. Go programs sometimes `recover` from a panic to keep a server up; Loom's unravel is a report that ends the thread, and the exit code (101) tells a script it happened.
```go
func inner() {
    defer fmt.Println("inner cleanup")
    panic("something that should never happen")
}
```
@end

## Where next

[Moves, `once` and `defer`](page:ownership) is about who owns a value and what runs when a scope ends. That's where `defer` lives, the thing that keeps cleanup honest on all of the exits you just met.
