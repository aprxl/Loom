title: Generics and `any`
summary: Type parameters checked per instance, wefts as parameter types, and dynamic dispatch.
---
## Generics

A type parameter goes in brackets, with bounds after a colon (`T: Ord + Copy`) or in a `where` clause. The compiler infers arguments where it can. To give them yourself, use brackets: `largest[i32](xs)`, `List[i32]()`, `s.parse[u16]()`.

Generics are checked **per instance**, in the style of Zig. A generic function is type-checked once for each list of arguments it's used with, and one nobody calls isn't checked at all. An error in a generic body is reported once, with a note naming the instance it came from.

@sample generics_basic

- **Generic warps** (`Pair[A, B]`) and their methods are compiled per list of arguments too.
- **Generic weaves** (`weave[T: Show] Twice into List[T]`) work the same way.
- **Wefts as parameter types are implicit generics.** `fun show_all(xs: [Show])` means `fun show_all[T: Show](xs: [T])`, and each use has static dispatch.
- Value parameters (`const N: usize`) work too: see [Const parameters](page:const-params).
- Generic wefts (`weft From[T]`) are not supported yet ([U0102](diag:U0102)).

## `any W`

`any Widget` is a value of some type that weaves `Widget`. Calls on it go through a table at run time, and a type test (`w is l: Label`) recovers the concrete type. A value converts to `any W` where one is expected, and an `any` of several wefts (`any (Widget + Show)`) can be used as an `any` of fewer.

@sample generics_any

Use a weft as a parameter type when the caller knows the type and you want static dispatch, and `any` when a list has to hold values of different types.

## Where next

[Collections and streams](page:collections) uses generics all over.
