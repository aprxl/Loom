title: Warps, wefts and weaves
summary: Data types, behavior types, and how they are joined.
---
Here is the idea the language is named for. A cloth is a **warp** (the threads held taut on the frame) crossed by a **weft** (the thread that runs through them), and a **weave** is the pattern that joins the two. In Loom:

- a **warp** declares data: a record, a set of cases, or both;
- a **weft** declares behavior: a list of methods, some with default bodies;
- a **weave** says that a warp has a weft's behavior, and gives the methods.

There is no inheritance and no class hierarchy to plan up front. Behavior is attached to data from the outside, which is why the same warp can be given a new weft years later, in another module. This chapter takes the pieces one at a time: records, methods, cases, wefts, weaves, deriving, and `through`, which is how a warp reuses another's behavior without being its child.

## Records

A warp with fields is a record. Fields can have defaults. **Construction is a call**: `Point(3, 4)` takes the fields in order, and `Point(y: 2)` names one and leaves the rest at their defaults. There are no `Point { x: 1 }` literals, so `if x == Foo {` can never be mistaken for one.

@sample types_records

- A named argument with nothing after the colon passes the variable of that name: `Point(x:, y:)` is `Point(x: x, y: y)`, and `Window(title:, height: 480)` mixes the two. In a pattern it binds: `let Point(x:, y:) = p`.
- `warp Meters(f64)` is a **tuple warp**: a record with positional fields, read as `.0`, `.1`. It has no body; give it behavior with a `weave`. A warp with no fields at all is written `warp Marker`.
- **`Show`** (what `{p}` and `print` use) is listed in the warp's header, and the compiler writes it. Any warp can be shown in its debug form with `{p:?}`, `Show` or not.
- A warp whose fields are all `Copy` is `Copy` too: assigning it copies. One that holds a `String` or a list moves instead, as [Moves, `once` and `defer`](page:ownership) explains.
- Fields are private to their module unless marked `pub`. `pub(get)` makes a field readable everywhere and writable only inside its module.

@try Add `let a = Point(1, 2, 3)`, then `Point(z: 1)`, then `Window()`. Each is an error that names the problem: [T0104](diag:T0104) for one value too many, [T0105](diag:T0105) for a field that doesn't exist, and T0104 again for `title`, which has no default.

@compare
-- Rust
Rust has struct literals instead of constructor calls, and defaults come from the `Default` trait, all fields at once, through struct update. Loom's defaults are per field and the constructor takes them by name.
```rust
#[derive(Debug, Default)]
struct Point { x: f64, y: f64 }

let q = Point { y: 2.0, ..Default::default() };
```
-- Kotlin
A Kotlin `data class` is the closest thing: a constructor call, defaults per parameter, named arguments. It also generates `equals`, `hashCode` and `toString`, which Loom leaves to the header.
```kotlin
data class Window(val title: String, val width: Int = 800, val height: Int = 600)

val w = Window("notes", height = 480)
```
-- Python
A `dataclass` gives the same call syntax. Nothing stops a later assignment to a wrong type, though; Loom's fields are typed at every use.
```python
from dataclasses import dataclass

@dataclass
class Window:
    title: str
    width: int = 800
    height: int = 600

w = Window("notes", height=480)
```
@end

## Methods and the leading dot

Methods live in the warp's body. A method takes `self`; a function without `self` is an **associated function**, called on the type: `Point.origin()`. A `const` in the body is an associated constant. `fun shift(mut self, ...)` may change the value it's called on, and then the value has to be a `var`.

Inside a method you rarely need to write `self`. A leading dot means "relative to the thing in context", so `.x` is `self.x`. It is a small notation that pays for itself in code full of field access.

@sample types_methods

The leading dot has three jobs:

- **In a method**, `.x` is `self.x` and `.0` is `self.0`.
- **In a dot-block**, `expr.{ ... }` opens a block whose subject is `expr`, so `.x` is the subject's field. On a `let` (or any fresh value) the block gives back the *edited value*: `p.{ .x += 10 }` leaves `p` alone. On a `var` it edits in place and gives `()`.
- **Where a type is expected**, it's one of that type's members: `.UNIT` is `Point.UNIT`, and `.Red` is `Color.Red`.

If a name is found by more than one of these rules the compiler asks you to write `self.name` or `Type.name` instead of guessing.

@try Change `p.{ .x += 10 }` to `moving.{ .x += 10 }`. Because `moving` is a `var`, the block edits it in place and returns `()`, so `moved` is `()` and `print("{moved}")` stops the build ([T0108](diag:T0108)): `()` isn't `Show`. Use `moving.copy().{ ... }` when you want an edited copy of a variable.

