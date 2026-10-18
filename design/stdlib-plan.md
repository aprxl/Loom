# Standard library plan

Status: proposal, 2026-10-18. Written against `main` at the V5 close-out; nothing
here is built yet. It is a plan, so every signature below is a sketch to argue
with, not a spec.

The goal: make Loom code more fun to write by filling the holes in `std`,
fixing the parts that are weak today, and doing it the Loom way. Rust's and
C++'s standard libraries are the map, not the template.

---

## 1. Principles

1. **Loom first, not Rust or C++ transliterated.** Where the language already
   has a better spelling, `std` doesn't grow a method for it:

   | Not adding | Because |
   |---|---|
   | `checked_add`, `wrapping_add`, `saturating_add` | `+?`, `+%`, `+\|` are operators (01-syntax) |
   | `unwrap_or`, `and_then`, `or_else`, `unwrap`, `expect` | `mend` and `?` (04-errors). 04 says `unwrap()`/`expect()` exist in `std`; they don't, and the doc should lose that sentence |
   | `Rc`, `Arc`, `Box` | spools and `^T` (03-memory) |
   | `Cell`, `RefCell` | waits on open question Q1 |
   | `Iterator` trait zoo (`IntoIterator`, `FromIterator`, ...) | `Iter` plus `Stream`, with `collect[C]` |
   | `try_*` families | `T ! E` plus `?` |

2. **Written in Loom; natives are leaves.** This is vm.md D9 and 9.3: anything
   generic or that calls back into user code is Loom (it then specialises and
   inlines), and a native never calls Loom. New code defaults to plain Loom in
   `runtime/std`, because that gives both backends for free, lets the optimizer
   see through it, and stays off the global native lock (the biggest runtime
   win in `vm-review-v5.md`). A native is only for: UTF-8 and Unicode tables,
   searching and hashing bytes, syscalls, clocks. Host-facing natives go in
   `loom_sys`, which both backends share.

3. **No API returns a borrow.** Borrows are second-class (03-memory), so the new
   text and slice functions that *view* their argument (`split_once`,
   `strip_prefix`, `trim_matches`) have to be compiler builtins like `trim` and
   `lines` are today, not Loom code, until V6's regions give a user-written
   function a way to say "result lives as long as the argument". Everything
   else is Loom. Step 0 below checks this.

4. **Replace, don't alias.** A weak existing API is changed in place; no old
   spelling stays behind. The list is in section 3.

5. **A perf gate per feature, deterministic first.** Section 4.

6. **Spool-ready and parallel-ready.** Collections allocate in the ambient
   spool, have `with_capacity`/`reserve`, and hold no hidden global state, so V6
   changes nothing in their API. Anything shared across threads names its
   synchronisation in its type, as `Atomic`, `Mutex` and `Pipe` do.

7. **Names.** snake_case verbs and nouns as in Rust where Rust's name is good;
   C++ names where they are better (`lower_bound`, `deque`). No keyword may
   be a method name (`any` is already `exists`; so `or`, `and`, `not`, `in`,
   `is` are out). Overflow families, where one is unavoidable (`pow`), use the
   operator's suffix words: `pow`, `pow_wrap`, `pow_sat`, `pow_checked`.

---

## 2. What is there now, and what is weak

Read from `runtime/std` and the compiler's builtin emitters (`heddle/build/*`).

| Module | State | Main gaps |
|---|---|---|
| `iter` | `Iter`, `Range`, `Stream` (map, filter, take, skip, zip, chain, fold, ...) | no `rev`, `step_by`, `flat_map`, `filter_map`, `peekable`, `scan`, `partition`, `min_by_key`; `collect` only makes a `List`; **`sorted_by` is O(n²)** (binary search, then `List.insert`, per item) |
| `list`, `slice` | push/pop/insert/remove/extend, sort, chunks, windows | no `sort_by`, `sort_by_key`, `retain`, `dedup`, `drain`, `swap_remove`, `resize`, `reserve`, `binary_search`; `extend` takes only slices |
| `map`, `set` | insertion-ordered hash map and set | no `retain`, `extend`, `Map.of(pairs)`, set algebra (`union`, ...); no ordered map; no deque, priority queue, bitset |
| `string` | `String`, `Str`, `Char` | **everything is ASCII**: `"é".to_upper()` does nothing, `'é'.is_alphabetic()` is false. No `split_once`, `strip_prefix`, `char_indices`, `rfind`, `splitn`, `pad`, `insert`, `from_utf8` |
| `num`, `math` | a dozen methods on `Int`/`Float`, `PI`, `EULER` | no `sin`/`cos`/`exp`/`ln`, `clamp`, `signum`, `gcd`, `rem_euclid`, `isqrt`, byte conversions, `MAX`/`MIN` |
| `option`, `error` | `?T` has only `take`; `T ! E` has no methods; `Ord` has only `lt` | no `Ordering`/`cmp`, `Default`, `map` on optionals |
| `fmt` | `Show`, `Fmt.put` | a user `Show` can't read the width/precision spec; no `dbg` |
| `io`, `fs` | `print`; `read`, `read_text`, `File.append` | no stdin, stderr, `print` without newline; can't write a file except by appending a line; no directories or paths |
| `time`, `rand` | `Duration` (no arithmetic), `sleep`; `Rng.seeded/next_u64/byte` | no clock; `Rng` can't make a range, float, bool or shuffle; no entropy seed |
| `sync` | `Atomic` (load/store/swap/cas/add/sub), `Mutex`, `Pipe`, `Spun` | no `try_*`, no iteration over a `Pipe`, no `RwLock`, `Once`/`Lazy`, no worker count |
| `process`, `env`, `term` | `exit`; `get`, `args`; raw mode, key polling | no child processes, env listing, cwd, colours, terminal size |
| testing | `assert(cond, msg)` | no `assert_eq` that shows both sides |
| operators | `ops.loom` declares `Copy`, `Eq`, `Ord`, `Hash`, `Integer` | 02-types promises `Add`, `Sub`, `Index` as wefts; `std` declares none, so `Duration`, `Instant` or a vector type can't use `+` (needs confirming in the compiler) |

