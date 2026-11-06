title: Gauges
summary: Number types limited to a range or a predicate, checked once on the way in and tracked by the compiler afterwards.
---
Plenty of numbers in a program aren't *any* number. A TCP port is 1 or more. A percentage runs from 0 to 1. A count of cents shouldn't go below zero. Usually that rule lives in a comment, or in a check at the top of one function, and every other function hopes it was honored.

A **gauge** puts the rule in the type. It is a numeric type with a restriction, so a `Port` can't be zero: the check happens where a number *becomes* a `Port`, and from then on the compiler (and you) can rely on it. Because the restriction is part of the type, the compiler can also follow it through arithmetic, which is where gauges earn their name.

## A number with a rule

A gauge is declared with `type` and `where`, followed by a range or a predicate:

@sample gauges_basic

- A **range** gauge is `where 1..` (one or more), `where 0..10` (0 up to but not including 10) or `where 0.0..=1.0` (including the end). It works for integers and floats.
- A **predicate** gauge is a hole lambda: `where _ % 2 == 0`. Anything the test accepts is in.
- A gauge is **its own type**: a `Port` is not interchangeable with a `u16`, and a `Digit` is not a `Percent`. But it widens to its base type by itself (`let n: u16 = web`), and it has its base's operators and methods.
- A literal assigned to a gauge is checked **while compiling**, so `let web: Port = 8080` costs nothing at run time.

A gauge's bounds must be known at compile time: literals and constants, not the result of a function call ([T0103](diag:T0103)).

@sample gauges_err_bounds

@try Change `Port`'s declaration to `where 1..1024` and check it. `connect(443)` is still fine, but `8080` is now outside the range, and so is the `9000` passed to `max`: the compiler points at each literal ([G0201](diag:G0201)).

@compare
-- Ada
Ada's `range` types are the ancestor. Ada checks at run time and raises `Constraint_Error`; Loom checks literals and constants while compiling and has a different operator for each way a run-time value can be handled.
```ada
type Port is range 1 .. 65_535;
subtype Digit is Integer range 0 .. 9;
```
-- Rust
Rust has no ranged integers, so the usual tool is a newtype with a checked constructor. It works, but every operation on the inside is back to a plain `u16`, and the rule is reimplemented per type.
```rust
struct Port(u16);
impl TryFrom<u16> for Port {
    type Error = ();
    fn try_from(n: u16) -> Result<Self, ()> {
        if n >= 1 { Ok(Port(n)) } else { Err(()) }
    }
}
```
-- Zig
Zig's integer types can have any bit width (`u4` is 0 to 15), which gives power-of-two ranges that are checked when the compiler can see the value. A gauge gives you any range, and predicates.
```zig
const d: u4 = 9;
```
@end

## Getting a number into a gauge

Getting a number **into** a gauge is the one checked step. A literal or a constant is checked at compile time. For anything else there are four forms, which differ in what happens when the number is outside:

| Form | When the value is outside |
|------|---------------------------|
| a literal or a constant | compile error ([G0201](diag:G0201)) |
| `x as Port` | unravels |
| `x as? Port` | gives an optional: `some(port)` or `none` |
| `x as| Port` | clamps into the range (range gauges only) |
| `if x is Port { ... }` | the test fails; inside the branch `x` is a `Port` |

@sample gauges_into

Use `as?` when a miss is a normal outcome (user input), `as|` when clamping is what you want (a volume knob), `as` when a miss would be a bug, and `is` when you want to branch and then keep using the checked name. `is` narrows: inside the `if`, `n` has become a `Port`, which is why the branch can return it as one.

A literal outside the range is an error at the literal:

@sample gauges_err

@try In `gauges_into`, change `(n + 1) as Even` to `n as Even`. Now the program stops at the first of the two, and the second is never reached. Then change `70000 as| Port` to `70000 as? Port`: you get `none` instead of `65535`.

@compare
-- Rust
Rust's `TryFrom` plays the role of `as?`, `.clamp()` plays `as|`, and a failed `.unwrap()` plays `as`. The difference is that the rule lives in a gauge's declaration instead of a hand-written constructor, and you get all three spellings of "outside" for free.
```rust
let n: i64 = 7;
let even = if n % 2 == 0 { Some(n) } else { None };
let port = u16::try_from(70000i64).ok();
let clamped = 70000i64.clamp(1, 65535);
```
@end

## Arithmetic keeps the rule

Here the idea pays off. The compiler tracks the interval a gauge's value can be in, through arithmetic, at compile time. `Cents + Cents` can't go below zero, so it's still a `Cents`: no cast needed. `Cents - Cents` can, so it's a plain `i64`.

@sample gauges_arith

`+`, `*`, `/`, `%`, `min` and `max` all keep a range when the result provably fits. Subtraction usually doesn't, so assigning it back is an error, and the compiler suggests the three ways in:

@sample gauges_err_arith

- Overflow of the base type still unravels as usual.
- The interval is worked out for range gauges. A predicate gauge can't be followed through arithmetic: `Even + Even` is a plain `i64`, and needs `as?` to become an `Even` again.

@try In `gauges_arith`, change `let rest: Cents = a % 7` to `a - 7`. A `Cents` minus a number might be negative, and the compiler refuses to call it `Cents`.

@compare
-- Ada
Ada checks every assignment into a range type at run time, which is simple and a little slow. Loom's intervals let the compiler skip the check when it can prove the value fits, and tell you when it can't.
```ada
Total : Cents := A + B;   -- checked at run time
```
-- Rust
A Rust newtype has none of this: `Cents(a.0 + b.0)` is a fresh construction and a fresh check every time, and subtraction needs an explicit decision about overflow.
```rust
let total = Cents::new(a.0 + b.0)?;
```
@end

## Check once, at the edge

Gauges also change how you structure a program. If a `Server` has a `Port` field, then **every** `Server` has a valid port, and no function that takes one needs to check. Validation moves to the edge of the program, where input comes in, and becomes a conversion:

@sample gauges_edge

`parse_port` is the only place that can say "not a port", and it does so with an ordinary error (see [Optionals and errors](page:errors)). `url` and everything else downstream can't be handed a bad one, because there's no way to make one.

@try Change `Server("example.org", port)` to `Server("example.org", 0)`. There's no way to build a `Server` with port 0, and the compiler says so at the literal ([G0201](diag:G0201)). The only way to get a different port is through `parse_port`.

## Not supported yet

- A gauge can't index an array or list directly: `arr[s]` with `s: Slot` is [T0101](diag:T0101). Widen it first, `arr[s as usize]`. The design lets a gauge prove an index in range and skip the bounds check; that doesn't happen yet.
- A gauge's bounds are literals and constants only; a call to one of your functions is [T0103](diag:T0103) (see [Compile-time code](page:compile-time) for what the evaluator can fold).

## Where next

[Shuttles](page:shuttles) are functions that can pause.
