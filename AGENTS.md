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
   warps.

Next: optionals and errors (`?`, `mend`, `fail`, `some`/`none`), then
arrays and lists, then wefts and weaves, each through every layer at once.

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
