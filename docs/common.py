# Shared pieces of the docs generator: escaping, inline markup, the page shell
# (top bar, sidebar, the thread's container), highlighters for Loom and for
# the other languages the book compares it with, and the file writer that
# `--check` uses to spot pages that are out of date.
#
# Every link is relative (a page knows how deep it sits: `depth`), so the site
# works from file:// and from any web root. The pages read fine without
# JavaScript; weave.js draws the thread and play/play.js makes samples run.
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


# The spool in the top bar: the thread the page hangs from starts here.
SPOOL = """<svg class="spool" viewBox="0 0 32 32" aria-hidden="true">
<rect x="6" y="3" width="20" height="5" rx="2.5" fill="#8a6a4f"/>
<rect x="6" y="24" width="20" height="5" rx="2.5" fill="#8a6a4f"/>
<rect x="9" y="8" width="14" height="16" fill="#ee7a5f"/>
<path d="M9 11h14M9 14h14M9 17h14M9 20h14" stroke="#c4533d" stroke-width="1.2"/>
<path d="M23 15c4 1 5 5 3 9" fill="none" stroke="#ee7a5f" stroke-width="1.6" stroke-linecap="round"/>
</svg>"""

TOP_LINKS = (
    ("book", "The book", "lang/index.html", ""),
    ("std", "Library", "std/index.html", ""),
    ("diag", "Codes", "lang/diagnostics.html", "wide"),
    ("play", "Playground", "playground.html", "play-link"),
)


def topbar(here_dir, current):
    root = rel_root(here_dir)
    links = []
    for key, label, href, cls in TOP_LINKS:
        classes = " ".join(c for c in (cls, "current" if key == current else "") if c)
        attr_cls = f' class="{classes}"' if classes else ""
        links.append(f'<a{attr_cls} href="{root}{href}">{label}</a>')
    return (
        '<header class="topbar">'
        '<button class="menu-toggle" type="button" aria-expanded="false" aria-label="Chapters">&#9776;</button>'
        f'<a class="brand" href="{root}index.html">{SPOOL}Loom <span class="docs-word">docs</span></a>'
        f'<nav class="topnav" aria-label="Sections">{"".join(links)}</nav>'
        "</header>"
    )


def sidebar(groups):
    """The left column: `groups` is [(heading, entries, style)] with entries
    [(label, href, current, extra)]; `style` is "beads" (the book's chapters
    on a thread; `extra` is the chapter's number and slug) or "plain"/"mono"."""
    parts = ['<nav class="sidebar" aria-label="Contents">']
    for heading, entries, style in groups:
        parts.append('<div class="group">')
        if heading:
            parts.append(f"<h3>{esc(heading)}</h3>")
        cls = "beads" if style == "beads" else ("plain-list mono" if style == "mono" else "plain-list")
        parts.append(f'<ul class="{cls}">')
        for label, href, current, extra in entries:
            classes = ' class="current"' if current else ""
            aria = ' aria-current="page"' if current else ""
            if style == "beads":
                num, slug = extra
                num_html = f'<span class="num">{esc(num)}</span>' if num else '<span class="num"></span>'
                parts.append(f'<li><a{classes}{aria} href="{href}" data-chapter="{attr(slug)}">{num_html}<span>{label}</span></a></li>')
            else:
                parts.append(f'<li><a{classes}{aria} href="{href}">{label}</a></li>')
        parts.append("</ul></div>")
    parts.append("</nav>")
    return "\n".join(parts)


FONTS = ("https://fonts.googleapis.com/css2?"
         "family=Alegreya:ital,wght@0,400..800;1,400..800"
         "&family=Caveat:wght@500..700"
         "&family=Fraunces:ital,opsz,wght,SOFT,WONK@0,9..144,300..800,0..100,0..1;1,9..144,300..800,0..100,0..1"
         "&family=Recursive:slnt,wght,CASL,CRSV,MONO@-15..0,300..800,0..1,0..1,0..1"
         "&display=swap")

PAGE = """<!DOCTYPE html>
<html lang="en" class="no-js">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<script>document.documentElement.classList.remove("no-js")</script>
<title>{title}</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="{fonts}">
<link rel="stylesheet" href="{root}style.css">
<script src="{root}weave.js" defer></script>{scripts}
</head>
<body{body_attrs}>
{topbar}
<div class="layout">
{sidebar}
<main class="weave{main_class}">
{body}
</main>
</div>
</body>
</html>
"""


