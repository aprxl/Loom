title: Generics and `any`
summary: Type parameters checked per instance, wefts as parameter types, and dynamic dispatch.
---
Sooner or later you write the same function twice: once for `i32`, once for `f64`, once for the warp you just invented. Generics let you write it once and say what it needs from the types it works on. Loom's generics are **checked per instance** and compiled to specialized code, so a generic function is as fast as the hand-written one.

Sometimes the types aren't known until the program runs: a list of widgets of different kinds, each with its own `name`. That's what `any W` is for, with calls that go through a table. This chapter covers both and how to choose.

## Generic functions

A type parameter goes in brackets after the function's name, with bounds after a colon: `T: Ord + Copy` means "any `T` that weaves `Ord` and `Copy`". The same bounds can go in a `where` clause after the signature. The compiler infers the arguments where it can, and you give them yourself with brackets: `largest[i32]([])`, `List[str]()`, `s.parse[u16]()`.

@sample generics_basic

- A bound is a weft. Several are joined with `+`.
- With no bound, the body can still use the type's operators if the types you call it with support them, and the compiler tells you when one doesn't: see the next section.
- A bound can be a `where` clause: `fun twice[T](x: T) -> String where T: Show`.
- Value parameters (`const N: usize`) work the same way: see [Const parameters](page:const-params).

@try Change `largest([2.5, 0.5])` to `largest([true, false])` and read the error: `bool` doesn't weave `Ord` ([T0108](diag:T0108)). Then call `largest[u8]([300])`, and notice that the literal is checked against the type you named.

@compare
-- Rust
Rust checks a generic against its bounds once, where it's defined: the body can only use what the bounds promise, for every `T`. The call sites are then simply correct or not.
```rust
fn largest<T: Ord + Copy>(xs: &[T]) -> Option<T> {
    let mut best = None;
    for &x in xs {
        if let Some(b) = best { if b >= x { continue; } }
        best = Some(x);
    }
    best
}
```
-- Go
Go's type parameters are constrained by interfaces such as `cmp.Ordered`, checked at the definition like Rust's. Explicit instantiation uses square brackets, as in Loom (`Largest[int](xs)`).
```go
func Largest[T cmp.Ordered](xs []T) (best T, ok bool) {
    for _, x := range xs {
        if !ok || x > best {
            best, ok = x, true
        }
    }
    return
}
```
-- TypeScript
TypeScript's generics are erased: they only exist for the type checker, and `T extends ...` is the bound. Loom produces real specialized code for each instance.
```typescript
function largest<T extends number | string>(xs: T[]): T | undefined {
  return xs.reduce<T | undefined>((b, x) => (b === undefined || x > b ? x : b), undefined);
}
```
@end

## Checked per instance

Loom type-checks a generic function **once for each list of arguments it's used with**, in the style of Zig (and C++ templates, before concepts). `largest` above is checked once for `i32`, once for `str` and once for `f64`. A generic function nobody calls isn't checked at all.

The bounds are checked first, at the call: that is where "this type doesn't weave `Ord`" belongs, and it says so. After that the body is checked with the real type plugged in, and an error in the body carries a note naming the instance it came from.

@sample generics_err

There are two errors, and they differ. The first is at the call of `smallest`, whose bound is spelled out: the message says what `T` was asked to be and what it is. The second is inside `biggest`, which promised nothing: `biggest([4, 2])` is fine, because `i32` can be compared, but for `Blob` the `>=` in the body has nothing to compare, and the note says `in biggest with T = Blob`. The first error is the one you want to read.

> Write the bounds you mean. Per-instance checking is what lets a generic body use any operator the instance supports, but it also means that a missing bound is found by whoever happens to call it with the wrong type. Bounds are the documentation of what a function needs.

@try Change `biggest`'s bounds to `T: Show + Ord + Copy`. The error inside the body is gone: now both calls fail at the call, with the message `smallest` already gave. Then delete the `Blob` line and see that `biggest([4, 2])` was never in doubt.

@compare
-- Rust
Rust would reject `biggest` with no `Ord` bound before any call exists, with error E0369 ("binary operation `>=` cannot be applied to type `T`"). The price of that guarantee is that every operation a generic uses must be written into its bounds.
```rust
fn biggest<T: Copy>(xs: &[T]) -> Option<T> {
    // error: `>=` cannot be applied to `T` -- add `T: Ord`
}
```
-- C++
Templates are checked per instance, and always have been: the error shows up at the instantiation, deep inside the template, as pages of notes. C++20 concepts add the Loom-style bound at the call site.
```cpp
template <std::totally_ordered T>
std::optional<T> biggest(std::span<const T> xs);
```
-- Zig
Zig's `comptime` generics are the model: a generic function is an ordinary function whose type parameter is a compile-time value, and it's checked when instantiated.
```zig
fn largest(comptime T: type, xs: []const T) ?T {
    var best: ?T = null;
    for (xs) |x| {
        if (best == null or x > best.?) best = x;
    }
    return best;
}
```
@end

## Generic warps

