title: Moves, `once` and `defer`
summary: Who owns a value, what a parameter can do with it, which resources must be used up, and what runs when a scope ends.
---
Without a garbage collector, someone has to decide when a value's memory comes back. Loom's answer is the one you'd give at a workbench: **every value has exactly one owner, and the owner's scope decides when it goes**. That one rule, and the compiler checking it, is what lets Loom free memory without a collector and without a reference count.

This chapter is the rules of that ownership, in the order you'll meet them. First, values: what assigning one does. Then how a function can read, edit or take what you pass it, and what the compiler refuses. Then two tools for resources that need more care than memory does: `once` warps, which can't be forgotten, and `defer`, which runs cleanup however a scope ends.

## One owner: copies and moves

Every variable owns its value. There is no type "reference to `T`" that you can hold on to, so there are no lifetimes to write. Assigning a value, or passing it to a `take` parameter, does one of two things:

- If the type is `Copy`, the value is **copied**, and both sides keep a value of their own. Numbers, `bool`, `char` and tuples of those are `Copy`, and so is a warp whose fields are all `Copy`.
- Otherwise it is **moved**: the new owner has it and the old one is gone. A `String`, a list, a map and any warp that holds one are not `Copy`.

@sample own_values

`Point` is made only of numbers, so `var b = a` leaves `a` untouched. `Person` holds a `String`, so `let q = p` moves it, and `p` can't be used again. (A `once` warp is never `Copy`, whatever its fields are; see below.)

@try After `let q = p`, add `print(p)`. The compiler says `p` was used after it moved, and points at both places. Then give `Person` a `Copy`-only field list (delete `name`) and watch the error disappear.

@compare
-- Rust
Rust's moves are the same: a move empties the old variable, and the compiler tracks it. The difference is that Rust makes you opt in to copying with `#[derive(Copy, Clone)]`, while Loom decides from the fields.
```rust
let p = Person { name: "ann".to_string(), age: 30 };
let q = p;                  // moved
println!("{}", p.age);      // error[E0382]: `p` was moved
```
-- C++
C++ copies by default and moves only when asked (`std::move`). A moved-from object is left in a valid but unspecified state, and the compiler won't stop you from using it. In Loom, using it is a compile error.
```cpp
std::string a = "ann";
std::string b = std::move(a);   // `a` is valid, but empty in practice
```
-- Swift
Swift's structs are value types that copy by default, with copy-on-write behind the scenes. Swift 5.9 added `consume`, which ends a variable's life like a Loom move does.
```swift
let a = "ann"
let b = consume a      // `a` can't be used after this
```
@end

## Reading, editing and taking: parameters

A function's parameters say what it may do with what you pass it, and the call site repeats the interesting part:

| Declared | Call site | The callee |
|----------|-----------|------------|
| `x: T` | `f(x)` | reads `x`; the caller keeps it |
| `mut x: T` | `f(mut x)` | edits `x` in place; the caller keeps it, changed |
| `take x: T` | `f(x)` | owns `x`; the caller loses it unless `T` is `Copy` |

`mut` is written at the call as well as the declaration, so a reader of the caller can see which arguments change. Method receivers are the exception, because `scores.sort()` is already clear.

@sample own_params

The compiler chooses whether a read parameter is passed by value or by address, depending on size; that is not something you spell out. A `mut` parameter of a scalar type (`swap(mut a, mut b)` above) is copied back to the caller when the function returns.

- A read or `mut` parameter is a **borrow**: it lasts exactly as long as the call. Nothing can return one or store one (more on that below).
- `take` is the only way to hand a value over. After `keep(names)` the caller's `names` is gone, but `bump(mut c)` leaves `c` right where it was, only changed.
- Taking a `Copy` value copies it, so the caller still has its own.

@try Add `print(names.len())` after `keep(names)`. The list moved into `keep`, so this is a use after a move ([M0101](diag:M0101)). Then change `keep`'s parameter from `take names` to `names` and watch the error go away.

@compare
-- Rust
Rust has the same three modes, but names them in the type: `T`, `&T` and `&mut T`. The borrowed forms are real types that can sit in structs and need lifetimes when returned. Loom's `x: T` and `mut x: T` are modes of the *parameter*, not types, which is why they need no lifetimes.
```rust
fn peek(c: &Counter) -> i32 { c.n }
fn bump(c: &mut Counter) { c.n += 1 }
fn keep(names: Vec<String>) -> usize { names.len() }
bump(&mut c);
```
-- Swift
Swift's `inout` is a `mut` parameter: exclusive access, written `&` at the call site. Swift 5.9 spells the other two modes `borrowing` and `consuming`.
```swift
func bump(_ c: inout Counter) { c.n += 1 }
func keep(_ names: consuming [String]) -> Int { names.count }
bump(&c)
```
-- C++
C++ gets the same effect with conventions: `const T&` to read, `T&` to edit, and a by-value or `T&&` parameter to take. Nothing stops a `const T&` from outliving what it refers to; Loom's borrows can't.
```cpp
int peek(const Counter& c) { return c.n; }
void bump(Counter& c) { c.n += 1; }
size_t keep(std::vector<std::string> names) { return names.size(); }
```
@end

