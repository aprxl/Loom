# Reads the standard library's sources (runtime/std/*.loom) for the docs: every
# `pub` item, with its doc comment, in the order the source declares it. The
# std pages are generated from this, so a doc comment in the `.loom` file is
# the only place an item is described.
#
# It parses the sources just enough: the header comment (the module's
# summary), `pub use` re-exports, warps and wefts with their fields, cases,
# methods and associated types, functions (also those in `extern "host"`
# blocks), and constants. `#builtin` marks an item the compiler or runtime
# implements. A doc comment's paragraphs and ``` code blocks are kept apart.
#
#   python docs/std_source.py [MODULE]    print what it finds
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
    """`fun f(a: T = 1) -> U = expr` and `fun f() {` -> `fun f(a: T = 1) -> U`."""
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


def doc_blocks(lines):
    """`///` lines -> [("p", text) | ("code", text)], paragraphs split at
    blank lines and the lines of each joined by a space."""
    blocks, para, code, in_code = [], [], [], False
    for ln in lines:
        if ln.strip().startswith("```"):
            if in_code:
                blocks.append(("code", "\n".join(code)))
                code = []
            elif para:
                blocks.append(("p", norm(" ".join(para))))
                para = []
            in_code = not in_code
            continue
        if in_code:
            code.append(ln)
        elif not ln.strip():
            if para:
                blocks.append(("p", norm(" ".join(para))))
                para = []
        else:
            para.append(ln)
    if para:
        blocks.append(("p", norm(" ".join(para))))
    if code:
        blocks.append(("code", "\n".join(code)))
    return blocks


def strip_doc(line, marker):
    body = line.strip()[len(marker):]
    return body[1:] if body.startswith(" ") else body


class Item:
    def __init__(self, kind, name, head="", doc=None, builtin=False, extern=False, is_pub=False, line=0):
        self.kind = kind          # warp, once warp, weft, fn, const, field, case, type
        self.name = name
        self.head = head          # the text after the name (a signature, for functions)
        self.doc = doc or []      # doc_blocks
        self.builtin = builtin
        self.extern = extern
        self.is_pub = is_pub
        self.line = line
        self.members = []         # methods, fields, cases, associated types

    @property
    def text(self):
        """The doc's paragraphs as one string (for summaries and search)."""
        return " ".join(t for k, t in self.doc if k == "p")

    def __repr__(self):
        return f"{self.kind} {self.name}{self.head}"


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
        elif not s and header and i + 1 < len(lines) and lines[i + 1].strip().startswith("//") \
                and not lines[i + 1].strip().startswith("///"):
            header.append("")
        else:
            break
        i += 1
    summary = doc_blocks(header)

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

    for number, raw in enumerate(lines[i:], start=i + 1):
        s = raw.strip()
        if s.startswith("///"):
            docs.append(strip_doc(s, "///"))
            continue
        if not s or s.startswith("//"):
            continue
        if s.startswith("#builtin"):
            builtin = True
            # `#builtin pub warp Field: Copy {}`: the item can follow it.
            s = s[len("#builtin"):].strip()
            if not s:
                continue
        ctx = stack[-1][0] if stack else "top"
        doc = doc_blocks(docs)
        handled = False

        if ctx in ("top", "extern"):
            if m := re.match(r"pub use std\.(\w+)\.(?:\{([^}]*)\}|(\w+))\s*$", s):
                names = m.group(2).split(",") if m.group(2) else [m.group(3)]
                reexports += [{"name": n.strip(), "from": m.group(1)} for n in names]
                handled = True
            elif s.startswith("use "):
                handled = True
            elif ctx == "top" and (m := re.match(r"(pub )?(once warp|warp|weft) (\w+)(.*?)\s*(\{.*)?$", s)):
                it = Item(m.group(2), m.group(3), norm(m.group(4)), doc, builtin, False, bool(m.group(1)), number)
                items.append(it)
                rest = (m.group(5) or "").strip()
                if rest.startswith("{") and rest != "{}":
                    track_braces(rest, "type", it)
                handled = True
            elif ctx == "top" and s == 'extern "host" {':
                stack.append(("extern", None))
                handled = True
            elif m := re.match(r"(pub )?(?:const fun|fun) (\w+)(.*)$", s):
                head = cut_body(m.group(2) + m.group(3))[len(m.group(2)):]
                items.append(Item("fn", m.group(2), head, doc, builtin, ctx == "extern", bool(m.group(1)), number))
                track_braces(s, "other")
                handled = True
            elif m := re.match(r"(pub )?const (\w+)(.*)$", s):
                items.append(Item("const", m.group(2), norm(m.group(3)), doc, is_pub=bool(m.group(1)), line=number))
                handled = True
            elif s == "}" and stack:
                stack.pop()
                handled = True
        elif ctx == "type":
            it = stack[-1][1]
            if m := re.match(r"(pub )?fun (\w+)(.*)$", s):
                head = cut_body("fun " + m.group(2) + m.group(3))
                it.members.append(Item("fn", m.group(2), head, doc, builtin, False, bool(m.group(1)), number))
                track_braces(s, "other")
                handled = True
            elif m := re.match(r"type (\w+)(.*)$", s):
                it.members.append(Item("type", m.group(1), norm("type " + m.group(1) + m.group(2)), doc, line=number))
                handled = True
            elif m := re.match(r"case (.*)$", s):
                it.members.append(Item("case", m.group(1), norm(m.group(1)), doc, line=number))
                handled = True
            elif m := re.match(r"(pub(?:\(get\))? )?(\w+): (.*)$", s):
                it.members.append(Item("field", m.group(2), norm(s), doc, is_pub=bool(m.group(1)), line=number))
                handled = True
            elif s == "}":
                stack.pop()
                handled = True
        if not handled:
            track_braces(s, "other")
        docs, builtin = [], False
    return {"summary": summary, "reexports": reexports, "items": items}


