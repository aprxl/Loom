# Loom

> *Data is the warp. Behavior is the weft. Programs are woven.*

Loom is an imagined systems programming language. It is statically typed,
compiled ahead of time, has no garbage collector, and mixes functional and
object-oriented styles around one idea: **data and behavior are separate
threads, and you weave them together.**

This started as a design sketch: the design notes and example programs were
written before any compiler existed. A Loom → Lua compiler now runs a growing
part of the language (see [Status](#status)).

```loom
use std.io.print
use std.math.PI

weft Shape {                                   // behavior: an interface
    fun area(self) -> f64
    fun name(self) -> str = "shape"             // default method
}

warp Circle: Shape {                           // data, promising Shape up front
    radius: f64
    fun area(self) -> f64 = PI * .radius * .radius   // `.radius` means `self.radius`
}

warp Rect { w: f64, h: f64 }

weave Shape into Rect {                        // behavior added after the fact
    fun area(self) -> f64 = .w * .h
    fun name(self) -> str = "rect"
}

fun main() {
    let shapes: List[any Shape] = [Circle(1.5), Rect(2, 3)]
    let total = shapes.map(_.area()).sum()     // `_.area()` is a hole lambda
    print("{shapes.len()} shapes, {total:.2} square units")
}
```

---

## Design principles

1. **Values first, pointers only when you ask for them.** Variables own their values.
   Borrows only exist while a call is running, so you never write a lifetime
   annotation. If you need a real graph, you allocate it in a *spool* and the
   pointer's type names that spool: `^Node@frame`.
2. **Nothing dangles, nothing is left loose.** The compiler checks that
   pointers don't outlive their spool. `once` types must be used up. `defer`
   runs cleanup code on the way out of a scope.
3. **The machine stays visible.** `u12` is a real type. Each arithmetic
   operator states what happens on overflow (`+ +% +| +?`). Bit layouts are
   declared. Instruction decoding is written as a pattern match.
4. **Infer inside a module, be explicit at its edges.** Private functions get
   their error sets and `given` contexts inferred. `pub` signatures spell
   everything out.
5. **One idea, many uses.** A leading `.` always means "relative to whatever
   is in context". `?` always means "bail out to the nearest `mend`". Generics
   are compile-time parameters. Deriving a weft is just a default method that
   inspects the type at compile time.

## Glossary

| Loom         | Meaning                                                | Nearest cousin                 |
|--------------|--------------------------------------------------------|--------------------------------|
| `warp`       | a data type: record, sum, or both                      | struct / enum                  |
| `weft`       | a set of behavior                                      | trait / protocol / interface   |
| `weave W into T` | give type `T` the behavior `W`                      | `impl W for T`                 |
| `through`    | forward a field's methods (delegation)                 | Go embedding, Kotlin `by`      |
| `spool`      | a memory region / allocator with a name                | arena, allocator               |
| `^T@s`       | pointer to a `T` that lives in spool `s`               | `Box<T>` / `&'s T`             |
| `given`      | ambient parameter, passed implicitly by type           | Scala `using`, Odin `context`  |
| `once`       | a type whose values must be used up exactly once       | linear types (Austral)         |
| `defer`      | code that runs at scope exit                           | Go/Zig `defer`, `errdefer`     |
| `mend`       | recover from an error or a missing value               | `??`, `orelse`, `catch`        |
| `unravel`    | panic                                                  | `panic!`                       |
| `shuttle`    | a function that can pause, yielding values out and pulling values in | generator / coroutine |
| `loom { }`   | structured-concurrency scope                           | nursery, scoped threads        |
| `thread`     | a task spawned on the enclosing loom                   | `spawn`                        |
| twill pattern| bit/nibble pattern with captures, used in `match`      | Erlang bit syntax, sort of     |
| gauge        | an integer or float type restricted to a range         | Ada subtypes                   |
| `const`      | code that runs at compile time                         | Zig `comptime`, `constexpr`    |
| `unsafe`     | the unchecked edge, with a stated reason               | Rust `unsafe`                  |
| bolt         | a package                                              | crate / module                 |

The weaving words name the *things* Loom is built from: data, behavior, memory,
patterns. The keywords you type every day (`defer`, `const`, `unsafe`, `yield`,
`once`) are the plain ones you already know. The flavor survives in the
diagnostics: forgetting to use up a `once` value is still a *loose end*.

---

## Signature features

A one-screen taste of each. The [design docs](design/) have the details.

### 1. Warp · Weft · Weave, plus delegation and UFCS

Data (`warp`) and behavior (`weft`) are declared separately and joined with
`weave`. A warp can also list its wefts and implement them inline, OOP-style.
There's no inheritance. `through` forwards another field's methods instead.
With uniform call syntax, any free function is also a method on its first
parameter.

```loom
warp Button: Widget {
    through frame: Frame         // Button gets Frame's methods, e.g. size()
    label: String
    fun draw(self, at: Pos, mut c: Canvas) {
        .frame.draw(at, mut c)   // a "super call" is just a field call
        c.text(at.{ .x += 2; .y += 1 }, .label)
    }
}
```

### 2. The leading dot

`.name` resolves against the thing in context. Inside a method that's `self`.
Inside a dot-block it's the block's subject. Where a type is expected, it's
that type's cases.

```loom
let win = Window(800, 600).{        // dot-block: configure a fresh value, yield it
    .title = "Loom"
    .border = .Rounded              // `Border.Rounded`, inferred from the field type
    .show()
}
```

### 3. `?` jumps to the nearest `mend`

`?` unwraps an optional or a result. On failure it jumps to the nearest
enclosing `mend`, or returns from the function if there isn't one. The same
operator covers optional chaining, error propagation, and try/catch.

```loom
let port = env.get("PORT")?.parse[u16]()? mend 8080

{
    transfer(mut ledger, alice, bob, 20_00)?
    transfer(mut ledger, bob, carol, 99_00)?
} mend e => print("stopped early: {e}")
```

### 4. Spools and region pointers

Allocation goes to a spool. A pointer's type records which spool it points
into, and the compiler rejects any pointer that would outlive its spool.
Functions take the spool as an ambient `given`.

```loom
fun parse(src: str) -> ^Expr@sp ! ParseError given sp: Spool { ... }

spool scratch = Arena(64.kib())
let tree = parse("2 * (x + 3)", given: scratch)?   // tree: ^Expr@scratch
scratch.reset()                                    // error if `tree` is still used later
```

### 5. `once` types and `defer`

A `once` value must be used up exactly once. The compiler won't let a
transaction fall out of scope; it reports a *loose end*. A *fallback defer*
ties it off on any exit where it's still live.

```loom
var tx = Tx.begin()
defer tx => tx.rollback()      // runs only on exits where `tx` is still live
tx.debit(from, amount)
ledger.check(to)?              // leaving early here rolls back
tx.commit(mut ledger)          // used up, so the defer does nothing on this path
```

### 6. Twill patterns

Binary and hex literals in patterns can contain lowercase capture letters.
Each capture binds an integer exactly as wide as the bits it covers.

```loom
match op {                          // op: u16
    0x00E0 => .clear()
    0x1nnn => .pc = n               // n: u12
    0x6xkk => .v[x] = kk            // x: u4, kk: u8. u4 into [u8; 16] needs no bounds check
    0x8xy4 => {
        let 0bc_ssss_ssss = .v[x] + .v[y]   // 9-bit add: carry in c, sum in s
        .v[x] = s; .v[0xF] = c
    }
    0xDxyn => .draw(.v[x], .v[y], rows: n)
}
```

### 7. Shuttles

`~>` means "yields these out" and `<~` means "pulls these in". Shuttle frames
have a fixed size known at compile time and are stored inline, with no heap
allocation.

```loom
fun lexer() ~> Token <~ char {
    // `pull` takes the next char in; `yield .Int(n, span)` sends a token out
}

var lex = lexer()
for ch in source.chars() {
    for tok in lex.push(ch) { print(tok) }   // each token this char completed
}
```

### 8. Loom blocks

Threads run inside a `loom { }` scope and can't outlive it. That makes it safe
for them to borrow local data. Data races show up as type errors.

```loom
let (primes, words) = loom {
    let a = thread count_primes_below(2_000_000)
    let b = thread word_counts(paths)
    (a.join(), b.join())
}

loom for (row, mut line) in pixels.chunks_mut(width).indexed() { ... }
```

### 9. `const`: compile time is ordinary Loom code

```loom
const CRC_TABLE: [u32; 256] = { ... }                 // const initializers run while compiling

pub warp Ring[T, const N: usize] {
    const assert(N.is_power_of_two(), "capacity must be a power of two")
    type Idx = uint(N.log2())                        // N = 256 gives u8 cursors that wrap for free
    ...
}
```

### 10. Small comforts

- **Gauges**: `type Port = u16 where 1..` gets checked at the boundaries, narrowed by `is`, and used to skip bounds checks.
- **Overflow-policy operators**: `+` traps, `+%` wraps, `+|` saturates, `+?` returns `?T`. The same suffixes work on `as`.
- **Hole lambdas**: `xs.filter(_ % 2 == 0)`, `xs.reduce(_ + _)`, `pairs.sorted_by(_.1 > _.1)`.
- **Sums with shared fields**: every `Token` case has a `span`, declared once.
- **Chained comparisons**: `if 0 <= x < width`.
- **Call-site `mut`**: `draw(scene, mut canvas)` shows at the call which arguments get mutated.
- **Construction is a call**: `Point(1, 2)`, so struct literals never make `if x == Foo {` ambiguous.

---

## Repository map

```
README.md                  you are here
AGENTS.md                  how to work on the repo (gates, toolchain, conventions)
design/
  01-syntax.md             lexical rules, statements, operators, patterns, the leading dot
  02-types.md              primitives, gauges, warps, wefts, weaves, through, generics, any
  03-memory.md             value semantics, borrows, projections, spools, given, once, defer, unsafe
  04-errors.md             ?T, T ! E, the ?/mend pair, fail, unravel
  05-concurrency.md        shuttles, loom blocks, threads
  06-compile-time.md       compile-time evaluation (`const`) and reflection
  grammar.ebnf             a grammar sketch
  open-questions.md        the parts that aren't settled yet
  lua-backend.md           decisions for the Loom → Lua 5.4 backend
  vm.md                    Treadle, Loom's own VM: the design and its milestones
examples/
  tour.loom                everyday syntax in one file
  widgets.loom             OOP side: wefts, weaves, through, any, dot-blocks
  chip8.loom               a CHIP-8 core: twill patterns, u12/u4, overflow operators
  calc.loom                expression compiler in arenas: spools, ^T@s, given
  ledger.loom              money transfers: gauges, once types, defer, error flow
  lexer.loom               shuttles: generators, push-driven lexer
  parallel.loom            loom blocks, threads, loom for
  compile_time.loom        compile-time tables, const params, reflection-based derive
crates/
  loom_syntax/             source files, spans, diagnostics, the lexer, the parser and AST
  loom_sys/                what a running program gets from its host, for both backends
  loom_host/               the bundled Lua VM (mlua, vendored, built with clang-cl)
  loom_isa/                Treadle's instruction set: op table, images, verifier
  loom_vm/                 Treadle, Loom's own VM: the interpreter
bench/                     benchmarks, their Lua versions, and results
  loom_compiler/           name resolution, type checking, lowering to Lua
  loom/                    the `loom` command-line tool
runtime/
  rt/                      loomrt, the Lua runtime every bundle carries
  std/                     the standard library, in Loom (declarations so far)
tests/                     fixture corpora (see tests/README.md)
```

The examples use `std` (`std.io`, `std.fs`, `std.mem`, …). So far `runtime/std`
declares what they need, which is enough for their names to resolve; the
bodies come with type checking and lowering.

## Status

The language is a design sketch (v0.1). A Loom → Lua 5.4 transpiler is being
built in `crates/`, following the plan in `design/lua-backend.md`:

- **Done:** the lexer, the parser, name resolution, and a first slice of the
  language that compiles to Lua and runs: numbers of every width with their
  overflow operators, strings and templates, tuples, functions with named and
  default arguments, control flow, and `match` on literals and tuples; and
  warps: records and cases, methods, UFCS, the leading dot, dot-blocks, case
  and twill patterns, with value semantics; and wefts and weaves, with
  default methods, derived `Show`, `Eq` and `Ord`, and `through`; and
  generics, checked and compiled per instance; `any W` with dynamic
  dispatch and type tests; optionals and errors: `?`, `mend`, `fail`,
  unions, and `main ! E`; arrays, lists and slices; the methods of text,
  characters and numbers; lambdas, `Iter` and lazy streams; `defer`;
  gauges; maps and sets; `once` types; tests; shuttles; and compile-time
  code (constants, `const` expressions and `const assert`, run in a
  sandbox, `const` parameters, `const if` and `const for`, and reflection:
  `T.fields`, `v.(f)`, and default methods that derive from a type's shape). `examples/widgets.loom`,
  `examples/tour.loom`, `examples/ledger.loom` and `examples/chip8.loom`
  run start to finish (chip8 wants a ROM: `loom run examples/chip8.loom --
  game.ch8`). `loom run <file>` runs a program on Treadle; `loom test <file>` runs its
  tests; `loom build <file>` writes Treadle's image (`--exe` an executable;
  `--backend lua` on `build`, `run`, `test` and `check` picks the Lua backend); `--emit tokens|ast|hir|lua`
  dumps a stage; `loom docs` opens the documentation in your browser. The other examples use more than this; the compiler says
  so rather than guessing.
- **Treadle**, Loom's own VM, is the default backend: a typed,
  optimizing pipeline and a register interpreter, meant to outrun the
  interpreters of other languages (`design/vm.md`). It runs the whole
  language: `loom run`, about 1.6× as fast as PUC Lua 5.4 and 10× the Lua backend on
  `bench/` (`bench/RESULTS.md`). `loom run examples/chip8.loom
  -- examples/digits.ch8` plays a small ROM.
- **Next:** shuttles on Treadle, then its optimizer, then threads and spools
  on it.

```bash
cargo run -p loom -- run tests/run/fizzbuzz.loom
cargo run -p loom -- run --backend lua tests/run/scalars.loom
cargo run -p loom -- build tests/run/fizzbuzz.loom --emit lua
cargo run -p loom -- docs
```
