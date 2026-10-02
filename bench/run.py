"""Times every benchmark on Treadle and on PUC Lua 5.4 (the one bundled in
`loom`), best of N wall-clock runs each, and prints a Markdown table with
the speedups and their geometric mean (design/vm.md §16).

    python bench/run.py [--runs N] [--lua-backend] [NAME ...]

Build first: `cargo build --release -p loom`. `--lua-backend` also times
Loom's Lua backend. Each run's output is checked against the Lua version's.
"""
import math
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOOM = os.path.join(ROOT, "target", "release", "loom.exe" if os.name == "nt" else "loom")
BENCHES = ["fib", "sieve", "mandelbrot", "nbody", "spectral_norm", "fannkuch", "binary_trees",
           "richards", "word_freq", "sort", "streams", "chip8"]


def best(cmd, runs):
    """The best wall-clock time of `runs` runs of `cmd`, and its output."""
    times, out = [], None
    for _ in range(runs):
        start = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, cwd=ROOT)
        times.append(time.perf_counter() - start)
        if p.returncode != 0:
            sys.exit(f"{' '.join(cmd)} failed ({p.returncode}):\n{p.stderr.decode(errors='replace')}")
        out = p.stdout
    return min(times), out


def main():
    args = sys.argv[1:]
    runs = 5
    if "--runs" in args:
        i = args.index("--runs")
        runs = int(args[i + 1])
        del args[i:i + 2]
    lua_backend = "--lua-backend" in args
    if lua_backend:
        args.remove("--lua-backend")
    names = args or BENCHES
    header = "| Benchmark | PUC Lua 5.4 | Treadle | Treadle vs Lua 5.4 |"
    rule = "|---|---|---|---|"
    if lua_backend:
        header += " Lua backend |"
        rule += "---|"
    print(header)
    print(rule)
    ratios = []
    for name in names:
        lua_t, lua_out = best([LOOM, "lua-host", f"bench/lua/{name}.lua"], runs)
        vm_t, vm_out = best([LOOM, "run", "--backend", "vm", f"bench/{name}.loom"], runs)
        if vm_out.replace(b"\r\n", b"\n") != lua_out.replace(b"\r\n", b"\n"):
            sys.exit(f"{name}: Treadle's output differs from Lua's:\n{vm_out!r}\n{lua_out!r}")
        ratio = lua_t / vm_t
        ratios.append(ratio)
        row = f"| `{name}` | {lua_t:.3f} s | **{vm_t:.3f} s** | {ratio:.2f}× |"
        if lua_backend:
            lb_t, _ = best([LOOM, "run", f"bench/{name}.loom"], runs)
            row += f" {lb_t:.3f} s |"
        print(row, flush=True)
    geo = math.exp(sum(math.log(r) for r in ratios) / len(ratios))
    print(f"\nGeometric mean: {geo:.2f}×; lowest: {min(ratios):.2f}×")


if __name__ == "__main__":
    main()