## Moves are checked

Using a variable after it moved is an error ([M0101](diag:M0101)). The check follows branches and goes through loop bodies twice, so a move inside a loop is caught on the second round.

@sample own_err_moved

Two more moves are refused because the value isn't yours to give away. You can't move a value **out of a borrow**: a read parameter, a loop's item, a lambda's captured variable ([M0102](diag:M0102)). And you can't move a value **out of an element** of a list, because that would leave a hole ([M0103](diag:M0103)).

@sample own_err_borrowed

The fixes are in the help text each error prints: copy the value (`s.copy()`), take ownership with a `take` parameter, or remove it from the list (`remove`, `pop`), which hands you the element and closes the gap.

@try Change `consume(s)` in `peek` to `consume(s.copy())`. The first error goes; the second stays, because `names[0]` still has to be copied or removed.

## One `mut` at a time

While a variable is lent as `mut`, nothing else in the call may touch it. `swap(mut n, mut n)` would give the callee two names for one place, so it's an error ([M0104](diag:M0104)). The check works per field, so `swap(mut p.x, mut p.y)` is fine, as `own_params` showed.

@sample own_err_overlap

This is Rust's "one `&mut` at a time" rule in a smaller form. It applies inside a single call, since a `mut` borrow can't outlive the call that received it.

@try Change the second `mut n` to `mut m` and declare `var m = 2` above. Now there are two places, and the call compiles.

## Borrows are second-class

A borrow lasts as long as the call that received it. A function can't return one or store one. That is the whole reason Loom signatures never mention a lifetime.

But a *local* can hold something that borrows another local: a slice of a list (`xs[1..]`), a stream over one, a lambda that uses variables around it. Such a value sees the local **as it is now**, so if the local changes, a borrow made earlier can't be used ([M0105](diag:M0105)):

@sample own_err_borrow

The same goes for a `for` loop. The loop reads the list as it goes, so its body can't change what it goes over:

@sample own_err_loop

Nor can a borrow outlive what it borrows ([M0106](diag:M0106)). A lambda that uses variables around it is passed as an argument or kept in a local, and that's all. Returning it, storing it in a warp or a list, or passing it to a `take` parameter is an error, and so is keeping it in a variable declared outside the block of a variable it uses. The same goes for a slice of a local list, and for a stream made from one or from such a lambda. A lambda that uses nothing around it is a plain function value and goes anywhere.

@sample own_err_escape

The lambda `x => x + k` borrows `k`, which dies when `adder` returns; `x => x * i` borrows the loop variable. Both would dangle. The help text names the fix: take what you need as parameters, or keep the lambda local. (When you really need a value to outlive its scope, it goes on the heap or in a spool: see [Pointers, spools and `given`](page:memory).)

@try In `own_err_borrow`, move `xs.push(4)` above the `let tail` line. A slice made after the change is fine, and the error is gone.

@compare
-- Rust
Rust lets you name how long a borrow lives (`'a`) and returns references from functions; the borrow checker proves they don't dangle. Loom trades that power for simplicity: borrows are not values, so they can't be returned or stored, and there's nothing to annotate.
```rust
fn longest<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.len() > b.len() { a } else { b }
}
```
-- Swift
Swift's mutable value semantics are the nearest relative: `inout` access is exclusive and lasts a call. Swift has no equivalent of M0105, since its collections are copied before they're changed.
```swift
var xs = [1, 2, 3]
let tail = xs[1...]
xs.append(4)      // `tail` still sees the old array
```
-- Go
Go has no borrow rules at all. A slice shares its backing array, so appending to the original may or may not be visible through the slice. Loom's M0105 stops the question from arising.
```go
xs := []int{1, 2, 3}
tail := xs[1:]
xs = append(xs, 4)   // does `tail` see the change? it depends on capacity
```
@end

## `once` warps

Memory isn't the only thing that needs care. A transaction that was begun should be committed or rolled back; a file handle should be closed; a builder should be finished. Forgetting any of these is a bug the compiler can find.

A `once warp` describes values that must be **used up exactly once**. They can't be copied, and they can't just go out of scope. The ways to use one up are to *destructure* it (`let Tx(moves) = self`, which only code that can see its fields may do, normally its own module) or to pass it to a `take` parameter that does. So the module that defines a `once` type decides which functions can tie it off.

@sample own_once

`commit` and `rollback` are the two ways out. Each takes `self` and unpacks it, which ties the value off.

- A warp that holds a `once` value is `once` itself, and so is a list of them.
- A `once` warp is never `Copy`, whatever its fields are.
- Nothing about `once` exists at run time. It is checked entirely at compile time.

