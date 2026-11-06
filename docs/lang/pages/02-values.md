title: Values and control flow
summary: Numbers, text, functions, `match`, loops, lambdas, and bit patterns.
---
Before there is a loom there is thread: plain values and the small machinery that moves them around. This chapter covers the ordinary stuff every program is made of: numbers and what happens when they overflow, text and templates, functions, `if`, `match` and loops, lambdas, and a pattern syntax for picking bits apart that Loom calls *twill*.

Most of it will look familiar. A few decisions are different, and they're worth noticing early: integers can have any width, overflow is chosen by the *operator* you write, and `match` insists you cover every case. The data types Loom is really about, warps and wefts, come in [the next chapter](page:types). Bindings (`let`, `var`) and the file layout are in [Getting started](page:start#the-shape-of-a-program).

## Numbers

Loom has `bool`, `char`, `str`, floats (`f32`, `f64`) and integers, and the integers are the unusual part: `u12`, `i24` and `u4` are ordinary types. Any width from 1 to 64 bits works, and a `uN` does its arithmetic at exactly N bits, so a `u12` overflows at 4096. Bits are cheap to say in Loom: a CHIP-8 emulator, for instance, keeps its sixteen 4-bit register numbers in `u4`s. `usize` and `isize` are the size of a pointer: 64 bits on Treadle and 63 on the Lua backend.

@sample values_numbers

