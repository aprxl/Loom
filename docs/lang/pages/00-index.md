title: Welcome to Loom
summary: What Loom is, how this book works, and how to get the most out of its live samples.
kind: preface
---
Loom is a statically typed language with no garbage collector, built around one small idea: **data and behavior are separate threads, and a program is what you get when you weave them together.** A **warp** declares data. A **weft** declares behavior. A **weave** joins the two. There is no inheritance, no class hierarchy to plan up front, and no hidden runtime chasing your objects around.

If you've written Rust, much of Loom will feel familiar: values have one owner, they move, and the compiler checks it. If you've written Go or Python, the surface will feel lighter than Rust's: there are no lifetimes in signatures, construction is just a call, and strings are templates. Loom borrows freely and says so; the **Elsewhere** boxes in each chapter show what the same idea looks like in languages you may already know.

## How this book works

This is a book you can poke at. Every Loom program in it is **live**: click into one, change it, and press **Run** (or `Ctrl`+`Enter`). The real Loom compiler and Treadle, its virtual machine, run in your browser, built for WebAssembly, so what you see is exactly what `loom run` says on your machine. Nothing is sent anywhere.

@sample index_first

@try Change `"weaver"` to your own name and run it again. Then delete the closing `"` and see what the compiler says about it.

Before a page ever reaches you, every sample on it is run by `python docs/gen.py --check`, and its output is compared with what the page shows. If a sample stops behaving the way the book claims, the docs fail to build. So the samples aren't illustrations; they're tests.

A few more things you'll meet along the way:

- **Try it** boxes suggest an edit to make. Break things on purpose: the error messages are part of the language too, and every one has a code you can look up in [Diagnostic codes](page:diagnostics).
- **Elsewhere** boxes compare a feature with Rust, Go, Swift, Python, TypeScript and others. Pick a language once and the book remembers it.
- Your edits are kept in your browser, so you can come back to them. The **Reset** button puts a sample back as it was.
- The thread running down the left side of each page is your place in the chapter. It's dyed as far as you've read.

@compare
-- Rust
Loom's ownership rules are close to Rust's: one owner, moves by default, borrows checked at compile time. What's missing on purpose is the lifetime syntax: a borrow can't outlive the call it's passed to, so signatures never name a lifetime.
```rust
fn longest<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.len() > b.len() { a } else { b }
}
```
-- Go
Like Go, Loom wants code to read plainly and compile fast, and it has no inheritance: behavior attaches to data from the outside, as Go's methods and interfaces do. Unlike Go, there's no garbage collector, errors are values the type system tracks (`T ! E`), and `nil` doesn't exist.
```go
type Shape interface { Area() float64 }
type Rect struct{ W, H float64 }
func (r Rect) Area() float64 { return r.W * r.H }
```
-- Python
Coming from Python, the biggest change is that types are checked before the program runs, and that a value has exactly one owner. The pleasant surprises: string templates work like f-strings, and `for`, slicing and comprehension-style streams feel at home.
```python
name = "weaver"
print(f"hello, {name}! {len(name)} letters")
```
@end

## What works today

The whole language is designed (the design lives in the repository's `README.md` and `design/*.md`), and most of it runs. This book describes the part that runs. Where the two differ, the design says where Loom is going and this book says where it is. Anything the compiler doesn't do yet stops with a `U` code, usually [U0102](diag:U0102), "not implemented yet". It never compiles it wrongly.

| Area | Chapter |
|------|---------|
| Numbers of every width up to 64 bits, overflow operators, casts, text, templates | [Values and control flow](page:values) |
| Functions, lambdas, `match`, twill (bit) patterns | [Values and control flow](page:values) |
| Warps, wefts, weaves, `through`, derived `Show`, `Eq`, `Ord`, `Hash` | [Warps, wefts and weaves](page:types) |
| Generics, `any W`, wefts as parameter types | [Generics and `any`](page:generics) |
| Arrays, lists, slices, maps, sets, lazy streams | [Collections and streams](page:collections) |
| Optionals, results, `?`, `mend`, `fail`, error unions, `unravel` | [Optionals and errors](page:errors) |
| Moves, `mut` and `take` parameters, `once` warps, `defer` | [Moves, `once` and `defer`](page:ownership) |
| Heap pointers, spools and arenas, regions, `given` | [Pointers, spools and `given`](page:memory) |
| Gauges: numbers limited to a range | [Gauges](page:gauges) |
| Shuttles: generators, duplex shuttles, sinks | [Shuttles](page:shuttles) |
| Constants computed by ordinary code, `const assert` | [Compile-time code](page:compile-time) |
| `const` parameters, `const if`, `const for`, `target` | [Const parameters](page:const-params) |
| Reflection, and default methods built from a type's shape | [Reflection](page:reflection) |
| Tests and `loom test` | [Tests](page:tests) |
| Threads on many cores: `loom`, `thread`, `loom for`, `Atomic`, `Mutex`, `Pipe` | [Threads](page:threads) |
| What isn't built yet, said plainly | [Not supported yet](page:not-yet) |

## The standard library

Names like `print` come from `std`. The [standard library](std:) lists every module, type and function. Only the [prelude](std:prelude) is in scope without a `use`: `List`, `Map`, `String`, `Show`, `assert` and friends.

Ready? Turn the page: [Getting started](page:start) writes a first program and shows the `loom` command.