A `once` value that might still be loose when its scope ends, or when a `return`, `fail`, `?` or `break` leaves, is a **loose end** ([L0301](diag:L0301)). The error says where it was made and where it is still loose, and suggests the ways to tie it off:

@sample own_err_loose

@try In `own_once`, delete the `a.commit()` line. Variable `a` is now loose when `main` ends, and the compiler says so.

@compare
-- Rust
Rust values can always be dropped, so the nearest tool is `#[must_use]`, which only warns when a *returned* value is ignored. The usual workaround is a `Drop` that cleans up (rolling back, say) whether or not you meant to. A `once` warp makes forgetting an error, and lets you pick between `commit` and `rollback` by name.
```rust
#[must_use = "commit or roll back"]
struct Tx { moves: Vec<i32> }
```
-- Swift
Swift's noncopyable types (`~Copyable`) have `consuming` methods that end a value's life, which is the same idea. Dropping one just runs `deinit`, though, so forgetting `commit` still compiles.
```swift
struct Tx: ~Copyable {
    consuming func commit() { /* ... */ }
}
```
-- C++
C++ ties cleanup to a destructor, so a forgotten transaction rolls back by itself. That is safe, but it can't *force* you to make a choice, and exceptions from destructors are a minefield.
```cpp
struct Tx { ~Tx() { if (!done) rollback(); } };
```
@end

## `defer`

`defer` runs code when its scope ends, **however it ends**: normally, or through `return`, `fail`, `?`, `break`, `continue` or an unravel. Defers run in reverse order of declaration. A `return` value is worked out before the defers run, and a defer sees variables as they are when it runs, not as they were when it was written.

@sample own_defer

`defer` is how you keep cleanup next to the thing it cleans: open something, defer its closing, and no exit path can skip it.

- A scope is a block: a loop body, an `if` branch and a function body all have their own defers.
- A defer can't change control flow. `return`, `fail`, `?`, `break` and `continue` inside one are [T0118](diag:T0118). It may `unravel`.

@sample own_err_defer

@try In `own_defer`'s `main`, move `continue` above the `defer` line in the loop. The defer is only registered once the scope reaches it, so iteration 1 no longer prints an "end of iteration" line.

@compare
-- Go
Go's `defer` is about the whole *function*: deferred calls run when the function returns, even if the `defer` was in a loop (so they pile up). Loom's `defer` belongs to its block, so the loop body above cleans up on every iteration.
```go
func f() {
    defer fmt.Println("declared first, runs last")
    defer fmt.Println("declared second, runs first")
}
```
-- Swift
Swift's `defer` is block-scoped like Loom's, and runs in reverse order too. Neither can `return` from inside one.
```swift
func plain() {
    defer { print("declared first, runs last") }
    defer { print("declared second, runs first") }
}
```
-- Zig
Zig has `defer` with the same block scope. For the error-only variant, Zig has `errdefer`; Loom's answer is the fallback defer below.
```zig
fn plain() void {
    defer std.debug.print("declared first, runs last\n", .{});
    defer std.debug.print("declared second, runs first\n", .{});
}
```
-- Rust
Rust cleans up through `Drop`, which runs when an owner goes out of scope. That is the same guarantee for a single value; `defer` is for ad-hoc cleanup that doesn't belong to a type.
```rust
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) { println!("cleanup"); }
}
```
@end

## Fallback defers

A **fallback defer**, `defer tx => tx.rollback()`, names a `once` value and runs only on the exits where that value is *still loose*. It is how `transfer` rolls back on an early exit without also rolling back after a commit:

@sample own_fallback

At every exit the compiler already knows whether `tx` was used up on that path, since it has to know that to check `once` at all. So a fallback defer costs nothing on the paths that don't need it. Giving `tx` a new value (`tx = Tx.begin()`) re-arms it.

In `transfer(false)` the early `return` leaves with `tx` still loose, so the defer fires and "rolled back" is printed. In `transfer(true)` the `commit` has already used `tx` up, so the defer stays quiet.

@try Delete the `defer tx => tx.rollback()` line. The `return` inside `if not good` is now a loose end ([L0301](diag:L0301)), and the compiler even suggests the defer.

@compare
-- Zig
`errdefer` is the ancestor: it runs only when the function returns an error. A Loom fallback defer is keyed on the *resource* instead of on "an error happened", so it also covers an early `return` that isn't an error, and stays silent after a commit.
```zig
var tx = try Tx.begin();
errdefer tx.rollback();
try tx.credit(5);
tx.commit();
```
-- Rust
In Rust the same pattern is a `Drop` guard with a flag the commit sets. Loom's compiler knows the flag's value on every path, so the flag isn't stored.
```rust
impl Drop for Tx {
    fn drop(&mut self) {
        if !self.done { self.rollback(); }
    }
}
```
@end

## Where next

[Pointers, spools and `given`](page:memory) puts values on the heap and in arenas: the next step when a value has to outlive its scope.
