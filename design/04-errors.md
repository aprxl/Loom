# 04 · Errors & absence

Loom has two kinds of "didn't work": **absence** (`?T`) and **failure**
(`T ! E`). A single operator bails out of either one, and a single keyword
recovers from either one.

| | Syntax |
|---|---|
| optional | `?T`, whose values are `some(x)` and `none` |
| result | `T ! E`, whose values are `ok(v)` and `err(e)`. `fn f() ! E` when there's no success value |
| raise | `fail e` |
| bail out | postfix `?` |
| recover | `expr mend fallback`, `expr mend e => handler` |
| give up | `unravel("message")` |

---

## Optionals and results

```loom
fn find(users: [User], name: str) -> ?User { ... }
fn read(path: str) -> Bytes ! IoError { ... }
fn save(cfg: Config) ! IoError { ... }        // `-> ()` left out

fn check(age: i32) -> u8 ! AgeError {
    if age < 0 { fail .Negative }             // leading dot: a case of the declared error type
    age as? u8 mend fail .TooOld(age)
}
```

A `T` converts to `?T` or `T ! E` implicitly, so you rarely write `some(x)`
or `ok(x)` when constructing; `err(e)` makes a failure without leaving the
function. The conversion goes one level deep: `??T` needs `some(some(x))`.

`fail e` always leaves the function, even inside the left of a `mend`: only
`?` jumps to a `mend`.

## `?`: bail to the nearest `mend`

Postfix `?` unwraps a `?T` or a `T ! E`. When there's no value, it **jumps to
the nearest `mend` that lexically encloses it**. If there's no such `mend` in
the function, it returns from the function.

That one rule covers three things other languages keep separate.

**Error propagation**, as in Rust:

```loom
fn load(path: str) -> Config ! (IoError | ParseError) {
    let text = fs.read_text(path)?          // no mend here, so an error returns from `load`
    parse_config(text)?
}
```

**Optional chaining**, as in Swift and Kotlin. `?.` isn't a separate operator:

```loom
let city = user?.address?.city mend "unknown"
let port = env.get("PORT")?.parse[u16]()? mend 8080
```

**try/catch as an expression.** A block on the left of `mend` works as a `try` block:

```loom
let cfg = {
    let text = fs.read_text(path)?
    let raw = toml.parse(text)?
    Config.from(raw)?
} mend e => {
    log.warn("bad config ({e}), using defaults")
    Config()
}
```

### The rules

1. The `?` has to be lexically inside the **left operand** of the `mend` it
   jumps to. It can cross loops and nested blocks, but not lambda or shuttle
   boundaries, because those are separate functions.
2. The left operand's success type has to match the handler's type, or the
   handler has to diverge (`return`, `fail`, `unravel`, `break`, …).
3. In `mend e => …`, the type of `e` is the **union of everything that can
   reach that mend**. If a `?` on an optional can reach it, `Missing` (a unit
   error type) is part of the union.
4. When a `?` leaves the function, the error has to fit the function's error
   type. See [conversions](#conversions) below.

### `mend`

`mend` has the lowest precedence of any binary operator, so put parentheses
around it inside arithmetic: `(counts.get(k) mend 0) + 1`.

```loom
let n = s.parse[i32]() mend 0                           // a fallback value
let f = File.open(p) mend unravel("can't open {p}")     // a "fallback" that diverges: this is `expect`
let v = cache.get(k) mend return none                   // early exit
let x = risky() mend e => { log.error("{e}"); default }  // a handler that sees the error
let port = flag("port") mend env_port() mend 8080       // chains left to right: the first that works wins
```

`unwrap()` and `expect()` exist in `std`, but `mend unravel("…")` reads well
enough that they're rarely used.

## Error types

Errors are ordinary warps that usually weave `Error`. Its default methods
build `message()` from the case name and payload.

```loom
warp LedgerError: Error {
    case UnknownAccount(AccountId)
    case Insufficient(account: AccountId, short_by: Cents)
    case Journal(from IoError)              // absorbs IoError automatically
}
```

### Conversions

When a `?` leaves a function, the error is converted:

1. **Unions widen.** An `IoError` fits `! (IoError | ParseError)`.
2. **`from` cases absorb.** `case Journal(from IoError)` means a `?` on an
   `IoError` inside a function returning `! LedgerError` wraps it in
   `.Journal`. This replaces `From` impls and the `#[from]` attribute. At most
   one case of an error type can absorb a given type.
3. **`any Error`** accepts any error. It's handy for `main` and scripts.

### Inferred error sets

A **private** function can write `! _` and the compiler fills in the union of
the errors it can actually produce. `pub` functions have to spell their error
types out. That's the general Loom rule: infer inside a module, be explicit at
its edges.

## Matching on results

```loom
match fs.read(path) {
    ok(bytes) => process(bytes)
    err(e: IoError) if e.is_not_found() => create(path)
    err(e) => fail e
}
```

## `unravel`: giving up

`unravel(msg)` is a panic. Its type is `never`.

- It unwinds the current thread and runs `defer`s on the way out, including
  fallback defers for `once` values. `once` values without a defer are leaked, and
  the compiler notes this in debug builds.
- When it reaches a `loom` block, the block cancels the sibling threads and
  unravels again in the parent (see [05](05-concurrency.md)).
- `bolt.loom` can set `unravel = .abort` for builds that don't need unwinding.
- It's meant for **bugs**, not expected failures. Plain arithmetic overflow and
  failed `as` casts unravel. So do out-of-bounds indexing and failed `const assert`s, but those happen at compile time.

---

## How this compares

| | Rust | Zig | Swift | Loom |
|---|---|---|---|---|
| propagate | `x?` | `try x` | `try x` | `x?` |
| optional chain | `.and_then(...)` | `if (x) \|v\|` | `x?.y` | `x?.y` (the same `?`) |
| default value | `.unwrap_or(d)` | `x orelse d` | `x ?? d` | `x mend d` |
| catch locally | closure + match | `catch \|e\|` | `do { } catch` | `{ ... } mend e => ...` |
| error set | enum + `From` | inferred sets | `any Error` | unions + `from` cases + `! _` |
| cleanup on error | `Drop` | `errdefer` | `defer` | `defer x => ...` on the resource |