def module_order():
    """The std modules, in the order the compiler embeds them."""
    with open(MODULES_RS, encoding="utf-8") as f:
        text = f.read()
    return re.findall(r'\("(\w+)", include_str!', text)


def listed_methods(t):
    """A weft's methods have no `pub`; a warp's are listed when `pub`."""
    return [m for m in t.members if m.kind == "type" or (m.kind == "fn" and (m.is_pub or t.kind == "weft"))]


def load():
    """Every std module, as the docs show it: [{name, summary, reexports,
    types, functions, consts}], only `pub` items."""
    modules = []
    for name in module_order():
        path = os.path.join(STD_DIR, name + ".loom")
        src = parse_module(path)
        pub = [it for it in src["items"] if it.is_pub]
        modules.append({
            "name": name,
            "path": f"runtime/std/{name}.loom",
            "summary": src["summary"],
            "reexports": src["reexports"],
            "types": [it for it in pub if it.kind in ("warp", "weft", "once warp")],
            "functions": [it for it in pub if it.kind == "fn"],
            "consts": [it for it in pub if it.kind == "const"],
        })
    return modules


if __name__ == "__main__":
    import sys

    want = sys.argv[1:] or None
    for mod in load():
        if want and mod["name"] not in want:
            continue
        print(f"std.{mod['name']}: {mod['summary']}")
        for t in mod["types"]:
            print(f"  {t!r}  {'' if t.doc else '(undocumented)'}")
            for m in t.members:
                if m.kind == "field" and not m.is_pub:
                    continue
                if m.kind == "fn" and m not in listed_methods(t):
                    continue
                print(f"    {m.kind} {m.head}  {'' if m.doc else '(undocumented)'}")
        for f in mod["functions"]:
            print(f"  fun {f.name}{f.head}  {'' if f.doc else '(undocumented)'}")
        for c in mod["consts"]:
            print(f"  const {c.name}{c.head}  {'' if c.doc else '(undocumented)'}")
