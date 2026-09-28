# Checks docs/std/data.py against runtime/std/*.loom.
#
# data.py is a hand-kept transcription of the standard library's doc comments.
# This parses the .loom files just enough to list every pub item (types, their
# fields, cases and methods, functions, constants, re-exports) with its doc
# comment, and reports every way the two disagree: an item missing on either
# side, a different signature, a different doc comment, a different order.
#
#   python docs/audit.py
import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STD_DIR = os.path.join(ROOT, "runtime", "std")
MODULES_RS = os.path.join(ROOT, "crates", "loom_compiler", "src", "modules.rs")


def norm(s):
    return re.sub(r"\s+", " ", s or "").strip()


def matching_paren(s, start):
    """Index of the ')' that closes the '(' at s[start]."""
    depth = 0
    for i in range(start, len(s)):
        if s[i] == "(":
            depth += 1
        elif s[i] == ")":
            depth -= 1
            if depth == 0:
                return i
    return len(s) - 1


def cut_body(sig):
    """`fn f(a: T = 1) -> U = expr` and `fn f() {` -> `fn f(a: T = 1) -> U`."""
    open_paren = sig.find("(")
    rest_from = matching_paren(sig, open_paren) + 1 if open_paren >= 0 else 0
    tail = sig[rest_from:]
    for marker in (" = ", " {"):
        k = tail.find(marker)
        if k >= 0:
            tail = tail[:k]
    if tail.endswith("{"):
        tail = tail[:-1]
    return norm(sig[:rest_from] + tail)


def doc_and_example(lines):
    """`///` lines -> (text with paragraphs joined by a space, code example)."""
    text, example, in_code = [], [], False
    for ln in lines:
        if ln.strip().startswith("```"):
            in_code = not in_code
            continue
        if in_code:
            example.append(ln)
        else:
            text.append(ln)
    return norm(" ".join(text)) or None, ("\n".join(example) if example else None)


def strip_doc(line, marker):
    body = line.strip()[len(marker):]
    return body[1:] if body.startswith(" ") else body


class Item:
    def __init__(self, kind, name, head="", doc=None, example=None, builtin=False, extern=False, is_pub=False):
        self.kind = kind
        self.name = name
        self.head = head          # text after the name (a signature, for functions)
        self.doc = doc
        self.example = example
        self.builtin = builtin
        self.extern = extern
        self.is_pub = is_pub
        self.members = []         # methods, fields, cases, associated types