- **Literals** take the type their context asks for: `let a: u12 = 4095`. With no context, a literal becomes the first numeric type it meets, and otherwise `i32` (or `f64` for a float). A suffix (`255u8`, `2.5f32`) picks one by hand, and `_` separates digits. A literal that doesn't fit its type is a compile error ([T0102](diag:T0102)).
- **The only implicit conversion is lossless integer widening**: `u8` to `u16` or `i32`. There is none between signed and unsigned of the same width, none that narrows, and none involving floats. Everything else is written with `as`.
- **`/` and `%` on integers truncate toward zero**, so `-7 / 2` is `-3` and `-7 % 3` is `-1`. A zero divisor unravels.
- **Bit operators bind tighter than comparisons**, so `flags & MASK == 0` means what it looks like. `~` is exclusive or when it has two operands, and bitwise not when it has one. A shift by the type's width or more unravels.
- Types wider than 64 bits and `f16` are not supported yet ([Not supported yet](page:not-yet#types-and-library)).

@try Add `let narrow: u8 = wide` after `wide` is made. The compiler refuses (T0101): `u16` doesn't fit in a `u8` without saying so. Then try `let big: u8 = 256`, which is caught even earlier ([T0102](diag:T0102)).

## Overflow is part of the operator

What should `200 + 100` be, when both are bytes? Languages disagree: C leaves signed overflow undefined, Go wraps without telling you, Rust panics in a debug build and wraps in a release build. Loom doesn't pick for you. Plain `+`, `-` and `*` **unravel** (Loom's word for a fatal runtime error) if the answer doesn't fit, in every build. When wrapping is what you want, you say so with a suffix on the operator:

| Operator | On overflow |
|----------|-------------|
| `+ - *` | unravels |
| `+% -% *%` | wraps around (two's complement) |
| `+| -| *|` | stops at the type's limit (saturates) |
| `+? -? *?` | gives an optional: `none` on overflow |

The same four policies apply to conversions: `as`, `as%`, `as|` and `as?`.

@sample values_overflow

The last line of that sample ends the program, which is why the sample is labelled *unravels*. An unravel prints where it happened and exits with code 101; [Optionals and errors](page:errors) shows the errors you can recover from instead.

@try Delete the `%` from `a +% 100`. The program unravels at the first line instead of printing `44`. Then change `u12` to `u16` in the `m` line and watch the three results change.

@compare
-- Rust
Rust picks by *method*: `wrapping_add`, `saturating_add`, `checked_add`. Plain `+` panics in a debug build but silently wraps in a release build unless you turn on `overflow-checks`. Loom's plain `+` is the same in every build.
```rust
let a: u8 = 200;
a.wrapping_add(100);    // 44
a.saturating_add(100);  // 255
a.checked_add(100);     // None
```
-- Zig
Zig is where the operators come from: `+%` wraps and `+|` saturates there too. Zig's plain `+` is checked in safe build modes; Loom's always is, and adds `+?` for a result you can test.
```zig
const a: u8 = 200;
const w = a +% 100; // 44
const s = a +| 100; // 255
```
-- Go
Go wraps silently on every integer operation and gives you no operator to ask for anything else. If you need a check, you write it by hand.
```go
var a uint8 = 200
a += 100 // 44, no complaint
```
@end

## Text

`str` is a view of UTF-8 text and `String` owns it, the same split as Rust's `&str` and `String` or Go's string and `[]byte`. A string literal converts to `String` wherever one is expected. **Lengths and positions count bytes**, and slicing cuts on bytes too: if the cut lands inside a character, the program unravels. Methods like `is_alphabetic` and `to_upper` follow Unicode (`"straße".to_upper()`), while the `_ascii` variants look at ASCII only.

@sample values_text

The full method lists are in [std.string](std:string#Str) and [std.num](std:num). A few things to notice:

- `find` gives an optional position, and `parse[T]()` gives a result: `ok(42)`, or `err(Invalid)` and `err(Overflow)` for text that isn't a number or doesn't fit. `mend 0` supplies a fallback; both are covered in [Optionals and errors](page:errors).
- A `String` you change must be a `var`: `out.push(c)`.
- `chars()`, `bytes()`, `lines()` and `words()` give lazy streams, which [Collections and streams](page:collections) explains.

@try Change `t[0..5]` to `t[0..9]`. Byte 9 is in the middle of `ö`, so the program unravels instead of printing half a character.

### Other kinds of string literal

@sample values_literals

- `r"..."` is a raw string: no escapes and no `{templates}`.
- `"""` starts a multi-line string. The indentation shared by all its lines is removed, and the closing `"""` can sit on a line of its own.
- `b"GET"` is a byte string: an array of bytes (`[u8; 3]`), ASCII only, which passes wherever a `[u8]` is wanted.
- `char` is one Unicode scalar, written `'é'` or `'\u{1F9F6}'`. `as` converts to and from integers.

@compare
-- Python
Python counts *code points*: `len("é")` is 1 and slicing never splits a character. Loom counts bytes like Go and Rust, so slicing is O(1) and exact, and it's your job to cut on a character boundary.
```python
s = "Wörld"
print(len(s), s[0:2])   # 5 Wö
```
-- Rust
Rust and Loom agree: `len` is bytes, slicing is by byte, and a cut inside a character is fatal (a panic there, an unravel here). `chars().count()` gives the characters.
```rust
let s = "Wörld";
println!("{} {}", s.len(), s.chars().count()); // 6 5
let _ = &s[0..2];                              // panics: inside 'ö'
```
-- Go
Go's strings are bytes too, but slicing never checks: you can cut a character in half and carry on with invalid UTF-8. `range` over a string decodes runes.
```go
s := "Wörld"
fmt.Println(len(s), len([]rune(s))) // 6 5
fmt.Println(s[0:2])                 // "W\xc3", broken UTF-8
```
@end

## Templates and format specs

A string with `{expr}` in it is a **template**: the expression is formatted into the string where it stands. Any expression works, and `{` is written `{{`. After a `:` comes a format spec, which looks like Python's: an optional fill and alignment (`<`, `^`, `>`), a sign, `0` for zero padding, a width, a precision (`.3`), and a type letter.

@sample values_templates

| Spec | Meaning | Example |
|------|---------|---------|
| `.3` | three decimals | `{pi:.3}` gives `3.142` |
| `6`, `<6`, `^6`, `>6` | width, with alignment | `{n:^6}` gives `  42  ` |
| `*^10` | fill with `*`, centered | `***loom***` |
| `06`, `+` | zero padding, always show the sign | `000042`, `+42` |
| `x X b o` | hex, upper hex, binary, octal; `#` adds `0x`/`0b` | `{255:#x}` gives `0xff` |
| `?` | debug form | `{name:?}` gives `"loom"` |

A `:` always starts the spec, so an expression that needs one of its own goes in parentheses.

@try Add `{1_000_000:,}` to one of the `print` calls. Loom has no thousands separator, and says so ([T0117](diag:T0117)) with a hint about parentheses. Then try `{pi:10.5}` and `{n:#010b}` and see how width and precision combine.

## Functions

A function says what it takes and what it gives back. Every parameter needs a type; the return type goes after `->`, and is left out when the function returns nothing. The body is a block whose last expression is the result, or `= expr` for a one-liner. `return` is for leaving early.

@sample values_functions

- **Arguments can be named** at the call (`area(h: 3.0, w: 2.0)`), in any order, and **a parameter can have a default** (`h: f64 = 1.0`). Named arguments come after the positional ones.
- **Several results travel as a tuple**: `(i32, i32)`, read with `.0` and `.1` or taken apart with `let (q, r) = ...`.
- **`mut` lets a function change the caller's variable**, and the call has to say so: `bump(mut k)`. Mutation is never a surprise at the call site. How this works for bigger values (and `take`, which consumes) is the subject of [Moves, `once` and `defer`](page:ownership).
- **Any function is also a method on its first parameter**: `cube(3)` can be written `3.cube()`. That's [method lookup](page:types#method-lookup-and-ufcs), covered in the next chapter.
- A function's name is a value you can pass around: see [Lambdas](page:values#lambdas).

@try Change `bump(mut k)` to `bump(k)`. The compiler tells you the call must say `mut` ([T0112](diag:T0112)). Then call `area(h: 3.0)` without a width and see what it asks for.

@compare
-- Python
Python has the same two features, with defaults evaluated at definition time (the famous mutable default). Loom has no `*args` or `**kwargs`: a function has the parameters it says.
```python
def area(w, h=1.0):
    return w * h

area(h=3.0, w=2.0)
```
-- Swift
Swift's argument labels are required at every call and fixed in order. Loom's names are optional, and can reorder. Swift marks a changed variable with `&` at the call, the same idea as `mut k`.
```swift
func area(w: Double, h: Double = 1.0) -> Double { w * h }
func bump(_ n: inout Int) { n += 1 }

area(w: 2.0, h: 3.0)
var k = 1
bump(&k)
```
-- Kotlin
Kotlin's named and default arguments are the closest match. The difference is mutation: Kotlin passes a copy of a number and cannot change the caller's variable at all.
```kotlin
fun area(w: Double, h: Double = 1.0) = w * h

area(h = 3.0, w = 2.0)
```
@end

## `if` and loops

`if` is an expression, so `let parity = if n % 2 == 0 { "even" } else { "odd" }` is how Loom spells a ternary. Conditions are `bool` only (no truthiness), and `else` can start a new line. Blocks are expressions as well, so they give the value of their last line.

@sample values_loops

- `for x in xs` goes over a range, an array, a list, a map, a stream... anything with items. `1..=4` includes the 4; `0..3` stops before the 3. For `(i, x)` pairs, use `.indexed()`.
- `while cond` checks before each round, and `loop` runs until a `break`. **`break` can carry a value**, which becomes the loop's value.
- A label before a loop (`outer:`) lets `break outer` and `continue outer` reach past inner loops.
- **Comparisons chain**: `0 <= x < width` means `0 <= x and x < width`, with `x` evaluated once. Mixing directions (`a < b > c`) is an error.
- `for mut x in xs` edits the elements in place; see [Collections and streams](page:collections).

@try In `found = loop { ... }`, change `break steps` to a plain `break`. The loop no longer has a value, so `found` is `()` and `print(found)` stops the build: `()` isn't `Show` ([T0108](diag:T0108)). Then try `if n { ... }` on a number and meet the error for a condition that isn't a `bool`.

@compare
-- Rust
Loom's loops are Rust's: `loop` can break with a value, labels name loops. Rust spells a label `'outer`, Loom `outer:`. Rust has no chained comparison, so `0 <= x && x < width` it is.
```rust
let found = loop {
    steps += 1;
    if steps % 7 == 0 { break steps; }
};
'outer: for row in 0..3 {
    for col in 0..3 {
        if col == 2 { continue 'outer; }
    }
}
```
-- Python
Python chains comparisons too, and `for ... in enumerate(...)` is Loom's `.indexed()`. Python has no `loop` or `break` with a value, but has `else` on loops, which Loom doesn't.
```python
if 0 <= x < width:
    print("in range")
for i, name in enumerate(["a", "b", "c"]):
    print(f"{i}={name}")
```
@end

## `match` and patterns

`match` compares a value against **patterns**, top to bottom, and runs the first arm that fits. It's an expression, and it must be **exhaustive**: if the arms leave a possible value uncovered, the compiler refuses and says which one ([P0101](diag:P0101)). An arm can add a guard with `if`, and arms are separated by newlines or commas.

@sample values_match

| Pattern | Matches |
|---------|---------|
| `42`, `'a'`, `"GET"`, `true` | a literal |
| `1..=9`, `'a'..='f'` | a range |
| `10|20|30` | any of several; both sides must bind the same names |
| `x` | anything, binding it to `x` |
| `_` | anything, binding nothing |
| `(a, b)` | a tuple |
| `Point(x, y)`, `Point(x:, ..)` | a record, by position or by name; `..` skips the rest |
| `.Circle(r)`, `some(n)` | a case; see [Cases](page:types#cases) |

`is` is a pattern test that gives a `bool` and binds for the `and` that follows: `if found is some(n) and n > 5 { ... }`.

Slice patterns (`[first, ..rest]`) and `name @ pattern` bindings are in the design but not built yet: see [Not supported yet](page:not-yet).

@try Delete the `_ => "big"` arm of `classify`. Because an `i32` has many more values than the arms name, the compiler stops with P0101. Next, delete just `(_, 0) => "Buzz"` from `fizz` and see it name the shape of the missing case.

@compare
-- Rust
Nearly the same. Rust's record patterns use braces (`Point { x: 0, .. }`) and a binding with a guard is `x if x < 0`; Loom's records are patterned like calls, and `Point(x:, y:)` is the shorthand for `Point(x: x, y: y)`.
```rust
match n {
    0 => "zero",
    1..=9 => "small",
    10 | 20 | 30 => "round",
    x if x < 0 => "negative",
    _ => "big",
}
```
-- Swift
Swift's `switch` is exhaustive too, with `case` for each arm and `where` for a guard. Ranges are written `1...9`.
```swift
let kind = switch n {
case 0: "zero"
case 1...9: "small"
case 10, 20, 30: "round"
case let x where x < 0: "negative"
default: "big"
}
```
-- Python
Python's `match` (3.10+) is not checked for exhaustiveness: a missing case just falls through. It has no range patterns, so a guard does the work.
```python
match n:
    case 0: "zero"
    case 10 | 20 | 30: "round"
    case x if x < 0: "negative"
    case _: "other"
```
@end

## Lambdas

`x => x * 2` is a lambda. Its parameter types come from where it's passed (`xs.map(x => x + 1)`), so a lambda stored in a `let` needs them written: `(x: i32) => x * 2`. Function types use the same arrow: `i32 => i32`, `(f64, f64) => bool`.

When a lambda is just one expression of its argument, a **hole** is shorter: each `_` inside an argument is a new parameter, and the argument becomes a lambda. `xs.filter(_ % 2 == 0)` is `xs.filter(n => n % 2 == 0)` and `xs.reduce(_ + _)` takes two parameters. A hole belongs to the innermost argument that holds it, so `clamp(_, 0, 255)` is a lambda too.

@sample values_lambdas

- A function's name is a value (`xs.filter(is_even)`), and a lambda sees the variables around it, even to change them (`total += x`).
- Lambdas you pass as arguments borrow what they capture and can't outlive the call. Storing or returning one means allocating it: see [Pointers, spools and `given`](page:memory).
- If you need the same value twice, name it: `xs.map(n => n * n)`.

@try Change `xs.filter(_ % 2 == 0)` to `xs.filter(_ % 2 == _)`. Two holes make a two-parameter lambda, and `filter` wants one.

@compare
-- Kotlin
Kotlin's implicit `it` is the closest relative, for one-parameter lambdas. A hole generalizes it to more than one parameter and to nested calls.
```kotlin
xs.filter { it % 2 == 0 }
xs.reduce { a, b -> a + b }
```
-- Swift
Swift names a closure's parameters by number, so `$0` can be used twice and means the same value both times. In Loom each `_` is a *new* parameter, which is why you name the value when you want it twice.
```swift
xs.filter { $0 % 2 == 0 }
xs.reduce(0) { $0 + $1 }
```
-- Rust
Rust closures need pipes and often a `.iter()` and a dereference. The captured-variable rules are stricter, because the borrow checker is tracking lifetimes.
```rust
xs.iter().filter(|n| *n % 2 == 0);
xs.iter().sum::<i32>();
```
@end

## Twill patterns

A binary, octal or hex literal in a pattern can hold `?` (any digit) and lowercase letters, which **capture**. All the bits with the same letter become one unsigned integer exactly that wide. This is what you reach for when decoding an instruction set or a byte format, where hand-written masks and shifts are easy to get wrong.

@sample values_twill

- The pattern's width must equal the value's: `0x1nnn` is 16 bits, so it matches a `u16`. The three `n`s are twelve bits, so `n` is a `u12`.
- `_` in a binary pattern is a separator and is ignored (`0b0???_????`).
- In hex patterns the digits `A` to `F` must be upper case; lower-case letters are captures. (`0x00e0` would capture `e`.)
- The compiler checks twill arms for exhaustiveness bit by bit, so deleting the `_` arm of `utf8_kind` names the patterns you missed.
- A twill pattern with no fixed digits can't fail, so `let` takes it. The pattern's width becomes the type of the right-hand side, which is how `let 0bc_ssss_ssss = a + b` works out an addition with its carry in 9 bits.

@try In `utf8_kind`, delete the `_ => "other"` arm. The compiler checks the arms bit by bit, finds a byte none of them covers, and names one (`240`, which starts `0b1111`).

@compare
-- Elixir
Elixir (like Erlang) has the same idea built in, with binary patterns. Loom's twill is the version for plain integers: the pattern is a literal, so the digits show the layout.
```elixir
case <<byte>> do
  <<0::1, _::7>> -> "ascii"
  <<0b110::3, _::5>> -> "starts a 2-byte character"
  <<0b10::2, _::6>> -> "continuation"
end
```
-- Rust
Rust has no bit patterns. You test with masks in guards and shift the fields out by hand, and the compiler can't tell you if the arms are exhaustive.
```rust
match byte {
    b if b & 0x80 == 0 => "ascii",
    b if b & 0xE0 == 0xC0 => "starts a 2-byte character",
    b if b & 0xC0 == 0x80 => "continuation",
    _ => "other",
}
```
@end

## Where next

[Warps, wefts and weaves](page:types) introduces Loom's own data and behavior types.