---

## 3. Existing APIs this plan replaces outright

No aliases and no deprecation window, in line with the code rules.

| Today | Becomes | Why |
|---|---|---|
| `sorted_by(items, before)` free function, O(n²) | same name and shape, O(n log n) and stable (a method of `Stream`, backed by one shared merge sort) | the call site was fine; the algorithm wasn't |
| `Str.trim`, `trim_start`, `trim_end` (ASCII whitespace) | Unicode `White_Space`; `trim_ascii` keeps the old behaviour | a no-break space staying put is a trap, not a feature |
| `Str.to_upper`, `to_lower`, `Char.to_upper`, `to_lower` (ASCII) | Unicode; `to_ascii_upper`, `to_ascii_lower` keep the old | |
| `Char.is_alphabetic`, `is_alphanumeric`, `is_whitespace`, `is_upper`, `is_lower` (ASCII) | Unicode; `is_ascii_alphabetic`, ... for the old | |
| `Char.is_digit` | stays ASCII `0..=9` (it already means that); `is_numeric` is the Unicode one | |
| `Str.words()` (ASCII whitespace) | Unicode whitespace, same name | consistent with `trim` |
| `List.extend(xs: [T])` | `extend(items: Stream[T])`; every slice, list, range and stream is one already, so no call site breaks | |
| `time.ms(n)`, `time.secs(n)` free functions | stay (`250.ms()` reads well via UFCS), plus `micros`, `mins`; `Duration` gains arithmetic and `Show` | |
| `Duration.nanos` as a plain field | `pub(get)` | stops raw construction going stale |
| `fs.File` (`#builtin`, copyable?) | `once warp File` with `close() ! IoError` | 03-memory names file handles that report close errors as the case for `once`. Needs a `defer f => f.close()` story (Q-D5) |
| `fs.IoError` (`NotFound`, `Denied`, `Other`) | adds `AlreadyExists`, `InvalidInput`, `UnexpectedEof`, `Interrupted`, `NotADirectory`; keeps `Other(message)` | |
| docs: 04-errors "`unwrap()` and `expect()` exist in std" | sentence removed | they don't, and `mend unravel` replaces them |

---

## 4. How features are built and gated

### 4.1 Mechanism per feature

| Tag | Meaning | Used for |
|---|---|---|
| **[L]** | plain Loom in `runtime/std` | nearly everything generic: sorting, `retain`, `dedup`, `Deque`, `BitSet`, `Instant` arithmetic, `Rng.range`, `assert_eq` |
| **[B]** | compiler-built Heddle in `heddle/build/*` | functions that must return a view of their argument, and the stream fusion that exists already |
| **[N]** | Rust native in `loom_vm` (+ `core.lua` twin for the Lua oracle) or `loom_sys` for host services | Unicode tables, byte search, syscalls, clocks, entropy |

Every new function has a tag in its phase. Anything [N] must land in both
backends in the same change (AGENTS: new features land on both). [L] arrives on
both for free.

### 4.2 Step 0: the spike that decides the split

A throwaway branch measuring three things, before any phase starts:

1. Does an [L] higher-order function, `fn retain[T](mut xs: List[T], keep: T => bool)`
   written in `std`, called with a lambda, compile at `-O2` to the same loop as a
   hand-written one? vm.md defers the *specialize lambdas* pass and says GVN plus
   inlining covers it "for now". `sorted_by` is the only HOF in `std` written
   in Loom today, so this is unmeasured.
2. Can a user-written `std` function return a view of its argument (`-> ?str`)?
   By 03-memory it can't; if it can, more moves from [B] to [L].
3. Can a prelude-less extension function be in scope for the receiver's type
   (decision D1)?

Outcomes: if (1) is ≤ 1.25x, [L] is the default as above. If it isn't, the
*specialize lambdas* pass comes first, or the HOFs become [B]. Either way it is a
measured decision, not a guess.

### 4.3 Gates

Rules, from what V3 and V5 taught:

- **Deterministic gates first.** `bench/counts.txt` already pins dispatch counts
  to ±2% and runs anywhere. Each new feature adds a micro-benchmark whose count is
  blessed, e.g. `Deque.push_back` ≤ N dispatches, `retain` ≤ 1.1x the hand loop's.
- **Complexity gates**, not only speed: run at n and 10n and assert the ratio
  (sort ≤ 12x for 10x, `Deque` ops flat, `BitSet` ops flat).
- **Allocation gates** with `vm-stats`: `collect` allocates once when the size is
  known; `String` building doesn't reallocate beyond doubling.
- **Wall-clock gates** are `#[ignore]` release tests as in AGENTS.md, best of N,
  as ratios against a hand-written Loom loop or the previous implementation on
  the same machine, never absolute times. Thresholds in this plan are
  *proposed*; each phase's first PR measures and freezes them (V5's scaling gate
  had a 0.1x margin and would flake; leave real headroom).
