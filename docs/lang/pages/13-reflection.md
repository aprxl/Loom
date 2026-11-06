title: Reflection
summary: Asking a type about itself while compiling, and default methods that derive from a type's shape.
---
Plenty of code depends on the *shape* of a type rather than on what it does: print any record, compare any two values, write a struct out as JSON, check that a header is eight bytes. In most languages that means one of two things. Either a run-time reflection API, where the program carries descriptions of its own types and looks them up as it runs, or a macro system that rewrites code before it's compiled.

Loom takes a third path. Compile-time code can ask a type about itself, and the answers are known while compiling, like a `const` parameter. Together with `const if` and `const for` from the [last chapter](page:const-params), that is enough to write code that adapts to any type, and **nothing reflective is left in the program**. The types involved are in [std.meta](std:meta).

## Asking a type

Every type has a few properties you can read in compile-time code, and in generic functions, where `T` is whatever the caller used:

| Expression | Gives you |
|------------|-----------|
| `T.name` | its name, `"Point"` (a warp's, without its arguments) |
| `T.kind` | a case of `Kind`: `.Record`, `.Sum`, `.Int`, `.Str`, `.List`, … |
| `T.fields` | a record's fields, a list of `Field`s |
| `T.cases` | a sum's cases, a list of `Case`s |
| `T.size`, `T.align` | how many bytes a value takes, and what its address is a multiple of |
| `f.name`, `f.is_pub`, `f.offset` | a field's name, whether it's `pub`, and where it starts in the value, in bytes |
| `c.name`, `c.fields` | a case's name, and its payload's fields |
| `.len()`, `.is_empty()` | of `T.fields`, `T.cases` or `c.fields` |

`T.kind` is a case of [`Kind`](std:meta#Kind): `Record` for a warp with fields, `Sum` for one with cases, and one for each of the built-in families (`Int`, `Float`, `Str`, `List`, `Optional`, `Array`...).

@sample refl_kinds

Notice that `str` and `String` are both `Str`, that a generic warp's name leaves off its arguments, and that `Light` takes two bytes: a tag, and a `bool` for the one case that has a payload.

@try Add `ask[u8]()` and `ask[Map[str, i32]]()` to `main`, and compare their sizes with what you'd guess. Then try `ask[Light]()` with another case that carries a `u64`, and watch `Light`'s size grow.

## Fields and cases

Properties are only half of it. Two forms use a field or a case that you've got in a compile-time variable:

- `v.(f)` is `v`'s field `f`, where `f` is a `Field`. It reads and assigns like `v.x`.
- `v is .(c)` tests whether `v` is case `c`, and `v.(c).(f)` reads a field of that case's payload. Read it inside `if v is .(c)`: on any other case it unravels.

`const for f in T.fields` writes its body out once per field, so `describe` below turns into straight-line code for each type it's called with. The code that comes out has no loop, no field names looked up, and no table: for `Point` it is two prints.

@sample refl_describe

Two things in it are decided at different times. `const if T.kind == .Record` and the `const for` are worked out while compiling. The `if v is .(c)` is an ordinary test at run time: it asks which case this particular value is in, and the compiler wrote one such test per case.

@try In `main`, call `describe(3)` as well. An integer is neither a record nor a sum, so it prints only `i32 is a Int`: both `const if` branches are skipped.

@compare
-- Zig
Zig's `@typeInfo` is the closest match: it hands compile-time code a description of the type, `inline for` walks it, and `@field` reads a field chosen at compile time (the counterpart of `v.(f)`).
```zig
fn describe(v: anytype) void {
    const T = @TypeOf(v);
    inline for (@typeInfo(T).@"struct".fields) |f| {
        std.debug.print("{s} = {any}\n", .{ f.name, @field(v, f.name) });
    }
}
```
-- Go
Go's `reflect` does this at run time, on a value of type `any`. It works on any struct, with no help from the compiler, but it does its lookups on every call, and a mistake is a panic instead of a compile error.
```go
func describe(v any) {
    t, rv := reflect.TypeOf(v), reflect.ValueOf(v)
    for i := 0; i < t.NumField(); i++ {
        fmt.Println(t.Field(i).Name, "=", rv.Field(i).Interface())
    }
}
```
-- C#
C# reflection is also a run-time API: the program carries metadata about its types, and you ask for it. The assembly is bigger for it, and trimming or ahead-of-time compilation has to be told what reflection may touch.
```csharp
foreach (var f in v.GetType().GetFields())
    Console.WriteLine($"{f.Name} = {f.GetValue(v)}");
```
-- Java
Java's reflection is run-time too. Annotations can tag fields and classes, and either a framework reads them by reflection at run time, or an annotation processor generates code at build time.
```java
for (Field f : v.getClass().getDeclaredFields()) {
    f.setAccessible(true);
    System.out.println(f.getName() + " = " + f.get(v));
}
```
@end

## Derive is a default method

Loom has no derive macros. A weft gives a default method that looks at `Self`, and every warp that lists the weft gets that method, worked out for its own fields. You write it in the same language as everything else, and you can read it, step through it and test it like any other function. `examples/compile_time.loom` writes `ToJson` this way.

@sample refl_derive

`Fields` has no required methods. `Point` and `User` list it, and each gets a `fields()` whose body is straight-line code for its own fields, with no loop and no table of names.

Writing the method yourself replaces the default, like any other default method.

The same forms work for writing. This `Scale` multiplies every field of a record by a number, and `CaseName` finds which case a sum is in. `CaseName` returns from inside a `const for`: the loop is unrolled into the function, so its body is ordinary run-time code, not code the sandbox runs.

@sample refl_scale

@try Give `Vec3` a field that isn't a number: add `label: String = "v"` after `z`. The line `self.(f) *= by` is checked for that field too, the compiler says a `String` can't be multiplied, and the note names the instance, `Scale.scale` for `Vec3`.

@compare
-- Rust
`#[derive]` runs a procedural macro: a function, in a separate crate, that reads the struct as a stream of tokens and writes more tokens. It sees the tokens of the struct, not the types of its fields, and it has to be compiled as a crate of its own. Loom's default method is written in the language itself.
```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct Point {
    x: i32,
    y: i32,
}
```
-- Java
Java uses annotations to mark types and then either run-time reflection (Jackson reads `@JsonProperty` while the program runs) or an annotation processor that generates source files before compilation.
```java
public record Point(@JsonProperty("x") int x, @JsonProperty("y") int y) {}
```
-- C#
C# source generators are the build-time route: a program the compiler loads that looks at the code being compiled and adds more. System.Text.Json uses one so serialization needs no run-time reflection.
```csharp
[JsonSerializable(typeof(Point))]
partial class PointContext : JsonSerializerContext {}
```
-- Go
Go has no derive at all. Standard-library packages like `encoding/json` use run-time reflection on the struct's fields, steered by struct tags; code generators (`go generate`) cover the rest.
```go
type Point struct {
    X int `json:"x"`
    Y int `json:"y"`
}
```
@end

### Refusing a type

A default method can also say what it can't do. A `const unravel` in a branch nobody takes costs nothing, and in a branch somebody takes it stops the build, and says which type did it:

@sample refl_err_kind

Both `Point` and `Light` list `Names`, but only `Light` reaches the `else`, so only `Light` is reported.

@try Delete `Light.Red.names()` and `warp Light` from the program. It compiles, runs, and prints `x, y`: the refusal only costs something when somebody asks for it.

## Sizes

`T.size`, `T.align` and `f.offset` are how values are laid out in memory: fields ordered by alignment (the largest first), cases sharing space behind a tag, and `?T` costing nothing when `T` has bit patterns it never uses. Both backends give the same answers.

@sample refl_layout

Look at the offsets: `len` (4 bytes) comes first, then `id` (2), then `flag` (1), which is not the order they were written in. The header takes 8 bytes because Loom put the largest fields first and the padding last. A `const assert` on `Header.size` is the way to be told the day an edit changes it.

@try Change `flag: u8` to `flag: u64` and run it. The header grows, the offsets move, and the `const assert` fails with K0101.

@compare
-- Rust
`size_of`, `align_of` and `offset_of!` (stable since Rust 1.77) ask the same questions. Rust's default `repr` is also free to reorder fields, which is why a Rust struct of the same fields comes out at 8 bytes too (though the layout isn't promised); only `#[repr(C)]` keeps the order you wrote.
```rust
struct Header { flag: u8, len: u32, id: u16 }
const _: () = assert!(std::mem::size_of::<Header>() == 8);
```
-- C
C never reorders: the struct is laid out in the order written, with padding between fields, so the same three fields take 12 bytes. You find out with `sizeof` and `offsetof`, and reorder by hand.
```c
struct Header { uint8_t flag; uint32_t len; uint16_t id; };
_Static_assert(sizeof(struct Header) == 12, "padding");
_Static_assert(offsetof(struct Header, id) == 8, "after len");
```
-- Zig
Zig's `@sizeOf`, `@alignOf` and `@offsetOf` are compile-time too. A plain `struct` has an unspecified layout and may be reordered; `extern struct` follows C.
```zig
const Header = struct { flag: u8, len: u32, id: u16 };
comptime {
    std.debug.assert(@sizeOf(Header) == 8);
}
```
@end

## Only while compiling

A `Field`, a `Case` or a list of them has no value when the program runs. Anything *read* from one does: a name is text, `.len()` is a number. Keeping the list itself is [K0111](diag:K0111).

@sample refl_err_runtime

The line `print(Point.fields.len())` is fine: it's a number. `let fields = Point.fields` asks the program to hold the list when it runs, and there's no such value. The fix is in the message: read what you need from it at compile time, `f.name`, `self.(f)`, `const for f in T.fields`.

@try Replace `let fields = Point.fields` with `let n = Point.fields.len()`. It compiles: `n` is a plain number the compiler already knows.

## Where next

The pieces on this page are the whole of reflection today. `T.bits` (for `#layout(bits)`), `f.ty` (a field's type) and `T.weaves(W)` (whether a type weaves a weft) are not built, and `Show`, `Eq`, `Ord` and `Hash` for warps are still done inside the compiler rather than written as default methods; [Not supported yet](page:not-yet) has the list.

Next, [Tests](page:tests) shows how to check all of this: `test` blocks and `loom test`.
