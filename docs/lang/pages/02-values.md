title: Values and control flow
summary: Numbers, text, `if`, `match`, loops, lambdas, and bit patterns.
---
## Numbers

`u12`, `i24` and `u4` are ordinary types: any width from 1 to 64 bits. A `uN` does its arithmetic at exactly N bits, so a `u12` overflows at 4096. `usize` and `isize` are 63 bits on the Lua backend. Types wider than 64 bits and `f16` are not supported yet ([U0102](diag:U0102)).

Each arithmetic operator says what happens on overflow:

| Operator | On overflow |
|----------|-------------|
| `+ - *` | unravels |
| `+% -% *%` | wraps around |
| `+| -| *|` | stops at the type's limit (saturates) |
| `+? -? *?` | gives an optional, `none` on overflow |

`as`, `as%`, `as|` and `as?` convert with the same four policies.

@sample values_numbers

- The only implicit conversion is lossless integer widening (`u8` to `u16`). There is none between signed and unsigned, and none involving floats.
- A number literal takes the type its context expects. With no context it becomes the first numeric type it meets, and otherwise `i32` (or `f64`).
- Integer `/` and `%` truncate. A zero divisor unravels. A shift by the type's width or more unravels.
- Bit operators bind tighter than comparisons, so `flags & MASK == 0` means what it looks like.

## Text

`str` is a view of UTF-8 text and `String` owns it. A string literal converts to `String` wherever one is expected. Lengths and positions count bytes, and slicing a string cuts on bytes, unravelling if it lands inside a character. The case and digit tests on `char` and the case changes on text cover ASCII only.

@sample values_text

The full method lists are in [std.string](std:string#Str) and [std.num](std:num).

## Control flow

Everything is an expression: `if`, `match` and blocks all have values. `match` must be exhaustive ([P0101](diag:P0101)), and an arm can have a guard. Loops are `for`, `while` and `loop`; `break` can carry a value, and a loop can have a label. Comparisons chain: `0 <= x < width`.

@sample values_control

## Lambdas

`x => x * 2` is a lambda. Its parameter types come from where it's passed, so a lambda stored in a `let` needs them written. A `_` inside an argument makes a hole lambda: each `_` is a new parameter, and the hole belongs to the innermost argument that holds it. A function's name is also a value. A lambda sees the variables around it.

@sample values_lambdas

## Twill patterns

A binary, octal or hex literal in a pattern can hold `?` (any digit) and lowercase letters, which capture. All the bits with the same letter become one unsigned integer exactly that wide. The pattern's width must equal the value's. The compiler checks twill arms for exhaustiveness bit by bit.

@sample values_twill

In hex twill patterns the digits `A`-`F` must be upper case. Lower case letters capture.

## Where next

[Warps, wefts and weaves](page:types) introduces Loom's own data and behavior types.
