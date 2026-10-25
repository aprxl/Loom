title: Collections and streams
summary: Arrays, lists, slices, maps, sets, and lazy streams.
---
## Arrays, lists and slices

- `[T; N]` is a fixed-size array. `[1, 2, 3]` is one, and `[0u8; 4]` repeats a value.
- `List[T]` is a growable array. A literal becomes a list where a list is expected: `let xs: List[i32] = [1, 2]`.
- `[T]` is a slice, a view of contiguous `T`s. An array or a list passes as a slice, and has every slice method.

Indexing is bounds-checked, and an out-of-range index unravels. `get` gives an optional instead. `xs[a..b]` makes a view without copying. A view reads the list as it is now, so changing the list while a view is live is an error ([M0105](diag:M0105)). `for mut x in xs` edits the elements in place.

@sample coll_lists

An index whose type can't reach past the end (a `u4` into a `[u8; 16]`) needs no check.

The methods are listed in [std.list](std:list#List) and [std.slice](std:slice#Slice). On slices and lists, `map` and `filter` are eager and make new lists. The stream methods are lazy.

Inside `x[...]`, `#` is the length of `x`: `xs[#-1]` is the last element, `xs[1..#-1]` leaves out the first and last, and `xs[#-3..]` is the last three. On text it's the length in bytes. `x` has to be a name or a field (`self.items[#-1]`), so it's never evaluated twice; for anything else, bind it with `let` first. `xs[#-1]` on an empty list unravels, because `#-1` overflows.

A list comprehension builds a list from a loop: `[for n in 1..=10 if n % 2 == 0 => n * n]` is `[4, 16, 36, 64, 100]`. The `if` is optional, the pattern is a `for` pattern, and the result is always a `List`. It is the loop that pushes each element, so `?`, moves and borrows inside it work as they would in that loop.

## Maps and sets

`Map[K, V]` and `Set[T]` remember the order things were first added, and going through one, or showing it, follows that order. Keys must weave `Hash`, which a warp gets by listing it. `m[k]` unravels when the key is missing, and `m.get(k)` gives an optional. `m[k] = v` adds or replaces. `[:]` is the empty map literal.

@sample coll_maps

Two maps are equal when they hold the same pairs in any order. See [std.map](std:map#Map) and [std.set](std:set#Set).

## Streams

A `Stream[T]` is a lazy sequence: nothing runs until something asks for the next item. Every stream method takes the stream, so a stream is read once: using it again after `s.map(f)` or `s.count()` is a move error (M0101). `chars()`, `bytes()`, `lines()`, `words()`, `indexed()`, `chunks(n)` and `windows(n)` make streams. Ranges, arrays, lists, slices and maps have every stream method, and so does any type that weaves `Iter`. `for` loops over any of them.

To make your own, weave `Iter`: give the type an `Item` and a `next` that returns `?Item`.

@sample coll_streams

`sum` and `product` need `Num`, `min` and `max` need `Ord`, and `join` needs `Show`. See [std.iter](std:iter#Stream) for the whole list. A generator function makes a stream too: see [Shuttles](page:shuttles).

## Where next

[Optionals and errors](page:errors) covers `?T` and `T ! E`.
