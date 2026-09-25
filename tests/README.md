# Fixture corpora

Driven by `crates/loom/tests/fixtures.rs`, which runs every fixture through
the real pipeline (resolve, type check, lower, run in the bundled Lua VM)
and reports all failures at once. `cargo test -p loom --test fixtures` runs
just these.

| Directory | Each `NAME.loom` is | Checked against |
|-----------|---------------------|-----------------|
| `run/` | a program | its output, exactly (`NAME.out`). With `NAME.unravel`, it must unravel with the message on that file's first line, at the Loom line the second line names (`line 4`). With `NAME.error`, `main` must fail with that error. |
| `fail/` | a program that must not compile | `//~ CODE …` comments: exactly those diagnostics, on the lines they're on |
| `examples/` | `NAME.out`: the exact output of `examples/NAME.loom`, which must run to the end | |
| `test/` | a file with tests | the report `loom test tests/test/NAME.loom` prints from the repository root (`NAME.out`); it exits with 0 exactly when that report says `ok` |

Programs in `run/` and `examples/` run sandboxed: no environment variables,
files in memory, no terminal and no waiting (see `design/lua-backend.md`).
`test/` runs the real `loom` binary, unsandboxed.

Still to come, as the milestones need them: `lower/` (reviewed snapshots of
emitted Lua).
