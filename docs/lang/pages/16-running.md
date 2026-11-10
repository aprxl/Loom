title: How programs run
summary: Treadle, the optimizer, images and executables, and the Lua backend.
---
The chapters so far were about what a program says. This one is about what happens when you run it: how `loom run` turns a file into something a machine can follow, what `loom build` leaves on disk, and when you'd reach for the other backend, Lua.

None of it changes what your program means. Every sample in this book prints the same thing whichever way it runs, except where this chapter says otherwise.

## From a file to Treadle

`loom run FILE` does the whole trip in one process. The file is parsed, its names are resolved, and it's type-checked. Then the compile-time code runs ([Compile-time code](page:compile-time)), and what it computed is written into the program as data. The checked program becomes **Heddle**, Loom's typed intermediate code, which the optimizer works on; Heddle becomes bytecode; and **Treadle**, Loom's own virtual machine, runs the bytecode.

@sample run_stages

- `loom build FILE --emit heddle` prints the Heddle after the optimizer, and `--emit bytecode` the instructions Treadle runs. `--emit tokens`, `ast` and `hir` show the earlier stages.
- `loom check FILE` goes as far as the bytecode and stops: it reports what `loom build` and `loom test` would, and writes nothing.
- A run that unravels prints where. With `LOOM_BACKTRACE=1` set, it also lists every call that led there, inlined ones included.

@try Run `loom build docs/lang/samples/run_stages.loom --emit heddle` in a terminal (the browser can't show stages). There's only `main`: the optimizer copied `sum_squares` and `square` into it, and the `inlined` lines at the top say where each piece came from. Add `-O0` before `build` and both come back as functions of their own, with a `call` to each.

## Optimizing, or not

Code is optimized unless you say `-O0`. At `-O2` (the default) the optimizer inlines small functions, folds constants, merges repeated work, moves what doesn't change out of loops, and drops the bounds and overflow checks it can prove never fail. Those checks are still there whenever it can't: an index out of range unravels at either level, at the same place.

- `-O0` compiles fastest, with every function kept as it was written. Reach for it when compiling time matters more than running time.
- `-O2` takes up to about twice as long to compile, and runs well ahead of `-O0` on loops and small functions.
- A program prints the same thing at both levels. A difference is a bug in the optimizer.

## Images and executables

`loom build FILE` writes **Treadle's image**, `FILE.lmi`: the program after every stage above, already optimized and with its constants computed. `loom run FILE.lmi` runs it without compiling anything again. The compiler isn't in the image, and neither is your source code, only what Treadle needs to run it.

`loom build --exe FILE` goes one step further and writes a standalone executable: Treadle and the image together, which runs on a machine with no Loom installed. It takes arguments like any program, and exits with the program's own exit code.

- `-o OUT` picks the file's name.
- An image holds a hash of its contents, and the loader checks it. A damaged image or executable is refused with a message, rather than run.
- An image is tied to the compiler that wrote it. Rebuild after updating Loom.
- An image is **trusted**. The loader checks the shape of its bytecode (every register, jump and call in range) but not what a pointer points to: that's the compiler's promise, so a file made by hand could touch any memory. Run only images you built.

@compare
-- Python
Python caches each module's bytecode in a `.pyc` file, but it always ships and reads the source: the cache is checked against it. A Loom image stands on its own, with the optimizer's work already done.
```python
import py_compile
py_compile.compile("tour.py")  # __pycache__/tour.cpython-313.pyc
```
-- Java
A `.class` file is bytecode too, and the JVM checks its types as it loads, because classes come from anywhere. Loom's loader checks only the bytecode's shape and trusts the compiler for the rest, which keeps loading small and fast.
```java
// javac Tour.java   -> Tour.class
// java Tour         -> verified, then run
```
-- Go
`go build` writes one native executable with the runtime inside. `loom build --exe` gives you the same kind of single file, with Treadle in the place of the runtime.
```go
// go build -o tour .
```
@end

## Threads

Treadle runs a program's threads ([Threads](page:threads)) on several workers: one per core by default. `LOOM_THREADS=N` picks how many. `LOOM_THREADS=1` runs every thread on one worker, in a fixed order, which helps when you're chasing a bug that depends on timing.

## The Lua backend

Loom also compiles to Lua 5.4. `--backend lua` on `run`, `test`, `build` and `check` picks it: `loom run --backend lua FILE` runs the program on the Lua bundled inside `loom`, and `loom build --backend lua FILE` writes `FILE.lua`, a single Lua file, with `FILE.lua.map` next to it to map its lines back to your source.

It's the older of the two backends and much slower. It stays because it's the reference Treadle is tested against: every program in Loom's test suite runs on both, and their output is compared. It's also a way to read your program as plain Lua.

@sample run_target

A few things are allowed to differ, and they're the same few everywhere:

| What | Treadle | Lua |
|---|---|---|
| `usize` and `target.pointer_bits` | 64 bits | 63 bits (Lua's integers are signed) |
| `x.hash()` | Treadle's own hash | a hash of the value's text |
| how deep recursion can go before `stack overflow` | far: a million calls deep is fine | Lua's limits |
| a `mut` argument when the function unravels | keeps the writes made before it | loses them |
| a shuttle dropped part-way through | runs the `defer`s it reached | leaves them to Lua's collector |

Equal values hash the same on both, so a map behaves the same: only the numbers differ.

@try Run `loom run --backend lua docs/lang/samples/run_target.loom` in a terminal. `usize.MAX` is half as big, and the hash is a different number, while the `true` stays.

## Where next

That's the whole trip, from a file to a run. [Not supported yet](page:not-yet) lists what the compiler doesn't do, chapter by chapter.
