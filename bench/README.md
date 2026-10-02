# Benchmarks

How fast Treadle, Loom's own VM, runs Loom, compared with the Lua backend
and with other interpreters (design/vm.md §16).

Each benchmark is a Loom program with a `const SIZE` saying how big a run is,
and an idiomatic Lua 5.4 version of it in `lua/`:

| Benchmark | What it stresses |
|---|---|
| `fib` | calls and returns |
| `sieve` | arrays, bounds checks, integer loops |
| `mandelbrot` | float arithmetic in tight loops |
| `nbody` | records of floats, a list of them |
| `spectral_norm` | small functions called in float loops |
| `fannkuch` | small-integer arrays, permutations |
| `binary_trees` | allocating and freeing |
| `richards` | sums, `match`, records, `any` dispatch |
| `word_freq` | `String`, `Map[String, u32]`, templates |
| `sort` | `List.sort` on records, comparing through `Ord` |
| `streams` | iterator chains: `filter`, `map`, `take_while`, `sum` |
| `chip8` | twill `match`, `u8` and `u12` wrapping arithmetic, an interpreter loop |

The lexer benchmark comes with shuttles (V2).

## Running them

Build `loom` in release mode, then `python bench/run.py` times each on
Treadle and on PUC Lua 5.4 (the best of five runs, wall clock, compilation
and start-up included), checks their outputs agree, and prints the table
with the geometric mean. By hand:

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
