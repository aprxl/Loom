# std

The Loom standard library, written in Loom (plus `extern "lua"` declarations
for the primitives that live in `runtime/rt`). Modules are embedded into the
`loom` binary (the list is in `crates/loom_compiler/src/modules.rs`); set
`LOOM_STD_DIR` to load them from disk while developing.

So far each module declares only what the examples use, which is enough for
name resolution. `#builtin` marks types and functions the compiler provides
itself.

The methods of the runtime's warps are declared here and implemented in
`runtime/rt/core.lua`, in `rt.ext` under their paths (`std.list.List.push`);
the list of those warps is `RUNTIME_WARPS` in `crates/loom_compiler/src/lower/mod.rs`,
and a test (`crates/loom_compiler/tests/rt_methods.rs`) checks every method
has an implementation:

- `List[T]` (`list.loom`) and `Slice[T]` (`slice.loom`, the methods of `[T]`,
  which arrays and lists have too);
- `String`, and `Str` and `Char`, the methods of `str` and `char`
  (`string.loom`);
- `Int[T]` and `Float[T]`, the methods of every integer and float type
  (`num.loom`), and `Optional[T]`, the methods of `?T` (`option.loom`);
- `Map[K, V]` and `Set[T]` (`map.loom`, `set.loom`);
- `Stream[T]` (`iter.loom`), the lazy streams `chars()`, `indexed()` and
  friends make, whose methods arrays, lists, slices, ranges and `Iter` types
  have too;
- `Shuttle[T, U]` and `Burst[T]` (`iter.loom`): what shuttle functions make,
  and what pushing into one gives back.

A generic one's implementation gets the descriptor of each type argument
first; a `mut self` method of `String` returns its result, then the new
string.

Most of the library after the first slices is plain Loom, not a native: a
method of a `#builtin` warp may have a body (`List.retain`, `Slice.sort_by`,
`Stream.min_by_key`, `Map.merge`). Adaptors that need to remember a lambda
can't be written that way (a closure can't be stored in a warp), so they
stay compiler builtins or are built from `map`, `filter` and friends.
`cmp.loom` has `Ordering` and the free functions `cmp`, `min`, `max`,
`clamp`, which work on any `Ord` type through UFCS; `deque.loom` is
`Deque[T]`. Host services (`io`, `time`) reach both backends through a
native, a `loom_vm` host function and the Lua host table.

`prelude.loom` lists the names every module sees without a `use`.
