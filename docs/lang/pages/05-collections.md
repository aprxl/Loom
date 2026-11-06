title: Collections and streams
summary: Arrays, lists, slices, maps, sets, and lazy streams.
---
Most programs are mostly about *many* of something: lines, scores, words, users. Loom gives you a small family of collections for that: arrays for a fixed number of items, lists that grow, slices to look at part of either, maps and sets for lookup, and **streams**, lazy sequences that all of the above can turn into.

The collections are ordinary generic warps from the standard library (see [Generics and `any`](page:generics)), so their methods are listed in [std.list](std:list#List), [std.slice](std:slice#Slice), [std.map](std:map#Map), [std.set](std:set#Set) and [std.iter](std:iter#Stream). This chapter is the tour, and the ideas that connect them.

## Arrays

An array has its length in its type: `[i32; 3]`. `[1, 2, 3]` is one, `[0u8; 4]` repeats a value four times, and `[[0; 3]; 2]` is two rows of three. Because the length is part of the type, arrays live inline, with no allocation. The length is worked out when compiling, from literals, constants and arithmetic on them.

@sample coll_arrays

- **Indexing is bounds-checked.** `fixed[3]` on a three-item array unravels (the last line of the sample), and `get` gives an optional instead: `fixed.get(7)` is `none`.
- An index whose type can't reach past the end needs no check at all: a `u4` into a `[u8; 16]` is always in range, which is why a CHIP-8 emulator can keep its registers in a `[u8; 16]` for free.
- `for mut x in xs` gives you each element to change in place.
- Arrays compare with `==` by value.

> `print` of an optional shows the value or `none`; `{x:?}` shows `some(...)`. Use whichever reads better.

@try Replace `fixed[3]` with `fixed[2]` and the program ends normally. Then change `[0u8; 4]` to `[0u8; 2 + 2]`: the length is a constant expression, so that works too. Try `[0u8; n]` after a `var n = 4`: the length has to be known when compiling, and the compiler says `n` has no value then ([K0105](diag:K0105)).

@compare
-- Rust
Rust's `[T; N]` is the same type, with the same inline layout. Indexing panics out of range in both languages; `get` returns an `Option` in both.
```rust
let fixed = [1, 2, 3];
let zeros = [0u8; 4];
let grid = [[0; 3]; 2];
let x = fixed.get(7); // None
```
-- Go
Go's arrays are values, copied on assignment, and their length is part of the type, too. In practice Go programs use slices almost everywhere, because an array is awkward to pass around.
```go
fixed := [3]int{1, 2, 3}
var zeros [4]uint8
```
-- C++
`std::array<int, 3>` is the safe counterpart of a C array. `operator[]` is unchecked, and `.at(i)` throws; Loom's plain index is the checked one.
```cpp
std::array<int, 3> fixed{1, 2, 3};
fixed.at(7); // throws std::out_of_range
```
@end

## Lists

`List[T]` is a growable array: push, insert, remove, in any order, and it's yours alone (it owns its items). A literal becomes a list where a list is expected: `let xs: List[i32] = [1, 2]`. `List.filled(0, 4)` makes four zeros, and `.copy()` makes an independent copy.

@sample coll_lists

- Everything that changes the list (`push`, `insert`, `remove`, `pop`, `extend`, `retain`, `dedup`, `truncate`, `sort`) needs the list to be a `var`.
- `remove(i)` gives back the item and `pop()` gives an optional, because a list can be empty. `first()` and `last()` are optionals too.
- `extend` takes a stream, so `xs.extend(other.map(...))` doesn't build an intermediate list.
- Lists are not limited to numbers: `List[String]` owns its strings, and what `xs` owns is freed when `xs` goes out of scope, with no collector involved.
- `list.map(f)` and `list.filter(f)` are **eager** and give a new list; the stream methods are lazy. [Streams](page:collections#streams) says more.

@try Make `xs` a `let` instead of a `var`, and run it. Every `push` and `insert` is now an error: `push` changes the value it's called on, and a `let` can't be changed ([T0107](diag:T0107)). Then try `xs.remove(99)`: out of range unravels.

@compare
-- Rust
`Vec<T>` is the same thing. Loom's methods are named for what they do to the list (`remove`, `retain`, `dedup`), and several take a stream where Rust takes an iterator.
```rust
let mut xs: Vec<i32> = vec![5, 3, 8];
xs.push(1);
xs.insert(0, 9);
xs.retain(|&n| n != 4);
xs.dedup();
```
-- Python
A Python `list` is a growable array of references to objects. Loom's `List[i32]` holds the numbers themselves, and every item has the same type.
```python
xs = [5, 3, 8]
xs.append(1)
xs.insert(0, 9)
xs = [n for n in xs if n != 4]
```
-- Go
Go's slices grow with `append`, and the old slice may or may not alias the new one. A `List` is one owner, so there's no aliasing to reason about.
```go
xs := []int{5, 3, 8}
xs = append(xs, 1)
```
@end

## Slices and views

`[T]` is a **slice**: a view of contiguous items, without owning them. An array or a list passes where a slice is wanted, and has every slice method (`len`, `get`, `sort`, `contains`, `chunks`, ...). Write a function over `[i32]` once and it works for both.

`xs[a..b]` makes a view of part of a list, without copying (and the end is left out, like a range). Views of `var`s can be changed through: `list[1..3].sort()` sorts those two items in the list itself.

@sample coll_slices

- Inside `x[...]`, **`#` is the length of `x`**. `xs[#-1]` is the last item, `xs[1..#-1]` leaves out the first and last, and `xs[#-3..]` is the last three. On text it's the length in bytes.
- `x` has to be a name or a field (`self.items[#-1]`), so that it's never evaluated twice. For anything else, bind it with `let` first.
- `xs[#-1]` on an empty list unravels, because `#-1` overflows.
- A `mut` slice parameter lets a function change its caller's items: `zero_out(mut list[..1])`.
- **A view reads the list as it is now.** Changing the list while a view of it is still going to be used is an error ([M0105](diag:M0105)), because the list might have moved.

@try After `let v = list[1..3]`, add `list.push(1)` and then `print(v)`. The compiler says `v` borrows `list`, which changed after the view was made ([M0105](diag:M0105)); swap the two lines and it's fine. The fix it suggests is `list.copy()`.

@compare
-- Rust
Rust's `&[T]` and `&mut [T]` are the same view, with the same range syntax. The borrow checker enforces the same rule (no growing a `Vec` while a slice of it lives), but through lifetimes on the reference; Loom checks it without a lifetime in sight. Rust has no `#`; the idiom is `xs[xs.len() - 1]`.
```rust
let xs = [10, 20, 30, 40, 50];
let last = xs[xs.len() - 1];
let mid = &xs[1..xs.len() - 1];
```
-- Python
Python slicing copies. `xs[-1]` counts from the end with a negative index, and nothing is ever out of date, at the price of copying and of a view that isn't a view.
```python
xs = [10, 20, 30, 40, 50]
xs[-1]; xs[1:-1]; xs[-3:]
```
-- Go
A Go slice is a view too, and it famously shares storage with the slice it was cut from: an `append` may or may not write to the original. Loom's M0105 is the compiler refusing that situation.
```go
xs := []int{10, 20, 30, 40, 50}
mid := xs[1 : len(xs)-1]
```
@end

## List comprehensions

`[for n in 1..=10 if n % 2 == 0 => n * n]` is `[4, 16, 36, 64, 100]`. After `for` comes a pattern, then `in` and anything you can loop over, an optional `if`, and the value after `=>`. The result is always a `List`. It's the loop that pushes each item, so `?`, moves and borrows inside it behave as they would in that loop.

@sample coll_comprehensions

A comprehension has one `for`. For a grid, nest one inside another, as `table` does.

@try Add `if r != c` to the inner comprehension of `table`, and the rows lose their diagonal items. Then try writing a second `for` in the same brackets and see the parser's complaint: nest them instead.

@compare
-- Python
The inspiration, in a different order: Python puts the value first.
```python
[n * n for n in range(1, 11) if n % 2 == 0]
```
-- Rust
Rust has no comprehensions; a chain of iterator adaptors does the same job.
```rust
let v: Vec<i32> = (1..=10).filter(|n| n % 2 == 0).map(|n| n * n).collect();
```
-- Haskell
List comprehensions are Haskell's too, with the value first and the guard last.
```haskell
[n * n | n <- [1..10], even n]
```
@end

## Maps

`Map[K, V]` is a table from keys to values. Keys must weave `Hash` (numbers, text, and warps that list `Hash` do: see [Deriving](page:types#deriving)). A literal has `key: value` pairs, `[:]` is the empty map, and `m[k] = v` adds or replaces. **A map remembers the order keys were first added**, and going through one, or showing it, follows that order. Sorting is something you ask for.

@sample coll_maps

- `m[k]` unravels when the key is missing, and `m.get(k)` gives an optional. `get_or(k, d)` supplies a default.
- `remove(k)` gives back the old value as an optional. `insert(k, v)` does too.
- `keys()`, `values()` and `pairs()` are streams. `sorted()` gives a list of pairs ordered by key.
- Two maps are equal when they hold the same pairs, in any order.
- A warp that writes its own `eq` and `hash` is still keyed structurally ([Not supported yet](page:not-yet#types-and-library)).

@try Change `ages.sorted()` to `ages.pairs().collect()` and compare the order. Then declare the map as `Map[f64, u8]` and see that `f64` can't be a key because it doesn't weave `Hash` ([T0108](diag:T0108)).

### Counting and updating

The most common thing to do with a map is to change one value in it. `entry(key, default)` gives a place you can assign to: the key is added with the default if it's new. `update(key, f)` applies a function, `retain` filters, and `merge` combines two maps.

@sample coll_counting

This is word counting in a loop: `counts.entry(word, 0) += 1`. For the common cases there are shortcuts, like `chars().counts()` on a stream.

@try Change `counts.entry(word.to_string(), 0) += 1` to `counts[word.to_string()] += 1`. The first new word unravels, because the key isn't there yet; `entry` is the version that adds it.

@compare
-- Python
A `dict` keeps insertion order since 3.7, the same as Loom's `Map`. The idiom for counting is `Counter` or `defaultdict`; Loom's `entry(k, default)` does the second.
```python
from collections import Counter
counts = Counter("the cat and the hat".split())
```
-- Rust
`HashMap` iterates in an unspecified order, which makes output differ between runs; Loom's order is always defined. The `entry` API is the model for Loom's.
```rust
let mut counts: HashMap<String, u32> = HashMap::new();
for word in text.split_whitespace() {
    *counts.entry(word.to_string()).or_insert(0) += 1;
}
```
-- Go
Go randomizes map iteration order on purpose, so that nobody relies on it. Loom goes the other way and promises one. Missing keys read as the zero value in Go; in Loom they unravel (`m[k]`) or are an optional (`m.get(k)`).
```go
counts := map[string]int{}
for _, w := range strings.Fields(text) {
    counts[w]++
}
```
-- Java
`LinkedHashMap` keeps insertion order like Loom's `Map`; plain `HashMap` doesn't.
```java
Map<String, Integer> counts = new LinkedHashMap<>();
for (String w : text.split(" ")) counts.merge(w, 1, Integer::sum);
```
@end

## Sets

`Set[T]` holds each value once, with the same order rule as maps, and its elements must weave `Hash`. `insert` returns whether the value was new. `Set.of([...])` builds one from an array, and the algebra you'd expect is there: `union`, `intersection`, `difference`, `symmetric_difference`, `is_subset`, `is_superset`, `is_disjoint`.

@sample coll_sets

Two sets are equal when they hold the same members, in any order. Also see [std.set](std:set#Set).

@try Change `Set.of([3])` to `Set.of([3, 9])` and run it: `is_subset` becomes `false`. Next make `tags` a `Set[i32]` and see what happens to `Set.of(["red", "green"])`.

@compare
-- Python
Python's `set` has the same algebra, spelled with operators, and has no defined order.
```python
a, b = {1, 2, 3, 4}, {3, 4, 5}
a | b; a & b; a - b; a ^ b
```
-- Rust
`HashSet` and `BTreeSet`: Loom's `Set` is hashing with an insertion order, and `sorted()` gives the `BTreeSet` view.
```rust
let a: HashSet<i32> = [1, 2, 3, 4].into();
let b: HashSet<i32> = [3, 4, 5].into();
a.union(&b); a.intersection(&b);
```
@end

## Streams

A `Stream[T]` is a **lazy** sequence: nothing runs until something asks for the next item. `map`, `filter`, `take` and the other adapters build a bigger stream out of a smaller one; a *consumer* (`collect`, `sum`, `count`, a `for` loop) pulls the items through. That means a stream can be infinite (`1..` counts forever) so long as something stops pulling.

@sample coll_lazy

Read the output in order. `pipeline built` comes before any `making`: building did nothing. `take(2)` asked for two even numbers, so the range made 1, 2, 3 and 4 and no more. The eager `xs.map(...)` on the list printed all its `mapping` lines at once, and before `after the eager map`.

Where streams come from:

- `chars()`, `bytes()`, `lines()` and `words()` on text, `keys()`, `values()` and `pairs()` on maps, `indexed()`, `chunks(n)` and `windows(n)` on anything with items.
- A range (`1..10`, `1..`), an array, a list, a slice, a map, or any type that weaves `Iter` has every stream method too, so `(1..=20).filter(...)` and `xs.sum()` just work.
- A generator function: see [Shuttles](page:shuttles).

**A stream is read once.** Each method takes the stream (it moves, in the sense of [Moves, `once` and `defer`](page:ownership)), so using it again after `s.count()` is a compile-time error, not a surprise empty result:

@sample coll_err_once

If you need the items twice, `collect` them into a list first, or make the stream again.

@try Change `let evens = ...` to `let evens = (1..=10).filter(_ % 2 == 0).collect()`. It's a list now, so it can be counted and then summed.

@compare
-- Rust
Rust's iterators are the model: lazy adapters, consumers, and an iterator that is consumed by the adapter that takes it. The compiler enforces the single use there too, through the move.
```rust
let first = (1..)
    .filter(|n| n % 2 == 0)
    .map(|n| n * 10)
    .take(2)
    .collect::<Vec<_>>();
```
-- Python
Generators and iterators are lazy as well, but reuse is silent: once exhausted, the second `sum` returns 0 and nothing complains.
```python
evens = (n for n in range(1, 11) if n % 2 == 0)
print(sum(1 for _ in evens))   # 5
print(sum(evens))              # 0: already used up
```
-- Java
`Stream<T>` is lazy and single-use too, but reuse is a runtime `IllegalStateException`. Loom refuses to compile it.
```java
var evens = IntStream.rangeClosed(1, 10).filter(n -> n % 2 == 0);
evens.count();
evens.sum(); // throws IllegalStateException
```
-- Haskell
Lists are lazy and reusable, because values are immutable and shared by a garbage collector. With ownership and no collector, Loom's streams are consumed.
```haskell
take 2 (map (* 10) (filter even [1 ..]))
```
@end

## Using streams

The adapters, in groups:

| Group | Methods |
|-------|---------|
| shape one stream | `map`, `filter`, `filter_map`, `flat_map`, `take`, `skip`, `take_while`, `skip_while`, `step_by`, `inspect`, `scan` |
| combine | `zip`, `chain`, `indexed`, `cycle` |
| group the items | `chunks(n)`, `windows(n)` |
| answer one question | `fold`, `reduce`, `all`, `exists`, `find`, `position`, `count`, `last`, `nth`, `each` |
| need a bound on `T` | `sum`, `product` (`Num`), `min`, `max` (`Ord`), `join` (`Show`) |

@sample coll_streams

- `sum` and `product` need `Num`, `min` and `max` need `Ord`, and `join` needs `Show`. [std.iter](std:iter#Stream) has the whole list.
- `max` and `min` give optionals, because a stream can be empty. `reduce` does too; `fold` takes a starting value and doesn't need to.
- `xs.map(...)` and `xs.filter(...)` directly on a list or an array are eager. Put `.take(...)`, `.skip(...)`, `.indexed()` or any other stream method first and the rest stay lazy.

@try Replace `xs.fold(0, (acc, x) => acc + x)` with `xs.reduce(_ + _)`. It gives an optional instead, because `reduce` has no starting value. Then replace `xs.all(_ > 3)` with `xs.all(_ > 4)` and watch the answer change.

## Collecting

Streams end by *collecting* into something: `collect()` makes a list, and `collect_as()` makes whatever collection you name (a `Set`, a `Map` from a stream of pairs, a `List`). A few consumers collect in a particular shape: `partition(f)` gives two lists, `group_by(key)` gives a map of lists, and `counts()` gives a map of counts.

@sample coll_collect

- `sorted()`, `sorted_by(before)` and `sorted_by_key(key)` take a stream and return a sorted list. `before` is a "comes first" function: `_.len() < _.len()`.
- `max_by_key` and `min_by_key` give the item whose key is largest or smallest, as an optional.
- The type of `collect_as()` comes from where it's going: a `let` with a type, or `collect_as[List[i32]]()`.

@try Change `Set[str]` to `List[str]` on the `names` line and `collect_as()` makes a list. Then make `lookup` a `Map[str, str]` and let the compiler tell you that the pairs are `(str, u8)`.

@compare
-- Rust
`collect::<T>()` is the one generic consumer, and the target type decides what it builds. Loom splits it into `collect()` for the list and `collect_as()` for anything else.
```rust
let names: HashSet<&str> = people.iter().map(|p| p.name).collect();
let (old, young): (Vec<_>, Vec<_>) = people.iter().partition(|p| p.age > 40);
```
-- Python
Comprehensions build any collection, and `itertools.groupby` needs a sorted input. Loom's `group_by` collects as it goes.
```python
names = {p.name for p in people}
lookup = {p.name: p.age for p in people}
```
-- Java
`Collectors.groupingBy` and `partitioningBy` are the same operations.
```java
Map<Boolean, List<Person>> split = people.stream()
    .collect(Collectors.partitioningBy(p -> p.age() > 40));
```
@end

## Your own streams

Anything that weaves `Iter` is a stream. The weft is small: a type `Item` and a `next` that returns the next item, or `none` when it's done. Write those and the type gets every stream method, and `for` loops over it.

@sample coll_iter

`next` is `mut self`, because an iterator changes as it's pulled. When a whole type is more than you need, `successors` makes a stream from a first item and a function that finds the next, and `from_fn` carries a state along. For a function that produces items as it runs, with `yield`, see [Shuttles](page:shuttles).

@try Make `Countdown` skip by two: change `.n -= 1` to `.n -= 2`, and `.n + 1` to `.n + 2`. Counting down from 5 steps below zero and unravels with an arithmetic overflow: `next` is ordinary code, with the usual checks.

@compare
-- Rust
`impl Iterator for Countdown` with an `Item` type and a `next` is the same shape, and gives every adapter. Loom's `next` takes `mut self` rather than `&mut self`.
```rust
impl Iterator for Countdown {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.n == 0 { return None; }
        self.n -= 1;
        Some(self.n + 1)
    }
}
```
-- Python
`__iter__` and `__next__` are the protocol, and the generator is the shortcut. Loom's shuttles (`yield`) are the generator form.
```python
def countdown(n):
    while n > 0:
        yield n
        n -= 1
```
@end

## Where next

[Optionals and errors](page:errors) covers `?T` and `T ! E`, which you've been meeting in `get`, `pop` and `parse` all through this chapter.