def page(title, here_dir, sidebar_html, body, current=None, play=False, body_attrs="", main_class=""):
    """A whole page. `current` is the top bar's section (book, std, diag,
    play); `play` loads the playground, for pages with samples."""
    root = rel_root(here_dir)
    scripts = f'\n<script src="{root}play/play.js" defer></script>' if play else ""
    return PAGE.format(title=esc(title), root=root, fonts=attr(FONTS), sidebar=sidebar_html, body=body,
                       topbar=topbar(here_dir, current), scripts=scripts,
                       body_attrs=(" " + body_attrs) if body_attrs else "",
                       main_class=(" " + main_class) if main_class else "")


# ------------------------------------------------------------------ highlighting

KEYWORDS = set("""fun let var const type warp weft weave into through case pub if else match for in
while loop break continue return fail is not and or mend defer ref spool given loom thread yield pull
unsafe extern use as any self Self mut take once test where true false some none ok err""".split())

NUMBER = re.compile(r"0x[0-9A-Fa-f_?a-z]+|0b[01_?a-z]+|0o[0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][-+]?\d+)?[a-z]\w*|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][-+]?\d+)?")
IDENT = re.compile(r"[A-Za-z_]\w*")
CHAR = re.compile(r"'(?:\\.[^']*|[^'\\])'")
ATTR = re.compile(r"#\w+")


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
            nxt = code[m.end()] if m.end() < n else ""
            if w in KEYWORDS and prev != ".":
                segs.append(("k", w))
            elif w[0].isupper() and any(ch.islower() for ch in w):
                segs.append(("t", w))
            elif nxt == "(":
                segs.append(("f", w))
            else:
                segs.append(("", w))
            i = m.end()
        elif c == "#" and (m := ATTR.match(code, i)):
            segs.append(("a", m.group(0)))
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
            segs.append(("s", "".join(buf)))
            segs.append(("h", "{"))
            buf = []
            inner, i = _tokens(code, i + 1, True)
            segs.extend(inner)
            if i < n and code[i] == "}":
                segs.append(("h", "}"))
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
        if not text:
            continue
        t = esc(text)
        out.append(f'<span class="{cls}">{t}</span>' if cls else t)
    return "".join(out)


def highlight_output(text):
    """Compiler output, as play/play.js colours it: `error[CODE]` and
    `unravel:` lines, `-->` locations, and a snippet's gutter and carets."""
    lines = []
    for ln in text.split("\n"):
        e = esc(ln)
        gut = re.match(r"(\s*\d*\s*\|)(.*)$", ln)
        if re.match(r"(error|warning)(\[[A-Z]\d{4}\])?:", ln) or ln.startswith("unravel:"):
            e = f'<span class="{"warn" if ln.startswith("warning") else "err"}">{e}</span>'
        elif ln.lstrip().startswith("-->") or re.match(r"\s*= (note|help)", ln):
            e = f'<span class="loc">{e}</span>'
        elif gut:
            rest = gut.group(2)
            cls = "caret" if re.match(r"\s*[\^~-]+", rest) else None
            e = f'<span class="gut">{esc(gut.group(1))}</span>' + (f'<span class="{cls}">{esc(rest)}</span>' if cls else esc(rest))
        lines.append(e)
    return "\n".join(lines)


# ------------------------------------------------------------------ other languages