def parse_module(path):
    with open(path, encoding="utf-8") as f:
        lines = f.read().replace("\r\n", "\n").split("\n")

    # Module summary: the header comment at the top of the file.
    header, i = [], 0
    while i < len(lines):
        s = lines[i].strip()
        if s.startswith("//!"):
            header.append(strip_doc(s, "//!"))
        elif s.startswith("//") and not s.startswith("///"):
            header.append(strip_doc(s, "//"))
        else:
            break
        i += 1
    summary = norm(" ".join(header)) or None

    reexports, items = [], []
    docs, builtin = [], False
    stack = []          # open braces: (kind, item) with kind "type", "extern" or "other"

    def track_braces(line, kind, item=None):
        """Track the braces a line opens or closes; the outermost opened one gets `kind`."""
        net = line.count("{") - line.count("}")
        for _ in range(max(net, 0)):
            stack.append((kind, item))
            kind, item = "other", None
        for _ in range(max(-net, 0)):
            if stack:
                stack.pop()

    for raw in lines[i:]:
        s = raw.strip()
        if s.startswith("///"):
            docs.append(strip_doc(s, "///"))
            continue
        if not s or s.startswith("//"):
            continue
        if s.startswith("#builtin"):
            builtin = True
            continue
        ctx = stack[-1][0] if stack else "top"
        text, example = doc_and_example(docs)
        handled = False

        if ctx in ("top", "extern"):
            if m := re.match(r"pub use std\.(\w+)\.(?:\{([^}]*)\}|(\w+))\s*$", s):
                names = m.group(2).split(",") if m.group(2) else [m.group(3)]
                reexports += [{"name": n.strip(), "from": m.group(1)} for n in names]
                handled = True
            elif s.startswith("use "):
                handled = True
            elif ctx == "top" and (m := re.match(r"(pub )?(once warp|warp|weft) (\w+)(.*?)\s*(\{.*)?$", s)):
                it = Item(m.group(2), m.group(3), norm(m.group(4)), text, example, builtin, False, bool(m.group(1)))
                items.append(it)
                rest = (m.group(5) or "").strip()
                if rest.startswith("{") and rest != "{}":
                    track_braces(rest, "type", it)
                handled = True
            elif ctx == "top" and s == 'extern "lua" {':
                stack.append(("extern", None))
                handled = True
            elif m := re.match(r"(pub )?(?:const fn|fn) (\w+)(.*)$", s):
                head = cut_body(m.group(2) + m.group(3))[len(m.group(2)):]
                items.append(Item("fn", m.group(2), head, text, None, builtin, ctx == "extern", bool(m.group(1))))
                track_braces(s, "other")
                handled = True
            elif m := re.match(r"(pub )?const (\w+)(.*)$", s):
                items.append(Item("const", m.group(2), norm(m.group(3)), text, is_pub=bool(m.group(1))))
                handled = True
            elif s == "}" and stack:
                stack.pop()
                handled = True
        elif ctx == "type":
            it = stack[-1][1]
            if m := re.match(r"(pub )?fn (\w+)(.*)$", s):
                head = cut_body("fn " + m.group(2) + m.group(3))
                it.members.append(Item("fn", m.group(2), head, text, None, builtin, False, bool(m.group(1))))
                track_braces(s, "other")
                handled = True
            elif m := re.match(r"type (\w+)(.*)$", s):
                it.members.append(Item("type", m.group(1), norm("type " + m.group(1) + m.group(2)), text))
                handled = True
            elif m := re.match(r"case (.*)$", s):
                it.members.append(Item("case", m.group(1), norm(m.group(1)), text))
                handled = True
            elif m := re.match(r"(pub(?:\(get\))? )?(\w+): (.*)$", s):
                it.members.append(Item("field", m.group(2), norm(s), text, is_pub=bool(m.group(1))))
                handled = True
            elif s == "}":
                stack.pop()
                handled = True
        if not handled:
            track_braces(s, "other")
        docs, builtin = [], False
    return {"summary": summary, "reexports": reexports, "items": items}


def std_module_order():
    with open(MODULES_RS, encoding="utf-8") as f:
        text = f.read()
    return re.findall(r'\("(\w+)", include_str!', text)


