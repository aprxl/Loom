# Proposed language conveniences

Status: proposal, 2026-10-21. These four additions are not implemented.
They extend the indexing, calls, patterns, and loops described in
[01-syntax](01-syntax.md), [02-types](02-types.md), and
[04-errors](04-errors.md). Proposed syntax uses `text` fences so the
documentation's parser checks continue to cover supported syntax.

## 1. Chained indexing

Comma-separated indices apply successive indexing operations:

```text
grid[row, col]              // grid[row][col]
grid[row, col] += 1         // grid[row][col] += 1
tables[name, row, col]      // tables[name][row][col]
```

This directly shortens the nested-array example in
[`tests/run/arrays.loom`](../tests/run/arrays.loom).

### Rules

- Evaluate the receiver once, then evaluate and apply each index from left
  to right. If an indexing step fails, later indices are not evaluated.
- Preserve the place being indexed. Assignment and compound assignment
  edit the original element; intermediate rows are not copied.
- Each step keeps ordinary indexing's type, ownership, and bounds rules.
  The same spelling works for nested arrays, lists, slices, and maps where
  the equivalent chain is valid.
- A range is an ordinary indexing step too: `grid[1..3, 0]` means
  `grid[1..3][0]`, selecting the first row of that slice. It does not select
  a column across several rows.
- Tuple keys stay explicit: `map[(x, y)]` indexes one tuple key, while
  `map[x, y]` means `map[x][y]`.
- Brackets resolved as generic arguments keep their existing meaning:
  `Map[String, u8]` and `f[T, N]` are unaffected.

The parser already represents comma-separated bracket arguments. The
checker currently requires a single index; the eventual implementation
must carry the chain through place and ownership checking on both backends.

## 2. Named-argument shorthand

Omitting the expression after a named argument uses the binding with the
same name:

```text
Point(x:, y:)               // Point(x: x, y: y)
connect(host:, port:)       // connect(host: host, port: port)
Token.Int(n, span:)         // Token.Int(n, span: span)
```

Warps are constructed with ordinary calls, so one rule covers both
constructors and functions. Explicit and shorthand arguments can be mixed:

```text
connect(host:, port: 8080)
```

### Rules

- Resolve the omitted expression exactly as the explicit name at that call
  site. A missing binding is a name error; it does not use a parameter's
  default or infer a field of `self`.
- Keep existing argument ordering, duplicate-label, visibility, and type
  checks. Named arguments still follow positional arguments.
- Keep existing read, copy, and move behavior. The shorthand does not imply
  mutation; mutable arguments retain the full spelling, such as
  `buffer: mut buffer`.
- Recognize an omitted expression only when the next token is a comma or
  the closing parenthesis. A newline after `name:` may still precede an
  ordinary argument expression.
- Keep `given:` as the existing ambient-argument syntax; it does not become
  a shorthand for a local named `given`.

The matching convenience in a record or case pattern binds a field under
its own name:

```text
let Point(x:, y:) = point   // let Point(x: x, y: y) = point
match shape {
    .Circle(radius:) => use_radius(radius)
    _ => {}
}
```

Here `radius` must be the declared field label. This introduces a binding
using the ordinary pattern rules; it does not look up an existing variable.

## 3. Bracket-local length: `@len`

Inside an index or slice bound, `@len` means the length of the receiver at
that indexing step:

```text
xs[@len - 1]               // xs[xs.len() - 1]
xs[1..@len - 1]            // xs[1..xs.len() - 1]
xs[@len - 3..]             // the last three elements
grid[row, @len - 1]        // grid[row][grid[row].len() - 1]
```

The expansions illustrate which length is used. Evaluation still happens
once for each receiver and index; the last example does not evaluate `row`
twice or copy the selected row.

`@len` is a contextual expression spelled with the existing `@` token and
the soft keyword `len`. Plain `len` and `end` remain ordinary names, including
inside brackets. This preserves code such as the local `end` slice bounds
in [`examples/calc.loom`](../examples/calc.loom).

