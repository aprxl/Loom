# Shared pieces of the docs generator: escaping, inline markup, the page shell
# and sidebar, a small highlighter for Loom code, and the file writer that
# `--check` uses to spot pages that are out of date.
#
# Every link is relative (a page knows how deep it sits: `depth`), so the site
# works from file:// and from any web root. No JavaScript.
import html
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))


def esc(s):
    return html.escape(s, quote=False)


def attr(s):
    return html.escape(s, quote=True)


# ------------------------------------------------------------------ output

class Writer:
    """Writes generated files, or (in check mode) compares them to what's on disk."""

    def __init__(self, check=False):
        self.check = check
        self.stale = []
        self.written = []

    def write(self, rel, text):
        path = os.path.join(HERE, *rel.split("/"))
        self.written.append(rel)
        data = text.encode("utf-8")
        if self.check:
            old = open(path, "rb").read() if os.path.exists(path) else None
            if old != data:
                self.stale.append(rel)
            return
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)


# ------------------------------------------------------------------ links

class Links:
    """Resolves `page:x`, `std:x#item`, `diag:X` link targets, relative to a page."""

    def __init__(self, lang_slugs, std_modules):
        self.lang = set(lang_slugs)
        self.std = set(std_modules)

    def resolve(self, target, here_dir):
        """`here_dir` is the page's directory below docs/ ('', 'lang', 'std', 'std/modules')."""
        if target.startswith("page:"):
            slug, _, frag = target[5:].partition("#")
            if slug not in self.lang:
                raise KeyError(f"unknown language page {slug!r}")
            dest = f"lang/{slug}.html"
        elif target.startswith("std:"):
            name, _, frag = target[4:].partition("#")
            if name == "":
                dest = "std/index.html"
            elif name not in self.std:
                raise KeyError(f"unknown std module {name!r}")
            else:
                dest = f"std/modules/{name}.html"
        elif target.startswith("diag:"):
            dest, frag = "lang/diagnostics.html", target[5:]
        else:
            return target
        rel = os.path.relpath(dest, here_dir or ".").replace("\\", "/")
        return rel + (f"#{frag}" if frag else "")


_INLINE = re.compile(r"\[((?:[^\]`]|`[^`]*`)+)\]\(([^)\s]+)\)|`([^`]+)`")
_CODE_ONLY = re.compile(r"`([^`]+)`")


def _plain(text):
    return re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", esc(text))


def inline(text, links=None, here_dir=""):
    """Doc text -> HTML. `code` spans always; with `links`, also
    [label](target) links (the label may hold code) and **bold**."""
    if text is None:
        return None
    if links is None:
        out, pos = [], 0
        for m in _CODE_ONLY.finditer(text):
            out.append(esc(text[pos:m.start()]))
            out.append(f"<code>{esc(m.group(1))}</code>")
            pos = m.end()
        out.append(esc(text[pos:]))
        return "".join(out)
    out, pos = [], 0
    for m in _INLINE.finditer(text):
        out.append(_plain(text[pos:m.start()]))
        if m.group(3) is not None:
            out.append(f"<code>{esc(m.group(3))}</code>")
        else:
            href = links.resolve(m.group(2), here_dir)
            out.append(f'<a href="{attr(href)}">{inline(m.group(1), links, here_dir)}</a>')
        pos = m.end()
    out.append(_plain(text[pos:]))
    return "".join(out)


# ------------------------------------------------------------------ shell

def rel_root(here_dir):
    return "../" * (len(here_dir.split("/")) if here_dir else 0)


def sidebar(here_dir, current_section, heading=None, entries=None, current_entry=None):
    """The left column. `entries` are (label, href, key) for the section's own pages."""
    root = rel_root(here_dir)
    parts = ['<nav class="sidebar">',
             f'<a class="brand" href="{root}index.html">Loom <span class="dim">docs</span></a>',
             '<ul class="sections">']
    for key, label, href in (("lang", "Language", "lang/index.html"),
                             ("std", "Standard library", "std/index.html"),
                             ("diag", "Diagnostic codes", "lang/diagnostics.html")):
        cls = ' class="current"' if key == current_section else ""
        parts.append(f'<li><a{cls} href="{root}{href}">{label}</a></li>')
    parts.append("</ul>")
    if entries:
        parts.append(f"<h3>{esc(heading)}</h3>")
        parts.append("<ul>")
        for label, href, key in entries:
            cls = ' class="current"' if key == current_entry else ""
            parts.append(f'<li><a{cls} href="{href}">{label}</a></li>')
        parts.append("</ul>")
    parts.append("</nav>")
    return "\n".join(parts)


