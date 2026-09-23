# 06 · Cards: compile-time Loom

In 1804 the Jacquard loom read its pattern from a chain of punched cards
before the weaving started. Loom's **cards** are the same idea: ordinary Loom
code that runs while the program compiles.

Loom has no separate macro language. Compile-time code is ordinary Loom code
with a few restrictions.

---

## Running code at compile time

```loom
const CRC_TABLE: [u32; 256] = card { ... }     // a block evaluated while compiling
const KIB = 1024                               // `const` initializers are card contexts anyway
let n = card fib(30)                           // any eligible call, evaluated once and inlined
card assert(Header.size == 20, "wire format changed")
```

A function can run as a card if everything it does is *card-safe*: pure
computation, allocation in the compiler's card spool, and `card` builtins. No
I/O (except `embed`), no `selvage`, no threads. You don't mark functions for
this. The compiler checks each function when it's called at compile time.

## Card parameters and types as values

```loom
fn zeros(card n: usize) -> [f32; n] = [0.0; n]

warp Matrix[card R: usize, card C: usize] {
    cells: [f32; R * C]
    fn transpose(self) -> Matrix[C, R] { ... }
}
```

- A `card` parameter must be known at compile time. The function is
  specialized for each distinct value.
- `type` is a card-only type. **Generics are card parameters:** `fn max[T: Ord]`
  is sugar for `fn max(card T: type where T: Ord, ...)`, with `T` inferred.
- Card functions can return types, so you can compute them:

```loom
card fn uint(bits: u8) -> type          // a builtin: uint(12) is u12
card fn Fixed(int_bits: u8, frac_bits: u8) -> type = warp {
    raw: int(int_bits + frac_bits)
    ...
}
type Q16_16 = Fixed(16, 16)
```

## Compile-time control flow

```loom
card if target.os == .Windows {
    win32_path(p)
} else if target.os == .Linux {
    posix_path(p)
} else {
    card unravel("unsupported OS: {target.os}")   // unravelling in a card is a compile error
}

card for f in Point.fields {            // unrolled while compiling
    print("{f.name}: {f.type.name}")
}
```

- The whole `card if` / `else if` chain is decided at compile time. **Branches
  that aren't taken are parsed but not type-checked**, so they can mention
  APIs that don't exist on the current target.
- `card for` unrolls. Its body is checked once per iteration, with the loop
  variable known at compile time.
- `card assert(cond, msg)` inside a generic warp or function is checked for
  each instantiation.

## Reflection

| Expression | Gives you |
|------------|-----------|
| `T.name` | `"Point"` |
| `T.kind` | `.Record`, `.Sum`, `.Int`, `.Float`, `.Array`, `.Pointer`, … |
| `T.size`, `T.align`, `T.bits` | layout facts (`bits` for `#layout(bits)`) |
| `T.fields` | card list of `Field { name, type, offset, is_pub }` |
| `T.cases` | card list of `Case { name, fields }` |
| `T.weaves(W)` | whether `T` weaves `W` |
| `v.(f)` | the field `f` of `v`, where `f` is a card `Field` |
| `v is .(c)` | whether `v` is the card `Case` `c` |
| `v.(c).(f)` | a field of the payload once `v` is known to be case `c` |
| `target` | `.os`, `.arch`, `.endian`, `.pointer_bits`, … |

Reflection only exists at compile time. After cards run, nothing
reflection-related is left in the binary.

## Derive is just a default method

Loom doesn't have `#[derive]` macros. A weft gives a default body that looks
at `Self` at compile time, and any warp that lists the weft gets that body,
specialized to its own shape:

```loom
pub weft Show {
    fn show(self, f: mut Fmt) {
        card if Self.kind == .Record {
            f.put("{Self.name}(")
            card for (i, fld) in Self.fields.indexed() {
                card if i > 0 { f.put(", ") }
                f.put("{fld.name}: {self.(fld)}")
            }
            f.put(")")
        } else if Self.kind == .Sum {
            ...
        } else {
            card unravel("can't derive Show for {Self.name}; weave it by hand")
        }
    }
}

warp Point: Show { x: f64, y: f64 }        // Point(x: 1, y: 2)
```

The derivation can be read, debugged, and overridden like any other code.
Writing the method yourself replaces it. See `examples/cards.loom` for a
complete `ToJson`.

## Embedding files

```loom
const FONT: [u8] = card embed("assets/font.bin")     // bytes, stored in rodata
const SCHEMA: str = card embed_text("schema.sql")
```

`embed` is the only I/O a card is allowed to do. The paths are relative to the
bolt root and become build dependencies.

## Limits

- Card evaluation is **deterministic**. The same inputs always give the same
  output, which makes caching safe.
- Card code runs under a **step budget**. Going over it is a compile error
  with a card-level stack trace. `bolt.loom` can raise the budget.
- When cross-compiling, card code sees **target** sizes and endianness
  (`usize` is the target's), not the host's.

## `bolt.loom`: the manifest is a card

```loom
use std.build.{Bolt, Dep}

pub card fn bolt() -> Bolt = Bolt(
    name: "chip8",
    version: "0.1.0",
    deps: [Dep.git("https://example.com/loom-term", tag: "v2")],
    unravel: .abort,
).{
    card if target.os == .Windows { .link("user32") }
}
```

Build configuration can use loops, conditions, and functions, without a
second configuration language.
