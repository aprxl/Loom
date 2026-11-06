title: Getting started
summary: A first program, the shape of a Loom file, and the `loom` command that runs it.
---
Every journey through a loom starts by tying on a thread. Ours is the usual one: a program that says hello. By the end of this chapter you'll know what a Loom file looks like, how names and modules work, and how to run, check, test and build a program from the command line.

## A first program

@sample start_hello

Three things are going on here:

- `use std.io.print` brings one name into scope. `print` isn't in the [prelude](std:prelude), so the file says where it comes from. `std` itself is always in scope, so `std.io.print("hi")` works without the `use`.
- `fun main()` is where every program starts. It takes nothing and, here, returns nothing.
- Strings are written in double quotes, and a newline ends a statement. There are no semicolons to forget (you can still use `;` to put two statements on one line).

@try Run it, then change `print` to `prnt`. The compiler tells you it doesn't know that name, and points at it.

@compare
-- Rust
Rust's `println!` is a macro with a format string; Loom's `print` is an ordinary function, and the formatting lives in the string itself (more on that below).
```rust
fn main() {
    println!("hello, loom");
}
```
-- Go
Go needs a package clause and an import; Loom's file *is* the module, and `use` imports a single name rather than a whole package.
```go
package main

import "fmt"

func main() {
    fmt.Println("hello, loom")
}
```
-- Python
Python runs top to bottom; Loom, like most compiled languages, starts at `main`. Everything else in the file is a declaration.
```python
print("hello, loom")
```
@end

## The shape of a program

A slightly bigger program shows most of the everyday syntax at once:

@sample start_basics

- `let` makes an immutable binding and `var` a mutable one. You can shadow a name with a new `let`.
- A block's last expression is its value, so `add` needs no `return`. `return` exists for leaving early.
- `fun f(...) -> T = expr` is a function whose whole body is one expression.
- Arguments can be named at the call (`area(2.0, h: 3.5)`), and a parameter can have a default (`h: f64 = 1.0`).
- A string with `{expr}` in it is a **template**: the expression is formatted into the string. `{expr:spec}` adds a format spec: `.3` for three decimals, `>5` to right-align in five columns, `04X` for zero-padded upper-case hex, `?` for the debug form.
- `if` is an expression too, which is how `kind` gets its value. There's no ternary operator because there doesn't need to be one.
- Comments are `// line`, `/* block, and /* they nest */ */`, and `/// doc` for documentation.

@try Make `x` negative, or set it to `0`, and watch `kind` change. Then try `count = 1` after changing `var count` to `let count`.

@compare
-- Python
Loom's templates are Python's f-strings without the `f`: any string literal can hold `{expr}`, and the spec after `:` is close to Python's format mini-language. Write `{{` for a literal brace.
```python
pi = 3.14159265
print(f"pi = {pi:.3f}, [{count:>5}], {255:04X}, {'hi'!r}")
```
-- Rust
`let`/`let mut` in Rust are `let`/`var` in Loom. Loom's templates do what `format!` does, but inline: there's no separate argument list to keep in step with the placeholders.
```rust
let x = 5;
let mut count = 0;
println!("pi = {:.3}, [{:>5}], {:04X}", pi, count, 255);
```
-- Kotlin
Kotlin's `val`/`var` split is the same idea as `let`/`var`, and its `"${expr}"` templates are the closest cousin of Loom's `"{expr}"`.
```kotlin
val x = 5
var count = 0
println("count = $count, q = ${17 / 5}")
```
@end

## Names have a case

The compiler enforces the case of every name. That isn't just tidiness: it's how the parser can tell `Foo[x]` (a type with an argument) from `foo[x]` (indexing a value) without knowing what either one is.

| Kind | Case | Example |
|------|------|---------|
| values, functions, fields, modules | `snake_case` | `port_count` |
| types, wefts, cases | `PascalCase` | `Button` |
| constants | `SCREAMING_CASE`, two or more characters | `MAX_SIZE` |

The four built-in cases `some`, `none`, `ok` and `err` are the only lowercase constructors.

> Go uses case to mean something too: an upper-case name is exported. In Loom, case says what *kind* of thing a name is, and `pub` says who can see it.

## Modules

A file is a module, and a directory is a module made of its files. `use std.io.print` imports a single name. A `use` path starts either with `std` or with a module that sits next to the root file: `use util.double` finds `util.loom`, and `use geometry.point.Point` finds `geometry/point.loom`. Everything is private to its module unless it's marked `pub`.

When you write a name, Loom looks for it in the enclosing scopes first, then in the module's own items, then in its imports, and finally in the prelude. The [prelude](std:prelude) is small on purpose: the collections, `String`, the core wefts like `Show` and `Eq`, `assert` and `unravel`, and a few helpers. `target` (what you're compiling for) and `heap` are always in scope too.

@compare
-- Rust
Rust's modules are declared with `mod` and paths start at `crate`; in Loom the file system is the module tree and there's nothing to declare. `pub` means the same thing.
```rust
mod geometry;
use crate::geometry::point::Point;
```
-- Python
Python's `from util import double` is Loom's `use util.double`. Unlike Python, a Loom name that isn't `pub` can't be imported at all.
```python
from util import double
from geometry.point import Point
```
@end

## The `loom` command

```text
loom run FILE [-- ARGS...]     compile and run a program
loom check FILE                report errors, write nothing
loom test FILE                 run the file's tests
loom build FILE [-o OUT]       write the program out: FILE.lua, or with --image
                               Treadle's image (FILE.lmi), or with --exe an executable
loom docs                      open these docs, offline
```

- `loom run` compiles the program and runs it on **Treadle**, Loom's own virtual machine. Arguments after `--` go to the program, which reads them with `env.args()` (the first one is the file's path). Its code is optimized; `-O0` turns that off for the fastest compile, with every call kept in a backtrace.
- `loom run --backend lua` (and `loom test --backend lua`) runs the program on the bundled Lua 5.4 instead, through the Lua that `loom build` writes. Both backends run the same language; the samples in this book are checked on Treadle.
- `loom build --image` writes Treadle's compiled image (`FILE.lmi`), which `loom run FILE.lmi` runs without compiling again. `loom build --exe` writes a standalone executable: Treadle and the image, without the compiler.
- `loom build --emit tokens|ast|hir|lua|heddle|bytecode` prints one stage of the compiler instead, if you're curious how your program looks on the way down.
- `--color auto|always|never` controls colored diagnostics.

| Exit code | Meaning |
|-----------|---------|
| 0 | success |
| 1 | `main` returned an error |
| 2 | a compile or usage error |
| 70 | an internal error: a bug in Loom, not in your program |
| 101 | the program unravelled |
| 130 | interrupted with Ctrl-C (after its `defer`s ran) |

`process.exit(code)` ends a program with any code you like, also after running its `defer`s.

## Where next

[Values and control flow](page:values) covers numbers, text, `match`, loops and lambdas. The parts that make Loom different start at [Warps, wefts and weaves](page:types).
