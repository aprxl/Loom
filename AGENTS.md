# Working on Loom

Loom is a language design (README.md, design/, examples/) plus a Loom → Lua 5.4
transpiler written in Rust (crates/, runtime/, tests/). The backend's
decisions live in `design/lua-backend.md`; read it before touching lowering or
the runtime.

## Gates (run before every commit)

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p loom --no-default-features --features lua55   # keeps emitted Lua inside the 5.4 ∩ 5.5 subset
```

Read your own diff before committing. One logical change per commit.

## Toolchain

- Rust stable ≥ 1.98, edition 2024, host `x86_64-pc-windows-msvc`.
- C code (the vendored Lua VM) is compiled with **clang-cl** and linked with
  **lld-link**, never MSVC `cl.exe`. `.cargo/config.toml` sets this up. If a
  build log shows `cl.exe`, that's a bug.

## Layout

| Path | What |
|------|------|
| `crates/loom_syntax` | source files, spans, diagnostics, tokens, lexer, parser, AST, AST printer (no C deps) |
| `crates/loom_host` | the only owner of the Lua VM (mlua, vendored) |
| `crates/loom_compiler` | everything semantic: resolve (`hir`, `modules`), then typeck, analyses, lowering, Lua emission |
| `crates/loom` | the `loom` binary (build, run, check, test, hidden lua-host) |
| `runtime/rt` | `loomrt` Lua sources, embedded into the binary |
| `runtime/std` | the standard library, written in Loom |
| `tests/` | fixture corpora (run, fail, unravel, lower, examples) |

## Conventions

- Diagnostics carry a code from `loom_syntax::diag::codes` (S lexer/syntax,
  N names, T types, M moves, L once, R regions, C concurrency, G gauges,
  P patterns, K const, U unsupported, I interop). Never invent a code inline.
- Unsupported features fail with a `U` code. Never miscompile silently.
- Anything whose iteration order can reach output uses `IndexMap`/`BTreeMap`,
  so builds are byte-for-byte reproducible.
- Keep the design docs in sync when behavior changes. If an example stops being
  accurate, fix the example or the implementation, not both silently.

## Status

Done: the spec renames, the workspace scaffold, the lexer, the parser, name
resolution (HIR, `std` declarations, the prelude), and two slices of the
language through every layer (type checking, lowering to Lua, the runtime,
`loom build` / `loom run`):

1. numbers of every width, strings, tuples, functions, control flow,
   `match` on literals and tuples, operators, casts, templates;
2. warps: records, cases, shared fields, tuple warps, construction, fields,
   methods, associated functions and constants, UFCS, the leading dot,
   dot-blocks, `is`, case and twill patterns with exhaustiveness over cases,
   `mut` parameters of table-backed types, and value semantics for `Copy`
   warps;
3. wefts and weaves (in progress): inline and separate weaves, default
   methods checked per type, derived `Show`/`Eq`/`Ord`, `Weft.f(x)`,
   `{x}`, `print`, `==`, `<` through hand-written weave methods, and
   `through` delegation;
4. generics, checked per instance: generic functions with bounds and
   inferred or explicit arguments, generic warps and their methods, generic
   weaves, and wefts as parameter types (implicit generics);
5. `any W`: dynamic dispatch through vtables, type tests (`w is b: Button`),
   and `any` of more wefts passing as `any` of fewer;
6. optionals and errors: `?` (to the nearest `mend`, or out of the
   function), `mend` and `mend e =>`, `fail`, `some`/`none`/`ok`/`err` as
   values and patterns, union types, error conversions (unions, `from`
   cases, `any Error`), and `main ! E`;
7. arrays, lists and slices: literals, `[x; n]`, bounds-checked indexing and
   index assignment, slicing, `for` and `for mut` over them, and the methods
   std declares on `List` and `Slice`;
8. `mut` parameters of scalars (copied back to the caller), and the methods
   of `str`/`String`, `char`, integers and floats, `s.parse[T]()`, and
   slicing text;
9. lambdas (with holes), function values and calling them; `Iter` with
   associated types (`type Item`, `I.Item`), `Stream[T]` and its methods,
   which lists, ranges and `Iter` types have too, `for` over any of them,
   `chars()`/`bytes()`/`lines()`/`words()`, `chunks`/`windows`, and `where`
   bounds on a builtin warp's parameters (`sum` needs `Num`, `sort` `Ord`).
   `examples/widgets.loom` runs, checked by `tests/examples/widgets.out`;
10. moves: use after move, moving out of borrows and elements, overlapping
    `mut` arguments (M01xx); match exhaustiveness by pattern usefulness
    (nested patterns, integer ranges, twill patterns); functions over Lua's
    200 locals; stack overflow as an unravel;
11. `defer` and fallback `defer x => …`, on every way out of a scope;
12. gauges: range and predicate gauges, literal checks (G0201), `as`/`as?`/
    `as|` into them, `is` tests and narrowing, interval arithmetic, and
    `env.get`. `examples/tour.loom` runs, checked by `tests/examples/tour.out`.

13. maps and sets: `Map()`, `[k: v]` literals, `m[k]` and `m[k] = v`, their
    methods, loops over them, and derived `hash`.

14. `once` warps and loose ends (L0301), and `std.fs` files.
    `examples/ledger.loom` runs, checked by `tests/examples/ledger.out`.
15. tests (`test name() { … }`, `loom test`, checked by `tests/test`), and
    the host: `env.args`, `time.sleep`, `term.raw_mode`, `term.poll_keys`.
16. `Rng` (in Loom), `rotate_left`/`rotate_right`, and `const assert` on
    what the constant folder can work out (K0101). `examples/chip8.loom`
    compiles and runs; `loom test examples/chip8.loom` passes its 3 tests.
17. shuttles: generators (`~> T`), duplex shuttles (`~> T <~ U`) and sinks,
    `yield`, `pull`, `push`/`close` and `once` bursts, as Lua coroutines.
    `examples/lexer.loom` runs but for its `lexer.Frame.size` assert.
18. compile-time code in a sandbox: constants computed by ordinary code,
    `const` expressions, `const assert` anywhere (K0101–K0107), byte
    strings.

Next: the rest of compile-time code (`const` parameters, `const if` and
`const for`, reflection) for `examples/compile_time.loom`, then threads
(`loom`, `thread`, `loom for`, `Atomic`) for `examples/parallel.loom`, then
spools.

Keep these green:
- `every_example_parses_cleanly` (`crates/loom_syntax`) and
  `every_doc_block_parses`: every example and every ```` ```loom ```` block in
  the README and design docs parses.
- `every_example_resolves_cleanly` (`crates/loom_compiler`).
- `every_example_fails_only_with_not_implemented_yet`: the type checker may
  reject an example only with U0102 until it covers what the example uses.
  Code v1 deliberately rejects must use a `U` code.
- `tests/run` and `tests/fail` (`crates/loom/tests/fixtures.rs`): every new
  feature gets fixtures there. `crates/loom_compiler/tests/rt_int.rs` checks
  the runtime's integer operations against the Rust oracle in `ty::int`.
