# 03 · Memory, ownership & resources

Loom doesn't have a garbage collector or lifetime annotations. It also doesn't
let pointers dangle. It gets there with four pieces:

1. **Mutable value semantics.** Variables own their values, and borrows only
   exist while a call is running.
2. **Spools.** Allocation happens in named regions, and a pointer's type says
   which region it points into.
3. **`once` types.** Resources that have to be closed can't be forgotten.
4. **`unsafe`.** An explicit, documented escape hatch for unchecked code.

---

## Values first

Every variable owns its value. Assigning a value, or passing it to a `take`
parameter, **copies** it if the type is `Copy` and **moves** it otherwise.
Using a variable after it has been moved is a compile error. Values that are
returned are always owned.

References aren't values you can hold on to. Nothing in Loom has type
"reference to T", so there's nothing to annotate with a lifetime.

## Parameter conventions

| Declared | Call site | Callee gets | Caller keeps it? |
|----------|-----------|-------------|------------------|
| `x: T` | `f(x)` | read-only access | yes |
| `mut x: T` | `f(mut x)` | exclusive read-write access | yes, possibly changed |
| `take x: T` | `f(x)` | ownership | no (unless `T: Copy`) |

The compiler decides whether a read parameter is passed by value or by
pointer, depending on size. You don't have to think about it.

**Exclusivity:** while a variable is lent out as `mut`, nothing else can touch
it. `swap(mut a, mut a)` is rejected. The check is static and works per field,
so `swap(mut p.x, mut p.y)` is fine.

## Borrows are second-class

A borrow, meaning a parameter declared without `take`, lasts exactly as long
as the call that received it. So a borrow can't be:

- returned
- stored in a field of an ordinary warp
- captured by a closure that escapes

That rule removes the need for lifetime annotations. There are three ways to
do the things it forbids.

### 1. `ref warp`: views that hold borrows

A `ref warp` can have borrowed fields. It's second-class itself, so it can
live in locals and parameters but can't be returned or stored anywhere else.

```loom
ref warp Parser {
    src: str             // a borrow: fine, because Parser is a ref warp
    pos: usize = 0
}

fn parse(src: str) -> Ast {
    var p = Parser(src)  // lives only inside this call
    p.program()
}
```

### 2. Projections: shuttles that lend a place