### Rules

- Its type is `usize`, matching the receiver's ordinary length.
- Support arrays, lists, and slices. In text slices it means the byte
  length, preserving UTF-8 boundary checks and the restriction against
  indexing text by a single position.
- In a chain, it belongs to the current receiver: in `grid[row, @len - 1]`
  it is the selected row's length. In nested index expressions, the
  innermost indexing step wins: `xs[ys[@len - 1]..@len]` uses `ys.len()`
  inside `ys[...]` and `xs.len()` for the outer slice's upper bound.
- An inner lambda or shuttle is a separate function and cannot capture an
  outer indexing step's `@len`. An index expression inside that function
  can provide its own receiver as usual.
- The shorthand follows the same borrowing and mutation checks as reading
  the receiver's length explicitly.
- Arithmetic and indexing remain checked normally. `xs[@len - 1]` on an
  empty sequence unravels; it does not wrap, clamp, or produce an optional.
- It is unavailable outside indexing or slicing, in generic-argument
  brackets, and in map subscripts. Region annotations keep their existing
  syntax and meaning.

## 4. List comprehensions

A comprehension builds a list from a loop, an optional filter, and an
element expression:

```text
let squares: List[i32] = [
    for n in 1..=20 if n % 2 == 0 => n * n
]

let names: List[String] = [for user in users => user.name.copy()]
```

The first example has the semantics of:

```loom
let squares: List[i32] = {
    var out = List[i32]()
    for n in 1..=20 {
        if n % 2 == 0 { out.push(n * n) }
    }
    out
}
```

The result is built directly. This is useful alongside the existing eager
array/list `map` and `filter` methods, without requiring intermediate lists.

### Rules

- Initially support one `for` clause, an optional `if` filter, and one
  `=>` element expression. Patterns follow ordinary `for` binding rules.
  Nested comprehensions can be written as element expressions.
- Evaluate the iterable once. For each item, evaluate the filter once,
  then evaluate and append the element only if the filter passes. Preserve
  the iterable's order.
- A comprehension produces `List[T]`. Infer `T` from the expected list type
  or the element expression. This form does not infer a fixed-size array,
  even when its input length is known.
- Use exactly the move and borrow rules of an explicit `for` plus `push`.
  A borrowed non-`Copy` element needs an explicit copy when producing an
  owned result. Comprehensions do not add implicit deep copies.
- The loop binding is local to the comprehension. The comprehension is an
  expression in the enclosing function, not a lambda or shuttle boundary.

In particular, `?` retains its usual destination:

```text
let ports: List[u16] = [
    for text in inputs => text.parse[u16]()?
]
```

A parse failure reaches the enclosing `mend`, or leaves the function when
there is none, using the normal error conversions. The partially built list
is dropped on an early exit. `return`, `break`, `continue`, and `defer` also
follow the equivalent explicit loop's scope and cleanup rules.

## Checks when implemented

Each addition needs run and fail fixtures on Lua and Treadle:

| Feature | Checks |
|---|---|
| Chained indexing | Reads and writes, compound assignment, evaluation order and failure short-circuiting, tuple map keys, ranges, unchanged generic arguments, existing borrow restrictions |
| Named shorthand | Constructors and functions, mixed arguments, missing bindings, duplicate labels, moves, explicit mutation, named patterns, multiline explicit arguments |
| `@len` | Empty and nonempty sequences, slices, receiver evaluated once, row-relative and nested lengths, checked subtraction, text boundaries, invalid contexts, unchanged local names and region syntax |
| Comprehensions | Empty output, filtering and order, nested forms, type inference, ownership, errors caught by `mend` or propagated, cleanup on early exits |

These proposals require no new VM instructions. Their eventual frontend
implementation should express the existing operations so the backends and
optimizer share the same semantics.