- **Compile-time gate.** `std` growth must not move the optimizer ratio (1.63x
  now, asserted at 1.75x) or the time of a hello-world `loom check` by more than
  5%. Hence: heavy modules (`json`, `path`, `unicode`, `args`) load only on `use`;
  the prelude's transitive set stays small.
- **No regressions**: all of `bench/` stays within noise; `word_freq` (1.12x) is
  the one to watch because strings and hashing are what it measures.
- **Both backends**: every new fixture in `tests/run` produces identical output
  on Lua and Treadle (the Lua backend is the oracle). The program generator
  gets the new methods in its vocabulary.

Per phase also ships: an `examples/` program or a section of one (checked by a
`.out` file), the `docs/std/data.py` entries (`python docs/gen.py --check`),
and the `runtime/std/README.md` module list.

---

## 5. Phases

Ordered by how much daily code each one improves and what it unlocks. Each is
independent enough to ship alone.

### Phase 0: foundations (unblocks the rest)

```text
// std.cmp
pub warp Ordering: Eq, Ord, Show {
    case Less, Equal, Greater
    pub fn reverse(self) -> Ordering
    /// Breaks a tie: `a.age.cmp(b.age).then(a.name.cmp(b.name))`.
    pub fn then(self, other: Ordering) -> Ordering
}
// `cmp` is a free function in the prelude, so `a.cmp(b)` works on every `Ord`
// type through UFCS. (A default method `Ord.cmp` recursed: the derived `<`
// runs through the runtime's comparison, which finds the new method.)
pub fn cmp[T: Ord](a: T, b: T) -> Ordering
pub fn min[T: Ord](a: T, b: T) -> T
pub fn max[T: Ord](a: T, b: T) -> T
pub fn clamp[T: Ord](x: T, lo: T, hi: T) -> T
/// Flips the order: `xs.sort_by_key(Reverse(_.score))` is highest first.
pub warp Reverse[T: Ord]: Ord { pub value: T }

// std.default
pub weft Default { fn default() -> Self }   // derived: field defaults, else the field type's

// std.option / std.result (methods of `?T` and `T ! E`)
//   ?T:      map, filter, zip, flatten, replace, insert, get_or_insert, take_if,
//            ok_or(e) -> T ! E
//   T ! E:   map, map_err, value() -> ?T, error() -> ?E
//   bool:    then(f) -> ?T, then_some(x) -> ?T
```

Plus: a real sort (below), `mem.swap/replace/take`, operator wefts if they are
missing (D4). Naming: `Ord.cmp` and `then` give `Ordering` chains;
`sort_by(less)` keeps the C++-style boolean comparator that hole lambdas make
short (`pairs.sorted_by(_.1 > _.1)`, as in 01-syntax) and `sort_by_key` is the
common case.

**Sorting** [L, with a native fast path for primitive keys]:

```text
pub fn sort(mut self) where T: Ord                          // exists; same algorithm underneath
pub fn sort_by(mut self, less: (T, T) => bool)
pub fn sort_by_key[K: Ord](mut self, key: T => K)
pub fn sort_unstable(mut self) where T: Ord                 // also _by, _by_key
pub fn is_sorted(self) -> bool where T: Ord
// Streams: sorted(), sorted_by(less), sorted_by_key(key) -> List[T]
pub fn binary_search(self, x: T) -> ?usize where T: Ord     // also _by_key
pub fn lower_bound(self, x: T) -> usize where T: Ord        // the insertion point; C++ name
pub fn upper_bound(self, x: T) -> usize where T: Ord
pub fn partition_point(self, pred: T => bool) -> usize
```

Stable sort: merge sort with insertion runs under ~16 and a pre-check for
already sorted or reversed input. One implementation, used by `sort`,
`sort_by*`, `sorted*`, `Map.sorted`, `Set.sorted`.

Gates: `sorted_by` on 10k/100k/1M items: ratio ≤ 12x for 10x data (the old one is
quadratic; assert it is now not); `sort` on records within 5% of today's
`bench/sort` (0.102 s); sorted and reversed inputs ≥ 3x faster than random;
stability test with equal keys.

### Phase 1: collections and streams (the biggest everyday win)

**Stream** (all [L] or the existing fused [B] path, per step 0):

```text
pub fn rev(take self) -> Stream[T]            // only on sources that know their end:
                                              // slices, lists, ranges, chars(), indexed() of those
pub fn step_by(take self, n: usize) -> Stream[T]
pub fn filter_map[U](take self, f: T => ?U) -> Stream[U]
pub fn flat_map[U](take self, f: T => Stream[U]) -> Stream[U]
pub fn flatten(take self) -> Stream[U] where T: Stream[U]
pub fn scan[S, U](take self, take init: S, f: (S, T) => ?U) -> Stream[U]
pub fn inspect(take self, f: T => ()) -> Stream[T]
pub fn peekable(take self) -> Peekable[T]     // warp: peek(mut self) -> ?T, next_if(mut self, f) -> ?T
pub fn cycle(take self) -> Stream[T]          // needs T: Copy
pub fn nth(take self, n: usize) -> ?T
pub fn min_by_key / max_by_key[K: Ord](take self, key: T => K) -> ?T
pub fn min_by / max_by(take self, less: (T, T) => bool) -> ?T
pub fn partition(take self, f: T => bool) -> (List[T], List[T])
pub fn unzip[A, B](take self) -> (List[A], List[B]) where T: (A, B)
pub fn dedup(take self) -> Stream[T] where T: Eq          // consecutive repeats
pub fn counts(take self) -> Map[T, usize] where T: Hash   // Python's Counter
pub fn group_by[K: Hash](take self, key: T => K) -> Map[K, List[T]]
pub fn chunks(take self, n: usize) -> Stream[List[T]]     // on any stream (slices keep their zero-copy one)
pub fn windows ... (slices only; keeps today's)
pub fn collect[C]() -> C where C: FromStream[T]  // List, Set, Map, String, Deque, ...
pub fn eq(take self, other: Stream[T]) -> bool where T: Eq
pub fn is_empty(take self) -> bool
// sources (std.iter free functions)
pub fn repeat[T: Copy](x: T) -> Stream[T]
pub fn from_fn[T](f: () => ?T) -> Stream[T]
pub fn successors[T](take first: ?T, f: T => ?T) -> Stream[T]
```