A warp takes type parameters too, and so do its methods, which are compiled per list of arguments just as functions are. Name the arguments (`Stack[str]()`), or leave them for the compiler to infer from the context or from the values you construct with: `Pair(1, "one")` is a `Pair[i32, str]`.

@sample generics_warps

- A method can add a bound of its own with `where`, needed only if that method is used: `peek` wants `T: Copy`, but the rest of `Stack` doesn't.
- `Stack[T]: Show` is derived like any other, and shows its fields in their debug form: the strings in `words` are quoted.
- The standard library's `List[T]`, `Map[K, V]`, `Set[T]` and `Stream[T]` are generic warps, nothing special.

@try Change `Stack[i32]` to `Stack[f64]` in the first declaration. The pushes of `1` and `2` still work, since a literal becomes whatever the context asks for. Then push a `"text"` and see the error.

@compare
-- Rust
Rust's generic structs are the same, with impl blocks that spell out the bounds a method needs.
```rust
struct Stack<T> { items: Vec<T> }

impl<T: Copy> Stack<T> {
    fn peek(&self) -> Option<T> { self.items.last().copied() }
}
```
-- Java
Java generics are erased, so a `Stack<int>` isn't possible: you box it as `Stack<Integer>`. Loom's `Stack[i32]` holds plain numbers.
```java
class Stack<T> {
    private final List<T> items = new ArrayList<>();
    void push(T x) { items.add(x); }
}
```
@end

## Generic weaves

A weave can be generic too, and then it applies to a whole family of types at once. `weave[T: Show] Twice into List[T]` gives every `List[T]` a `twice` method, as long as its items can be shown. A weave can also name one instance only: `weave Total into List[i32]`.

@sample generics_weaves

The brackets go straight after `weave`, because they introduce the parameters the rest of the line uses. The standard library uses the same form: `weave[T] FromStream into List[T]` is how a list is built from a stream.

@try Call `names.total()`. There is no `Total` for `List[str]`, so the method doesn't exist. Today the compiler reports a missing method as [U0102](diag:U0102), "not implemented yet"; read it as "no such method". The same goes for `.twice()` on a list whose items aren't `Show`.

@compare
-- Rust
A generic impl is the same: `impl<T: Display> Twice for Vec<T>`. Rust's coherence rules decide which overlapping impls are allowed; Loom's per-instance lookup is simpler but also allows fewer.
```rust
impl<T: fmt::Debug> Twice for Vec<T> {
    fn twice(&self) -> String { format!("{0:?}{0:?}", self) }
}
```
-- Haskell
A conditional instance is the closest: `instance Show a => Twice [a]`.
```haskell
class Twice a where twice :: a -> String
instance Show a => Twice [a] where
  twice xs = show xs ++ show xs
```
@end

## Wefts as parameter types

A weft in a parameter's type position is an **implicit generic**. `fun loud(a: Animal)` means `fun loud[A: Animal](a: A)`, and `fun show_all(xs: [Show])` means `fun show_all[T: Show](xs: [T])`. Every use has static dispatch: `loud(Dog())` and `loud(Cat())` are two specialized functions.

@sample generics_params

Write the generic out when you need to name the type twice, as `duet` does: `a: Animal, b: Animal` would let the two be different animals, while `A` makes them the same type. Inside the brackets (`[Show]`), the weft applies to the items, so the list is homogeneous: all `i32`, or all `str`, but not both.

> Wefts as parameter types is Loom's `impl Trait` in argument position.

@try Pass `loud` something that doesn't weave `Animal`, like `loud(42)`. The error names the weft the argument was missing. Then change `show_all([1, 2, 3])` to `show_all([1, "two"])`: one list, one item type.

@compare
-- Rust
`impl Trait` in argument position is the same sugar for a generic, with static dispatch. The explicit form is `fn loud<A: Animal>(a: &A)`.
```rust
fn loud(a: &impl Animal) -> String { a.speak().to_uppercase() }
```
-- Swift
`some Protocol` is Swift's version: the caller picks the concrete type, and it's resolved at compile time.
```swift
func loud(_ a: some Animal) -> String { a.speak().uppercased() }
```
@end

## `any W`: dynamic dispatch

A list of `Dog`s and `Cat`s can't be a `List[A]` for any one `A`. For that, write `any Animal`: a value of *some* type that weaves `Animal`, with the type forgotten until run time. A value converts to `any W` wherever one is expected, and a call on it goes through a table of that type's methods.

@sample generics_any

`ui` holds three values of two types. `width` isn't written in `Spacer`, so the table for `Spacer` points at the weft's default.

- Conversion is implicit: a `Label` goes into a `List[any Widget]` without a wrapper.
- Calls cost an indirect jump instead of a static call. Use a weft as a parameter type when the caller knows the type; use `any` when a collection or a field has to hold values of different types.
- `any Show` works for the builtin wefts too: `List[any Show]` can hold numbers, text and warps together.
- A `mut self` method of a scalar type can't be called through `any` yet ([Not supported yet](page:not-yet#types-and-library)).

@try Add `warp Button: Widget { fun name(self) -> String = "button" }` and a `Button()` in the list. The loop prints it with no other change. Then delete `Spacer`'s `name`: the compiler says `Spacer` doesn't have the `name` that `Widget` needs ([T0113](diag:T0113)).