A function can't return a borrow. It *can* `yield` one and then wait. A
**projection** is a [shuttle](05-concurrency.md#shuttles) whose output is a
*place* rather than a value. It's declared with `~> ref T` (read) or
`~> mut T` (read-write).

```loom
weave[T] Index[usize] into List[T] {
    fn at(mut self, i: usize) ~> mut T {
        if i >= .len { unravel("index {i} out of bounds (len {.len})") }
        yield mut .items[i]       // lend the slot; the caller uses it; we resume afterwards
    }
}

xs[3] += 1                         // xs.at(3) projected, edited, released

fn entry(mut self, take key: K, take or: V) ~> mut V { ... }
counts.entry(word, or: 0) += 1
```

The caller can use the projected place until the end of the statement that
asked for it. After that the projection resumes, so it can clean up, rebalance,
and so on. A projection that `yield`s **many** places is a mutable iterator,
and that's how `for mut x in xs` and `chunks_mut` work. This idea comes from
Hylo's subscripts, reused through shuttles.

### 3. Spools

When you need a pointer that's actually stored, like a tree, a graph, or a
cache, allocate the data in a spool.

---

## Spools

A **spool** is an allocator you can name. `std.mem` provides:

| Spool | Frees | Pointers are |
|-------|-------|--------------|
| `heap` | each allocation, when its owning pointer is dropped | owning, move-only |
| `Arena(cap)` | everything at once, on `reset()` or at end of scope | plain addresses, `Copy` |
| `Pool[T](n)` | slots individually; it has a fixed size | plain addresses, `Copy` |
| `static` | never | plain addresses, `Copy` |

You can write your own by weaving the `Spool` weft (`alloc`, `free`, `reset`).

### Declaring and using spools

```loom
spool frame = Arena(1.mib())           // `frame` is now also the name of a region

let p: ^Particle@frame = frame.put(Particle(pos, vel))
let q = ^Particle(pos, vel)            // `^expr` allocates in the *ambient* spool (see `given`)
let v = p^                             // postfix ^ dereferences (reads the whole value)
p.pos += p.vel                         // field access derefs automatically
```

A `spool` binding can't be moved or reassigned, and it ends with its scope.

### Region types

- `^T@s` is a pointer to a `T` that lives in spool `s`. The region name is
  literally the name of the spool variable or `given` parameter, so there's no
  lifetime algebra to learn.
- `T@s` works for any type that allocates, e.g. `List[i32]@frame` or
  `String@scratch`. The value and everything it owns live in `s`.
- **Inside a warp**, `^T` and fields that allocate follow the region of the
  warp that contains them. A `^Expr` inside an `Expr@tree` is a
  `^Expr@tree`. Each value lives in one region, which covers almost every data
  structure you'd write. A warp that genuinely needs two regions declares them:
  `warp Link[@a, @b] { from: ^Node@a, to: ^Node@b }`.

### Leaving out the region

| Position | `@` omitted means |
|----------|-------------------|
| read / `mut` parameter | **any** spool (the function is region-polymorphic) |
| return type, field, `take` parameter | `@heap` for owning types, `@static` for slices |

So `fn sum(xs: List[i32])` accepts a list from any spool, and
`fn make() -> List[i32]` returns one on the heap.

### What the region checker enforces

1. A `@s` value can't be returned, stored, or captured anywhere that outlives `s`.
2. `s.reset()` requires that no `@s` value is used afterwards. The check is
   flow-sensitive, like a borrow checker, but it has exactly one question to
   answer.
3. Arena pointers can't be sent to other threads unless the spool is a
   `SharedArena`.

```
error[R0101]: returned value points into spool `tmp`, which ends here
  --> dangling.loom:4:5
   |
 2 |     spool tmp = Arena(1.kib())
   |           --- spool declared here
 3 |     let tree = parse("1 + 1", given: tmp) mend unravel("")
 4 |     tree
   |     ^^^^ `tree: ^Expr@tmp`
   = note: the return type `^Expr` means `^Expr@heap`
   = help: take `given out: Spool` and return `^Expr@out`
```

### Moving data between spools

`.copy()` makes a deep copy in the ambient spool. The calc example uses this
as a tiny semispace collector: it folds a tree from one arena into a new one
and then resets the old arena.

### Slices and `static`

`str` and `[T]` don't own anything. Declared as a parameter, they're borrows.
In any other position they're `@static` unless they carry a region
annotation, and string literals are `str@static`. Because of that,
`fn name(self) -> str = "button"` is fine, and a field `label: str` holds a
static string. For text that belongs to the value, use `String`.

---

## `given`: ambient parameters

Some values need to reach deep into the call graph: allocators, loggers,
random number generators, thread pools. Passing them through every function by
hand is tedious, and hiding them in globals makes them invisible. `given` sits
between the two.

```loom
fn render(scene: Scene) given log: Logger, sp: Spool {
    log.debug("render {scene.name}")
    let batch = ^Batch(...)            // allocates in `sp`
    draw_batch(batch)                  // passes `log` and `sp` on implicitly
}

fn main() {
    given log = Logger.stderr()        // in scope for the rest of the block
    spool frame = Arena(4.mib())
    loop {
        render(scene, given: frame)    // provide a given explicitly, at one call
        frame.reset()
    }
}
```

- **Resolution is by type.** The innermost `given` of the required type wins.
  The name only matters inside the callee.
- **Private functions infer their givens.** A helper that allocates or logs
  takes whatever its caller has in scope. **`pub` functions have to declare
  theirs**, so a library's API is honest about what it needs.
- **The ambient spool** is the innermost `given` of type `Spool`, or `heap`
  when there isn't one. It's used by `^expr`, `.copy()`, collection
  constructors, and string templates that turn into `String`s.
- A `given` whose type is a weft is generic, with static dispatch: `given sp: Spool`
  is monomorphized separately for `Arena` and for `heap`.

---

## `once` types (and loose ends)

A `once warp` describes values that **must be used up exactly once**. They
can't be copied, and they can't just go out of scope. `once` is a contextual
keyword: it's only special directly before `warp`.

```loom
pub once warp Tx {
    moves: List[Move]

    pub fn commit(take self, ledger: mut Ledger) ! LedgerError {
        let Tx(moves) = self              // destructuring *ties off* the value
        ...
    }
    pub fn rollback(take self) { let Tx(_) = self }
}
```

- The only ways to use up a `once` value are to **destructure** it, which
  is only possible where its fields are visible (normally its own module), or
  to pass it to a `take` parameter that deals with it. So the module that
  defines a `once` type decides which functions can tie it off.
- A container of `once` values is `once` too: `List[Tx]` has to be drained.
- A `once` value that might be left unused is a **loose end**. The compiler
  says so and lists the tie-off functions it knows about:

```
error[L0301]: loose end: `tx` is never tied off
  --> forgetful.loom:2:9
  |
2 |     var tx = Tx.begin()
  |         ^^ `Tx` is a `once` type: it must be used up exactly once
4 | }
  | - ...but it is still loose when the scope ends here
    = help: tie it off with `tx.commit(..)` or `tx.rollback()`
    = help: or add `defer tx => tx.rollback()` right after it's created
```

Good uses: transactions, file handles that need to report close errors,
GPU command buffers, "remember to call `finish()`" builders, and protocol
states (a `Handshake` that must turn into a `Session`).

## `defer`

`defer` runs code when the scope exits, whether that's a normal exit,
`return`, `fail`, `?`, `break`, or an `unravel`. Defers run in reverse order
of declaration.

```loom
let fd = sys.open(path)?
defer sys.close(fd)                   // plain defer: always runs
```

**Fallback defers** name a `once` value:

```loom
var tx = Tx.begin()
defer tx => tx.rollback()             // runs only on exits where `tx` is still live
```

At every exit, the compiler already knows whether `tx` has been used up on
that path, since it has to know that to check `once` types. So a native
backend compiles a fallback defer into cleanup code on the paths that need it
and nothing on the others, with no runtime flag. (The Lua backend uses a flag
anyway; see [lua-backend](lua-backend.md).) This is Zig's `errdefer`, but
keyed on the resource instead of on "an error happened".

Defer bodies can't change control flow. They can't `return`, `fail`, `break`,
or use `?`. They're allowed to `unravel`.

---

## `unsafe`: the unchecked edge

`unsafe` blocks are where you can:

- dereference raw pointers `*T` and do pointer arithmetic
- call `extern` functions and `unsafe fn`s
- reinterpret bits: `bits_as[T](x)`
- read and write `Volatile[T]` registers
- break exclusivity through `Shared` cells

**Every `unsafe` block has to say why it's sound**, as a string, so audits
can just grep for `unsafe`:

```loom
extern "C" {
    fn write(fd: i32, buf: *u8, len: usize) -> isize
}

pub fn raw_print(s: str) {
    unsafe "fd 1 is open for the whole process; s outlives the call" {
        write(1, s.ptr(), s.len())
    }
}
```

`unsafe fn` marks a whole function as unsafe to call, and its doc comment
has to include a `# Safety` section describing what callers must guarantee.

### Layout control

```loom
#layout(C)                         // C ABI order, alignment, and padding
warp Timespec { sec: i64, nsec: i64 }

#layout(bits, big)                 // packed exactly, MSB first, big-endian bytes
warp Ipv4Header {
    version: u4
    ihl: u4
    dscp: u6
    ecn: u2
    total_len: u16
    ident: u16
    flags: u3
    frag_offset: u13
    ttl: u8
    proto: u8
    checksum: u16
    src: u32
    dst: u32
    const assert(Self.bits == 160)         // checked at compile time
}

let hdr = Ipv4Header.read(packet[0..20])   // generated by #layout(bits); no unsafe needed
```