PAGE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="{root}style.css">
</head>
<body>
<div class="layout">
{sidebar}
<main>
{body}
</main>
</div>
</body>
</html>
"""


def page(title, here_dir, sidebar_html, body):
    return PAGE.format(title=esc(title), root=rel_root(here_dir), sidebar=sidebar_html, body=body)


# ------------------------------------------------------------------ highlighting

KEYWORDS = set("""fn let var const type warp weft weave into through case pub if else match for in
while loop break continue return fail is not and or mend defer ref spool given loom thread yield pull
unsafe extern use as any self Self mut take once test where true false some none ok err""".split())

NUMBER = re.compile(r"0x[0-9A-Fa-f_?a-z]+|0b[01_?a-z]+|0o[0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][-+]?\d+)?[a-z]\w*|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][-+]?\d+)?")
IDENT = re.compile(r"[A-Za-z_]\w*")
CHAR = re.compile(r"'(?:\\.[^']*|[^'\\])'")


def _tokens(code, i, in_template):
    """Scan Loom source into (class, text) segments. In a template's `{...}`,
    stops after the matching `}`. Returns (segments, next index)."""
    segs = []
    depth = 0
    n = len(code)
    while i < n:
        c = code[i]
        if c == "/" and code.startswith("//", i):
            j = code.find("\n", i)
            j = n if j < 0 else j
            segs.append(("c", code[i:j]))
            i = j
        elif c == "/" and code.startswith("/*", i):
            j = code.find("*/", i + 2)
            j = n if j < 0 else j + 2
            segs.append(("c", code[i:j]))
            i = j
        elif c == '"':
            s, i = _string(code, i)
            segs.extend(s)
        elif c == "'" and (m := CHAR.match(code, i)):
            segs.append(("s", m.group(0)))
            i = m.end()
        elif c.isdigit():
            m = NUMBER.match(code, i)
            segs.append(("n", m.group(0)))
            i = m.end()
        elif c.isalpha() or c == "_":
            m = IDENT.match(code, i)
            w = m.group(0)
            prev = code[i - 1] if i else ""
            if w in KEYWORDS and prev != ".":
                segs.append(("k", w))
            elif w[0].isupper() and any(ch.islower() for ch in w):
                segs.append(("t", w))
            else:
                segs.append(("", w))
            i = m.end()
        elif c == "#" and (m := re.compile(r"#\w+").match(code, i)):
            segs.append(("k", m.group(0)))
            i = m.end()
        else:
            if in_template:
                if c == "{":
                    depth += 1
                elif c == "}":
                    if depth == 0:
                        return segs, i
                    depth -= 1
            segs.append(("", c))
            i += 1
    return segs, i


def _string(code, i):
    """Scan a string literal starting at code[i] == '"'; template holes are code."""
    segs = []
    buf = ['"']
    i += 1
    n = len(code)
    while i < n:
        c = code[i]
        if c == "\\" and i + 1 < n:
            buf.append(code[i:i + 2])
            i += 2
        elif c == '"':
            buf.append('"')
            i += 1
            break
        elif c == "{" and code.startswith("{{", i):
            buf.append("{{")
            i += 2
        elif c == "{":
            buf.append("{")
            segs.append(("s", "".join(buf)))
            buf = []
            inner, i = _tokens(code, i + 1, True)
            segs.extend(inner)
            buf.append("}" if i < n and code[i] == "}" else "")
            i += 1
        else:
            buf.append(c)
            i += 1
    if buf:
        segs.append(("s", "".join(buf)))
    return segs, i


def highlight(code):
    segs, _ = _tokens(code, 0, False)
    out = []
    for cls, text in segs:
        t = esc(text)
        out.append(f'<span class="{cls}">{t}</span>' if cls else t)
    return "".join(out)


def highlight_output(text):
    """Compiler output: colour the `error[CODE]` lines and the `-->` locations."""
    lines = []
    for ln in text.split("\n"):
        e = esc(ln)
        if re.match(r"(error|warning)(\[[A-Z]\d{4}\])?:", ln):
            e = f'<span class="err">{e}</span>'
        elif ln.lstrip().startswith("-->"):
            e = f'<span class="loc">{e}</span>'
        lines.append(e)
    return "\n".join(lines)