def audit(modules):
    """Returns a list of problems; empty means data.py matches the source."""
    problems = []
    order = std_module_order()
    names = [m["name"] for m in modules]
    if names != order:
        problems.append(f"module list/order differs: data.py has {names}, modules.rs has {order}")
    for mod in modules:
        name = mod["name"]
        path = os.path.join(STD_DIR, name + ".loom")
        if not os.path.exists(path):
            problems.append(f"std.{name}: no such file {path}")
            continue
        src = parse_module(path)

        def P(msg):
            problems.append(f"std.{name}: {msg}")

        if norm(mod["summary"]) != src["summary"]:
            P(f"summary differs\n      data:   {norm(mod['summary'])!r}\n      source: {src['summary']!r}")
        if mod.get("reexports", []) != src["reexports"]:
            P(f"re-exports differ\n      data:   {mod.get('reexports', [])}\n      source: {src['reexports']}")

        # Private items (a module's own helpers) never appear in the spec.
        pub_items = [it for it in src["items"] if it.is_pub]
        src_types = [it for it in pub_items if it.kind in ("warp", "weft", "once warp")]
        src_fns = [it for it in pub_items if it.kind == "fn"]
        src_consts = [it for it in pub_items if it.kind == "const"]
        data_types = mod.get("types", [])

        for what, data_names, src_names in (
            ("types", [t["name"] for t in data_types], [t.name for t in src_types]),
            ("functions", [f["name"] for f in mod.get("functions", [])], [f.name for f in src_fns]),
            ("constants", [c["name"] for c in mod.get("consts", [])], [c.name for c in src_consts]),
        ):
            if data_names != src_names:
                P(f"{what} differ\n      data:   {data_names}\n      source: {src_names}")

        src_by_name = {t.name: t for t in src_types}
        for t in data_types:
            s = src_by_name.get(t["name"])
            if not s:
                continue

            def Q(msg, t=t):
                P(f"{t['name']}: {msg}")

            if t["kind"] != s.kind:
                Q(f"kind {t['kind']!r} vs {s.kind!r}")
            if norm(t.get("suffix", "")) != s.head:
                Q(f"header {norm(t.get('suffix', ''))!r} vs {s.head!r}")
            if bool(t.get("builtin")) != s.builtin:
                Q(f"builtin flag {bool(t.get('builtin'))} vs {s.builtin}")
            if norm(t.get("doc")) != (s.doc or ""):
                Q(f"doc differs\n      data:   {norm(t.get('doc'))!r}\n      source: {s.doc!r}")
            if (t.get("example") or None) != s.example:
                Q(f"example differs\n      data:   {t.get('example')!r}\n      source: {s.example!r}")
            # A weft's methods have no `pub`; a warp's are listed when `pub`.
            methods = [m for m in s.members if m.kind == "type" or (m.kind == "fn" and (m.is_pub or s.kind == "weft"))]
            data_methods = t.get("methods", [])
            if [m["sig"] for m in data_methods] != [m.head for m in methods]:
                Q(f"methods differ\n      data:   {[m['sig'] for m in data_methods]}\n      source: {[m.head for m in methods]}")
            else:
                for dm, sm in zip(data_methods, methods):
                    if norm(dm.get("doc")) != (sm.doc or ""):
                        Q(f"{dm['sig']}: doc differs\n      data:   {norm(dm.get('doc'))!r}\n      source: {sm.doc!r}")
                    if bool(dm.get("builtin")) != sm.builtin:
                        Q(f"{dm['sig']}: builtin flag {bool(dm.get('builtin'))} vs {sm.builtin}")
            src_cases = [m.head for m in s.members if m.kind == "case"]
            if t.get("cases", []) != src_cases:
                Q(f"cases differ\n      data:   {t.get('cases', [])}\n      source: {src_cases}")
            # Every `pub` field is listed; a listed private one must still exist.
            src_fields = {m.head: m.is_pub for m in s.members if m.kind == "field"}
            data_fields = t.get("fields", [])
            for f in data_fields:
                if f not in src_fields:
                    Q(f"field {f!r} isn't in the source")
            for f, is_pub in src_fields.items():
                if is_pub and f not in data_fields:
                    Q(f"pub field {f!r} is missing from data.py")

        src_fn_by_name = {f.name: f for f in src_fns}
        for f in mod.get("functions", []):
            s = src_fn_by_name.get(f["name"])
            if not s:
                continue

            def Q(msg, f=f):
                P(f"fn {f['name']}: {msg}")

            if norm(f["suffix"]) != s.head:
                Q(f"signature {norm(f['suffix'])!r} vs {s.head!r}")
            if norm(f.get("doc")) != (s.doc or ""):
                Q(f"doc differs\n      data:   {norm(f.get('doc'))!r}\n      source: {s.doc!r}")
            if bool(f.get("builtin")) != s.builtin:
                Q(f"builtin flag {bool(f.get('builtin'))} vs {s.builtin}")
            if bool(f.get("extern")) != s.extern:
                Q(f"extern flag {bool(f.get('extern'))} vs {s.extern}")
        src_const_by_name = {c.name: c for c in src_consts}
        for c in mod.get("consts", []):
            s = src_const_by_name.get(c["name"])
            if not s:
                continue
            if norm(c["suffix"]) != s.head:
                P(f"const {c['name']}: {norm(c['suffix'])!r} vs {s.head!r}")
            if norm(c.get("doc")) != (s.doc or ""):
                P(f"const {c['name']}: doc differs")
    return problems


if __name__ == "__main__":
    import sys

    sys.path.insert(0, os.path.join(ROOT, "docs", "std"))
    from data import MODULES

    probs = audit(MODULES)
    for p in probs:
        print("  -", p)
    print(f"{len(probs)} problem(s)")
    sys.exit(1 if probs else 0)