`collect[C]` replaces "`collect` makes a list": `FromStream[T]` is a small weft
(`fn from_stream(take s: Stream[T]) -> Self`) woven by `List`, `Set`, `Map` (for
`(K, V)` items), `String` (for `char` and `str`), and the new collections, so
`words.collect[Set[String]]()` and `chars.collect[String]()` work and a user type can
join in. Bare `collect()` stays a list when the target type is known from context.

Gates: a `map/filter/sum` chain stays one fused loop (today `streams`: 12x PUC
Lua); each new adaptor chained in front of `sum` is within 1.25x of the hand loop
by dispatch count; `collect` into `List` allocates once when the source knows its
length; `counts` on 4M items is within 10% of the hand-written `Map` loop in
`word_freq`.

**List and Slice**:

```text
// List
pub fn with_capacity(n: usize) -> List[T]
pub fn reserve(mut self, extra: usize)
pub fn capacity(self) -> usize
pub fn shrink_to_fit(mut self)
pub fn resize(mut self, n: usize, x: T)
pub fn swap_remove(mut self, i: usize) -> T        // O(1)
pub fn retain(mut self, keep: T => bool)
pub fn dedup(mut self) where T: Eq                  // also dedup_by_key
pub fn drain(mut self, r: Range[usize]) -> List[T]  // removes and returns the range
pub fn split_off(mut self, at: usize) -> List[T]
pub fn append(mut self, take other: List[T])        // moves, leaves `other` empty
pub fn extend(mut self, items: Stream[T])           // was: slices only
// Slice
pub fn starts_with(self, p: [T]) -> bool where T: Eq   // ends_with
pub fn rotate_left(mut self, n: usize)                 // rotate_right
pub fn concat(self) -> List[U] where T: [U]            // also join(sep)
pub fn count_of(self, x: T) -> usize where T: Eq
pub fn iter_rev ... no: `xs.rev()` through Stream
```

