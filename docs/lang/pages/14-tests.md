title: Tests
summary: `test name() { ... }` and `loom test`.
---
A test is written like a function, with `test` in place of `fun`. It takes no parameters and returns nothing, or `! E` so it can use `?`. `loom test FILE` runs the file's tests one after another and prints a report like cargo's.

@sample tests_pass

- A test passes when it returns. It **fails** when it unravels (a failed `assert`, say) or returns an error.
- A passing test prints nothing. For a failing one the report shows what it printed, and then why it failed.
- `loom test` runs every test, even after one fails. A file of tests needs no `main`.
- Tests exist only for `loom test`. `loom run` and `loom build` leave them out, and nothing can call one: a test's name isn't in scope, so a test and a function can share it. Two tests can't.
- Tests go at the top level of a module ([S0212](diag:S0212) inside a function). A test with the wrong signature is [T0119](diag:T0119).
- Every command type-checks tests, so `loom check` catches a broken one.

@sample tests_fail

`loom test` exits with 0 when every test passed, 1 when one failed, and 70 when a test hit a bug in Loom. Tests run unsandboxed: they can read the environment and files.

## Where next

[Not supported yet](page:not-yet) lists what's missing.
