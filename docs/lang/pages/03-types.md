title: Warps, wefts and weaves
summary: Data types, behavior types, and how they are joined.
---
A **warp** is a data type. A **weft** is a set of behavior. A **weave** gives a warp a weft's behavior. There is no inheritance.

## Records

A warp with fields is a record. Fields can have defaults. A method takes `self`, and a function without `self` is an associated function, called on the type: `Point.origin()`.

**Construction is a call.** `Point(3, 4)` takes the fields in order, and `Point(y: 2)` names one and leaves the rest at their defaults. There are no `Point { x: 1 }` literals, so `if x == Foo {` is never ambiguous.

@sample types_records

- **The leading dot.** `.x` means "relative to the thing in context". In a method that's `self`, so `.x` is `self.x` and `.0` is `self.0`. In a dot-block it's the block's subject. Where a type is expected, it's one of that type's cases: `.Red` is `Color.Red`.
- **Dot-blocks.** `expr.{ ... }` opens a block with `expr` as its subject. On a fresh value or an immutable place it yields the edited value (`p.{ .x += 10 }` above). On a `var` it edits the variable in place and yields `()`.
- **Show.** A warp is shown with `{p}` only if it weaves `Show`, which needs no code. Any warp can be shown in its debug form with `{p:?}`.
- **Tuple warps.** `warp Meters(f64)` is a record with one positional field. It has no body, so its methods go in a `weave`, where `.0` means `self.0`.
- **Visibility.** Fields and items are private to their module unless marked `pub`. `pub(get)` makes a field readable everywhere and writable only inside its module.

## Cases

A warp with `case` lines is a sum type. Fields written beside the cases belong to every case, and can be read without a `match`.

@sample types_cases

`match` and `is` see the cases. A `match` that leaves a case out is an error, and the compiler says which one:

@sample types_err_match

## Wefts and weaves

A weft lists methods. A method with a body is a default. A warp joins a weft **inline** by listing it in its header, or **separately** with `weave`, which can live in another module.

@sample types_wefts

- A weft can require other wefts (`weft Ord: Eq`) and can have associated types (`type Item`; see `Iter` in [std.iter](std:iter#Iter)).
- `Shape.name(x)` calls a weft's method explicitly. A default method is checked for each type that uses it.
- `weave W into T` is allowed only when `W` or `T` is defined in your program ([T0115](diag:T0115) otherwise).
- A weft used as a parameter type is a generic: see [Generics and `any`](page:generics).

## Deriving

List `Show`, `Eq`, `Ord` or `Hash` in a warp's header and leave the methods out, and the compiler supplies them:

- `Show` writes the debug form, less any fields every case shares: `Point(x: 3, y: 4)`.
- `Eq`: every field is equal and, for a warp with cases, it's the same case.
- `Ord`: fields compare in order and the first difference decides. Earlier cases come first.
- `Hash` makes a warp a usable map key. `Error` gets a `message` that is its `Show` form.

A derived method needs every field to weave the weft too.

@sample types_derive

Right now these defaults are built into the compiler. They're meant to become ordinary default methods that inspect the type with [reflection](page:reflection), as `ToJson` does.

## `through`

`through` forwards a field's methods, so a warp can reuse another's behavior without inheriting from it. Fields aren't forwarded, a method the outer warp defines wins, and a `Button` is not a `Frame`.

@sample types_through

## Method lookup

`x.f(args)` looks for, in order:

1. a method in `x`'s warp body;
2. a method from a weave into `x`'s type;
3. for something with items that isn't a stream (an array, list, slice, range, or a type that weaves `Iter`), a method of `Stream`;
4. a method forwarded through `through`;
5. a free function in scope whose first parameter accepts `x`.

Two matches at one level are an ambiguity error; write `Weft.f(x)` or `module.f(x)` to choose.

@sample types_ufcs

## Where next

[Generics and `any`](page:generics) covers type parameters and dynamic dispatch.
