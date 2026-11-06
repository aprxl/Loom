# Generates the whole docs site from its hand-kept sources.
#
#   python docs/gen.py                        regenerate every page
#   python docs/gen.py --check [--loom PATH]  verify, changing nothing:
#                                             1. the generated pages on disk are up to date
#                                             2. every relative link and anchor resolves
#                                             3. every Loom sample behaves as its page says,
#                                                and every std module's example program runs
#                                             (and notes when the playground's wasm is older
#                                             than the compiler: python docs/play/build.py)
#   python docs/gen.py --bless [--loom PATH]  run the samples and rewrite their .out files
#   ... --only NAME,NAME                      with --check or --bless: only these samples
#                                             (--check then checks nothing else)
#
# --loom names the compiler for the samples (default: $LOOM, else
# target/release or target/debug in this repository). --check exits 1 on any
# problem, so it can run in CI.
#
# Sources:            runtime/std/*.loom      the standard library and its doc comments
#                     docs/lang/pages/*.md    the language pages' prose
#                     docs/lang/samples/*     every Loom code sample, and its output
#                     crates/loom_syntax/src/diag/codes.rs   the diagnostic codes
# Generated:          docs/index.html, docs/playground.html, docs/lang/*.html, docs/std/**/*.html,
#                     docs/play/loom-wasm.js (by docs/play/build.py, not this file)
# Hand-written:       docs/style.css, docs/weave.js (the thread), docs/play/play.js (the playground)
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)


import lang_pages  # noqa: E402
import samples  # noqa: E402
import std_pages  # noqa: E402
import std_source  # noqa: E402
from common import Writer  # noqa: E402


class MemoryWriter(Writer):
    """A Writer that also keeps every file's text, for the link checker."""

    def __init__(self, check=False):
        super().__init__(check)
        self.files = {}

    def write(self, rel, text):
        self.files[rel] = text
        super().write(rel, text)


def generate(check):
    w = MemoryWriter(check=check)
    modules = std_source.load()
    site = lang_pages.generate(modules, w)
    std_pages.generate(modules, w, [p["slug"] for p in site.pages])
    return w


def check_links(files):
    """Every relative href/src in the generated pages points at a real file and anchor."""
    problems = []
    ids = {rel: set(re.findall(r'\bid="([^"]+)"', text)) for rel, text in files.items()}
    for rel, text in sorted(files.items()):
        base = os.path.dirname(rel)
        for target in re.findall(r'(?:href|src)="([^"]*)"', text):
            if re.match(r"[a-z]+:", target) or target == "":
                continue
            path, _, frag = target.partition("#")
            dest = os.path.normpath(os.path.join(base, path)).replace("\\", "/") if path else rel
            if dest in files:
                if frag and frag not in ids[dest]:
                    problems.append(f"{rel}: link to {target!r}: no anchor #{frag} in {dest}")
            elif not os.path.exists(os.path.join(HERE, *dest.split("/"))):
                problems.append(f"{rel}: link to {target!r} goes nowhere ({dest})")
    return problems


def check_std_examples(exe):
    """Each example program in a std module's header comment runs and exits 0
    (`dbg` writes to stderr, so stderr is allowed)."""
    import subprocess
    import tempfile

    problems, n = [], 0
    with tempfile.TemporaryDirectory() as tmp:
        for mod in std_source.load():
            for code in std_pages.examples(mod):
                n += 1
                path = os.path.join(tmp, f"{mod['name']}_example.loom")
                with open(path, "w", encoding="utf-8", newline="\n") as f:
                    f.write(code + "\n")
                p = subprocess.run([exe, "run", path, "--color", "never"], cwd=tmp, capture_output=True,
                                   timeout=120, env=dict(os.environ, LOOM_THREADS="1"))
                if p.returncode != 0:
                    problems.append(f"std.{mod['name']}'s example exits {p.returncode}:\n"
                                    + p.stdout.decode("utf-8", "replace") + p.stderr.decode("utf-8", "replace"))
    print(f"  {n} examples run")
    return problems


def main(argv):
    check = "--check" in argv
    bless = "--bless" in argv
    loom = None
    if "--loom" in argv:
        loom = argv[argv.index("--loom") + 1]
    only = None
    if "--only" in argv:
        only = [n.strip() for n in argv[argv.index("--only") + 1].split(",") if n.strip()]
    unknown = [a for a in argv if a.startswith("--") and a not in ("--check", "--bless", "--loom", "--only")]
    if unknown:
        print(f"unknown option {unknown[0]}; see the top of docs/gen.py")
        return 2

    if bless:
        exe = samples.find_loom(loom)
        if not exe:
            print("no loom binary: pass --loom PATH or set $LOOM")
            return 2
        names, problems = samples.check(exe, bless=True, only=only)
        for p in problems:
            print(" -", p)
        print(f"blessed {len(names)} samples with {exe}")
        if problems:
            return 1

    if not check:
        w = generate(check=False)
        print(f"wrote {len(w.written)} files")
        return 0

    if only:
        exe = samples.find_loom(loom)
        if not exe:
            print("no loom binary: pass --loom PATH or set $LOOM")
            return 2
        names, problems = samples.check(exe, only=only)
        for p in problems:
            print(" -", p)
        print(f"{len(names)} samples run with {exe}: " + ("FAILED" if problems else "ok"))
        return 1 if problems else 0

    failed = False
    print("generated pages ...")
    w = generate(check=True)
    for rel in w.stale:
        print(f"  - {rel} is out of date (run python docs/gen.py)")
    failed |= bool(w.stale)

    print("links ...")
    problems = check_links(w.files)
    for p in problems:
        print("  -", p)
    failed |= bool(problems)

    print("samples ...")
    exe = samples.find_loom(loom)
    if not exe:
        print("  - no loom binary: pass --loom PATH, set $LOOM, or build one with cargo")
        failed = True
    else:
        names, problems = samples.check(exe)
        for p in problems:
            print("  -", p)
        print(f"  {len(names)} samples run with {exe}")
        failed |= bool(problems)

    print("std examples ...")
    if exe:
        problems = check_std_examples(exe)
        for p in problems:
            print("  -", p)
        failed |= bool(problems)

    print("playground ...")
    sys.path.insert(0, os.path.join(HERE, "play"))
    import build as play_build  # noqa: E402

    built, now = play_build.built_hash(), play_build.sources_hash()
    if built is None:
        print("  - docs/play/loom-wasm.js is missing: python docs/play/build.py")
        failed = True
    elif built != now:
        # Not a failure: the samples are checked against `loom` above. But
        # the Run buttons use the older build until it's rebuilt.
        print(f"  note: loom-wasm.js was built from older sources ({built}, now {now}):"
              " python docs/play/build.py")

    print("FAILED" if failed else "ok")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
