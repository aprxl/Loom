title: Reflection
summary: What compile-time code learns about a type, and default methods that derive from a type's shape.
---
Compile-time code can ask a type about itself. The answers are known while compiling, like a `const` parameter, so `const if` and `const for` can use them, and nothing reflective is left in the program. The types involved are in [std.meta](std:meta).

## Asking a type

| Expression | Gives you |
|------------|-----------|
| `T.name` | its name, `"Point"` (a warp's, without its arguments) |
| `T.kind` | a case of `Kind`: `.Record`, `.Sum`, `.Int`, `.Str`, `.List`, … |
| `T.fields` | a record's fields, a list of `Field`s |
| `T.cases` | a sum's cases, a list of `Case`s |
| `f.name`, `f.is_pub` | a field's name, and whether it's `pub` |
| `c.name`, `c.fields` | a case's name, and its payload's fields |
| `.len()`, `.is_empty()` | of `T.fields`, `T.cases` or `c.fields` |

Two things use them in code that runs:

- `v.(f)` is `v`'s field `f`, where `f` is a `Field`. It reads and assigns like `v.x`.
- `v is .(c)` tests whether `v` is case `c`, and `v.(c).(f)` reads a field of that case's payload. Read it inside `if v is .(c)`: on any other case it unravels.

`const for f in T.fields` writes its body out once per field, so `describe` below turns into straight-line code for each type it's called with.

@sample refl_describe

## Derive is a default method

Loom has no derive macros. A weft gives a default method that looks at `Self`, and every warp that lists the weft gets that method, worked out for its own fields. `examples/compile_time.loom` writes `ToJson` this way.

@sample refl_derive

Writing the method yourself replaces the default, like any other default method.

## Only while compiling

A `Field`, a `Case` or a list of them has no value when the program runs. Anything read from one does: a name is text, `.len()` is a number. Keeping the list itself is [K0111](diag:K0111).

@sample refl_err_runtime

## Not supported yet

- `T.size`, `T.align`, `T.bits`, `f.ty` and `T.weaves(W)` are [U0102](diag:U0102).
- std's `Show`, `Eq`, `Ord` and `Hash` for warps are still built into the compiler rather than written as default methods with reflection.
