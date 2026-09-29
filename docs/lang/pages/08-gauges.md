title: Gauges
summary: Number types limited to a range or a predicate.
---
A **gauge** is a numeric type with a restriction: a range (`u16 where 1..`) or a predicate written as a hole lambda (`i64 where _ % 2 == 0`).

A gauge is its own type. It widens to its base type by itself (a `Port` is a `u16`), and has its base's methods. Getting a number **into** a gauge is checked:

| Form | When the value is outside |
|------|---------------------------|
| a literal or a constant | compile error ([G0201](diag:G0201)) |
| `x as Port` | unravels |
| `x as? Port` | gives an optional |
| `x as| Port` | clamps into the range (range gauges only) |
| `if x is Port { ... }` | the test fails; inside the branch `x` is a `Port` |

Arithmetic on range gauges follows intervals at compile time. `Cents + Cents` can't go below zero, so it's still `Cents`. `Cents - Cents` can, so it's a plain `i64`. Overflow still unravels as usual.

@sample gauges_basic

A gauge's bounds must be known at compile time: literals and constants, not the result of a function call ([T0103](diag:T0103)). A literal outside the range is an error at the literal:

@sample gauges_err

## Where next

[Shuttles](page:shuttles) are functions that can pause.