(`split_at`, `first_mut` and friends are not added: slicing syntax `xs[..i]`,
`xs[i..]` and `xs[i]` already do their jobs, and a function can't return a view.)

Gates: `retain`, `dedup`, `swap_remove`, `resize` within 1.1x of the hand loop
by dispatch count; `drain` and `split_off` are one `mem.move`, not a per-item loop
(allocation and dispatch count asserted); `reserve` then `push` n items does no
reallocation.

**Map and Set**:

```text
// Map
pub fn of(pairs: [(K, V)]) -> Map[K, V]
pub fn with_capacity(n: usize) -> Map[K, V]
pub fn get_or(self, key: K, default: V) -> V           // like `m.get(k) mend d`, no ?V built
pub fn retain(mut self, keep: (K, V) => bool)
pub fn extend(mut self, pairs: Stream[(K, V)])
pub fn remove_entry(mut self, key: K) -> ?(K, V)
pub fn update(mut self, key: K, f: V => V)             // only if present
pub fn merge(mut self, take other: Map[K, V], f: (V, V) => V)
pub fn first / last (self) -> ?(K, V)                  // insertion order is part of the contract
// Set
pub fn union / intersection / difference / symmetric_difference(self, other: Set[T]) -> Set[T]
pub fn is_subset / is_superset / is_disjoint(self, other: Set[T]) -> bool
pub fn retain(mut self, keep: T => bool)
pub fn extend(mut self, items: Stream[T])
pub fn pop(mut self) -> ?T                              // removes the oldest
```

**New containers** (each `[L]` unless noted, each gets a section in the docs):

| Type | Like | Notes |
|---|---|---|
| `Deque[T]` | `VecDeque`, `std::deque` | ring buffer: `push_back/front`, `pop_back/front`, `front/back`, indexing, `iter`, `rotate`, `make_contiguous`. Gate: push/pop O(1) flat at 1k..1M; 1M `push_back` + `pop_front` ≤ 1.3x `List` push + index loop |
| `PriorityQueue[T]` | `BinaryHeap`, `priority_queue` | max-first by `Ord`; min-first with `Reverse`. `push`, `pop`, `peek`, `from(list)` (heapify in O(n)), `into_sorted`. `Heap` is not used: it would collide with the `heap` spool |
| `SortedMap[K, V]`, `SortedSet[T]` | `BTreeMap`, `std::map` | B-tree: ordered iteration, `range(lo..hi)`, `first/last`, `lower_bound`. Fixes `Map.sorted()` being an O(n log n) copy every time. Gate: 1M inserts within 3x of `Map` insert; `range` is O(log n + k) |
| `BitSet` and `BitSet[const N: usize]` | `std::bitset` | inline fixed form and growable form; `set/clear/toggle/test`, `count`, `iter` over set bits, `and/or/xor`. Uses `count_ones`, `trailing_zeros` already in `Int`. Gate: `count` and bulk ops ≥ 8x the `[bool]` equivalent per element |
| `Grid[T]` | not in Rust/C++ | width×height, `grid[(x, y)]`, `rows()`, `neighbors4/8(x, y)`, `fill`, `map`. Not a standard-library classic, but chip8, games and puzzle code reinvent it constantly |

### Phase 2: text

Unicode correctness first (decision D2), then the missing everyday API.

**Unicode** [N, generated tables]: a script (`python tools/gen_unicode.py`, one
pinned Unicode version, output committed) emits two-stage lookup tables for
the VM natives and the same data for `core.lua`, loaded lazily so a program that
never touches non-ASCII pays nothing. ASCII stays an inline fast path in the
compiled method (`b < 0x80` first). Covers: `is_alphabetic`, `is_numeric`,
`is_alphanumeric`, `is_whitespace`, `is_upper`, `is_lower`, `is_control`,
simple one-to-one `Char.to_upper/to_lower`, full-mapping `Str.to_upper/to_lower`
(ß→SS), `Str.trim*`, `Str.words`, plus `eq_ignore_case`. Grapheme clusters and
normalisation are a later proposal (they are large).

Gates: ASCII-only `to_upper` on 1 MB within 5% of today's; non-ASCII within 4x
of ASCII; `Char.is_alphabetic` on ASCII compiles to a compare, not a native call;
hello-world binary and image sizes grow by < 2% (tables are lazy).

**Str** [B for view-returning, N for search, L for the rest]:

```text
pub fn split_once(self, sep: str) -> ?(str, str)             // rsplit_once
pub fn splitn(self, n: usize, sep: str) -> Stream[str]       // rsplit, split_terminator
pub fn split_by(self, f: char => bool) -> Stream[str]
pub fn strip_prefix(self, p: str) -> ?str                    // strip_suffix
pub fn trim_matches(self, f: char => bool) -> str            // trim_start_matches, trim_end_matches
pub fn char_indices(self) -> Stream[(usize, char)]
pub fn rfind(self, part: str) -> ?usize                      // find_by(f), matches, match_indices
pub fn count_of(self, part: str) -> usize
pub fn char_at(self, i: usize) -> ?char                      // i counts characters; O(i), say so in the doc
pub fn is_char_boundary(self, i: usize) -> bool
pub fn is_ascii(self) -> bool
pub fn eq_ignore_case(self, other: str) -> bool
pub fn capitalize(self) -> String
pub fn pad_left(self, width: usize, fill: char = ' ') -> String   // pad_right, center
pub fn escape(self) -> String                                // debug-form escapes
pub fn from_utf8(bytes: [u8]) -> String ! Utf8Error          // String.from_utf8_lossy
pub fn to_bytes(self) -> List[u8]
pub fn parse_radix[T: Parse](self, radix: u32) -> T ! NumError
```

**String** (the builder): `insert(i, c)`, `insert_str(i, s)`, `remove(i) -> char`,
`truncate(n)`, `retain(f)`, `reserve(n)`, `capacity()`, `split_off(at)`,
`drain(range)`, `extend(chars: Stream[char])`, `+=`-style append through
`push_str` taking a template without a temporary (already streams).

**Fmt** (so user `Show` weaves behave like built-ins):

```text
pub fn spec(self) -> Spec          // width, fill, align, precision, radix, debug flag
pub fn put_str(mut self, s: str)
pub fn pad(mut self, s: str)       // writes `s` honouring the spec: `{name:>12}` works for user types
pub fn debug_struct(mut self, name: str) -> StructFmt   // StructFmt.field(name, any Show), .finish()
// std.fmt free functions
pub fn dbg[T: Show](take x: T) -> T      // prints `file:line  expr = value`, returns x
pub fn format(...)                       // not needed: templates are already the format string
```

`dbg` needs the compiler to hand it the expression's source text, so it is the
one item here that is language-assisted. Gates: `pad` on a 1M-iteration loop
adds no allocation beyond the output buffer; `{x:>8}` on a user `Show` within 1.2x
of the same spec on an `i32`.

Gates for the rest: `split_once`, `strip_prefix`, `lines` stay allocation-free
(allocation counter 0); `splitn` and `rsplit` on 1 MB within 1.2x of `split`;
`pad_left` allocates once.

### Phase 3: numbers and math

```text
// Int[T]
pub fn signum / is_negative (self)
pub fn clamp(self, lo: T, hi: T) -> T
pub fn abs_diff(self, other: T) -> unsigned T            // never overflows
pub fn rem_euclid / div_euclid / div_floor / div_ceil
pub fn gcd(self, other: T) -> T                          // lcm
pub fn isqrt(self) -> T
pub fn ilog10 / ilog(base) (self) -> u32                 // next to `log2`
pub fn next_power_of_two(self) -> T                      // Unravels on overflow, like `pow`
pub fn count_zeros / reverse_bits / swap_bytes
pub fn pow_wrap / pow_sat / pow_checked
pub fn to_be_bytes / to_le_bytes(self) -> [u8; T.size]   // from_be_bytes / from_le_bytes
pub fn to_string_radix(self, radix: u32) -> String
// associated constants, if `Int[T]` can carry them: u8.MAX, i32.MIN, T.BITS
// Float[T]
pub fn sin cos tan asin acos atan atan2 sinh cosh tanh exp exp2 ln log2 log10 log(base) cbrt hypot
pub fn fract signum clamp copysign mul_add lerp(self, to: T, t: T)
pub fn to_degrees / to_radians
pub fn is_finite / is_infinite / is_sign_negative
pub fn total_cmp(self, other: T) -> Ordering
pub fn to_bits / from_bits
// std.math constants: TAU, SQRT_2, LN_2, LN_10, INV_PI, PHI, plus per-type EPSILON, MAX, MIN_POSITIVE, INF, NAN
```

`EULER` is `e`, kept because a constant needs two characters; `E` is reserved for
the lexer's case rule, so `math.E` is not available and the name stays as is.

Also in this module (decision-free, [L]): `std.hash` with `combine(a, b)` and
stable `fnv1a(bytes)` / `wyhash(bytes)` for users who need hashes that don't
change between versions.

Gates: `gcd` on u64 within 1.2x of the textbook Euclid loop by dispatch count;
`sin/cos/exp/ln` are one native call each (no Loom wrapper overhead beyond the
call); `pow` with a constant exponent unrolls at -O2; `isqrt` exact for all u64
edge cases (property test against `f64` sqrt with correction). Trig results
match between Lua and Treadle to the last bit where the platform libm allows,
else within 1 ulp, and the fixture says which.

### Phase 4: I/O, files, time, randomness, the system

All host services (`io`, `fs`, `env`, `process`, `term`, `time`) go through
`loom_sys` and are marked as host natives so the compile-time sandbox refuses them
(vm.md 9.3, K0104).

**io.** The pair of wefts mirrors Rust's `Read`/`Write` and C++'s streams, and
makes everything else composable:

```text
pub fn print(x: any Show)                   // exists
pub fn eprint(x: any Show)                  // stderr, newline
pub fn write(x: any Show)                   // stdout, no newline  (ewrite for stderr)
pub fn read_line() -> ?String ! IoError     // stdin; none at end of input
pub fn read_all() -> String ! IoError       // slurp stdin
pub fn lines() -> Stream[String ! IoError]  // stdin as a stream: `for line in io.lines()`
pub weft Reader { fn read(mut self, mut buf: [u8]) -> usize ! IoError }
pub weft Writer { fn write_bytes(mut self, bytes: [u8]) ! IoError }  // + default write_str, flush
pub warp BufReader[R: Reader]   // read_line, lines, read_to_string, bytes
pub warp BufWriter[W: Writer]   // batches small writes; flush on drop
pub warp Cursor                 // a Reader/Writer over a List[u8], for tests
```

Gates: `print` in a 1M-line loop does not flush per line (already true; keep it a
gate); `io.lines()` on a 100 MB file within 1.5x of a `BufReader.lines` loop and
allocates one `String` per line at most; `BufWriter` ≥ 5x faster than unbuffered
`write_bytes` for 1-byte writes.

**fs and path.**

```text
pub fn write_text(path: str, text: str) ! IoError      // write(bytes), append_text
pub fn exists(path: str) -> bool
pub fn read_dir(path: str) -> List[DirEntry] ! IoError
pub fn create_dir_all / remove_file / remove_dir_all / copy / rename (...) ! IoError
pub fn metadata(path: str) -> Meta ! IoError           // size, kind, modified: SystemTime
pub once warp File {
    pub fn open(path: str) -> File ! IoError           // File.create, File.append (exists)
    pub fn read_line(mut self) -> ?String ! IoError    // weaves Reader and Writer
    pub fn close(take self) ! IoError
}
// std.path
pub warp Path(String)   // join, parent, file_name, stem, extension, with_extension,
                        // components, is_absolute, normalize; `/` separator on every OS
```

`Path` joins with `/` in the language even on Windows and converts at the host
boundary, so tests are portable. The Lua oracle supports exactly the same host
calls through `loom_sys`, so there are no `U` diagnostics.

**time.**

```text
pub warp Duration { ... }   // + - * / by integer, Ord, Eq, Show ("1.5s", "250ms"),
                            // as_secs_f64, as_millis, checked arithmetic via the operators
pub warp Instant: Ord       // monotonic clock: `Instant.now()`, `elapsed()`, `since(other)`, `+ Duration`
pub warp SystemTime         // `unix_now()`, `unix_secs()`; calendar dates are a later proposal
pub fn measure[T](f: () => T) -> (T, Duration)
```

Needs the operator wefts (D4) for `+`/`-` on durations. Gates: `Instant.now()`
≤ 100 ns by wall clock (vDSO or `QueryPerformanceCounter`), one native call;
`Duration` arithmetic compiles to integer ops (dispatch count equals the raw
`u64` expression); `sleep` must not hold the native lock (vm-review-v5).

**rand.**

```text
pub fn from_entropy() -> Rng                      // OS seed; every other constructor stays deterministic
pub fn range[T: Integer](mut self, r: Range[T]) -> T   // unbiased (Lemire), accepts a..b and a..=b
pub fn float(mut self) -> f64                     // [0, 1), 53 bits
pub fn bool(mut self) -> bool                     // chance(p)
pub fn shuffle[T](mut self, mut xs: [T])          // Fisher-Yates
pub fn choose[T](mut self, xs: [T]) -> ?T         // sample(xs, n)
pub fn fill(mut self, mut buf: [u8])
pub fn gaussian(mut self) -> f64
pub fn jump(mut self) -> Rng                      // xoshiro's jump: an independent stream per thread
```

`jump` is there for V5: each `thread` gets `rng.jump()`, deterministic and race-free.
Gates: `range` modulo-bias test (chi-squared on 10^7 draws, deterministic seed);
`next_u64` stays at today's dispatch count; `shuffle` of 1M ≤ 1.2x of the hand loop.

**env, process, term.**

```text
// env:     vars() -> Map[String, String], set(name, value), current_dir(), home_dir(), temp_dir()
// process: run(program: str, args: [str]) -> Output ! IoError   // status, stdout, stderr captured
//          pub once warp Child { wait, kill, stdin/stdout pipes }  // spawn
//          id() -> u32
// term:    size() -> (u16, u16), is_tty() -> bool, Style: "error".red().bold(), .on_blue(), .reset
//          clear(), cursor_to(x, y), hide_cursor(); colours turn off when stdout is not a terminal
```

A running child must be killed or waited for when its `Child` is dropped via the
`once` rule, which also covers the "process hygiene" problem we have on the
build machine: a program that spawns can't forget the child.

### Phase 5: concurrency (fits V5 as built)

```text
// Atomic[T]
pub fn fetch_and / fetch_or / fetch_xor / fetch_max / fetch_min (self, v: T) -> T
pub fn update(self, f: T => T) -> T         // compare-exchange loop; f must be pure
// Mutex[T]
pub fn try_lock(self) ~> mut T ... ?        // shape depends on projection rules: open question D6
// Pipe[T]
pub fn try_send(self, take item: T) -> bool
pub fn try_recv(self) -> ?T
pub fn len(self) -> usize
pub fn is_closed(self) -> bool
//   weaves Iter: `for job in pipe { ... }` ends when the pipe is closed and drained.
//   `Pipe.unbounded()` would defeat back-pressure; not proposed.
// new
pub warp RwLock[T]                          // read() ~> ref T (many), write() ~> mut T (one)
pub warp Once[T] / Lazy[T]                  // computed once, safely, on first use; `lazy.get()`
pub warp Barrier                            // loom threads wait for each other: wait(self)
pub fn workers() -> usize                   // the pool's size, so grain sizes can be chosen
// Spun[T]
pub fn is_done(self) -> bool
// parallel helpers on slices and ranges, built on `loom for` and `chunks_mut`:
pub fn par_map[U](self, f: T => U) -> List[U]
pub fn par_for_each(mut self, f: mut T => ())      // chunks_mut underneath
pub fn par_sum / par_min / par_max / par_fold ... (self)
```

`par_*` is Rayon / C++17 `std::execution::par` in miniature: `xs.par_map(f)` is
`loom { loom for ... }` with the chunking already built in V5; the point is not
having to write the loom block for the common case. Everything here is a
suspension point and obeys the cancel rules in 05-concurrency (a `Pipe` wait is
already one; `RwLock` and `Barrier` waits are added to the list).

Gates: `par_map` on `par_mandelbrot`-sized work scales ≥ 0.7x of 8 cores, same
margin question as the existing gate (use the same decision); `Pipe` iteration
within 1.1x of a hand `recv` loop; `try_*` allocate nothing; `Atomic.update`
single-thread cost ≤ 2x `fetch_add`; the V5 `par_stress` pattern is extended to
cover every new primitive (1000 runs, plain and checked, 2/4/8 workers).

Sequencing note: until the natives lose their global lock (the lock-free table
follow-up from the V5 review), `Pipe`/`Mutex`/`RwLock` operations serialise
through it. That caps their scaling, not their correctness. Everything pure-Loom
in this plan scales regardless, which is one more reason for principle 2.

### Phase 6: V6-aligned (spools, regions, `given`)

Waits for V6 to land the language side; designed now so nothing in phases 1 to 5
has to change.

```text
// std.mem
pub weft Spool { fn alloc(...); fn free(...); fn reset(mut self) }   // 03-memory: "alloc, free, reset"
pub warp Arena: Spool            // exists as a stub
pub warp SharedArena: Spool      // thread-safe, so its pointers may cross threads
pub warp Pool[T]: Spool          // fixed slots, O(1) alloc and free
pub fn size_of[T]() -> usize     // align_of; also `T.size` already exists in compile-time code
pub fn swap / replace / take / drop
```

Ambient services through `given`, the Loom-specific part. Each is a weft with a
default so programs that never mention it keep working:

| Given | Default | What it buys |
|---|---|---|
| `Writer` for `print` | stdout | tests and tools capture or redirect all printing in one `given out = Buffer()` |
| `Rng` for `rand.range(..)`, `rand.shuffle(..)` | per-thread, entropy-seeded | `given rng = Rng.seeded(42)` makes a whole program deterministic with no plumbing; threads get `jump`ed streams |
| `Clock` for `time.now()` | the system clock | a test clock: `given clock = FakeClock()` makes sleeps and timeouts testable |
| `Logger` for `std.log` (`log.info("...")`, levels, `{key=value}` fields) | stderr, level from `LOOM_LOG` | the showcase for `given`; pub functions declare `given log: Logger` honestly |
| `Pool` for `loom` | the global pool | `loom(given: my_pool)`; this is the part of 05 that is "planned" |

Open dependency: "a `given` with a default" is not in 03-memory (`heap` is the
one built-in default). It needs a rule, e.g. `weft Rng` marked `#default(...)`,
or a defaults table in `bolt.loom`. See D7.

Also with V6: every collection's constructors take the ambient spool; `copy`
becomes spool-aware (already stated for `.copy()`); `SortedMap` and `Deque`
allocate nodes and rings in it, which is what makes arena-backed trees cheap.

Gates: `Arena` alloc ≥ 10x faster than `heap` alloc for small objects, `reset`
O(1); `Pool` alloc/free ≤ 5 dispatches; an unused `given` costs 0 dispatches after
inlining; collections' single-thread benchmarks unchanged by spool-awareness
(within noise).

### Phase 7: flagship derives and testing (what the compile-time design is for)

02-types promises "`derive` is just a default method". `tests/run/derive_json`
already proves it for `ToJson`. Promote that and add the others:

```text
warp Config: Default, ToJson, FromJson, Args {
    host: String = "localhost"
    port: u16 = 8080
    verbose: bool = false
}

fn main() ! any Error {
    let cfg = Config.from_args()?        // --host x --port 80 --verbose; also --help from the doc comments
    let saved = cfg.to_json()            // {"host":"localhost","port":8080,"verbose":false}
    let back = Config.from_json(saved)?
}
```

- `std.json`: `Json` value warp, `parse(text) -> Json ! JsonError`, `ToJson` and
  `FromJson` derived by reflection (fields, cases, optionals, lists, maps), pretty
  printing. Loaded on `use`.
- `Args` derive: flags from field names and types, defaults from field defaults,
  `--help` text from `///` doc comments.
- `Random` derive: `rng.gen[Config]()` builds one from field types.
- Testing: `assert_eq(a, b)` / `assert_ne` (print both sides with `Show`),
  `assert_close(a, b, eps)`, `assert_unravels(f)`, `unreachable(msg)`, `todo()`.
  These are [L] and need no compiler support beyond `Show` and `Eq`.
- `bench` as a test-like item (`bench name() { ... }` or `std.bench.run(f)`):
  runs a closure N times, reports min/median, and can assert a ratio against another
  closure. This turns the "perf gate per feature" rule into one line per feature
  instead of a hand-made `#[ignore]` Rust test each time. Needs `loom bench`; a
  later, separate proposal.

Gates: derived `to_json` of a 1 MB nested value within 2x of a hand-written
serializer's dispatch count; `parse` of 10 MB within 3x of the same in Lua `cjson`-
class code is *not* a target (interpreter); the target is linear time and one
allocation per node. Derive compile time: `loom check` of a program deriving
`ToJson` for 20 types within 10% of the same program with hand-written impls.

---

## 6. Recommended first slice

Smallest set that changes how programs feel, all [L] except the three [B] text
functions, and nothing waits on V6:

1. Step 0 spike (section 4.2).
2. Phase 0: `Ordering`, `Ord.cmp`, `Default`, optional/result methods, `bool.then`,
   the O(n log n) `sorted_by` and `sort_by*`.
3. Phase 1 Stream: `rev`, `step_by`, `filter_map`, `flat_map`, `min_by_key`,
   `peekable`, `counts`, `collect[C]`; List `retain`, `dedup`, `drain`,
   `reserve`, `with_capacity`; `Deque`.
4. Phase 2 slice of text: `split_once`, `strip_prefix`, `char_indices`,
   `pad_left`, `String.insert/remove`.
5. Phase 4 slice: `eprint`, `read_line`/`lines` on stdin, `Instant`, `Rng.range/float/shuffle`.
6. `assert_eq`.

Every example in `examples/` gets read once for places that would now be
shorter; the ones that win become the acceptance diff.

---

## 7. Decisions for Caio

Recommended answer first.

| # | Question | Recommend | Alternative |
|---|---|---|---|
| D1 | How are extension functions in scope? | A resolver rule: functions in the module that defines the receiver's type (`std.list` for `List`, ...) are always callable as methods. No `use`, no prelude churn | Add every name to the prelude (long and shadow-prone); or `use` them |
| D2 | Make `Str`/`Char` Unicode-correct in place (with `_ascii` variants for the old)? | Yes, in place: ASCII-only behaviour is the surprise, and lazy tables keep it free | Keep ASCII and add `_unicode` variants |
| D3 | Comparator style | `sort_by(less)` boolean (C++ style, fits hole lambdas), `sort_by_key`, and `Ord.cmp -> Ordering` for chains and `binary_search` | `Ordering`-returning comparators only (Rust) |
| D4 | Are operator wefts (`Add`, `Sub`, `Mul`, `Index`, ...) implemented? `Duration`, `Instant` and `Grid` want them | Check, and if not, land them before Phase 4 | Methods only (`d.plus(e)`), which reads badly |
| D5 | `File` as a `once` warp (close errors reported) | Yes, with one-shot `read_text`/`write_text` covering most uses so `File` is rare | Keep it a plain handle closed on drop |
| D6 | `Mutex.try_lock` shape (a projection that may not yield) | Postpone; `lock` is enough until a use shows up | `try_lock() -> ?Guard` once a guard type exists |
| D7 | Defaults for ambient `given` (stdout, entropy `Rng`, system clock) | A declared default per weft, so a program that never says `given` still runs | Always require an explicit `given` (honest, noisy) |
| D8 | Build order vs. the V5 review follow-ups (gvn/tidy compile time, concurrency bugs, lock-free natives) | Phase 0 and 1 are [L] and don't touch those; start them while the other session fixes V5, take Phase 5 after the lock-free decision | Wait for V5 follow-ups |

Pending questions already on the list (the optimizer compile-time gate, the build
server, `projects/spindle`) are untouched by this plan, except that `spindle` is
the best real-world program to port as the Phase 1 and Phase 2 acceptance diff.

## 8. Not planned

Regex, calendar dates and time zones, grapheme clusters and normalisation, async
I/O (`std.reel`, its own design), networking, compression, cryptographic hashes
and CSPRNGs, a package registry. Each deserves its own proposal once this
foundation exists.