@compare
-- Rust
Rust's `impl` block holds methods and associated functions, and `self` is spelled out on every field. `&mut self` is Loom's `mut self`.
```rust
impl Point {
    fn len(&self) -> f64 { (self.x * self.x + self.y * self.y).sqrt() }
    fn shift(&mut self, dx: f64) { self.x += dx; }
    fn origin() -> Point { Point { x: 0.0, y: 0.0 } }
}
```
-- Swift
Swift also drops `self.` inside methods, and uses `mutating` where Loom uses `mut self`. Swift's structs copy on assignment, like Loom's `Copy` warps.
```swift
struct Point {
    var x = 0.0, y = 0.0
    func len() -> Double { (x * x + y * y).squareRoot() }
    mutating func shift(_ dx: Double) { x += dx }
    static func origin() -> Point { Point() }
}
```
-- Kotlin
Kotlin's `apply { ... }` and `with` open a block with a receiver, the nearest relative of a dot-block. A Loom dot-block can also yield an edited copy when the subject is immutable.
```kotlin
val win = Window("notes").apply { width = 1024 }   // width is a var
```
@end

## Cases

A warp with `case` lines is a **sum type**: a value is exactly one of the cases, and each can carry its own fields. `match` takes it apart, `is` tests it, and a leading dot names a case where the type is known (`.Dot`). Several payload-free cases can share a line: `case North, South, East, West`.

Fields written beside the cases belong to **every** case and can be read without a `match`. ASTs, tokens and events nearly always have this shape: a position plus one of several kinds of thing.

@sample types_cases

- A case is constructed like a call: `Shape.Circle(2.0)`. Where the expected type is known, `.Rect(w: 3, h: 4)` is enough. A payload-free case needs no parentheses.
- Shared fields are passed by name when you construct: `Token.Number(42, line: 7)`.
- `if t is .Number(n) and n > 40` tests and binds in one step, and `and` can use `n`.
- A `match` over a warp with cases must be exhaustive.

### The compiler counts the cases

A `match` that leaves a case out is an error, and the compiler says which one:

@sample types_err_match

That is the reason to prefer cases over a field with a tag: add a `case` later and the compiler walks you to every `match` that needs a new arm.

@try Add `case Triangle(base: f64, height: f64)` to `Shape` in `types_cases` and run it. The `area` method stops compiling until you give it an arm. Then put `_ => "something else"` in place of the arms of the last `match`: it compiles, and from now on a new case there is silently "something else". That is the price of a catch-all.

@compare
-- Rust
Rust's `enum` is the same sum type. Fields common to every variant need a wrapper struct around the enum, which is the shape Loom lets you write directly.
```rust
enum TokenKind { Word(String), Number(i64), End }
struct Token { line: u32, kind: TokenKind }
```
-- Kotlin
A `sealed` hierarchy gets the same exhaustive `when`, but each case is a separate class, and a shared field goes on the base class or the interface.
```kotlin
sealed interface Shape {
    data class Circle(val radius: Double) : Shape
    data class Rect(val w: Double, val h: Double) : Shape
    data object Dot : Shape
}
```
-- TypeScript
A discriminated union is the idiom. The tag (`kind`) is a field you maintain by hand, and exhaustiveness comes from a trick with `never`; Loom's cases carry their own tag.
```typescript
type Shape =
  | { kind: "circle"; radius: number }
  | { kind: "rect"; w: number; h: number }
  | { kind: "dot" };
```
-- Haskell
Algebraic data types are where `case` and `match` come from. The declaration is nearly the same.
```haskell
data Shape = Circle Double | Rect Double Double | Dot
```
@end

## Wefts

A weft lists methods. One with a body is a **default**; one without is a requirement that each warp provides. A warp joins a weft **inline** by listing it in its header (`warp Circle: Shape`) and writing the methods in its body, or **separately** with `weave`, which can live in another module.

@sample types_wefts

