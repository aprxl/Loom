title: Getting started
summary: Running a program, the `loom` commands, and the shape of a first program.
---
## A first program

@sample start_hello

`print` isn't in the prelude, so the file says where it comes from with `use std.io.print`. `std` itself is always in scope, so `std.io.print("hi")` works without a `use`. Every program starts at `fn main()`.

A bigger one:

@sample start_basics

- `let` is immutable and `var` is mutable. Shadowing is allowed.
- The last expression of a block is its value, so `a + b` needs no `return`.
- `fn f(...) -> T = expr` is a function with an expression body.
- Arguments can be named, and a parameter can have a default.
- A string with `{expr}` in it is a template. `{expr:spec}` adds a format spec: `.3` for three decimals, `>5` to right-align in five columns, `04X` for zero-padded upper-case hex, `?` for the debug form.
- A newline ends a statement. Use `;` to put several on one line.
- Comments are `// line`, `/* block, and /* they nest */ */`, and `/// doc`.

## Names have a case

The compiler enforces the case of a name, which lets the parser tell `Foo[x]` (type arguments) from `foo[x]` (an index).

| Kind | Case | Example |
|------|------|---------|
| values, functions, fields, modules | `snake_case` | `port_count` |
| types, wefts, cases | `PascalCase` | `Button` |
| constants | `SCREAMING_CASE`, two or more characters | `MAX_SIZE` |

The four built-in cases `some`, `none`, `ok` and `err` are the only lowercase constructors.

## Modules

A file is a module, and a directory is a module made of its files. `use std.io.print` imports a name. A `use` path starts with `std` or with a module next to the root file: `use util.double` finds `util.loom`, and `use geometry.point.Point` finds `geometry/point.loom`. Items are private to their module unless marked `pub`.

A name is looked up in the enclosing scopes, then the module's own items, then its imports, then the prelude. See [std.prelude](std:prelude) for what the prelude holds. `target` and `heap` are also always in scope.

## The `loom` command

```text
loom run FILE [-- ARGS...]     compile and run a program
loom check FILE                report errors, write nothing
loom build FILE [-o OUT]       write FILE.lua (and FILE.lua.map)
loom test FILE                 run the file's tests
```

- `loom run` compiles in memory and runs the result in the bundled Lua VM. Arguments after `--` go to the program, which reads them with `env.args()` (the first is the file's path).
- `loom run --backend vm` (and `loom test --backend vm`) runs it on Treadle, Loom's own VM, instead. Its code is optimized; `-O0` turns that off (the fastest compile, every call kept in a backtrace).
- `loom build --emit tokens|ast|hir|lua|heddle|bytecode` prints one stage of the compiler instead of writing Lua.
- `--color auto|always|never` controls colored diagnostics.

| Exit code | Meaning |
|-----------|---------|
| 0 | success |
| 1 | `main` returned an error |
| 2 | a compile or usage error |
| 70 | an internal error: a bug in Loom, not in your program |
| 101 | the program unravelled |

Ctrl-C (130), `process.exit` and an instruction budget for `run` aren't done yet.

## Where next

[Values and control flow](page:values) covers numbers, text, `match` and lambdas. The parts that make Loom different start at [Warps, wefts and weaves](page:types).
