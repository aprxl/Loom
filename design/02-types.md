# 02 · Types

Loom is statically typed and uses local bidirectional inference. Every
parameter needs a type. Return types can be left out only on private,
expression-bodied functions.

---

## Primitive types

| Type | Notes |
|------|-------|
| `bool` | |
| `char` | a Unicode scalar value |
| `str` | a UTF-8 slice, borrowed or `@static` (see [03](03-memory.md#slices-and-static)) |
| `i1`…`i128`, `u1`…`u128` | **any bit width**. `u12`, `u4`, and `i24` are ordinary types |
| `isize`, `usize` | pointer-sized |
| `f16`, `f32`, `f64` | |
| `()` | unit |
| `never` | the type of `return`, `fail`, `unravel`, `break`, etc. |

**Bit-precise integers.** A `uN` does arithmetic at exactly N bits, so a
`u12` overflows at 4096. Outside a packed layout it's stored in the next
power-of-two number of bytes. Inside a `#layout(bits)` warp it takes exactly
N bits.

**Free bounds checks.** Indexing a `[T; N]` with a `uK` where `2^K <= N` can't
go out of bounds, so no check is emitted. That's why the CHIP-8 example stores
its registers in `[u8; 16]` and indexes them with `u4`.

**`uint(n)` / `int(n)`** are compile-time functions that return the type with
`n` bits, so `u12` is sugar for `uint(12)`. See [06-cards](06-cards.md).

## Built-in composites

| Syntax | Type |
|--------|------|
| `(A, B)` | tuple; fields are `.0`, `.1` |
| `[T; N]` | fixed-size array, stored inline |
| `[T]` | slice: a view of contiguous `T`s |
| `?T` | optional: `some(x)` or `none`. A `T` converts to `?T` implicitly |
| `T ! E` | result: `ok(v)` or `err(e)` (see [04](04-errors.md)) |
| `A => B` | function |
| `^T`, `^T@s` | pointer into a spool (see [03](03-memory.md)) |
| `*T` | raw pointer; only usable inside `selvage` |
| `any W` | a value of some type that weaves `W`, with dynamic dispatch |
| `A \| B` | anonymous tagged union, mostly used for error sets |

`List[T]`, `Map[K, V]`, `Set[T]`, and `String` come from `std` and aren't built in.

---

## Warps

A `warp` declares a data type. Its body can contain fields, cases, or both.

### Records

```loom
warp Point {
    x: f64 = 0          // defaults
    y: f64 = 0

    fn len(self) -> f64 = (.x * .x + .y * .y).sqrt()
    fn origin() -> Point = Point()      // no `self`: an associated function, called as `Point.origin()`
    const UNIT = Point(1, 0)
}

let p = Point(3, 4)                 // positional
let q = Point(y: 2)                 // named; x takes its default
```

**Construction is a function call.** Every warp has a memberwise constructor
that takes its fields in order, by position or by name. There are no
`Point { x: 1 }` literals, so an `if` condition can never be mistaken for a
struct literal. To provide custom construction, write associated functions
(`Point.polar(r, θ)`). The memberwise constructor is only available inside the
warp's module when the warp has private fields.

**Tuple warps and newtypes:** `warp Meters(f64)` is a record with one positional field.

### Sums (cases)

```loom
warp Shape {
    case Circle(radius: f64)
    case Rect(w: f64, h: f64)
    case Dot                        // no payload
}

let s = Shape.Circle(2.0)
let t: Shape = .Rect(w: 3, h: 4)    // leading dot: the case is inferred from the expected type
```

Several unit cases can share one line: `case North, South, East, West`.

### Sums with shared fields

Fields declared next to cases are shared by **every** case. You can read them
without a `match`.

```loom
warp Token {
    span: Span                      // every token has one

    case Ident(String)
    case Int(u64)
    case Eof
}

let t = Token.Int(42, span: Span(0, 2))   // shared fields are passed as named arguments
print(t.span.start)                        // no match needed
```

In Rust this would be a wrapper struct around an enum. Loom supports it
directly because ASTs, tokens, and events almost always have this shape.

### Visibility

Fields and members are private to the module by default. `pub` exports them.
**`pub(get)`** exports a field read-only, so other modules can see it but only
this module can assign to it.

### Copy and move

A warp whose fields are all `Copy` becomes `Copy` automatically, unless it's
`loose` or holds an owning pointer. Values of other types move when they're
passed to `take` parameters or assigned. Every non-loose type has an explicit
deep `.copy()`.

---

## Wefts

A `weft` is a set of behavior. It can have required methods, default methods,
associated types, and super-wefts.

```loom
weft Widget: Show {                 // anything that's a Widget must also be Show
    fn size(self) -> (u16, u16)
    fn draw(self, at: Pos, c: mut Canvas)
    fn name(self) -> str = "widget" // default
}

weft Iter {
    type Item
    fn next(mut self) -> ?Item
}

weft From[T] {
    fn from(take x: T) -> Self
}
```

Operators are wefts: `Add`, `Sub`, `Eq`, `Ord`, `Index` (see projections in
[03](03-memory.md#projections)), `Show` (used by interpolation), `Hash`, and `Iter`.

**Wefts used as parameter types are implicit generics.** `fn show_all(xs: [Show])`
means `fn show_all[T: Show](xs: [T])`, which is monomorphized with static
dispatch. For dynamic dispatch, write `any Show`.

---

## Weaves

A weave connects a weft to a warp. It can be written two ways.

**Inline**, OOP-style: list the weft in the warp's header and implement its
methods in the body.

```loom
warp Circle: Shape {
    radius: f64
    fn area(self) -> f64 = PI * .radius * .radius
}
```

**Separately**, typeclass-style, and possibly in a different module from the warp:

```loom
weave Shape into Rect {
    fn area(self) -> f64 = .w * .h
}

weave[T: Show] Show into List[T] {       // generic weave
    fn show(self, f: mut Fmt) { ... }
}
```

**Orphan rule:** `weave W into T` is only allowed when `W` or `T` belongs to
your bolt.

**Deriving.** When a warp lists a weft in its header but leaves out some
required methods, the weft's default bodies have to cover them. Defaults can
inspect `Self` at compile time, so "derive" isn't a special feature. It's just
a well-written default method. See [06-cards](06-cards.md#derive-is-just-a-default-method).

```loom
warp User: Eq, Hash, Show, ToJson { name: String, age: u8 }   // all four come from defaults
```

---

## `through`: delegation instead of inheritance

```loom
warp Button: Widget {
    through frame: Frame
    label: String

    fn draw(self, at: Pos, c: mut Canvas) {       // override
        .frame.draw(at, mut c)                     // "super" is just the field
        c.text(at.{ .x += 2; .y += 1 }, .label)
    }
}
```

The rules:

1. Every **method** of the `through` field (inherent or from a weave) can be
   called on the outer warp, with `self` rebound to the field. **Fields are not
   forwarded**, so the inner object's state stays encapsulated and you write
   `button.frame.w`.
2. A method defined on the outer warp takes priority over a forwarded one.
3. Forwarded methods count toward the outer warp's weft conformance. That's
   how `Button: Widget` gets `size()` without writing it.
4. If two `through` fields both provide a method, it's an error, unless the
   outer warp defines the method itself.

There's no subtyping. A `Button` is not a `Frame`. Where a `Frame` is needed,
pass `button.frame`.

---

## UFCS and method lookup

`x.f(args)` is resolved by trying these in order:

1. a method defined in `x`'s warp body
2. a method from a weave into `x`'s type, where the weft is in scope
3. a method forwarded through a `through` field
4. a free function in scope whose first parameter accepts `x`

If two candidates at the same level match, it's an ambiguity error. Qualify it
as `Weft.f(x)` or `module.f(x)`. `x.f` without parentheses is always field
access, and methods are never called implicitly.

Rule 4 is how Loom gets extension methods without special syntax:

```loom
fn kib(n: usize) -> usize = n * 1024
let size = 64.kib()                    // same as kib(64)
```

---

## Generics

```loom
fn largest[T: Ord + Copy](xs: [T]) -> ?T {
    var best: ?T = none
    for x in xs { if best is some(b) and b >= x { continue }; best = x }
    best
}

warp Pair[A, B] { first: A, second: B }

warp Matrix[card R: usize, card C: usize] { cells: [f32; R * C] }

fn describe[T](x: T) -> String where T: Show + Hash = "{x} #{x.hash():x}"
```

- Generics are monomorphized.
- Type parameters are really **card parameters** of type `type`. `[T: Ord]` is
  sugar for `card T: type where T: Ord`, with `T` inferred from the arguments.
  Value parameters such as `card N: usize` work the same way.
- To give explicit type arguments, use brackets: `s.parse[u16]()`, `List[i32]()`.

---

## Dynamic dispatch: `any W`

`any Widget` is a value of *some* type that weaves `Widget`. It's two words:
the data and a weave table. Small values (≤ 2 words) are stored inline, and
larger ones are boxed in the ambient spool.

```loom
var ui: List[any Widget] = [Label("hi"), Button.sized("OK", 10)]   // coerced implicitly
for w in ui {
    if w is b: Button { print("button {b.label}") }                // type-test pattern
}
```

To require several wefts: `any (Widget + Show)`.

---

## Gauges

A **gauge** is a numeric type limited to a range or a predicate.

```loom
type Port      = u16 where 1..
type Percent   = f32 where 0.0..=1.0
type Cents     = i64 where 0..
type Even      = i64 where _ % 2 == 0      // predicates are hole lambdas
```

- A gauge is a distinct nominal type. It widens implicitly to its base type
  (`Port → u16`).
- **Into the gauge:**

  | Form | Out-of-range behavior |
  |------|-----------------------|
  | literal / constant | compile error |
  | `x as Port` | unravel |
  | `x as? Port` | gives `?Port` |
  | `x as\| Port` | clamps (range gauges only) |
  | `if x is Port { … }` | narrows `x` to `Port` inside the branch |

- **Arithmetic** on range gauges keeps track of intervals. `Cents + Cents` has
  interval `0..`, which still fits `Cents`, so no cast is needed. `Cents - Cents`
  has interval `..`, so the result is plain `i64`. Overflow still traps as usual.
- The optimizer uses gauges. Indexing `[T; 32]` with a `u8 where 0..32` skips
  the bounds check.

---

## Unions

`A | B` is an anonymous tagged union of distinct types. It's mostly used for
errors: `Config ! (IoError | ParseError)`. A value of a member type converts to
the union implicitly. To match on one, use type patterns:

```loom
match err {
    e: IoError => retry(e)
    e: ParseError => report(e)
}
```

Unions are flat and unordered, so `(A | B) | A` is the same type as `A | B`.