# Just enough to colour the short snippets in "Elsewhere" boxes: keywords,
# strings, comments, numbers and capitalised type names.
FOREIGN = {
    "rust": ("//", "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while dyn Box Some None Ok Err async await"),
    "go": ("//", "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var nil true false"),
    "swift": ("//", "as break case catch class continue default defer do else enum extension false for func guard if import in init inout let mutating nil protocol return self Self some any static struct switch throw throws true try var where while"),
    "kotlin": ("//", "as break class continue data do else enum false for fun if in interface is null object open override package return sealed super this throw true try typealias val var when while"),
    "python": ("#", "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield match case"),
    "typescript": ("//", "as async await break case catch class const continue default delete do else enum export extends false finally for function if implements import in instanceof interface let new null of return switch this throw true try type typeof undefined var void while yield readonly"),
    "c": ("//", "auto break case char const continue default do double else enum extern float for goto if inline int long register return short signed sizeof static struct switch typedef union unsigned void volatile while NULL"),
    "cpp": ("//", "auto bool break case catch char class const constexpr continue default delete do double else enum explicit false float for if inline int long namespace new nullptr operator private protected public return short static struct switch template this throw true try typename using virtual void while"),
    "java": ("//", "abstract boolean break byte case catch char class continue default do double else enum extends final finally float for if implements import instanceof int interface long new null package private protected public record return sealed short static super switch this throw throws true false try var void while yield"),
    "haskell": ("--", "case class data deriving do else if import in instance let module newtype of then type where"),
    "ocaml": ("(*", "and as begin do done else end exception false fun function if in let match module mutable of open rec sig struct then to true try type val when while with"),
    "lua": ("--", "and break do else elseif end false for function goto if in local nil not or repeat return then true until while"),
    "zig": ("//", "const var fn pub return if else while for break continue struct enum union error try catch defer errdefer comptime orelse undefined null true false"),
    "csharp": ("//", "abstract as bool break case catch class const continue default do double else enum false finally for foreach if in int interface is namespace new null out override private public readonly record ref return sealed static string struct switch this throw true try using var void while yield"),
    "elixir": ("#", "def defp defmodule do end fn if else case cond when true false nil with"),
}
FOREIGN_ALIASES = {"ts": "typescript", "js": "typescript", "javascript": "typescript", "py": "python",
                   "c++": "cpp", "cs": "csharp", "c#": "csharp", "hs": "haskell", "ml": "ocaml", "rs": "rust"}
_FOREIGN_TOKEN = re.compile(
    r'(?P<s>"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'|`[^`]*`)'
    r"|(?P<n>\b\d[\d_]*(?:\.\d+)?\b)"
    r"|(?P<w>[A-Za-z_][A-Za-z0-9_]*[!?]?)"
)
# In Rust, `'a` is a lifetime: only a one-character literal is quoted.
_RUST_TOKEN = re.compile(
    r'(?P<s>"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\')'
    r"|(?P<n>\b\d[\d_]*(?:\.\d+)?\b)"
    r"|(?P<w>[A-Za-z_][A-Za-z0-9_]*[!?]?)"
)


def highlight_foreign(code, lang):
    """A snippet in another language, coloured as well as a page needs."""
    lang = FOREIGN_ALIASES.get(lang.lower(), lang.lower())
    comment, words = FOREIGN.get(lang, ("//", ""))
    keywords = set(words.split())
    out = []
    for line in code.split("\n"):
        body, rest = line, ""
        k = _comment_start(line, comment, '"' if lang == "rust" else "\"'`")
        if k >= 0:
            body, rest = line[:k], line[k:]
        pos, parts = 0, []
        for m in (_RUST_TOKEN if lang == "rust" else _FOREIGN_TOKEN).finditer(body):
            parts.append(esc(body[pos:m.start()]))
            text = m.group(0)
            if m.group("s"):
                parts.append(f'<span class="s">{esc(text)}</span>')
            elif m.group("n"):
                parts.append(f'<span class="n">{esc(text)}</span>')
            elif text in keywords:
                parts.append(f'<span class="k">{esc(text)}</span>')
            elif text[0].isupper() and any(ch.islower() for ch in text):
                parts.append(f'<span class="t">{esc(text)}</span>')
            elif body[m.end():m.end() + 1] == "(":
                parts.append(f'<span class="f">{esc(text)}</span>')
            else:
                parts.append(esc(text))
            pos = m.end()
        parts.append(esc(body[pos:]))
        if rest:
            parts.append(f'<span class="c">{esc(rest)}</span>')
        out.append("".join(parts))
    return "\n".join(out)


def _comment_start(line, marker, quotes):
    """Where a line comment starts, outside strings, or -1."""
    quote = None
    i = 0
    while i < len(line):
        c = line[i]
        if quote:
            if c == "\\":
                i += 2
                continue
            if c == quote:
                quote = None
        elif c in quotes:
            quote = c
        elif line.startswith(marker, i):
            return i
        i += 1
    return -1
