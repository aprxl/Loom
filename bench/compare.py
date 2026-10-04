"""Treadle against other languages on bench/: PUC Lua 5.4 (bundled in
loom), LuaJIT, CPython and Bun, each running an idiomatic version of the
same program (bench/lua, bench/luajit where 5.4's integers are needed,
bench/py, bench/js), checked to print what the Loom program prints.

    python bench/compare.py [--runs N] [NAME ...]

The best wall-clock time of N runs (3 by default), start-up included.
"""
import os
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOOM = os.path.join(ROOT, "target", "release", "loom.exe" if os.name == "nt" else "loom")
BENCHES = ["fib", "sieve", "mandelbrot", "nbody", "spectral_norm", "fannkuch", "binary_trees",
           "richards", "word_freq", "maps", "sort", "streams", "chip8", "lexer"]


def luajit_file(name):
    own = os.path.join("bench", "luajit", f"{name}.lua")
    return own if os.path.exists(os.path.join(ROOT, own)) else os.path.join("bench", "lua", f"{name}.lua")


def commands(name):
    return {
        "Treadle": [LOOM, "run", "--backend", "vm", f"bench/{name}.loom"],
        "PUC Lua 5.4": [LOOM, "lua-host", f"bench/lua/{name}.lua"],
        "LuaJIT": ["luajit", "-e", f"dofile([[{luajit_file(name)}]])()"],
        "CPython": [sys.executable, f"bench/py/{name}.py"],
        "Bun": ["bun", f"bench/js/{name}.js"],
    }


def best(cmd, runs):
    times, out = [], None
    for _ in range(runs):
        start = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, cwd=ROOT)
        times.append(time.perf_counter() - start)
        if p.returncode != 0:
            sys.exit(f"{' '.join(cmd)} failed:\n{p.stderr.decode(errors='replace')}")
        out = p.stdout.replace(b"\r\n", b"\n")
    return min(times), out


def main():
    args = sys.argv[1:]
    runs = 3
    if "--runs" in args:
        i = args.index("--runs")
        runs = int(args[i + 1])
        del args[i:i + 2]
    for tool in ("luajit", "bun"):
        if shutil.which(tool) is None:
            sys.exit(f"`{tool}` isn't on the PATH")
    names = args or BENCHES
    langs = list(commands(names[0]))
    print("| Benchmark | " + " | ".join(langs) + " |")
    print("|---|" + "---|" * len(langs))
    ratios = {l: [] for l in langs}
    for name in names:
        row, want = {}, None
        for lang, cmd in commands(name).items():
            t, out = best(cmd, runs)
            if want is None:
                want = out
            elif out != want:
                sys.exit(f"{name}: {lang} printed something else")
            row[lang] = t
        for l in langs:
            ratios[l].append(row[l] / row["Treadle"])
        print(f"| `{name}` | " + " | ".join(f"{row[l]:.3f} s" for l in langs) + " |", flush=True)
    geo = lambda xs: __import__("math").prod(xs) ** (1 / len(xs))
    print("\nGeometric mean of each one's time over Treadle's (above 1: Treadle is faster):")
    for l in langs[1:]:
        print(f"  {l}: {geo(ratios[l]):.2f}x")


if __name__ == "__main__":
    main()
