# Fixture corpora

These directories fill up as the milestones land. Each one is driven by the
fixture harness in `crates/loom/tests/` (not written yet; today the lexer is
tested by unit tests in `crates/loom_syntax` and the CLI by
`crates/loom/tests/cli.rs`).

| Directory | Each file is | Checked against |
|-----------|--------------|-----------------|
| `run/<milestone>/` | a program | exact stdout (`.out`), optional `.err`, `.code`, `.stdin`, `.args` |
| `fail/<area>/` | a program that must not compile | `//~ ERROR L0301` annotations, optional `.stderr` golden |
| `unravel/` | a program that unravels | exit code 101 and the remapped Loom location |
| `lower/` | a lowering case | a reviewed snapshot of the emitted Lua |
| `examples/<name>.out` | expected output of `examples/<name>.loom` | its presence turns that example's check on |
