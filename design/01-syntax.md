# 01 · Syntax

Loom's syntax is brace-based, uses newlines to end statements, and has no
semicolons in normal code. Most of it will look familiar. The unusual parts
are the leading dot, dot-blocks, hole lambdas, twill patterns, and the
overflow-policy operators.

---

## Lexical basics

- Source files are UTF-8 and use the extension `.loom`.
- Comments: `// line`, `/* block, and /* they nest */ */`, `/// doc comment`.
- **Case carries meaning, and the compiler enforces it:**
  - values, functions, fields, modules: `snake_case` (start with a lowercase letter or `_`)
  - types, wefts, cases: `PascalCase` (start with an uppercase letter and contain a lowercase one, or are a single letter like `T`)
  - constants: `SCREAMING_CASE` (two or more characters, all uppercase letters, digits, or `_`)
  - primitive types (`i32`, `u12`, `f64`, `bool`, `char`, `str`, `never`) are keywords
  - The four built-in cases `some`, `none`, `ok` and `err` are the only lowercase constructors.

  The parser relies on this rule. In `Foo[x]` the brackets are type arguments,
  and in `foo[x]` or `TABLE[x]` they're an index.
- Keywords:
  `fn let var const type warp weft weave into through case pub if else match for in while loop break continue return fail is not and or mend hem loose ref spool given loom thread pass pull card selvage extern use as any self Self mut`.
  Some keywords are *contextual*: `take` only at the start of a parameter (so
  `iter.take(3)` is an ordinary method call), `from` only inside a case field
  (so `fn transfer(from: AccountId, ...)` is fine), and `get` only inside `pub(get)`.

### Literals

| Kind       | Examples |
|------------|----------|
| integer    | `42`, `1_000_000`, `0xFF`, `0o755`, `0b1010`, `255u8`, `4096usize` |
| float      | `3.14`, `1e-9`, `2.5f32` |
| char       | `'a'`, `'\n'`, `'\u{1F9F6}'` |
| string     | `"hello {name}"`, `r"C:\raw\{no interp}"`, `"""multi-line"""`, `b"bytes"` |
| array/list | `[1, 2, 3]`, `[0u8; 4096]` (repeat) |
| map        | `["x": 1.5, "y": 2.0]` |
| range      | `0..n`, `0..=n`, `..n`, `n..` |

Strings interpolate with `{expr}` or `{expr:spec}`. Write `{{` and `}}` for
literal braces. An interpolated string is a **template**. It allocates a
`String` only when it's stored somewhere that needs one. If you pass it
straight to `print`, a `Fmt`, or a `String.push_str`, it streams without a
temporary buffer. Format specs look like Python's: `{x:>8}`, `{pi:.3}`,
`{op:04X}`, `{val:?}` (debug form).

Multi-line `"""` strings strip the indentation shared by all their lines.

---

## Statements and newlines

A newline ends a statement. Use `;` to put several statements on one line.

### The continuation rule

A newline does **not** end the statement when:

1. the line ends inside `(...)` or `[...]`, or ends with a binary operator, `,`, `=`, or `=>`; **or**
2. the next line starts with `.`, `?`, or a binary operator (including `mend`)
   **and is indented deeper than the line where the statement began.**

Rule 2 lets method chains and dot-blocks both start lines with `.`:

```loom
let names = people                 // statement starts here
    .filter(_.age >= 18)           // deeper indent: continues the chain
    .map(_.name)

let win = Window(800, 600).{
    .title = "Loom"                // same indent as the next line:
    .resizable = true              // so these are two separate statements
}
```

Loom's formatter already produces this layout, so the rule simply formalizes
how people indent chains.

---

## Bindings

```loom
let x = 5                      // immutable
var count = 0                  // mutable
let (q, r) = divmod(17, 5)     // destructuring
let Point(x, y) = origin       // warp destructuring (positional or `x: px`)
const LIMIT = 64               // compile-time constant (evaluated as a card)
given log = Logger.stderr()    // ambient context for the rest of the block (see 03)
spool frame = Arena(1.mib())   // a named memory region (see 03)
```

Shadowing is allowed. A type annotation goes after the pattern: `let n: u16 = 7`.

---

## Functions

```loom
fn add(a: i32, b: i32) -> i32 {
    a + b                                // the last expression is the result
}

fn add(a: i32, b: i32) -> i32 = a + b    // expression body

fn connect(host: str, port: u16 = 80, timeout: Duration = 5.secs()) -> Conn ! NetError

fn save(data: [u8]) ! IoError            // "returns nothing, might fail": `-> ()` is omitted
```

The full shape of a signature is
`fn name[generics](params) -> Ret ! Err given ctx: Type where bounds`.

- **Named arguments.** Any parameter can be passed by name: `connect("db", timeout: 1.secs())`.
  Named arguments come after positional ones.
- **Parameter conventions**: `x: T` reads, `mut x: T` edits in place,
  `take x: T` consumes. Mutation is marked at the call site too:
  `sort(mut scores)`. Method receivers are the exception because
  `scores.sort()` is already clear. See [03-memory](03-memory.md).
