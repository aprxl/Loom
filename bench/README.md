# Benchmarks

How fast Treadle, Loom's own VM, runs Loom, compared with the Lua backend
and with other interpreters (design/vm.md §16).

Each benchmark is a Loom program with a `const SIZE` saying how big a run is,
and an idiomatic Lua 5.4 version of it in `lua/`:

| Benchmark | What it stresses |
|---|---|
| `fib` | calls and returns |
| `mandelbrot` | float arithmetic in tight loops |

The rest of the suite (sieve, nbody, binary trees, …) comes as Treadle runs
what they need.

## Running them

Build `loom` in release mode, then time each way of running a benchmark (the
best of five runs, wall clock, compilation and start-up included):

```bash
cargo build --release -p loom
```

```bash
target/release/loom run --backend vm bench/fib.loom
```

```bash
target/release/loom run bench/fib.loom
```

```bash
target/release/loom lua-host bench/lua/fib.lua
```

The last runs the Lua version on the Lua 5.4 bundled in `loom` (a Lua file
that returns its `main` function). Results, with the machine and versions,
go in `RESULTS.md`.

## The dispatch counts

Wall-clock time is too noisy to fail a test on, so `counts.txt` pins how many
instructions Treadle dispatches for each benchmark at a small size. A change
that moves a count by more than 2% fails
`cargo test -p loom --features vm-stats`; one that means to writes the new
counts with `LOOM_BLESS=1`.
