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
| `crates/loom_syntax` | source files, spans, diagnostics, tokens, lexer (no C deps, builds fast) |
| `crates/loom_host` | the only owner of the Lua VM (mlua, vendored) |
| `crates/loom_compiler` | everything semantic: resolve, typeck, analyses, lowering, Lua emission |
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

Step 0 (spec renames) and the workspace scaffold are done. The lexer is
implemented. Everything after the lexer (parser, typeck, lowering, run mode)
is planned but not started.
