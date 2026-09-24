# Fixture corpora

Driven by `crates/loom/tests/fixtures.rs`, which runs every fixture through
the real pipeline (resolve, type check, lower, run in the bundled Lua VM)
and reports all failures at once. `cargo test -p loom --test fixtures` runs
just these.

| Directory | Each `NAME.loom` is | Checked against |
|-----------|---------------------|-----------------|
| `run/` | a program | its output, exactly (`NAME.out`). With `NAME.unravel`, it must unravel with the message on that file's first line, at the Loom line the second line names (`line 4`). With `NAME.error`, `main` must fail with that error. |
| `fail/` | a program that must not compile | `//~ CODE …` comments: exactly those diagnostics, on the lines they're on |

Still to come, as the milestones need them: `lower/` (reviewed snapshots of
emitted Lua) and `examples/NAME.out` (the expected output of
`examples/NAME.loom`, which turns that example's check on).
