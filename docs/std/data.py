# Mirrors runtime/std/*.loom. Order matches crates/loom_compiler/src/modules.rs.

MODULES = [

{
  "name": "prelude",
  "summary": "The prelude: names every module can use without a `use`. A module's own items and imports shadow these.",
  "reexports": [
    {"name": "String", "from": "string"},
    {"name": "List", "from": "list"},
    {"name": "Map", "from": "map"},
    {"name": "Set", "from": "set"},
    {"name": "Error", "from": "error"},
    {"name": "Missing", "from": "error"},
    {"name": "assert", "from": "error"},
    {"name": "unravel", "from": "error"},
    {"name": "Show", "from": "fmt"},
    {"name": "Fmt", "from": "fmt"},
    {"name": "Copy", "from": "ops"},
    {"name": "Eq", "from": "ops"},
    {"name": "Ord", "from": "ops"},
    {"name": "Hash", "from": "ops"},
    {"name": "Integer", "from": "ops"},
    {"name": "Iter", "from": "iter"},
    {"name": "Stream", "from": "iter"},
    {"name": "Shuttle", "from": "iter"},
    {"name": "Burst", "from": "iter"},
    {"name": "Num", "from": "num"},
  ],
  "functions": [
    {"name": "uint", "suffix": "(bits: u8) -> type", "builtin": True,
     "doc": "The unsigned integer type with `bits` bits: `uint(12)` is `u12`."},
    {"name": "int", "suffix": "(bits: u8) -> type", "builtin": True,
     "doc": "The signed integer type with `bits` bits: `int(24)` is `i24`."},
  ],
},

{
  "name": "env",
  "summary": "The process environment.",
  "functions": [
    {"name": "get", "suffix": "(name: str) -> ?String", "extern": True,
     "doc": "The environment variable `name`, if it's set."},
    {"name": "args", "suffix": "() -> List[String]", "extern": True,
     "doc": "The command-line arguments, starting with the program name."},
  ],
},

{
  "name": "error",
  "summary": "Errors and giving up.",
  "types": [
    {"kind": "weft", "name": "Error", "suffix": ": Show",
     "doc": "Something that went wrong. The default `message` is built from the case name and payload.",
     "methods": [
       {"sig": "fn message(self) -> String", "builtin": True,
        "doc": "What went wrong, in words. Derived: the value's `Show` form."},
     ]},
    {"kind": "warp", "name": "Missing", "suffix": ": Error",
     "doc": "The error a `?` on an empty optional produces."},
  ],
  "functions": [
    {"name": "unravel", "suffix": "(message: str) -> never", "extern": True,
     "doc": "Gives up: unwinds the current thread, running `defer`s on the way out."},
    {"name": "assert", "suffix": '(condition: bool, message: str = "assertion failed")', "extern": True,
     "doc": "Unravels with `message` unless `condition` holds. In a `const` context, a failed assertion is a compile error."},
  ],
},

{
  "name": "fmt",
  "summary": "Formatting.",
  "types": [
    {"kind": "weft", "name": "Show",
     "doc": "Something that can be written as text: `print`, `{x}` in templates.",
     "methods": [
       {"sig": "fn show(self, mut f: Fmt)", "builtin": True,
        "doc": "Writes the value to `f`. A warp that doesn't write its own gets its debug form, less any fields shared by every case: `Point(x: 3, y: 4)`, `Word(\"let\")`, `Red`."},
     ]},
    {"kind": "warp", "name": "Fmt", "builtin": True,
     "doc": "Where `show` writes to.",
     "methods": [
       {"sig": "fn put(mut self, x: any Show)",
        "doc": "Writes `x` the way `{x}` would."},
     ]},
  ],
},

{
  "name": "fs",
  "summary": "Files.",
  "types": [
    {"kind": "warp", "name": "IoError", "suffix": ": Error",
     "doc": None,
     "cases": ["NotFound(path: String)", "Denied(path: String)", "Other(message: String)"]},
    {"kind": "warp", "name": "File", "builtin": True,
     "doc": "An open file.",
     "methods": [
       {"sig": "fn append(path: str) -> File ! IoError",
        "doc": "Opens `path` for appending, creating it if needed."},
       {"sig": "fn write_line(mut self, line: str) ! IoError", "doc": None},
     ]},
  ],
  "functions": [
    {"name": "read", "suffix": "(path: str) -> List[u8] ! IoError", "extern": True, "doc": None},
    {"name": "read_text", "suffix": "(path: str) -> String ! IoError", "extern": True, "doc": None},
  ],
},

{
  "name": "io",
  "summary": "Console input and output.",
  "functions": [
    {"name": "print", "suffix": "(x: any Show)", "extern": True,
     "doc": "Writes `x` and a newline to standard output."},
  ],
},

{
  "name": "iter",
  "summary": "Iteration.",
  "types": [
    {"kind": "weft", "name": "Iter",
     "doc": "Something `for` can loop over, one item at a time.",
     "example": (
        "warp Countdown(u32): Iter {\n"
        "    type Item = u32\n"
        "    fn next(mut self) -> ?u32 {\n"
        "        if .0 == 0 { return none }\n"
        "        .0 -= 1\n"
        "        .0 + 1\n"
        "    }\n"
        "}"
     ),
     "methods": [
       {"sig": "type Item",
        "doc": "What it gives."},
       {"sig": "fn next(mut self) -> ?Item",
        "doc": "The next item, or `none` once there are no more."},
     ]},
    {"kind": "warp", "name": "Stream", "suffix": "[T]", "builtin": True,
     "doc": "A lazy stream of `T`s: `s.chars()`, `xs.indexed()` and `stream.map(f)` are streams. Nothing is computed until something asks for the next item. Everything else with items has these methods too: arrays, lists and slices, ranges, and types that weave `Iter`. `(1..=10).filter(_ % 2 == 0)` is a stream of the range's items. The methods that make a new stream use up this one: a stream is read once.",
     "methods": [
       {"sig": "fn next(mut self) -> ?T", "doc": "The next item, or `none` once there are no more."},
       {"sig": "fn map[U](take self, f: T => U) -> Stream[U]", "doc": "`f` of each item."},
       {"sig": "fn filter(take self, f: T => bool) -> Stream[T]", "doc": "The items `f` says yes to."},
       {"sig": "fn take(take self, n: usize) -> Stream[T]", "doc": "The first `n` items."},
       {"sig": "fn skip(take self, n: usize) -> Stream[T]", "doc": "The items after the first `n`."},
       {"sig": "fn take_while(take self, f: T => bool) -> Stream[T]", "doc": "The items before the first one `f` says no to."},
       {"sig": "fn skip_while(take self, f: T => bool) -> Stream[T]", "doc": "The items from the first one `f` says no to."},
       {"sig": "fn indexed(take self) -> Stream[(usize, T)]", "doc": "Each item with its index: `(0, a), (1, b), …`."},
       {"sig": "fn zip[U](take self, other: Stream[U]) -> Stream[(T, U)]", "doc": "Pairs of items, one from each, until either runs out."},
       {"sig": "fn chain(take self, other: Stream[T]) -> Stream[T]", "doc": "These items, then `other`'s."},
       {"sig": "fn collect(take self) -> List[T]", "doc": "A list of the items."},
       {"sig": "fn count(take self) -> usize", "doc": "How many items there are."},
       {"sig": "fn last(take self) -> ?T", "doc": "The last item."},
       {"sig": "fn each(take self, f: T => ())", "doc": "Calls `f` with each item."},
       {"sig": "fn fold[A](take self, take init: A, f: (A, T) => A) -> A", "doc": "Combines the items, starting with `init`: `f(f(init, a), b)`…"},
       {"sig": "fn reduce(take self, f: (T, T) => T) -> ?T", "doc": "Combines the items, starting with the first; `none` if there are none."},
       {"sig": "fn all(take self, f: T => bool) -> bool", "doc": "Whether `f` says yes to every item (`true` when there are none)."},
       {"sig": "fn exists(take self, f: T => bool) -> bool", "doc": "Whether `f` says yes to some item. (`any` is a keyword.)"},
       {"sig": "fn find(take self, f: T => bool) -> ?T", "doc": "The first item `f` says yes to."},
       {"sig": "fn position(take self, f: T => bool) -> ?usize", "doc": "The index of the first item `f` says yes to."},
       {"sig": "fn sum(take self) -> T where T: Num", "doc": "The sum of the items (0 when there are none). Unravels when it doesn't fit."},
       {"sig": "fn product(take self) -> T where T: Num", "doc": "The product of the items (1 when there are none). Unravels when it doesn't fit."},
       {"sig": "fn min(take self) -> ?T where T: Ord", "doc": "The smallest item (the first, among equals)."},
       {"sig": "fn max(take self) -> ?T where T: Ord", "doc": "The largest item (the last, among equals)."},
       {"sig": "fn join(take self, sep: str) -> String where T: Show", "doc": "The items as text, with `sep` between them."},
     ]},
    {"kind": "warp", "name": "Shuttle", "suffix": "[T, U]", "builtin": True,
     "doc": "A shuttle: a function paused between the `T`s it yields and the `U`s it pulls. Calling `fn f() ~> T <~ U` makes one without running any of it. A generator (`~> T`, which pulls nothing) is a stream of what it yields: `for x in fib()`, `fib().take(10)`.",
     "methods": [
       {"sig": "fn push(mut self, take input: U) -> Burst[T]", "doc": "Resumes it with `input` as the value of its pending `pull` (the first push starts it). The burst is what it yields before its next `pull`."},
       {"sig": "fn close(mut self) -> Burst[T]", "doc": "Makes its pending and later `pull`s give `none`. The burst is what it yields from here on."},
     ]},
    {"kind": "once warp", "name": "Burst", "suffix": "[T]", "builtin": True,
     "doc": "What a shuttle yields between one input and its next `pull`. It's lazy: going through it is what runs the shuttle. It's `once`, so outputs can't be dropped by accident: go through it with `for` (or a `Stream` method), or `drain()` it.",
     "methods": [
       {"sig": "fn drain(take self)", "doc": "Runs the shuttle to its next `pull`, throwing away what it yields."},
     ]},
  ],
},

{
  "name": "list",
  "summary": "Growable arrays.",
  "types": [
    {"kind": "warp", "name": "List", "suffix": "[T]", "builtin": True,
     "doc": "A growable array of `T`. Build one with `List()`, `List.filled(x, n)`, or a literal where a list is expected: `let xs: List[i32] = [1, 2, 3]`. It has every method of a slice (`[T]`), too.",
     "methods": [
       {"sig": "fn filled(x: T, n: usize) -> List[T]", "doc": "`n` copies of `x`."},
       {"sig": "fn push(mut self, take x: T)", "doc": "Adds `x` at the end."},
       {"sig": "fn pop(mut self) -> ?T", "doc": "Takes the last element off, if there is one."},
       {"sig": "fn insert(mut self, i: usize, take x: T)", "doc": "Puts `x` at index `i`, moving the rest up. Unravels if `i > len`."},
       {"sig": "fn remove(mut self, i: usize) -> T", "doc": "Takes out the element at `i`, moving the rest down. Unravels if `i >= len`."},
       {"sig": "fn clear(mut self)", "doc": "Removes every element."},
       {"sig": "fn truncate(mut self, n: usize)", "doc": "Keeps the first `n` elements."},
       {"sig": "fn extend(mut self, xs: [T])", "doc": "Adds a copy of every element of `xs` at the end."},
       {"sig": "fn copy(self) -> List[T]", "doc": "A deep copy."},
     ]},
  ],
},

{
  "name": "map",
  "summary": "Hash maps.",
  "types": [
    {"kind": "warp", "name": "Map", "suffix": "[K, V]", "builtin": True,
     "doc": "A map from `K` to `V` that remembers insertion order: going through it, and showing it, follow the order keys were first added. Build one with `Map()`, or a literal where a map is expected: `let ages: Map[str, u8] = [\"ada\": 36, \"alan\": 41]` (`[:]` is empty). `m[k]` is the value for `k`, and unravels when there's none (`m.get(k)` gives an optional); `m[k] = v` adds or replaces it. `for (k, v) in m` goes through the pairs, and a map has the methods of `Stream` too, over its pairs. Keys must be `Hash`.",
     "methods": [
       {"sig": "fn len(self) -> usize", "doc": "How many keys there are."},
       {"sig": "fn is_empty(self) -> bool", "doc": None},
       {"sig": "fn get(self, key: K) -> ?V where K: Hash", "doc": "The value for `key`."},
       {"sig": "fn has(self, key: K) -> bool where K: Hash", "doc": "Whether there's a value for `key`."},
       {"sig": "fn insert(mut self, take key: K, take value: V) -> ?V where K: Hash", "doc": "Adds `value` for `key`, and gives back the value it replaces."},
       {"sig": "fn remove(mut self, key: K) -> ?V where K: Hash", "doc": "Takes out the value for `key`; later keys keep their order."},
       {"sig": "fn clear(mut self)", "doc": "Removes every key."},
       {"sig": "fn keys(self) -> Stream[K]", "doc": "The keys, in order."},
       {"sig": "fn values(self) -> Stream[V]", "doc": "The values, in the order of their keys."},
       {"sig": "fn pairs(self) -> Stream[(K, V)]", "doc": "The pairs, in order."},
       {"sig": "fn sorted(self) -> List[(K, V)] where K: Ord", "doc": "A list of the pairs, sorted by key."},
       {"sig": "fn copy(self) -> Map[K, V]", "doc": "A deep copy."},
     ]},
  ],
},

{
  "name": "math",
  "summary": "Mathematical constants and functions.",
  "consts": [
    {"name": "PI", "suffix": ": f64 = 3.141592653589793", "doc": None},
    {"name": "EULER", "suffix": ": f64 = 2.718281828459045", "doc": None},
  ],
},

{
  "name": "mem",
  "summary": "Memory: spools and sizes.",
  "types": [
    {"kind": "weft", "name": "Spool",
     "doc": "A region to allocate in. `heap` is the global one."},
    {"kind": "warp", "name": "Arena", "suffix": ": Spool", "builtin": True,
     "doc": "A spool that allocates by bumping a pointer and frees everything at once.",
     "fields": ["capacity: usize"],
     "methods": [
       {"sig": "fn reset(mut self)", "doc": "Frees everything allocated so far."},
     ]},
  ],
  "functions": [
    {"name": "kib", "suffix": "(n: usize) -> usize", "doc": None},
    {"name": "mib", "suffix": "(n: usize) -> usize", "doc": None},
  ],
},

{
  "name": "meta",
  "summary": "Reflection: what compile-time code can learn about a type. `T.name`, `T.kind`, `T.fields` and `T.cases` are known while compiling, and so is everything read from them; none of it is left in the program.",
  "types": [
    {"kind": "warp", "name": "Kind", "suffix": ": Eq, Show",
     "doc": "What kind of type a type is: `T.kind`.",
     "cases": ["Record", "Sum", "Tuple", "Unit", "Bool", "Int", "Float", "Char", "Str", "Array", "Slice",
               "List", "Map", "Set", "Optional", "Result", "Fn", "Any", "Union", "Other"]},
    {"kind": "warp", "name": "Field", "suffix": ": Copy", "builtin": True,
     "doc": "A field of a record, or of a case's payload: an item of `T.fields` or `c.fields`. It has a `name: str`, `is_pub: bool` and `offset: usize` (where it starts in the value, in bytes), and `v.(f)` is `v`'s field `f`. Only compile-time code has one."},
    {"kind": "warp", "name": "Case", "suffix": ": Copy", "builtin": True,
     "doc": "A case of a sum: an item of `T.cases`. It has a `name: str` and `fields`, and `v is .(c)` tests whether `v` is case `c`. Only compile-time code has one."},
  ],
},

{
  "name": "num",
  "summary": "Numbers.",
  "types": [
    {"kind": "warp", "name": "NumError", "suffix": ": Error",
     "doc": "Why text didn't parse as a number.",
     "cases": ["Empty", "Invalid", "Overflow"]},
    {"kind": "weft", "name": "Parse", "builtin": True,
     "doc": "Types `str.parse` can make: every integer and float type."},
    {"kind": "weft", "name": "Num", "suffix": ": Ord, Copy, Show", "builtin": True,
     "doc": "Every number type: the integers and the floats."},
    {"kind": "warp", "name": "Int", "suffix": "[T]", "builtin": True,
     "doc": "The methods of every integer type, `T`.",
     "methods": [
       {"sig": "fn abs(self) -> T", "doc": "The value without its sign. Unravels for the smallest signed value."},
       {"sig": "fn min(self, other: T) -> T", "doc": None},
       {"sig": "fn max(self, other: T) -> T", "doc": None},
       {"sig": "fn pow(self, exp: u32) -> T", "doc": "`self` to the power `exp`. Unravels when it doesn't fit."},
       {"sig": "fn trailing_zeros(self) -> u32", "doc": "How many zero bits come after the lowest one bit (all of them for 0)."},
       {"sig": "fn leading_zeros(self) -> u32", "doc": "How many zero bits come before the highest one bit."},
       {"sig": "fn count_ones(self) -> u32", "doc": "How many bits are one."},
       {"sig": "fn is_power_of_two(self) -> bool", "doc": "Whether it's 2 to the power of something: one bit is one, and it's above zero."},
       {"sig": "fn log2(self) -> u32", "doc": "The base-2 logarithm, rounded down: the position of the highest one bit. Unravels for zero and below."},
       {"sig": "fn rotate_left(self, n: u32) -> T", "doc": "The bits moved `n` places towards the top; those pushed out come back in at the bottom. `n` counts modulo the type's width."},
       {"sig": "fn rotate_right(self, n: u32) -> T", "doc": "The bits moved `n` places towards the bottom; those pushed out come back in at the top. `n` counts modulo the type's width."},
       {"sig": "fn to_string(self) -> String", "doc": None},
     ]},
    {"kind": "warp", "name": "Float", "suffix": "[T]", "builtin": True,
     "doc": "The methods of every float type, `T`.",
     "methods": [
       {"sig": "fn abs(self) -> T", "doc": None},
       {"sig": "fn min(self, other: T) -> T", "doc": "The smaller one; a NaN loses to a number."},
       {"sig": "fn max(self, other: T) -> T", "doc": None},
       {"sig": "fn sqrt(self) -> T", "doc": None},
       {"sig": "fn floor(self) -> T", "doc": None},
       {"sig": "fn ceil(self) -> T", "doc": None},
       {"sig": "fn round(self) -> T", "doc": "Halfway rounds away from zero."},
       {"sig": "fn trunc(self) -> T", "doc": None},
       {"sig": "fn pow(self, exp: T) -> T", "doc": None},
       {"sig": "fn is_nan(self) -> bool", "doc": None},
       {"sig": "fn to_string(self) -> String", "doc": None},
     ]},
  ],
},

{
  "name": "ops",
  "summary": "The wefts behind operators and value semantics. A warp that lists `Eq`, `Ord` or `Hash` in its header without writing the method gets one that works field by field (see 02-types, \"Deriving\").",
  "types": [
    {"kind": "weft", "name": "Copy",
     "doc": "Values that are copied rather than moved."},
    {"kind": "weft", "name": "Eq",
     "doc": "`==` and `!=`.",
     "methods": [
       {"sig": "fn eq(self, other: Self) -> bool", "builtin": True,
        "doc": "Whether the values are equal. Derived: every field is equal, and for a warp with cases, it's the same case."},
     ]},
    {"kind": "weft", "name": "Ord", "suffix": ": Eq",
     "doc": "`<`, `<=`, `>`, `>=`.",
     "methods": [
       {"sig": "fn lt(self, other: Self) -> bool", "builtin": True,
        "doc": "Whether `self` comes first. Derived: fields compare in order, the first that differs decides, and earlier cases come first."},
     ]},
    {"kind": "weft", "name": "Hash",
     "doc": None,
     "methods": [
       {"sig": "fn hash(self) -> u64", "builtin": True, "doc": None},
     ]},
    {"kind": "weft", "name": "Integer", "suffix": ": Ord, Copy, Show", "builtin": True,
     "doc": "Every integer type, `u1` through `i128`."},
  ],
},

{
  "name": "option",
  "summary": "Optional values: `?T`.",
  "types": [
    {"kind": "warp", "name": "Optional", "suffix": "[T]", "builtin": True,
     "doc": "The methods of every optional type, `?T`.",
     "methods": [
       {"sig": "fn take(mut self) -> ?T", "doc": "The value, leaving `none` in its place."},
     ]},
  ],
},

{
  "name": "rand",
  "summary": "Pseudo-random numbers.",
  "types": [
    {"kind": "warp", "name": "Rng",
     "doc": "A deterministic xoshiro256** generator: the same seed always gives the same numbers, on every machine. Not for secrets.",
     "methods": [
       {"sig": "fn seeded(seed: u64) -> Rng", "doc": "A generator that always produces the same sequence for `seed`."},
       {"sig": "fn next_u64(mut self) -> u64", "doc": "The next 64 random bits."},
       {"sig": "fn byte(mut self) -> u8", "doc": "A random byte: the top 8 bits of `next_u64`."},
     ]},
  ],
},

{
  "name": "set",
  "summary": "Hash sets.",
  "types": [
    {"kind": "warp", "name": "Set", "suffix": "[T]", "builtin": True,
     "doc": "A set of `T` that remembers insertion order. Build one with `Set()` or `Set.of(xs)`. `for x in s` goes through it in order, and a set has the methods of `Stream` too. Its elements must be `Hash`.",
     "methods": [
       {"sig": "fn of(xs: [T]) -> Set[T] where T: Hash", "doc": "A set of the elements of `xs` (each once)."},
       {"sig": "fn len(self) -> usize", "doc": None},
       {"sig": "fn is_empty(self) -> bool", "doc": None},
       {"sig": "fn has(self, x: T) -> bool where T: Hash", "doc": "Whether `x` is in the set."},
       {"sig": "fn insert(mut self, take x: T) -> bool where T: Hash", "doc": "Adds `x`; whether it wasn't there yet."},
       {"sig": "fn remove(mut self, x: T) -> bool where T: Hash", "doc": "Takes `x` out; whether it was there."},
       {"sig": "fn clear(mut self)", "doc": "Removes every element."},
       {"sig": "fn sorted(self) -> List[T] where T: Ord", "doc": "A list of the elements, sorted."},
       {"sig": "fn copy(self) -> Set[T]", "doc": "A deep copy."},
     ]},
  ],
},

{
  "name": "slice",
  "summary": "Slices: views of contiguous elements.",
  "types": [
    {"kind": "warp", "name": "Slice", "suffix": "[T]", "builtin": True,
     "doc": "`[T]`: a view of contiguous `T`s, in an array or a list. Arrays and lists have these methods too.",
     "methods": [
       {"sig": "fn len(self) -> usize", "doc": "How many elements there are."},
       {"sig": "fn is_empty(self) -> bool", "doc": None},
       {"sig": "fn get(self, i: usize) -> ?T", "doc": "The element at `i`, or `none` past the end."},
       {"sig": "fn first(self) -> ?T", "doc": None},
       {"sig": "fn last(self) -> ?T", "doc": None},
       {"sig": "fn contains(self, x: T) -> bool where T: Eq", "doc": "Whether some element equals `x`."},
       {"sig": "fn index_of(self, x: T) -> ?usize where T: Eq", "doc": "The index of the first element equal to `x`."},
       {"sig": "fn copy_from(mut self, src: [T])", "doc": "Copies `src` over the start of this slice. Unravels if `src` is longer."},
       {"sig": "fn fill(mut self, x: T)", "doc": "Sets every element to a copy of `x`."},
       {"sig": "fn swap(mut self, i: usize, j: usize)", "doc": None},
       {"sig": "fn reverse(mut self)", "doc": None},
       {"sig": "fn sort(mut self) where T: Ord", "doc": "Sorts in order, keeping equal elements in the order they were in."},
       {"sig": "fn to_list(self) -> List[T]", "doc": "A new list with a copy of every element."},
       {"sig": "fn map[U](self, f: T => U) -> List[U]", "doc": "A new list of `f` of each element. (`xs.indexed()` and the other methods of `Stream` work on slices too, lazily.)"},
       {"sig": "fn filter(self, f: T => bool) -> List[T]", "doc": "A new list with a copy of each element `f` says yes to."},
       {"sig": "fn chunks(self, n: usize) -> Stream[[T]]", "doc": "Views of `n` elements at a time, one after another; the last may be shorter. Unravels if `n` is 0."},
       {"sig": "fn windows(self, n: usize) -> Stream[[T]]", "doc": "Views of every run of `n` elements in a row: `[a, b]`, `[b, c]`, … Unravels if `n` is 0."},
     ]},
  ],
},

{
  "name": "string",
  "summary": "Text.",
  "types": [
    {"kind": "warp", "name": "String", "builtin": True,
     "doc": "Owned, growable UTF-8 text. `str` is a view of text; a string literal converts to `String` wherever one is expected. A `String` has every method of `str`, too.",
     "methods": [
       {"sig": "fn with_capacity(n: usize) -> String", "doc": "An empty string with room for `n` bytes."},
       {"sig": "fn of(chars: [char]) -> String", "doc": "The text of `chars`, in order."},
       {"sig": "fn push(mut self, c: char)", "doc": "Adds `c` at the end."},
       {"sig": "fn push_str(mut self, s: str)", "doc": "Adds `s` at the end."},
       {"sig": "fn pop(mut self) -> ?char", "doc": "Takes the last character off, if there is one."},
       {"sig": "fn clear(mut self)", "doc": None},
     ]},
    {"kind": "warp", "name": "Str", "builtin": True,
     "doc": "The methods of `str`: a view of UTF-8 text. Lengths and positions count bytes.",
     "methods": [
       {"sig": "fn len(self) -> usize", "doc": "How many bytes it takes."},
       {"sig": "fn is_empty(self) -> bool", "doc": None},
       {"sig": "fn char_count(self) -> usize", "doc": "How many characters it has."},
       {"sig": "fn contains(self, part: str) -> bool", "doc": None},
       {"sig": "fn starts_with(self, prefix: str) -> bool", "doc": None},
       {"sig": "fn ends_with(self, suffix: str) -> bool", "doc": None},
       {"sig": "fn find(self, part: str) -> ?usize", "doc": "The byte position of the first `part`, if there is one."},
       {"sig": "fn trim(self) -> str", "doc": "Without whitespace at either end."},
       {"sig": "fn trim_start(self) -> str", "doc": None},
       {"sig": "fn trim_end(self) -> str", "doc": None},
       {"sig": "fn to_upper(self) -> String", "doc": "Upper case, for ASCII letters."},
       {"sig": "fn to_lower(self) -> String", "doc": "Lower case, for ASCII letters."},
       {"sig": "fn replace(self, from: str, to: str) -> String", "doc": "Every `from` replaced with `to`."},
       {"sig": "fn repeat(self, n: usize) -> String", "doc": "`n` copies, one after another."},
       {"sig": "fn split(self, sep: str) -> List[String]", "doc": "The parts between each `sep`."},
       {"sig": "fn chars(self) -> Stream[char]", "doc": "The characters, one at a time."},
       {"sig": "fn bytes(self) -> Stream[u8]", "doc": "The bytes of its UTF-8, one at a time."},
       {"sig": "fn lines(self) -> Stream[str]", "doc": "The lines, without their line breaks (a newline, or a carriage return and a newline)."},
       {"sig": "fn words(self) -> Stream[str]", "doc": "The words: what's between runs of whitespace."},
       {"sig": "fn to_string(self) -> String", "doc": "The text as a `String` of its own."},
       {"sig": "fn copy(self) -> String", "doc": None},
       {"sig": "fn parse[T: Parse](self) -> T ! NumError", "doc": '''The number the text spells: `"42".parse[u8]()`. Decimal, with an optional sign; floats also take a fraction, an exponent, `inf`, `NaN`.'''},
     ]},
    {"kind": "warp", "name": "Char", "builtin": True,
     "doc": "The methods of `char`. The tests are for ASCII.",
     "methods": [
       {"sig": "fn is_digit(self) -> bool", "doc": "`0` through `9`."},
       {"sig": "fn is_alphabetic(self) -> bool", "doc": None},
       {"sig": "fn is_alphanumeric(self) -> bool", "doc": None},
       {"sig": "fn is_whitespace(self) -> bool", "doc": "A space, tab, newline, or carriage return."},
       {"sig": "fn is_upper(self) -> bool", "doc": None},
       {"sig": "fn is_lower(self) -> bool", "doc": None},
       {"sig": "fn to_upper(self) -> char", "doc": None},
       {"sig": "fn to_lower(self) -> char", "doc": None},
       {"sig": "fn to_digit(self, radix: u32 = 10) -> ?u32", "doc": "Its value as a digit in `radix`: `'f'.to_digit(16)` is `15`."},
       {"sig": "fn digit(self) -> u8", "doc": "The value of a decimal digit: `'7'.digit()` is `7`. Unravels for anything but `0` through `9`."},
       {"sig": "fn len_utf8(self) -> usize", "doc": "How many bytes it takes in UTF-8."},
     ]},
  ],
},

{
  "name": "sync",
  "summary": "Sharing between threads.",
  "types": [
    {"kind": "warp", "name": "Atomic", "suffix": "[T]", "builtin": True,
     "doc": "A value that threads can read and update without locks."},
    {"kind": "warp", "name": "Mutex", "suffix": "[T]", "builtin": True,
     "doc": "A value that one thread at a time can lock and use."},
  ],
},

{
  "name": "target",
  "summary": "What `target` describes: the machine the program is built for. That's the machine doing the build, with 63-bit pointers on the Lua backend (`usize` is 63 bits) and 64-bit ones on Treadle, Loom's own VM.",
  "types": [
    {"kind": "warp", "name": "Os", "suffix": ": Eq, Show",
     "doc": "An operating system.",
     "cases": ["Windows", "Linux", "MacOS", "Other"]},
    {"kind": "warp", "name": "Arch", "suffix": ": Eq, Show",
     "doc": "A processor architecture.",
     "cases": ["Amd64", "Arm64", "Other"]},
    {"kind": "warp", "name": "Endian", "suffix": ": Eq, Show",
     "doc": "The order of a number's bytes in memory.",
     "cases": ["Little", "Big"]},
    {"kind": "warp", "name": "Target",
     "doc": "The type of `target`, known at compile time: `const if target.os == .Windows`.",
     "fields": ["pub os: Os", "pub arch: Arch", "pub endian: Endian", "pub pointer_bits: u8"]},
  ],
},

{
  "name": "term",
  "summary": "The terminal.",
  "functions": [
    {"name": "raw_mode", "suffix": "(on: bool)", "extern": True,
     "doc": "Turns raw mode on or off: keys arrive immediately and aren't echoed."},
    {"name": "poll_keys", "suffix": "(keymap: str) -> u16", "extern": True,
     "doc": "A bit set of the keys in `keymap` that are held down right now."},
  ],
},

{
  "name": "time",
  "summary": "Time.",
  "types": [
    {"kind": "warp", "name": "Duration",
     "doc": "A span of time, in nanoseconds.",
     "fields": ["nanos: u64"]},
  ],
  "functions": [
    {"name": "ms", "suffix": "(n: u64) -> Duration", "doc": None},
    {"name": "secs", "suffix": "(n: u64) -> Duration", "doc": None},
    {"name": "sleep", "suffix": "(d: Duration)", "extern": True,
     "doc": "Pauses the current thread for `d`."},
  ],
},

]