@compare
-- Rust
`Box<dyn Trait>` is the same dispatch, and has the same indirect call. `dyn` values are always behind a pointer; Loom's `any` is a value, and where it lives is up to the compiler.
```rust
let ui: Vec<Box<dyn Widget>> = vec![Box::new(Label::new("hi")), Box::new(Spacer)];
for w in &ui {
    println!("{}", w.name());
}
```
-- Swift
Swift 5.6 spells the idea `any` as well: an existential, a value of some type that conforms to a protocol.
```swift
let ui: [any Widget] = [Label(text: "hi"), Spacer()]
for w in ui { print(w.name()) }
```
-- Go
A Go interface value is dynamic dispatch with no ceremony: any value with the right methods converts implicitly. Until generics arrived in Go 1.18 it was the only way to write polymorphic code, so it is idiomatic everywhere; in Loom it is the less common form.
```go
ui := []Widget{Label{"hi"}, Spacer{}}
for _, w := range ui {
    fmt.Println(w.Name())
}
```
-- Java
Every Java object reference to an interface is `any`: all dispatch is dynamic, and the JIT removes the cost where it can. The value is always a heap object.
```java
List<Widget> ui = List.of(new Label("hi"), new Spacer());
for (Widget w : ui) System.out.println(w.name());
```
@end

## Type tests and several wefts

An `any` has forgotten its concrete type, but a **type test** gets it back: `w is l: Label` is true when `w` holds a `Label`, and binds it as `l`. The same pattern works in a `match`. To require several wefts at once, write `any (Widget + Show)`; one with more wefts can be passed where one with fewer is wanted.

@sample generics_tests

- `w is l: Label` binds `l` for the `and` and the body that follow, like any other `is`.
- A `match` over an `any` needs a catch-all arm (`_`), because other types can weave the weft ([P0101](diag:P0101) otherwise).
- `any (Widget + Show)` converts to `any Widget` implicitly, but not back: an `any Widget` isn't always `Show` ([T0108](diag:T0108)).

@try Add a `Button` warp that weaves `Widget`, put one in `ui`, and watch the second loop send it to `something else`. Then remove `_ => ...` from the `match`: the compiler asks for it.

@compare
-- Rust
A trait object can't be turned back into a concrete type. You add `as_any()` to the trait and use `downcast_ref` from `std::any::Any`.
```rust
if let Some(l) = w.as_any().downcast_ref::<Label>() {
    println!("{}", l.text.len());
}
```
-- Swift
A conditional cast with `as?` does the job, as it does for any class or existential.
```swift
if let l = w as? Label { print(l.text.count) }
```
-- Go
A type assertion with the comma-ok form, or a type switch.
```go
if l, ok := w.(Label); ok {
    fmt.Println(len(l.Text))
}
```
-- Java
Pattern matching for `instanceof` binds a typed name in the same breath.
```java
if (w instanceof Label l) {
    System.out.println(l.text().length());
}
```
@end

## Static or dynamic?

You've now got two ways to say "anything that does this":

| | Weft as parameter type / bound | `any W` |
|---|---|---|
| When the type is known | when compiling | when running |
| Calls | direct, and inlinable | through a table |
| Code | one copy per type used | one copy |
| A list of mixed types | impossible | yes |
| One function returning different types | no | yes |

Start with the generic form, because it's the faster and the more precise one: `loud(a: Animal)` can't be handed a `Cat` where a `Dog` was meant, and the compiler specializes it. Reach for `any` when the mixture is the point.

### Wefts with type parameters

A weft can take type parameters of its own, written like a generic function's: `weft Convert[T]`. `Convert[u32]` and `Convert[str]` are different wefts, so a type can weave both, each with its own methods. A bound names the arguments (`T: Convert[U]`), and the call `Convert[u32].convert(x)` says which weaving it means.

@sample generics_weft

- Without the arguments, `Convert.convert(x)` and `x.convert()` work when the type weaves one `Convert` only. With two, the compiler lists them ([T0116](diag:T0116)) and you pick: `Convert[u32].convert(x)`.
- A default method (`twice`) sees `T` as whatever the weaving gave it, and is checked once for each weaving that uses it.
- A bound says what a call needs, not which weaving the body uses: inside `to`, `x.convert()` still sees every `Convert` that `T` weaves. A type with two of them makes it ambiguous, and `Convert[U].convert(x)` picks the one the bound names.
- `T: Convert[U]` is checked when `to` is called, with both arguments known: `to[Feet, str](f)` stops with [T0108](diag:T0108), because a `Feet` doesn't weave `Convert[str]`.
- A weft of `const` or region parameters, `any Convert[u32]`, a generic weft as a parameter's type, and inferring `T` and `U` from the arguments or from where the result goes aren't supported yet: see [Not supported yet](page:not-yet).

Wefts with associated types (`type Item`) cover many of the cases where a weft would otherwise need a parameter: see [Warps, wefts and weaves](page:types#wefts).

## Where next

[Collections and streams](page:collections) uses generics all over: `List[T]`, `Map[K, V]`, and the lazy `Stream[T]`.