- A weft used as a parameter type, like `describe(s: Shape)`, accepts anything that weaves it. This is a generic in disguise; [Generics and `any`](page:generics) shows what it means.
- `Shape.name(x)` calls a weft's method explicitly. A default method is type-checked for each warp that uses it.
- Weaving is explicit. Unlike Go's interfaces, a type doesn't satisfy a weft by accident of having the right methods; someone has to say so.
- Operators are wefts too: `==` is `Eq`, `<` is `Ord`, `{x}` is `Show`, and `for` loops over `Iter`.
- A weft can take type parameters, `weft Convert[T]`, and a warp can weave it for several: [Generics and `any`](page:generics#wefts-with-type-parameters).

### Wefts that build on wefts

A weft can require another (`weft Greeter: Named`), and can name a type that each weave fills in (`type Item`). Whoever weaves `Greeter` must weave `Named` too. A required weft the warp doesn't mention is derived when every method it has has a default; otherwise the compiler names the missing one ([T0108](diag:T0108)).

@sample types_supers

An associated type is read from outside as `C.Item`, the way `first_or` does. The standard library's `Iter` is exactly this shape, with `type Item` and a `next`; see [std.iter](std:iter#Iter).

@try Remove `Named` from `Cat`'s header: `warp Cat: Greeter`. The compiler says a `Greeter` needs a `Named` as well, and suggests a weave.

@compare
-- Rust
A trait is a weft, and `trait Greeter: Named` is the supertrait. Rust's `impl Trait for Type` is always separate from the type; Loom lets you pick inline or separate.
```rust
trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> &str { "shape" }
}
struct Circle { radius: f64 }
impl Shape for Circle {
    fn area(&self) -> f64 { 3.0 * self.radius * self.radius }
}
```
-- Go
Go's interfaces are structural: a type satisfies one by having the methods, with no declaration. That makes them easy to retrofit and easy to satisfy by accident. Loom's are nominal, and there are no default methods in a Go interface.
```go
type Shape interface{ Area() float64 }

type Circle struct{ Radius float64 }
func (c Circle) Area() float64 { return 3 * c.Radius * c.Radius }
```
-- Swift
A protocol with an extension for defaults is nearly the same thing, including associated types and protocol inheritance.
```swift
protocol Shape { func area() -> Double }
extension Shape { func name() -> String { "shape" } }

struct Circle: Shape {
    var radius: Double
    func area() -> Double { 3 * radius * radius }
}
```
-- Haskell
A type class is a weft: required methods, defaults, superclasses. A class instance is a weave. Haskell resolves instances by type alone, which is why it can have at most one per type.
```haskell
class Shape a where
  area :: a -> Double
  name :: a -> String
  name _ = "shape"

instance Shape Circle where
  area (Circle r) = 3 * r * r
```
@end

## Weaving by hand

A weave adds a weft to a warp after the fact. That includes the wefts every program uses: writing `weave Show into Money` is how `{price}` learns to print a `Money`, `weave Eq` is what `==` calls, and `weave Ord` is what `<`, `sort` and `max` use.

@sample types_weaves

- `weave W into T { ... }` is allowed only when `W` or `T` is defined in your program. Weaving a library weft into a library type, like `weave Show into i32`, is rejected ([T0115](diag:T0115)); wrap the type in a warp of your own and weave that.
- A weave can be empty: `weave Show into Meters {}` is how a tuple warp gets a derived `Show`, `Eq` or `Ord`. See [Deriving](page:types#deriving).
- Generic weaves (`weave[T: Show] Twice into List[T]`) are in [Generics and `any`](page:generics#generic-weaves).
- `Show` writes into a `Fmt` with `f.put(...)`. A width or alignment in the template (`{price:>10}`) still pads what `show` wrote.

@try Add `weave Show into i32 { fun show(self, mut f: Fmt) { f.put("int") } }` to the sample and read the error. Then remove `Money`'s `Eq` weave and try `price == Money(1234)`: the compiler says `Money` doesn't weave `Eq`.

@compare
-- Rust
Rust's `impl Trait for Type` is the same move, with the same orphan rule: you may implement a trait for a type only if one of them is yours.
```rust
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "${}.{:02}", self.cents / 100, self.cents % 100)
    }
}
```
-- Swift
An `extension` adds protocol conformance to a type you don't own. Swift allows it between two foreign pieces too, as a "retroactive" conformance, and warns because two libraries might both add it.
```swift
extension Money: CustomStringConvertible {
    var description: String { "$\(cents / 100).\(cents % 100)" }
}
```
-- Kotlin
Kotlin can add extension functions to a class, but not make it implement an interface after the fact. That is what a weave does and an extension doesn't.
```kotlin
fun Money.show() = "\$${cents / 100}.${cents % 100}"
```
@end

## Deriving

List `Show`, `Eq`, `Ord` or `Hash` in a warp's header and leave the methods out, and the compiler supplies them:

- `Show` writes the debug form, less any fields every case shares: `Version(major: 1, minor: 4)`.
- `Eq`: every field is equal and, for a warp with cases, it's the same case.
- `Ord`: fields compare in order and the first difference decides. Earlier cases come first.
- `Hash` makes a warp usable as a map key. `Error` gets a `message` that is its `Show` form.

A derived method needs every field to weave the weft too.

@sample types_derive

Right now these four are built into the compiler. They're meant to become ordinary default methods that inspect the type with [reflection](page:reflection), the way `ToJson` already does, so that deriving is not a special feature but a well-written default.

@try Give `Version` a field `level: Level`, then pass it in each `Version(...)` call. `Version` derives `Hash`, but `Level` doesn't: the compiler says it can't derive `Hash` ([T0108](diag:T0108)) and points at the field. Add `Hash` to `Level`'s header and it works again.

@compare
-- Rust
`#[derive(...)]` is the same convenience, written as an attribute. Rust's `Ord` also requires `Eq` and `PartialOrd` spelled out; Loom's one `Ord` weft covers all three of `<`, `<=` and `sort`.
```rust
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Version { major: u32, minor: u32 }
```
-- Haskell
`deriving` is the original. Constructors compare in declaration order and fields left to right, as in Loom.
```haskell
data Level = Low | Mid Int | High deriving (Show, Eq, Ord)
```
-- Python
A `dataclass` generates methods when asked. `order=True` compares as tuples of fields; `frozen=True` makes instances hashable.
```python
@dataclass(order=True, frozen=True)
class Version:
    major: int
    minor: int
```
@end

## `through`

`through` forwards a field's methods, so a warp can reuse another's behavior without inheriting from it. The rules are short:

- Every method of the `through` field, its own or woven, can be called on the outer warp.
- **Fields are not forwarded.** The inner warp's state stays inside; you write `button.frame.w`.
- A method the outer warp defines wins over a forwarded one. Inside it, `.frame.describe()` is the "call super".
- Forwarded methods count toward a weft the outer warp lists: a `Sticker` that writes nothing is still a `Describe`.
- There is no subtyping. A `Button` is not a `Frame`; where a `Frame` is needed, pass `button.frame`.

@sample types_through

@try Delete `Button`'s own `describe`. Now `b.describe()` is `Frame`'s, because the forwarded one is all that's left. Then restore it and add a second `through` field whose warp also has a `describe`: the compiler says `describe` comes through more than one field ([T0116](diag:T0116)) and asks you to write it yourself.

@compare
-- Go
Go's struct embedding is the closest idea, and does more: it promotes the embedded type's *fields* as well as its methods, so `b.W` works. Loom forwards methods only, keeping the inner state out of reach.
```go
type Button struct {
    Frame
    Label string
}
b.Size() // promoted method
b.W      // promoted field
```
-- Kotlin
Class delegation (`by`) is the same for interfaces: the compiler writes the forwarding methods. It works for an interface only, so there is no `by` for a concrete class.
```kotlin
class Button(val frame: Frame, val label: String) : Describe by frame {
    override fun describe() = "button '$label' over ${frame.describe()}"
}
```
-- Rust
Rust has no delegation syntax. You forward by hand, or pull in a crate (`delegate`, `ambassador`). Implementing `Deref` to get the inner methods is the common shortcut, and it is widely considered a misuse because it looks like inheritance.
```rust
impl Describe for Button {
    fn describe(&self) -> String { self.frame.describe() }
}
```
@end

## Method lookup and UFCS

`x.f(args)` looks for `f` in this order:

1. a method in `x`'s warp body;
2. a method from a weave into `x`'s type;
3. for something with items that isn't a stream (an array, list, slice, range, or a type that weaves `Iter`), a method of `Stream`;
4. a method forwarded through `through`;
5. a free function in scope whose first parameter accepts `x`.

The first level with a match wins. That's the whole of the rule, and it's why a method you write in the body is never shadowed by a weft's default. The last step is Loom's **uniform call syntax**: any free function is a method on its first parameter, so `64.kib()` is `kib(64)`. It's how a library gets extension methods without special syntax.

@sample types_ufcs

`x.f` without parentheses is always a field, and methods are never called implicitly.

### When two methods match

Two matches at the same level are an ambiguity error. Weaving two wefts that both have `hi` into the same warp is fine until someone calls `g.hi()`:

@sample types_err_ambiguous

The compiler says which candidates it found. Write `Hello.hi(g)` (the weft) or `module.f(x)` (a free function) to choose.

@try Add `fun area(c: Circle) -> f64 = 0.0` to the sample and run it. `c.area()` still prints `12`: the weave's method is found at level 2, the free function waits at level 5.

@compare
-- Rust
Rust has no free-function-as-method: `kib(64)` and `64.kib()` are different things, and an extension method needs a trait.
```rust
trait Kib { fn kib(self) -> usize; }
impl Kib for usize { fn kib(self) -> usize { self * 1024 } }

64usize.kib();
```
-- Kotlin
Extension functions are the same idea, with the same rule: a member always beats an extension. Kotlin makes you declare the receiver; Loom's is just the first parameter.
```kotlin
fun Int.kib() = this * 1024

64.kib()
```
-- C#
Extension methods need a static class and a `this` marker. Like Loom's, they lose to a real method of the same name.
```csharp
static class Sizes {
    public static int Kib(this int n) => n * 1024;
}

64.Kib();
```
@end

## Where next

[Generics and `any`](page:generics) covers type parameters and dynamic dispatch, and what it means to use a weft as a parameter type.
