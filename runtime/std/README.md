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
  (`num.loom`);
- `Map[K, V]` and `Set[T]` (`map.loom`, `set.loom`);
- `Stream[T]` (`iter.loom`), the lazy streams `chars()`, `indexed()` and
  friends make, whose methods arrays, lists, slices, ranges and `Iter` types
  have too.

A generic one's implementation gets the descriptor of each type argument
first; a `mut self` method of `String` returns its result, then the new
string.

`prelude.loom` lists the names every module sees without a `use`.
