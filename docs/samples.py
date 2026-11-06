# Runs every Loom code sample under docs/lang/samples/ with a `loom` binary and
# compares what happens with what the docs claim.
#
# Each sample is `NAME.loom`. Its last line says what to do with it (last, so
# the line numbers in diagnostics are the ones the docs show):
#
#   //@ run         `loom run`: exits 0, prints NAME.out
#   //@ unravel     `loom run`: exits 101 (an unravel); NAME.out is what it printed
#   //@ error       `loom run`: exits 1 (`main` returned an error)
#   //@ check       `loom check`: exits 0
#   //@ check-fail CODE...
#                   `loom check`: exits 2 with exactly these diagnostic codes, in
#                   order; NAME.out is the diagnostics
#   //@ test        `loom test`: exits 0; NAME.out is the report
#   //@ test-fail   `loom test`: exits 1; NAME.out is the report
#
# NAME.out holds stdout; when the sample also wrote to stderr, that follows
# after a `--- stderr ---` line (diagnostics and unravel reports go there).
# Programs run from the repository root, so paths in diagnostics read
# `docs/lang/samples/NAME.loom:LINE:COL`.
#
#   python docs/gen.py --check [--loom PATH]     compare
#   python docs/gen.py --bless [--loom PATH]     rewrite the .out files
#   ... --only NAME,NAME                         just these samples
import os
import re
import subprocess

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SAMPLES = os.path.join(ROOT, "docs", "lang", "samples")
REL = "docs/lang/samples"
STDERR_MARK = "--- stderr ---\n"

MODES = {
    # mode: (subcommand, expected exit code, has an .out file)
    "run": ("run", 0, True),
    "unravel": ("run", 101, True),
    "error": ("run", 1, True),
    "check": ("check", 0, False),
    "check-fail": ("check", 2, True),
    "test": ("test", 0, True),
    "test-fail": ("test", 1, True),
}


def find_loom(explicit=None):
    """The loom binary to use: --loom, $LOOM, a cargo build in target/, or PATH."""
    cands = [explicit, os.environ.get("LOOM")]
    for profile in ("release", "debug"):
        cands.append(os.path.join(ROOT, "target", profile, "loom.exe"))
        cands.append(os.path.join(ROOT, "target", profile, "loom"))
    for c in cands:
        if c and os.path.isfile(c):
            # absolute: Windows looks a relative program up from the parent's
            # directory, not the `cwd` the samples run in
            return os.path.abspath(c)
    return None


def sample_names():
    return sorted(f[:-5] for f in os.listdir(SAMPLES) if f.endswith(".loom"))


def read_source(name):
    with open(os.path.join(SAMPLES, name + ".loom"), encoding="utf-8", newline="") as f:
        return f.read().replace("\r\n", "\n")


def directive(name):
    """(mode, expected codes) from the sample's last line."""
    last = read_source(name).rstrip("\n").rsplit("\n", 1)[-1]
    m = re.match(r"//@ (\S+)((?: [A-Z]\d{4})*)\s*$", last)
    if not m or m.group(1) not in MODES:
        raise ValueError(f"{name}.loom: last line must be `//@ MODE` with MODE one of {sorted(MODES)}")
    codes = m.group(2).split()
    if (m.group(1) == "check-fail") != bool(codes):
        raise ValueError(f"{name}.loom: `//@ check-fail` takes the expected codes, and no other mode does")
    return m.group(1), codes


def mode_of(name):
    return directive(name)[0]


def shown_source(name):
    """The sample as the docs show it: without the `//@` line, so the
    line numbers in its diagnostics match."""
    lines = read_source(name).rstrip("\n").split("\n")
    return "\n".join(lines[:-1]).rstrip("\n")


def out_path(name):
    return os.path.join(SAMPLES, name + ".out")


def read_out(name):
    p = out_path(name)
    if not os.path.exists(p):
        return None
    with open(p, encoding="utf-8", newline="") as f:
        return f.read().replace("\r\n", "\n")


def shown_out(name):
    """The sample's output as the docs show it: paths without the directory."""
    text = read_out(name)
    if text is None:
        return None
    text = text.replace(REL + "/", "")
    return text.rstrip("\n")


def execute(loom, name):
    cmd, _, _ = MODES[mode_of(name)]
    args = [loom, cmd, f"{REL}/{name}.loom", "--color", "never"]
    env = dict(os.environ, LOOM_THREADS="1")  # the reference schedule: thread output is in a fixed order
    p = subprocess.run(args, cwd=ROOT, capture_output=True, timeout=120, env=env)
    out = p.stdout.decode("utf-8", "replace").replace("\r\n", "\n")
    err = p.stderr.decode("utf-8", "replace").replace("\r\n", "\n")
    return p.returncode, out, err


def combine(out, err):
    if out and err:
        return out + STDERR_MARK + err
    return out or err


def check(loom, bless=False, only=None):
    """Runs the samples (`only` those, if given) and lists what differs."""
    problems = []
    names = sample_names()
    if only:
        missing = [n for n in only if n not in names]
        if missing:
            return [], [f"no such sample: {', '.join(missing)}"]
        names = [n for n in names if n in only]
    for name in names:
        try:
            mode = mode_of(name)
        except ValueError as e:
            problems.append(str(e))
            continue
        _, want_code, has_out = MODES[mode]
        code, out, err = execute(loom, name)
        if code != want_code:
            problems.append(f"{name}: `{mode}` should exit {want_code}, exited {code}\n{combine(out, err)}")
            continue
        if mode in ("run", "check", "test") and err:
            problems.append(f"{name}: `{mode}` wrote to stderr\n{err}")
            continue
        got = combine(out, err) if has_out else ""
        if mode == "check-fail":
            found = re.findall(r"error\[([A-Z]\d{4})\]", got)
            if found != expected_codes(name):
                problems.append(f"{name}: expected diagnostics {expected_codes(name)}, got {found}\n{got}")
                continue
        if not has_out:
            if os.path.exists(out_path(name)):
                problems.append(f"{name}.out exists but `{mode}` has no output")
            continue
        want = read_out(name)
        if bless:
            with open(out_path(name), "w", encoding="utf-8", newline="") as f:
                f.write(got)
        elif want is None:
            problems.append(f"{name}: no {name}.out (run with --bless)")
        elif want != got:
            problems.append(f"{name}: output differs\n--- expected\n{want}--- got\n{got}")
    known = set(sample_names())
    for f in os.listdir(SAMPLES):
        if f.endswith(".out") and f[:-4] not in known:
            problems.append(f"{f}: no sample {f[:-4]}.loom")
    return names, problems


def expected_codes(name):
    return directive(name)[1]
