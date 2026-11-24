# Working on Loom

Loom is a language design (README.md, design/, examples/) plus a Loom → Lua 5.4
transpiler written in Rust (crates/, runtime/, tests/). The backend's
decisions live in `design/lua-backend.md`; read it before touching lowering or
the runtime.

## Gates (run before every commit)

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p loom --no-default-features --features lua55   # keeps emitted Lua inside the 5.4 ∩ 5.5 subset
cargo test -p loom_vm --all-features                          # Treadle with its run-time checks on
cargo test -p loom --features vm-checked,vm-stats             # the fixtures on a checked Treadle, and bench/counts.txt
```

Read your own diff before committing. One logical change per commit.
When work is done, commit it and push it to GitHub.

## Perf gates (release build, run on an idle machine)

Wall-clock gates are `#[ignore]`d, so `cargo test --workspace` stays
deterministic under load. Run them after a change to the optimizer or the
thread pool, and put the numbers in `bench/RESULTS.md`:

```
cargo test --release -p loom --test compile_time -- --ignored --nocapture   # -O2 vs -O0, slowest example
cargo test --release -p loom --test perf_gates -- --ignored --nocapture --test-threads=1   # spawn+join < 2 us, par_mandelbrot >= 5.6x on 8 workers, a box < 15 ns, an arena put < half a box, arena trees >= 4x Lua
```

## Agent workflow

- Sonnet 5.5 is the main agent and writes the code.
- Opus 5.5 is the advisor (the `advisor` tool): consult it before committing
  to a design or architecture choice, when stuck, and before calling work done.
  Let it make the important decisions.
- Haiku 5.5 is the explorer: spawn it (`Agent`, `model: haiku`, `Explore`) to
  find code and retrieve facts. A plain `grep` is fine when it's cheaper than
  spawning one. Keep to 2–3 agents at a time.

## Priorities

Treadle (the VM) is the main focus. The Lua backend is a secondary feature:
keep it up to date and correct (it's the default for `loom run` and the
oracle the fixtures and the generator compare against), so a new language
feature lands on both, but don't spend time making it faster or cleverer.
Speed work, reviews and design effort go to Treadle.

## Toolchain

- Rust stable ≥ 1.98, edition 2024, host `x86_64-pc-windows-msvc`.
- C code (the vendored Lua VM) is compiled with **clang-cl** and linked with
  **lld-link**, never MSVC `cl.exe`. `.cargo/config.toml` sets this up. If a
  build log shows `cl.exe`, that's a bug.

## Layout