- **Uniform call syntax.** `f(x, y)` can be written `x.f(y)`. Any free
  function is a method on its first parameter. Lookup order is in [02-types](02-types.md#ufcs-and-method-lookup).

### Lambdas

```loom
x => x * 2
(a, b) => a + b
(p: Point) => p.len()
x => {
    let y = x * x
    y + 1
}
```

Function types use the same arrow: `i32 => i32`, `(f64, f64) => bool`.
Lambdas that are passed as arguments borrow what they capture and can't
escape. To store or return a closure, allocate it, which moves its captures:
`^(x => x + offset)` of type `^(i32 => i32)`.

### Hole lambdas

A `_` inside an argument turns that argument into a lambda. Each `_` is a new
parameter, taken from left to right.

```loom
xs.map(_ * 2)                  // x => x * 2
xs.filter(_.age > 18)          // p => p.age > 18
xs.reduce(_ + _)               // (a, b) => a + b
pairs.sorted_by(_.1 > _.1)     // (a, b) => a.1 > b.1
xs.map(clamp(_, 0, 255))       // x => clamp(x, 0, 255)
type Even = i64 where _ % 2 == 0
```

**Rule:** a hole belongs to the innermost *argument* (or `where` operand) that
properly contains it. A bare `_` passed as an argument therefore makes the
enclosing call the lambda, which is why `clamp(_, 0, 255)` works. A hole never
reaches beyond one argument, so there are no Scala-style surprises. If you
need the same value twice, name it: `xs.map(n => n * n)`.

---

## Control flow

Everything is an expression. Blocks evaluate to their last expression.

```loom
let kind = if n < 0 { "negative" } else if n == 0 { "zero" } else { "positive" }

if user is some(u) and u.age >= 18 { greet(u) }      // `is` binds; `and` sees the bindings

match shape {
    .Circle(r) if r > 100 => "huge circle"
    .Circle(_) => "circle"
    .Rect(w, h) => "{w}×{h} rect"
}

for x in xs { ... }
for (i, x) in xs.indexed() { ... }
for mut x in scores { x *= 2 }                       // edit elements in place
while queue.pop() is some(job) { run(job) }
let found = loop { if ready() { break 42 } }         // `break` can carry a value
outer: for row in grid { for c in row { if c == '#' { break outer } } }
```

- `match` must be exhaustive. Arms are `pattern [if guard] => expr` and are
  separated by newlines or commas.
- `return`, `fail` (return an error, see [04](04-errors.md)), `break`, and `continue`
  all have type `never`, so they fit in any expression position.

---

## Operators

### Precedence, from tightest to loosest

| Level | Operators | Notes |
|-------|-----------|-------|
| 1 | `f(x)` `a[i]` `a.b` `a?` `p^` `v.{ }` | postfix |
| 2 | `-x` `~x` `^x` | negate, bitwise not, allocate |
| 3 | `as` `as%` `as\|` `as?` | conversions |
| 4 | `*` `/` `%` and `*%` `*\|` `*?` | |
| 5 | `+` `-` and `+%` `-%` `+\|` `-\|` `+?` `-?` | |
| 6 | `<<` `>>` | |
| 7 | `&` | bitwise and |
| 8 | `~` | bitwise xor (same symbol as unary not, like Go's `^`) |
| 9 | `\|` | bitwise or |
| 10 | `..` `..=` | ranges |
| 11 | `==` `!=` `<` `<=` `>` `>=` `is` | chainable |
| 12 | `not` | |
| 13 | `and` | |
| 14 | `or` | |
| 15 | `mend` | |
| 16 | `=>` | lambda, right-associative |

Assignments (`=`, `+=`, `+%=`, `-|=`, `~=`, …) are statements, not
expressions, so `if (x = y)` can't happen. As a convenience, the body of a
match arm or a lambda can be a single assignment, and it then has type `()`:
`0x1nnn => .pc = n`, `xs.each(x => total += x)`.

**Bitwise operators bind tighter than comparisons.** This fixes C's precedence
bug, so `flags & MASK == 0` means what it looks like it means.

**Chained comparisons.** `a < b <= c` means `a < b and b <= c`, and `b` is
evaluated only once. Mixing directions (`a < b > c`) is an error because it's
never what anyone meant.

`<~` is always a single token, so write `a < ~b` with a space.

### Overflow policy lives in the operator

| Suffix | Meaning | `200u8 + 100` |
|--------|---------|---------------|
| (none) | trap: overflow unravels, in debug and release builds | unravel |
| `%`    | wrap (two's complement) | `44` |
| `\|`   | saturate at the type's bounds | `255` |
| `?`    | checked: result is `?T` | `none` |

The suffixes apply to `+ - *` and to conversions: `300 as u8` unravels,
`300 as% u8` gives `44`, `300 as| u8` gives `255`, and `300 as? u8` gives `none`.
Shifts trap when the shift amount is ≥ the operand's width.

### Implicit widening and the expected type

The only implicit numeric conversion is **lossless integer widening**:
`u8 → u16`, `u12 → u16`, `u8 → i16`, and so on. There is no conversion between
signed and unsigned of the same width, and no conversion involving floats.

When the expected type of an arithmetic expression is known, its operands are
widened to that type *before* the operation:

```loom
let op: u16 = .mem[pc] << 8 | .mem[pc +% 1]   // u8s widen to u16 first, so the shift is fine
let cells = List.filled(' ', w * h)           // w, h: u16, and the product is computed in usize
```

---

## Patterns

| Pattern | Matches |
|---------|---------|
| `_` | anything, binds nothing |
| `x`, `mut x` | anything, binding it (`mut` borrows the place mutably, e.g. in `for`) |
| `42`, `'a'`, `"GET"`, `true` | a literal |
| `0..10`, `'a'..='z'` | a range |
| `(a, b)`, `[first, ..rest]` | tuples, slices |
| `.Circle(r)`, `.Rect(w: width, ..)` | a case of the expected type (`..` ignores the remaining fields) |
| `Point(x, y)` | a record, destructured |
| `some(x)`, `none`, `ok(v)`, `err(e)` | optionals and results |
| `e: IoError` | type test: matches if the value is an `IoError`, binding it as `e` (for unions and `any W`) |
| `p \| q` | either one; both sides must bind the same names |
| `0x8xy4`, `0b1??0_rrrr` | twill pattern (below) |

Patterns look through `^` pointers automatically.

### Twill patterns

A binary (`0b`), octal (`0o`), or hex (`0x`) literal used in a pattern may contain:

- digits, which must match exactly (in hex twill, the digits A–F **must be uppercase**)
- `_`, a separator that is ignored
- `?`, a don't-care digit
- **lowercase letters, which are captures**

Every digit with the same letter is concatenated, most significant first, into
one unsigned integer exactly that wide:

```loom
match instr {                       // instr: u16
    0x00E0 => clear_screen()
    0x1nnn => jump(n)               // n: u12
    0x8xy4 => add(x, y)             // x: u4, y: u4
    0xFx?? => misc(x)               // don't care about the low byte
}

match byte {                        // byte: u8
    0b0???_???? => ascii(byte)
    0b110a_aaaa => lead2(a)         // a: u5
    0b1110_aaaa => lead3(a)         // a: u4
    0b10??_???? => continuation()
    _ => invalid()
}
```

The pattern's width has to equal the scrutinee's width. The compiler lowers
twill arms to mask-and-compare tests, or a jump table when that's cheaper, and
checks them for exhaustiveness bit by bit. Delete the `_` arm and it tells you
which bit patterns are unhandled.

A twill pattern with no fixed digits can't fail, so it can be used with `let`.
Its width then becomes the expected type of the right-hand side:

```loom
let 0bc_ssss_ssss = a + b      // the pattern is 9 bits, so a + b is computed in u9
```

In expressions, a hex literal may use either case (`0xff`). In patterns,
lowercase letters are captures. The compiler warns when a capture letter is
also a hex digit (`0x00e0` captures `e`), because that's almost always a typo
for `0x00E0`.

---

## The leading dot

A leading `.` means "relative to the thing in context". `.name` resolves like this:

1. **Inside a dot-block**, it's a member of the block's subject.
2. **Inside a method** (and outside any dot-block), it's a member of `self`: `.x` means `self.x`.
3. **Where a type is expected**, in an expression or a pattern, it's a case,
   constant, or associated function of that type: `.Red` means `Color.Red`.

When rule 3 and rule 1 or 2 both find something, the compiler reports an
error and asks you to write `self.name` or `Type.name`. Patterns only use rule 3.

```loom
warp Circle {
    radius: f64
    fn scaled(self, k: f64) -> Shape = .Circle(.radius * k)
    //                                  ^ rule 3    ^ rule 2
}
```

## Dot-blocks

`expr.{ ... }` opens a block where `expr` is the subject:

- If `expr` is a **mutable place** (a `var`, a `mut` parameter, or a field
  reached through one of those), the block edits it **in place** and evaluates to `()`.
- Otherwise (a fresh value, or an immutable place of a `Copy` type), the block
  works on **that value**, or a copy of it, and **evaluates to the edited value**.

```loom
let win = Window(800, 600).{ .title = "Loom"; .show() }   // builder
let moved = origin.{ .x += 10 }                            // origin is immutable: yields an edited copy
player.{ .x += dx; .y += dy }                              // player is a var: edited in place
let copy = player.copy().{ .hp = 100 }                     // edited copy of a var, made explicit
```

Watch out: `let p2 = player.{ ... }` on a `var` gives `()`, and the compiler
says so and suggests `.copy()`.

---

## Attributes

`#name` or `#name(args)` go before an item: `#inline`, `#test`,
`#layout(C)`, `#layout(bits, big)`, `#deprecated("use foo")`.

## Modules and bolts

- A file is a module, and a directory is a module made of its files.
- `use std.fs.{File, read}`, `use app.parser as p`.
- Items are private to their module unless they're marked `pub`. Fields can be
  `pub(get)`, which makes them readable everywhere but writable only inside the module.
- A package is a **bolt**. Its manifest, `bolt.loom`, is a card program
  rather than a config format (see [06-cards](06-cards.md)).
