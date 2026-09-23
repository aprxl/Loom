# 06 · Compile-time Loom (`const`)

Loom has no separate macro language. Code that runs while the program
compiles is ordinary Loom code marked with `const`, with a few restrictions.

`const` already means "a value known at compile time" (`const MAX = 64`), so
the same keyword marks everything else that happens at compile time: blocks,
conditions, loops, assertions, and parameters.

> Loom's `const fn` means **compile-time only**, as in Zig's `comptime`. It is
> *not* Rust's `const fn`, which means "may also be evaluated at compile time".

---

## Running code at compile time

```loom
const CRC_TABLE: [u32; 256] = { ... }          // const initializers already run at compile time
let table = const { build_table() }            // a block evaluated while compiling
let n = const fib(30)                          // any eligible call, evaluated once and inlined
const assert(Header.size == 20, "wire format changed")
```

A function can run at compile time if everything it does is *const-safe*:
pure computation, allocation in the compiler's own spool, and the
compile-time builtins. No I/O (except `embed`), no `unsafe`, no threads. You
don't mark functions for this. The compiler checks each function when it's
called at compile time.

## `const` parameters and types as values

```loom
fn zeros(const n: usize) -> [f32; n] = [0.0; n]

warp Matrix[const R: usize, const C: usize] {
    cells: [f32; R * C]
    fn transpose(self) -> Matrix[C, R] { ... }
}
```

- A `const` parameter must be known at compile time. The function is
  specialized for each distinct value.
- `type` is a compile-time-only type. **Generics are `const` parameters:**
  `fn max[T: Ord]` is sugar for `fn max(const T: type where T: Ord, ...)`,
  with `T` inferred.
- `const fn`s can return types, so you can compute them:

```loom
const fn uint(bits: u8) -> type          // a builtin: uint(12) is u12
const fn Fixed(int_bits: u8, frac_bits: u8) -> type = warp {
    raw: int(int_bits + frac_bits)
    ...
}
type Q16_16 = Fixed(16, 16)
```

## Compile-time control flow

```loom
const if target.os == .Windows {
    win32_path(p)
} else if target.os == .Linux {
    posix_path(p)
} else {
    const unravel("unsupported OS: {target.os}")   // unravelling at compile time is a compile error
}

const for f in Point.fields {            // unrolled while compiling
    print("{f.name}: {f.type.name}")
}
```

- The whole `const if` / `else if` chain is decided at compile time.
  **Branches that aren't taken are parsed but not type-checked**, so they can
  mention APIs that don't exist on the current target.
- `const for` unrolls. Its body is checked once per iteration, with the loop
  variable known at compile time.
- `const assert(cond, msg)` inside a generic warp or function is checked for
  each instantiation.

## Reflection

| Expression | Gives you |
|------------|-----------|
| `T.name` | `"Point"` |
| `T.kind` | `.Record`, `.Sum`, `.Int`, `.Float`, `.Array`, `.Pointer`, … |
| `T.size`, `T.align`, `T.bits` | layout facts (`bits` for `#layout(bits)`) |
| `T.fields` | compile-time list of `Field { name, type, offset, is_pub }` |
| `T.cases` | compile-time list of `Case { name, fields }` |
| `T.weaves(W)` | whether `T` weaves `W` |
| `v.(f)` | the field `f` of `v`, where `f` is a compile-time `Field` |
| `v is .(c)` | whether `v` is the compile-time `Case` `c` |
| `v.(c).(f)` | a field of the payload once `v` is known to be case `c` |
| `target` | `.os`, `.arch`, `.endian`, `.pointer_bits`, … |

Reflection only exists at compile time. Once compile-time code has run,
nothing reflection-related is left in the binary.

## Derive is just a default method

Loom doesn't have `#[derive]` macros. A weft gives a default body that looks
at `Self` at compile time, and any warp that lists the weft gets that body,
specialized to its own shape:

```loom
pub weft Show {
    fn show(self, f: mut Fmt) {
        const if Self.kind == .Record {
            f.put("{Self.name}(")
            const for (i, fld) in Self.fields.indexed() {
                const if i > 0 { f.put(", ") }
                f.put("{fld.name}: {self.(fld)}")
            }
            f.put(")")
        } else if Self.kind == .Sum {
            ...
        } else {
            const unravel("can't derive Show for {Self.name}; weave it by hand")
        }
    }
}

warp Point: Show { x: f64, y: f64 }        // Point(x: 1, y: 2)
```

The derivation can be read, debugged, and overridden like any other code.
Writing the method yourself replaces it. See `examples/compile_time.loom` for
a complete `ToJson`.

## Embedding files

```loom
const FONT: [u8] = embed("assets/font.bin")         // bytes, stored in rodata
const SCHEMA: str = embed_text("schema.sql")
```

`embed` is the only I/O compile-time code is allowed to do. The paths are
relative to the bolt root and become build dependencies.

## Limits

- Compile-time evaluation is **deterministic**. The same inputs always give
  the same output, which makes caching safe.
- It runs under a **step budget**. Going over it is a compile error (K0102)
  with a compile-time stack trace. `bolt.loom` can raise the budget.
- When cross-compiling, compile-time code sees **target** sizes and
  endianness (`usize` is the target's), not the host's.

## `bolt.loom`: the manifest is compile-time code

```loom
use std.build.{Bolt, Dep}

pub const fn bolt() -> Bolt = Bolt(
    name: "chip8",
    version: "0.1.0",
    deps: [Dep.git("https://example.com/loom-term", tag: "v2")],
    unravel: .abort,
).{
    const if target.os == .Windows { .link("user32") }
}
```

Build configuration can use loops, conditions, and functions, without a
second configuration language.

## Where the name went

In 1804 the Jacquard loom read its pattern from a chain of punched cards
before the weaving started. Earlier drafts of Loom called compile-time code
"cards" for that reason. The keyword is now the plain `const`, and the
error codes keep the old letter: K0101 for a failed `const assert`, K0102 for
an exhausted step budget.