| Path | What |
|------|------|
| `crates/loom_syntax` | source files, spans, diagnostics, tokens, lexer, parser, AST, AST printer (no C deps) |
| `crates/loom_sys` | what a running program gets from its host, for both backends: output, arguments, outcomes |
| `crates/loom_host` | the only owner of the Lua VM (mlua, vendored) |
| `crates/loom_isa` | Treadle's instruction set: the op table, images, the verifier, listings |
| `crates/loom_vm` | Treadle, Loom's own VM (design/vm.md): the interpreter, its stack, natives |
| `crates/loom_compiler` | everything semantic: resolve (`hir`, `modules`), then typeck, analyses, lowering, Lua emission |
| `crates/loom` | the `loom` binary (build, run, check, test, hidden lua-host) |
| `runtime/rt` | `loomrt` Lua sources, embedded into the binary |
| `runtime/std` | the standard library, written in Loom |
| `tests/` | fixture corpora (run, fail, unravel, lower, examples, opt: the optimizer's snapshots) |

## Conventions

- Diagnostics carry a code from `loom_syntax::diag::codes` (S lexer/syntax,
  N names, T types, M moves, L once, R regions, C concurrency, G gauges,
  P patterns, K const, U unsupported, I interop). Never invent a code inline.
- Unsupported features fail with a `U` code. Never miscompile silently.
- `unsafe` Rust is welcome wherever it makes sense: for speed, when an
  invariant (the bytecode verifier, a checked bound) already guarantees what
  a check would, or for what safe Rust can't express. Every block gets a
  `// SAFETY:` comment saying why it's sound. The workspace lint is
  `unsafe_code = "warn"`, so each use is a deliberate `#[allow(unsafe_code)]`.
- Anything whose iteration order can reach output uses `IndexMap`/`BTreeMap`,
  so builds are byte-for-byte reproducible.
- Keep the design docs in sync when behavior changes. If an example stops being
  accurate, fix the example or the implementation, not both silently.
- The docs site (`docs/`, opened by `loom docs`) is generated: edit
  `docs/lang/pages/*.md` (the book; `docs/lang/STYLE.md` says how it's
  written) or the `///` doc comments in `runtime/std/*.loom` (the std pages
  are made from them), run `python docs/gen.py`, and when behavior changes,
  `python docs/gen.py --check --loom target/debug/loom.exe` (every sample in
  the book runs) and `python docs/play/build.py` (the browser playground's
  wasm build of the compiler; it needs `rustup target add wasm32-wasip1`).

## Status

Done: the spec renames, the workspace scaffold, the lexer, the parser, name
resolution (HIR, `std` declarations, the prelude), and two slices of the
language through every layer (type checking, lowering to Lua, the runtime,
`loom build` / `loom run`):

1. numbers of every width, strings, tuples, functions, control flow,
   `match` on literals and tuples, operators, casts, templates;
2. warps: records, cases, shared fields, tuple warps, construction, fields,
   methods, associated functions and constants, UFCS, the leading dot,
   dot-blocks, `is`, case and twill patterns with exhaustiveness over cases,
   `mut` parameters of table-backed types, and value semantics for `Copy`
   warps;
3. wefts and weaves (in progress): inline and separate weaves, default
   methods checked per type, derived `Show`/`Eq`/`Ord`, `Weft.f(x)`,
   `{x}`, `print`, `==`, `<` through hand-written weave methods, and
   `through` delegation;
4. generics, checked per instance: generic functions with bounds and
   inferred or explicit arguments, generic warps and their methods, generic
   weaves, and wefts as parameter types (implicit generics);
5. `any W`: dynamic dispatch through vtables, type tests (`w is b: Button`),
   and `any` of more wefts passing as `any` of fewer;
6. optionals and errors: `?` (to the nearest `mend`, or out of the
   function), `mend` and `mend e =>`, `fail`, `some`/`none`/`ok`/`err` as
   values and patterns, union types, error conversions (unions, `from`
   cases, `any Error`), and `main ! E`;
7. arrays, lists and slices: literals, `[x; n]`, bounds-checked indexing and
   index assignment, slicing, `for` and `for mut` over them, and the methods
   std declares on `List` and `Slice`;
8. `mut` parameters of scalars (copied back to the caller), and the methods
   of `str`/`String`, `char`, integers and floats, `s.parse[T]()`, and
   slicing text;
9. lambdas (with holes), function values and calling them; `Iter` with
   associated types (`type Item`, `I.Item`), `Stream[T]` and its methods,
   which lists, ranges and `Iter` types have too, `for` over any of them,
   `chars()`/`bytes()`/`lines()`/`words()`, `chunks`/`windows`, and `where`
   bounds on a builtin warp's parameters (`sum` needs `Num`, `sort` `Ord`).
   `examples/widgets.loom` runs, checked by `tests/examples/widgets.out`;
10. moves: use after move, moving out of borrows and elements, overlapping
    `mut` arguments (M01xx); match exhaustiveness by pattern usefulness
    (nested patterns, integer ranges, twill patterns); functions over Lua's
    200 locals; stack overflow as an unravel;
11. `defer` and fallback `defer x => …`, on every way out of a scope;
12. gauges: range and predicate gauges, literal checks (G0201), `as`/`as?`/
    `as|` into them, `is` tests and narrowing, interval arithmetic, and
    `env.get`. `examples/tour.loom` runs, checked by `tests/examples/tour.out`.

13. maps and sets: `Map()`, `[k: v]` literals, `m[k]` and `m[k] = v`, their
    methods, loops over them, and derived `hash`.

14. `once` warps and loose ends (L0301), and `std.fs` files.
    `examples/ledger.loom` runs, checked by `tests/examples/ledger.out`.
15. tests (`test name() { … }`, `loom test`, checked by `tests/test`), and
    the host: `env.args`, `time.sleep`, `term.raw_mode`, `term.poll_keys`.
16. `Rng` (in Loom), `rotate_left`/`rotate_right`, and `const assert` on
    what the constant folder can work out (K0101). `examples/chip8.loom`
    compiles and runs; `loom test examples/chip8.loom` passes its 3 tests.
17. shuttles: generators (`~> T`), duplex shuttles (`~> T <~ U`) and sinks,
    `yield`, `pull`, `push`/`close` and `once` bursts, as Lua coroutines.
    `examples/lexer.loom` runs but for its `lexer.Frame.size` assert.
18. compile-time code in a sandbox: constants computed by ordinary code,
    `const` expressions, `const assert` anywhere (K0101–K0108), byte
    strings.
19. `const` parameters: `fun zeros(const n: usize) -> [f32; n]`,
    `warp Ring[T, const N: usize]`, value generic arguments, `const assert`
    per instance, `uint(n)`/`int(n)`, types inside warps, and `?T.take()`.
    `examples/compile_time.loom` compiles but for `const if`.
20. `const if` (only the branch taken is checked) and `const for`
    (unrolled, each item checked on its own), and `target` (`std.target`).
    `examples/compile_time.loom` fails only for reflection and `std.sys`.
21. reflection (`std.meta`): `T.name`, `T.kind`, `T.fields`, `T.cases`,
    `f.name`, `c.fields`, `v.(f)`, `v is .(c)`, `v.(c).(f)`, `T.size`,
    `T.align` and `f.offset` (from `ty::layout`), and default
    methods that reflect on `Self` (`ToJson`, checked by
    `tests/run/derive_json`). `examples/compile_time.loom` fails only for
    `std.sys`.

Treadle, Loom's own VM (`design/vm.md`, milestones V0–V9), is what `loom run`,
`test`, `build` and `check` use; `--backend lua` selects the Lua backend instead. The typed program becomes Heddle
(`loom_compiler::heddle`, a typed SSA IR), Heddle becomes bytecode
(`loom_compiler::codegen`, into `loom_isa`'s images), and `loom_vm` runs it.

22. Treadle V0: integers of every width and overflow policy, floats, `bool`,
    `char`, `str` literals, tuples, control flow, `match` on literals,
    ranges, tuples and twill, functions (named and default arguments, `mut`
    scalars), and `print`, `assert` and `unravel` with templates. Every
    `tests/run` fixture also runs on Treadle; `tests/vm_skip.txt` lists the
    ones that can't yet, with the milestone that fixes each.
    `bench/RESULTS.md` has the numbers.

23. Treadle V1: the sequential language (design/vm.md §17): warps,
    optionals and results, `any`, lists, strings, maps and sets, `defer`,
    lambdas, generics, streams, constants, gauges, the host, `loom test`
    on Treadle, backtraces, Ctrl-C (130) and `process.exit`, with memory
    laid out for real and freed by ownership; the checked build leaks
    nothing, whatever way a run ends (a return, an unravel, an exit,
    Ctrl-C). 1.65× PUC Lua on `bench/` (`bench/RESULTS.md`). The program
    generator (`crates/loom/tests/gen.rs`) runs random programs on both
    backends: 300 seeds in `cargo test`, 20 000 with `--ignored`.

24. Treadle V2: shuttles (design/vm.md §10.14). Each shuttle body is a step
    function the frame pass (`heddle/frame.rs`) lays out; `f.Frame.size`
    on both backends (40 bytes for `examples/lexer.loom`'s lexer); bursts,
    `close`, dropping runs the `defer`s reached (P2); a generator a `for`
    loop calls is built in place; and projections (`~> mut T`, `~> ref T`)
    on both backends. `tests/vm_skip.txt` is empty. Held shuttles keep
    their frames in boxes: D5's inline frames are V8's.

25. Treadle V3, the optimizer (design/vm.md §7, §17): inline, prune, SCCP,
    cells, GVN, ipcp, ranges, LICM and cleanup at `-O2` (`-O0` runs none);
    a pass's snapshots in `tests/opt`; `LOOM_PASSES` and `LOOM_PASS_TIMES`.
    87% of bounds and 62% of overflow checks gone (`check_counts.rs`),
    compiling at 1.5× `-O0` (`tests/compile_time.rs`, ignored). Speed gate
    met: 2.74× PUC Lua (bench/RESULTS.md; design/vm-review-v3.md has the
    run-time review its changes came from). Deferred passes: specialize
    lambdas, SROA, self tail calls.

26. Treadle V4, compile-time code (design/vm.md §17): constants,
    `const` expressions and `const assert`s run as jobs of one image on an
    in-process Treadle VM, for both backends (`consteval.rs`,
    `heddle/build/jobs.rs`), under a budget (100M ticks, 1 GiB, 20 s) with
    the host refused (K0104). The Lua sandbox is gone and `loom_compiler`
    depends on `loom_host` only in tests: 239 ms against 495 ms over the
    corpus. The values computed are baked: data in the image (with
    relocations) and literals in the Lua bundle, for numbers, text,
    tuples, arrays, lists, records and optionals (`baked.rs`); `target` is
    the target being built. Building twice gives the same bytes. Left on
    purpose: building the program from the job build's module, and baking
    maps, sets and sums (they're computed when read).

27. Treadle V5, threads (design/vm.md §11), done (limits below). `loom`,
    `thread`, `Spun[T]`, `join`, `sched_yield` and the capture rules
    (C0401–C0405) on both backends; the reference schedule (a thread runs
    at once, until it parks; the spawner waits at the front, woken tasks
    go to the back), cancel, failure and exit/Ctrl-C through a loom;
    `Atomic`, `Mutex`, `Pipe`, `loom for` with `#disjoint`/`chunks_mut`.
    The Lua backend runs threads as coroutines (`rt.spawn`), Treadle
    switches tasks inside the dispatch loop (`loom_vm::sched`) and runs
    them on several workers (`loom_vm::pool`: one lock for the scheduler
    and the natives, the dispatch loops run without it; `LOOM_THREADS`,
    one worker per core by default, `LOOM_THREADS=1` is the reference
    schedule; `tests/par` checks what's deterministic at 2/4/8 workers,
    `par_stress` is the `--ignored` 1000-run version, and
    `thread_fixtures_on_many_workers` runs the thread fixtures of
    `tests/run` on 8 workers and checks only how they end).
    `Range.chunks`, `Map.entry` and `sorted_by` came with
    `examples/parallel.loom`, which runs the same on Lua and Treadle at
    any worker count (a test in `cli.rs`). `par_mandelbrot` scales 6.3×
    on 8 workers; spawn+join costs 0.31 µs on one worker and 0.9 µs on 8
    (idle workers spin on the ready count and leave the lock alone).
    `loom for` over an exclusive integer range gives a task a run of items
    when there are several workers (`task.grain`; `par_spawn` on 8 workers
    0.56 s to 0.07 s); other iterators keep a task per item.

28. Treadle V6, slice 1 (design/v6-spools.md): heap pointers on both
    backends. `^e` boxes a value, `p^` reads it, `^T` is a type (`?^T` costs
    nothing on Treadle: the pointer is never null), so warps can hold
    themselves (`case Node(^Tree, ^Tree)`). A pointer owns its box and moves
    like any non-`Copy` value. Fields, methods, patterns, `{p}`, `==`, `<`
    and hashing see through it; a `take` pointer taken apart by `match` hands
    its parts over by value. Assignment through one (`p.x = 1`, `p^ += 1`)
    needs a `var`. Boxes cost 10 ns to allocate, read and free
    (`perf_gates.rs`, gate 15 ns); `bench/ptr_trees` is `binary_trees` with
    pointers. Spools, `^T@s`, `given` and the region checker are slice 2.

29. Treadle V6, slice 2 (design/v6-spools.md): spools and regions on both
    backends. `spool x = Arena(cap)` (ended with its scope, never moved,
    M0107), `x.put(v)`, `x.reset()`, `^T@s`; a warp that holds pointers has a
    hidden region parameter (`Expr` is `Expr@r`); `given sp: Spool` is an
    implicit generic over the spool's type (an instance for the heap, one for
    arenas), passed on by the innermost `given` in scope or `given: x`, and
    `^e` allocates in it; a read parameter written `^Expr` takes any spool.
    `String` copies (`.copy()`) go in the ambient arena (capacity 0: the arena
    owns the bytes). `ref warp` is accepted. The region checker
    (`typeck/regions.rs`) gives R0101 (outlives its spool), R0102 (used after
    `reset`) and R0104 (wrong spool for a parameter). `examples/calc.loom`
    runs, checked by `tests/examples/calc.out`. Not done: `given` of anything
    but a spool, defaults and inference of `given`s (slice 3), R0103
    (threads), collections in a spool (slice 4).

30. Treadle V6, slice 3 (design/v6-spools.md): `given` for any weft.
    `given name = value` (or nameless) from there to the end of the block,
    `given: x` at one call, and a weft's `#given_default(Type)` when nothing
    is in scope (the call makes it with the type's `default`; a spool's
    default is the heap). An unused `given` costs no dispatches
    (`bench/given_unused`). Every function spells its `given`s: a private
    function doesn't infer them from what it calls (Q9 below). Not done:
    `loom(given: pool)`, `yield`/`pull` in a `loom` body.

31. Treadle V6, slice 4 (design/v6-spools.md): `Pool[T](n)` (`put`,
    `free`, `reset`; a put, a read and a free cost 6 dispatches, gate in
    `bench_counts.rs`), `SharedArena(cap)` (allocates under the VM's lock,
    its pointers may cross to threads), R0103 (any other spool's pointers
    can't), and a guard that a warp in a spool can't hold a list, map, box
    or stream (U0102: it would leak). Not done: collections allocated in a
    spool (their growth would need the spool in the header), `Deque` and
    `SortedMap` nodes in a spool.

V5 is done. Its gates are asserted by `#[ignore]`d tests (`perf_gates.rs`,
`compile_time.rs`), with these limits: mandelbrot with `loom for` scales
about 5.7× on 8 workers (6.1–6.3× best of N in `bench/RESULTS.md`) against
the gate's 5.6× (0.7 × 8 physical cores; 0.36 of the 16 logical ones);
spawn+join takes 0.3 µs on one worker and about 1 µs on 8 (gate 2 µs);
the optimizer's compile-time ratio is about 1.63× `-O0` (design says 1.5×;
the test asserts 1.75×); only `loom for` over exclusive integer ranges
chunks, at a fixed grain; there are no per-worker deques or stealing.
`yield`/`pull` in a loom and `loom(given: …)` wait for V6; I/O is not a
cancel point yet (design/vm.md §11.4, §17).

32. Treadle V8, speed: COMPLETE (2026-11-02). Natives off the pool lock (text,
    string building, mutex, templates, messages, constants, print queues),
    per-worker deques and stack reuse, O(1) `Map.first`/`last`, native map
    reindex and compaction, fused list pipelines, and the perf gates in
    `bench/RESULTS.md`'s "V8 summary" (geomean 2.73x over PUC Lua). The `fn`
    keyword is `fun`.

33. Treadle V9, by default: COMPLETE (2026-11-10). `loom run`, `test`,
    `build` and `check` use Treadle unless told `--backend lua`; `loom build`
    writes `<file>.lmi` (`--exe` a standalone executable). Images are
    trusted, hash-checked and run through the verifier, not verified for
    memory (design/vm.md §8.6). The book's "How programs run" page; §18's
    list checked in `bench/RESULTS.md` ("V9 switch").

34. Generic wefts (2026-11-24): `weft Into[T]`, weaves with arguments
    (`weave Into[u64] into Meters`), super-wefts and bounds with arguments, bounds
    that pick the weaving in generic code, `Into[u64].convert(x)`, and static
    functions (`u64.from(x)`, `T.from(x)`). Left: `any W[A]`, a generic weft as a
    parameter's type, inferring a weft's arguments from the result type,
    associated types through a generic weft, `const` and region parameters of a
    weft (`tests/run/generic_weft`).

Next: what's left of the deferred work of each milestone, std included, in
LEDGER.md ("Roadmap"); what V8 left is under "Later" there.

Keep these green:
- `every_example_parses_cleanly` (`crates/loom_syntax`) and
  `every_doc_block_parses`: every example and every ```` ```loom ```` block in
  the README and design docs parses.
- `every_example_resolves_cleanly` (`crates/loom_compiler`).
- `every_example_fails_only_with_not_implemented_yet`: the type checker may
  reject an example only with U0102 until it covers what the example uses.
  Code v1 deliberately rejects must use a `U` code.
- `tests/run` and `tests/fail` (`crates/loom/tests/fixtures.rs`): every new
  feature gets fixtures there. A `tests/run` fixture also runs on Treadle,
  unless `tests/vm_skip.txt` lists it; one it lists that passes is a
  failure, so take it out. `crates/loom_compiler/tests/rt_int.rs` checks
  the runtime's integer operations against the Rust oracle in `ty::int`.
