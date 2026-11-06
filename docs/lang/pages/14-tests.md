title: Tests
summary: `test name() { ... }`, what makes a test pass or fail, and `loom test`.
---
Loom ships its own test runner. A test is a declaration in the language, it sits in the same file as the code it checks, and `loom test FILE` runs all of them and prints a report like cargo's. There's no framework to choose, no naming convention to remember, and no separate build of the program to keep in step.

This is a short chapter, because there isn't much to it. That is the point.

## Writing a test

A test is written like a function, with `test` in place of `fun`. It takes no parameters and returns nothing, or `! E` so it can use `?`.

@sample tests_pass

- A test passes when it returns. It **fails** when it unravels (a failed `assert`, say) or returns an error.
- A passing test prints nothing. For a failing one the report shows what it printed, and then why it failed.
- `loom test` runs every test, one after another, even after one fails. A file of tests needs no `main`.
- `test name() = expr` is the short form, as for functions.

@try Change `clamps_low` to expect `1` instead of `0`, and run it. The report now has a `FAILED` line and says which `assert` failed. Then add a message as a second argument, `assert(x, "why")`, to see where it shows up.

`assert(condition)` and `assert(condition, message)` are the only helpers: the message can hold `{...}` holes, so the numbers that matter can go into it, which is how you get "left" and "right" in a failure.

@compare
-- Rust
Rust marks a function with `#[test]`, and conventionally puts tests in a `#[cfg(test)]` module in the same file, so they're compiled only for `cargo test`. Loom's `test` is the same idea as a declaration, so there is no cfg module to write.
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_low() {
        assert_eq!(clamp(-5, 0, 10), 0);
    }
}
```
-- Zig
Zig's `test "name" { ... }` blocks are also declarations in the file, built only by `zig test`. A test fails when it returns an error or panics, and `try` is the way to use `?`-style propagation inside one.
```zig
test "clamps low" {
    try std.testing.expectEqual(@as(i32, 0), clamp(-5, 0, 10));
}
```
-- Go
Go finds tests by name and signature: a function `TestXxx(t *testing.T)` in a file ending `_test.go`. A test reports a failure with `t.Errorf` and carries on, or stops with `t.Fatalf`. A Loom test stops at its first failed `assert`.
```go
func TestClampLow(t *testing.T) {
    if got := clamp(-5, 0, 10); got != 0 {
        t.Errorf("clamp = %d, want 0", got)
    }
}
```
-- Python
pytest discovers functions named `test_*` and rewrites the plain `assert` to report both sides of a comparison. Loom's `assert` doesn't introspect: you get your message, and the line.
```python
def test_clamps_low():
    assert clamp(-5, 0, 10) == 0
```
@end

## Tests next to your program

A test can use anything in its file: the warps, the functions, the constants. Because tests exist only for `loom test`, they can sit right under the code they check, in a file that has a `main`, and cost nothing when the program runs.

@sample tests_shapes

- Tests exist only for `loom test`. `loom run` and `loom build` leave them out, and nothing can call one.
- A test's name isn't in scope, so a test and a function can share it: `test len()` and `fun len()` above. Two tests can't share a name ([N0104](diag:N0104)).
- Every command type-checks tests, so `loom check` catches a broken one without running anything.

@try Change `some(2)` to `some(3)` in `push_then_pop`. The report shows the failing line and the message you wrote, "last in, first out", and the other test still runs.

## When a test fails

A failing test shows what it printed, and then why. A test fails by unravelling, with the same report a program would give, or by returning an error, which is shown by its `Show`:

@sample tests_fail

`odd_number` printed a line, then failed on the `?`: the report shows the line, then `error: Bad(n: 3)`. `wrong_length` unravelled on its `assert`, and the report points at it. `halves` passed and says nothing more.

`loom test` exits with 0 when every test passed, 1 when one failed, and 70 when a test hit a bug in Loom itself. A test that calls `process.exit` counts as failed, and the rest still run.

@try In `tests_fail`, change `half(3)` to `half(4)`. The second test now passes and prints nothing: its `print`s are only shown when it fails.

## Where tests go

Tests go at the top level of a module. A test inside a function is [S0212](diag:S0212), and a test with the wrong signature is [T0119](diag:T0119):

@sample tests_err_sig

Both come from the compiler, before anything runs, so `loom check` shows them too.

## Tests run unsandboxed

A test is an ordinary run of your code. It can read the environment and files, which is why a test that depends on them will pass on one machine and fail on another. If the answer can be known while compiling, a `const assert` is stricter: it runs on every build, in the sandbox, and can't depend on anything outside the program.

(`const assert(clamp(-5, 0, 10) == 0, "clamp low")` at the top of the file would check `clamp` on every build, as [Compile-time code](page:compile-time#const-assert) showed.)

`loom test --backend lua` runs the same tests on the Lua backend.

## What's not there

Loom's test support is the smallest it could be. There is no `assert_eq` (write `assert(a == b, "{a} vs {b}")`), no way to pick one test by name, no setup and teardown, no way to mark a test as expected to fail, and no way to check that code unravels. A test that wants a clean state builds one, and `defer` cleans up. These may come; they aren't built.

## Where next

[Threads](page:threads) is the last of the big features, and the one the compiler checks hardest. [Not supported yet](page:not-yet) lists what's missing across the language.
