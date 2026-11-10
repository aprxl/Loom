"""Profile-guided optimization of the `loom` binary, and its gate.

    python bench/pgo.py            build target/pgo/release/loom.exe
    python bench/pgo.py --gate     build it, then time it against target/release/loom.exe

The steps: build `loom` instrumented, run it over the training set, merge the
profiles with `llvm-profdata`, and build it again with them. Both builds get
the rustflags of `.cargo/config.toml` (setting RUSTFLAGS replaces them, and
the dispatch loop's layout depends on them) plus the PGO flag.

The training set is what the gate doesn't time: the `tests/run` fixtures, the
examples, and the benchmarks outside bench/run.py's suite. The gate times
that suite, best of N, the two binaries interleaved: the geometric mean of
PGO over plain must be at most GEOMEAN, no benchmark may be more than WORST
slower, and compiling the slowest example must not get slower.

Needs `llvm-profdata` of the LLVM that rustc uses (`rustc -vV`), on PATH or in
`C:\\Program Files\\LLVM\\bin`. The profiles are not kept in the repository.
"""
import glob
import math
import os
import re
import shutil
import subprocess
import sys
import time
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = ".exe" if os.name == "nt" else ""
PLAIN = os.path.join(ROOT, "target", "release", "loom" + EXE)
GEN_DIR = os.path.join(ROOT, "target", "pgo-gen")
USE_DIR = os.path.join(ROOT, "target", "pgo")
PROFILES = os.path.join(ROOT, "target", "pgo-profiles")
MERGED = os.path.join(PROFILES, "loom.profdata")
PGO = os.path.join(USE_DIR, "release", "loom" + EXE)

GEOMEAN = 0.95
WORST = 1.03
RUNS = 5

sys.path.insert(0, os.path.join(ROOT, "bench"))
from run import BENCHES  # noqa: E402  (the gate set: bench/run.py's suite)


def config_rustflags():
    with open(os.path.join(ROOT, ".cargo", "config.toml"), "rb") as f:
        config = tomllib.load(f)
    return config["target"]["cfg(all())"]["rustflags"]


def profdata():
    found = shutil.which("llvm-profdata") or r"C:\Program Files\LLVM\bin\llvm-profdata.exe"
    if not os.path.exists(found):
        sys.exit("llvm-profdata not found")
    rustc = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    want = re.search(r"LLVM version: (\d+)\.", rustc).group(1)
    have = subprocess.run([found, "--version"], capture_output=True, text=True, check=True).stdout
    got = re.search(r"LLVM version (\d+)\.", have).group(1)
    if want != got:
        sys.exit(f"llvm-profdata is LLVM {got}, rustc's is LLVM {want}: the profile would not load")
    return found


def build(target_dir, extra):
    env = dict(os.environ)
    env["RUSTFLAGS"] = " ".join(config_rustflags() + extra)
    env["CARGO_TARGET_DIR"] = target_dir
    subprocess.run(["cargo", "build", "--release", "-q", "-p", "loom"], cwd=ROOT, env=env, check=True)


def training():
    """Each command the instrumented binary runs."""
    gate = set(BENCHES)
    cmds = []
    for f in sorted(glob.glob(os.path.join(ROOT, "tests", "run", "*.loom"))):
        cmds.append(["run", f])
    for f in sorted(glob.glob(os.path.join(ROOT, "examples", "*.loom"))):
        cmds.append(["test" if f.endswith("chip8.loom") else "run", f])
        cmds.append(["check", f])
    for f in sorted(glob.glob(os.path.join(ROOT, "bench", "*.loom"))):
        name = os.path.splitext(os.path.basename(f))[0]
        if name not in gate:
            cmds.append(["run", f])
    return cmds


def train(loom):
    shutil.rmtree(PROFILES, ignore_errors=True)
    os.makedirs(PROFILES)
    cmds = training()
    start = time.perf_counter()
    for cmd in cmds:
        # Whether a fixture unravels or exits non-zero doesn't matter here:
        # what it ran is profiled either way. Files the programs write land
        # with the profiles, not in the repository.
        subprocess.run([loom, "--color", "never", *cmd], cwd=PROFILES, capture_output=True,
                       stdin=subprocess.DEVNULL, timeout=600)
    print(f"trained on {len(cmds)} runs in {time.perf_counter() - start:.1f} s")


def make():
    tool = profdata()
    build(GEN_DIR, ["-C", f"profile-generate={PROFILES}"])
    train(os.path.join(GEN_DIR, "release", "loom" + EXE))
    raw = glob.glob(os.path.join(PROFILES, "*.profraw"))
    subprocess.run([tool, "merge", "-o", MERGED, *raw], check=True)
    print(f"merged {len(raw)} profiles")
    build(USE_DIR, ["-C", f"profile-use={MERGED}", "-C", "llvm-args=-pgo-warn-missing-function=false"])
    print(f"built {os.path.relpath(PGO, ROOT)}")


def timed(cmd):
    start = time.perf_counter()
    p = subprocess.run(cmd, cwd=ROOT, capture_output=True)
    if p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed ({p.returncode}):\n{p.stderr.decode(errors='replace')}")
    return time.perf_counter() - start, p.stdout


def compare(args_of):
    """Best of RUNS for each binary, the runs interleaved, outputs equal."""
    best = {PLAIN: math.inf, PGO: math.inf}
    outs = {}
    for _ in range(RUNS):
        for loom in (PLAIN, PGO):
            t, out = timed([loom, *args_of])
            best[loom] = min(best[loom], t)
            outs[loom] = out
    if outs[PLAIN] != outs[PGO]:
        sys.exit(f"{args_of}: the two binaries print different things")
    return best[PLAIN], best[PGO]


def gate():
    # The plain binary, from the same sources, with the config's flags alone.
    subprocess.run(["cargo", "build", "--release", "-q", "-p", "loom"], cwd=ROOT, check=True)
    print("| Benchmark | plain | PGO | PGO / plain |")
    print("|---|--:|--:|--:|")
    ratios, failed = [], []
    for name in BENCHES:
        plain, pgo = compare(["run", f"bench/{name}.loom"])
        r = pgo / plain
        ratios.append(r)
        print(f"| `{name}` | {plain:.3f} s | {pgo:.3f} s | {r:.3f} |", flush=True)
        if r > WORST:
            failed.append(f"{name} is {r:.3f}x the plain binary's time (at most {WORST})")
    geo = math.exp(sum(math.log(r) for r in ratios) / len(ratios))
    print(f"\nGeometric mean {geo:.3f} (gate: at most {GEOMEAN})")
    if geo > GEOMEAN:
        failed.append(f"geometric mean {geo:.3f} > {GEOMEAN}")
    # Compiling: the slowest example, built at -O2, nothing run.
    plain, pgo = compare(["build", "--emit", "bytecode", "examples/chip8.loom"])
    print(f"compile `examples/chip8.loom`: {plain * 1000:.1f} ms plain, {pgo * 1000:.1f} ms PGO ({pgo / plain:.3f})")
    if pgo > plain * WORST:
        failed.append(f"compiling is {pgo / plain:.3f}x slower")
    if failed:
        sys.exit("PGO gate failed:\n  " + "\n  ".join(failed))
    print("PGO gate: ok")


def main():
    make()
    if "--gate" in sys.argv[1:]:
        gate()


if __name__ == "__main__":
    main()
