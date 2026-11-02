# Plan: `collect[C]` and `Default`

Status: proposal, 2026-10-20. Both are in `design/stdlib-plan.md` (phase 0 and
phase 1) and were deferred because they need the compiler. Nothing here is
built. "Fixture" below means a small Loom program in `tests/run` with the
output it must print (`.out`); it runs on Lua and on Treadle and the two must
agree.

## 1. `collect[C]`

### What exists

`Stream[T].collect()` is a compiler builtin that gives a `List[T]` and is part
of the fused stream path. Everything else a stream is gathered into is written
by hand (`Set.extend`, `Map.extend`, a `String` loop).

### The proposal

```text
pub weft FromStream[T] {
    fun from_stream(take items: Stream[T]) -> Self
}
// woven by: List[T], Set[T] (T: Hash), Map[K, V] for items (K, V),
// String for char and for str items, Deque[T], PriorityQueue[T], BitSet for usize.
// On Stream[T]:  pub fun collect[C: FromStream[T]](take self) -> C
```

Two spellings must work: `words.collect[Set[String]]()` (explicit), and
`let s: Set[String] = words.collect()` (the target type decides). A bare
`words.collect()` with no target stays a `List`, so no existing program changes.

### Compiler work

1. **Inference of `C` from the expected type.** Today a generic method's type
   arguments come from the arguments or are written. `collect` has none that
   mention `C`, so the checker must take `C` from the expected type of the call,
   and default it to `List[T]` when there is none. This is the one real
   language change; it is the same machinery `parse[T]` would use if it were
   inferred.
2. **Keep the fused path.** `collect()` into a list must stay the builtin
   (`heddle/build/stream.rs`), so a chain ending in `collect` still fuses and
   allocates once. The weft path is only taken for other targets. Concretely:
   the builder keeps its `"collect"` arm for `C = List[T]`; any other `C`
   resolves to the weft's `from_stream`, which is plain Loom.
3. **Where the weaves live.** Each is a `weave FromStream[T] into X` in the
   module that defines `X`, written in Loom (a loop of `push`/`insert`).
   `std` weaves into `std` types, so this needs the orphan rule to allow it
   (it already does for `Show`).

### Fixtures

| Fixture | Checks |
|---|---|
| `collect_list` | bare `collect()` and `collect[List[T]]()` are the same; a fused chain allocates once (dispatch count equal to today's) |
| `collect_set` | duplicates vanish, order of first appearance kept; `Set[String]` from `words()` |
| `collect_map` | pairs become entries, a repeated key keeps the last value; `Map[str, usize]` from `indexed()` |
| `collect_string` | `chars().filter(..).collect[String]()` and from a stream of `str` |
| `collect_target` | `let s: Set[i32] = xs.collect()` infers `C`; a wrong target (`Set` of a non-`Hash` item) is a type error with a code, not a crash |
| `collect_user` | a user warp weaves `FromStream[T]` and `collect` builds it |
| `collect_empty` | every target from an empty stream |
| count gate | `collect_list` chain stays within 1.0x of today's dispatch count; `collect[Set]` within 1.25x of a hand loop (`tests/std_counts.rs`) |

### Risks

The inference rule changes what some existing calls mean if an expected type
is a stream or list by accident (`let xs: List[i32] = s.collect()` is fine;
passing `s.collect()` where a `Stream` is expected would now pick `C = Stream`
and be rejected). The fix is the rule "the weft must be woven by `C`", with a
clear error naming the types that are.

## 2. `Default`

### What exists

Nothing: field defaults in a warp (`port: u16 = 8080`) are used by
construction, but there is no way to ask a type for its default value.

### The proposal

```text
pub weft Default { fun default() -> Self }
```

Woven by hand for: every integer (0), floats (0.0), `bool` (false), `char`
(`'\0'`), `str`/`String` (empty), `?T` (`none`), `List`/`Set`/`Map`/`Deque` and
the other collections (empty), tuples (each part's default), arrays `[T; N]`
(N defaults), `()`.

Derived for a warp that lists `Default` in its header (the way `Eq`, `Ord` and
`Hash` are derived): each field takes its declared default if it has one,
otherwise its type's `Default`; a warp with cases takes the first case if it
has no payload, otherwise it is an error asking for a hand-written one.

```text
warp Config: Default {
    host: String = "localhost"
    port: u16 = 8080
    verbose: bool
}
// Config.default()  ->  Config(host: "localhost", port: 8080, verbose: false)
```

`T.default()` is called with the type, so it is an associated function with
no receiver: the checker already resolves `Rng.seeded(5)` that way. In generic
code `T: Default` then `T.default()` must work, which needs that call form on
a type parameter (the same support `Ord`'s `T::lt` style uses does not exist
here, so this is the second real compiler change).

### Compiler work

1. A `Default` weft in `ops.loom` (`#builtin`, like `Eq`), and `check_derive`
   in `typeck/weave.rs` learns it: every field must weave `Default` or have a
   declared default; otherwise the same error code as the other derives.
2. Lowering: the derived method is a function that builds the value from the
   field defaults. Both backends already construct warps with field defaults,
   so the derived body is "construct with no arguments" plus the missing
   fields' `Default::default()` calls.
3. `T.default()` on a type parameter bound by `Default`.

### Fixtures

| Fixture | Checks |
|---|---|
| `default_scalars` | every integer width, floats, `bool`, `char`, `str`, `String` |
| `default_containers` | `?T`, `List`, `Set`, `Map`, tuples, arrays, `()` |
| `default_derive` | the `Config` above; field default beats type default; nested derived warps |
| `default_generic` | `fun zero[T: Default]() -> T = T.default()` at three types |
| `default_missing` (fail) | a field whose type has no `Default` and no declared default is a type error with a code naming the field |
| `default_cases` | a warp with cases: first payload-free case; a payload-only warp errors |
| `default_collect` | with `collect`: `Map[K, V]` entry defaults through `get_or(k, V.default())` |

## 3. Order of work

1. `Default` first: smaller, no inference change, and `collect` fixtures use it.
2. The call form `T.default()` on a type parameter.
3. `FromStream` and the inference of `C` from the expected type.
4. Weaves for the std collections, then the count gates.

Each step ends with the five commit gates plus the new fixtures on both
backends; the dispatch-count and scaling gates (`bench/counts.txt`,
`tests/std_counts.rs`) are extended with the `collect` cases in step 3.
