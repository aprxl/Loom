# Renders the language pages (docs/lang/*.html), the diagnostic codes page, and
# the landing page (docs/index.html).
#
# The prose is hand-kept in docs/lang/pages/NN-slug.md, in a tiny markup this
# file parses (see PAGE FORMAT below). Every Loom code sample lives in
# docs/lang/samples/ and is run by `python docs/gen.py --check`; a page can only
# show code by naming a sample, so nothing on these pages is untested.
#
# PAGE FORMAT
#   title: Page title              header lines, then a line with only ---
#   summary: One sentence.
#   ---
#   ## Heading / ### Heading       h2 / h3 (h2 gets an anchor)
#   Paragraph text with `code`, **bold** and [links](page:slug).
#   - bullet                       (an indented line continues it)
#   1. numbered item
#   | a | b |                      a table; the second line is |---|---|
#   > A note, shown in a box.
#   @sample name                   a sample from docs/lang/samples, with its output
#   ```text ... ```               a block that isn't Loom (commands, layouts)
#
# Link targets: page:slug, page:slug#anchor, std:module, std:module#Item,
# diag:K0101, or an ordinary relative or absolute URL.
import json
import os
import re

import samples
from common import HERE, Links, esc, highlight, highlight_output, inline, page, sidebar

PAGES_DIR = os.path.join(HERE, "lang", "pages")
CODES_RS = os.path.join(os.path.dirname(HERE), "crates", "loom_syntax", "src", "diag", "codes.rs")

MODE_TAG = {
    "run": ("runs", "ok"),
    "unravel": ("unravels", "warn"),
    "error": ("exits with an error", "warn"),
    "check": ("compiles", "ok"),
    "check-fail": ("doesn't compile", "err"),
    "test": ("tests pass", "ok"),
    "test-fail": ("tests fail", "err"),
}

OUT_HEAD = {
    "run": "Output",
    "unravel": "Output",
    "error": "Output",
    "check-fail": "The compiler says",
    "test": "Test report",
    "test-fail": "Test report",
}


class PageError(Exception):
    pass


# ---------------------------------------------------------------- parsing

def slugify(text):
    s = re.sub(r"`", "", text).lower()
    s = re.sub(r"[^a-z0-9]+", "-", s).strip("-")
    return s or "section"


def load_pages():
    pages = []
    for fname in sorted(os.listdir(PAGES_DIR)):
        m = re.match(r"(\d+)-([a-z0-9-]+)\.md$", fname)
        if not m:
            continue
        with open(os.path.join(PAGES_DIR, fname), encoding="utf-8", newline="") as f:
            text = f.read().replace("\r\n", "\n")
        head, sep, body = text.partition("\n---\n")
        if not sep:
            raise PageError(f"{fname}: no `---` line after the header")
        meta = {}
        for ln in head.split("\n"):
            k, _, v = ln.partition(":")
            meta[k.strip()] = v.strip()
        for req in ("title", "summary"):
            if req not in meta:
                raise PageError(f"{fname}: missing `{req}:`")
        pages.append({"slug": m.group(2), "file": fname, "title": meta["title"],
                      "summary": meta["summary"], "body": body.strip("\n")})
    return pages


def parse_blocks(fname, body):
    """Body text -> list of (kind, payload) blocks."""
    blocks = []
    lines = body.split("\n")
    i = 0
    while i < len(lines):
        ln = lines[i]
        if not ln.strip():
            i += 1
            continue
        if ln.startswith("```"):
            lang = ln[3:].strip()
            if lang == "loom":
                raise PageError(f"{fname}: ```loom blocks aren't allowed; write a sample and use @sample")
            j = i + 1
            while j < len(lines) and not lines[j].startswith("```"):
                j += 1
            blocks.append(("pre", "\n".join(lines[i + 1:j])))
            i = j + 1
        elif ln.startswith("@sample "):
            blocks.append(("sample", ln[len("@sample "):].strip()))
            i += 1
        elif ln.startswith("## ") or ln.startswith("### "):
            level = 2 if ln.startswith("## ") else 3
            blocks.append((f"h{level}", ln[level + 1:].strip()))
            i += 1
        elif ln.startswith("- ") or re.match(r"\d+\. ", ln):
            ordered = not ln.startswith("- ")
            items = []
            while i < len(lines) and lines[i].strip():
                cur = lines[i]
                if (cur.startswith("- ") and not ordered) or (ordered and re.match(r"\d+\. ", cur)):
                    items.append(re.sub(r"^(- |\d+\. )", "", cur))
                elif cur.startswith("  ") and items:
                    items[-1] += " " + cur.strip()
                else:
                    break
                i += 1
            blocks.append(("ol" if ordered else "ul", items))
        elif ln.startswith("|"):
            rows = []
            while i < len(lines) and lines[i].startswith("|"):
                rows.append(lines[i])
                i += 1
            cells = [[c.strip() for c in r.strip().strip("|").split(" | ")] for r in rows
                     if not re.match(r"\|[-| :]+\|$", r.strip())]
            blocks.append(("table", cells))
        elif ln.startswith("> "):
            para = []
            while i < len(lines) and lines[i].startswith("> "):
                para.append(lines[i][2:])
                i += 1
            blocks.append(("note", " ".join(para)))
        else:
            para = []
            while i < len(lines) and lines[i].strip() and not re.match(r"(```|@sample |#{2,3} |- |\d+\. |\||> )", lines[i]):
                para.append(lines[i].strip())
                i += 1
            blocks.append(("p", " ".join(para)))
    return blocks


# ---------------------------------------------------------------- rendering

class Site:
    def __init__(self, pages, std_modules):
        self.pages = pages
        self.links = Links([p["slug"] for p in pages] + ["diagnostics"], [m["name"] for m in std_modules])
        self.sample_page = {}     # sample name -> page slug
        self.errors = []

    # -- blocks
    def inl(self, text, here="lang"):
        return inline(text, self.links, here)

    def render_sample(self, name, slug):
        if name not in samples.sample_names():
            raise PageError(f"{slug}: no sample {name!r}")
        if name in self.sample_page:
            raise PageError(f"sample {name!r} is used by both {self.sample_page[name]} and {slug}")
        self.sample_page[name] = slug
        mode, codes = samples.directive(name)
        tag, tag_cls = MODE_TAG[mode]
        if codes:
            tag += ": " + ", ".join(f'<a href="diagnostics.html#{c}">{c}</a>' for c in codes)
        parts = [
            f'<div class="sample" id="sample-{name}">',
            f'<div class="sample-head"><span class="file">samples/{name}.loom</span>'
            f'<span class="tag {tag_cls}">{tag}</span></div>',
            f'<pre class="code"><code>{highlight(samples.shown_source(name))}</code></pre>',
        ]
        out = samples.shown_out(name)
        if out is not None:
            mark = samples.STDERR_MARK.strip()
            if mark in out:
                stdout, _, stderr = out.partition(mark + "\n")
                sections = [("Output", stdout.rstrip("\n"), False), ("Error output", stderr.rstrip("\n"), True)]
            elif mode in ("check-fail", "unravel", "error"):
                sections = [("The compiler says" if mode == "check-fail" else "Error output",
                             out, True)]
            else:
                sections = [(OUT_HEAD[mode], out, False)]
            for head, text, is_err in sections:
                if not text.strip() and not is_err:
                    continue
                shown = highlight_output(text) if is_err else esc(text)
                parts.append(f'<div class="sample-out"><div class="out-head">{head}</div>'
                             f'<pre><code>{shown}</code></pre></div>')
        parts.append("</div>")
        return "\n".join(parts)

    def render_blocks(self, page_info, blocks):
        slug = page_info["slug"]
        out, seen_ids = [], set()
        for kind, payload in blocks:
            if kind == "p":
                out.append(f"<p>{self.inl(payload)}</p>")
            elif kind in ("h2", "h3"):
                sid = slugify(payload)
                while sid in seen_ids:
                    sid += "-2"
                seen_ids.add(sid)
                if kind == "h2":
                    out.append(f'<h2 id="{sid}">{self.inl(payload)}</h2>')
                else:
                    out.append(f'<h3 id="{sid}">{self.inl(payload)}</h3>')
            elif kind in ("ul", "ol"):
                out.append(f"<{kind}>" + "".join(f"<li>{self.inl(it)}</li>" for it in payload) + f"</{kind}>")
            elif kind == "table":
                head, rows = payload[0], payload[1:]
                t = ['<table class="plain"><thead><tr>'
                     + "".join(f"<th>{self.inl(c)}</th>" for c in head) + "</tr></thead><tbody>"]
                for r in rows:
                    t.append("<tr>" + "".join(f"<td>{self.inl(c)}</td>" for c in r) + "</tr>")
                t.append("</tbody></table>")
                out.append("".join(t))
            elif kind == "note":
                out.append(f'<div class="callout">{self.inl(payload)}</div>')
            elif kind == "pre":
                out.append(f'<pre class="plain"><code>{esc(payload)}</code></pre>')
            elif kind == "sample":
                out.append(self.render_sample(payload, slug))
        return "\n".join(out)

    # -- pages
    def sidebar_for(self, current):
        entries = [(inline(p["title"]), f'{p["slug"]}.html', p["slug"]) for p in self.pages]
        return sidebar("lang", "lang" if current != "diagnostics" else "diag", "Language", entries, current)

    def render_page(self, idx):
        p = self.pages[idx]
        blocks = parse_blocks(p["file"], p["body"])
        body = [
            f'<div class="crumb"><a href="index.html">Language</a>{"" if p["slug"] == "index" else " / " + inline(p["title"])}</div>',
            f'<h1>{inline(p["title"])}</h1>',
            f'<p class="summary">{self.inl(p["summary"])}</p>',
            self.render_blocks(p, blocks),
        ]
        prev = self.pages[idx - 1] if idx > 0 else None
        nxt = self.pages[idx + 1] if idx + 1 < len(self.pages) else None
        if p["slug"] == "not-yet":
            nxt = {"slug": "diagnostics", "title": "Diagnostic codes"}
        pager = ['<div class="pager">']
        pager.append(f'<a href="{prev["slug"]}.html">&larr; {inline(prev["title"])}</a>' if prev else "<span></span>")
        pager.append(f'<a href="{nxt["slug"]}.html">{inline(nxt["title"])} &rarr;</a>' if nxt else "<span></span>")
        pager.append("</div>")
        body.append("\n".join(pager))
        return page(f'{p["title"].replace(chr(96), "")} · Loom', "lang", self.sidebar_for(p["slug"]), "\n".join(body))


# ---------------------------------------------------------------- diagnostics

LETTERS = {
    "S": "lexer and parser",
    "N": "names and modules",
    "T": "types",
    "M": "moves and borrows",
    "L": "`once` values",
    "R": "regions",
    "C": "concurrency",
    "G": "gauges",
    "P": "patterns",
    "K": "compile-time code",
    "U": "features not supported yet",
    "I": "interop",
}


def parse_codes():
    """[(group heading, [(code, summary)])] in the order codes.rs declares them."""
    with open(CODES_RS, encoding="utf-8") as f:
        text = f.read()
    body = text[text.index("codes! {"):text.index("#[cfg(test)]")]
    groups, current = [], None
    for ln in body.split("\n"):
        m = re.match(r"\s*// ([A-Z]\d\dxx): (.*)$", ln)
        if m:
            current = (f"{m.group(1)}: {m.group(2)}", [])
            groups.append(current)
            continue
        m = re.match(r'\s*([A-Z]\d{4}) = ("(?:[^"\\]|\\.)*");$', ln)
        if m:
            if current is None:
                raise PageError("codes.rs: a code before any group comment")
            current[1].append((m.group(1), json.loads(m.group(2))))
    return groups


def render_diagnostics(site, groups):
    where = {}      # code -> [(sample, page slug)]
    for name in samples.sample_names():
        for c in samples.expected_codes(name):
            if name in site.sample_page:
                where.setdefault(c, []).append((name, site.sample_page[name]))
    titles = {p["slug"]: p["title"].replace("`", "") for p in site.pages}
    known = {c for _, cs in groups for c, _ in cs}
    for c in where:
        if c not in known:
            raise PageError(f"a sample expects {c}, which isn't in codes.rs")

    body = [
        '<div class="crumb"><a href="index.html">Language</a> / Diagnostic codes</div>',
        "<h1>Diagnostic codes</h1>",
        '<p class="summary">Every code the compiler can report, with its one-line meaning. '
        "Generated from <code>crates/loom_syntax/src/diag/codes.rs</code>.</p>",
        "<p>A diagnostic looks like <code>error[K0101]: const assertion failed</code>. "
        "The letter says which part of the compiler found it, and the digits are stable, "
        "so a code is safe to search for. Where a page of these docs shows the error, "
        "the code links to that sample.</p>",
        "<p>Anything the compiler doesn't do yet fails with a <code>U</code> code. "
        "It never compiles it wrongly. See "
        '<a href="not-yet.html">Not supported yet</a>.</p>',
    ]
    used = []
    for head, codes in groups:
        letter = head[0]
        used.append(letter)
        body.append(f'<h2 id="group-{head[:5]}">{esc(head)}</h2>')
        body.append('<table class="plain codes"><thead><tr><th>Code</th><th>Meaning</th><th>Shown in</th></tr></thead><tbody>')
        for code, summary in codes:
            first_on_page = {}
            for name, pg in where.get(code, []):
                first_on_page.setdefault(pg, name)
            links = ", ".join(f'<a href="{pg}.html#sample-{name}">{esc(titles[pg])}</a>'
                              for pg, name in first_on_page.items())
            body.append(f'<tr id="{code}"><td><code>{code}</code></td><td>{inline(summary)}</td><td>{links}</td></tr>')
        body.append("</tbody></table>")
    reserved = [f"{l} ({LETTERS[l]})" for l in "RCI" if l not in used]
    if reserved:
        body.append(f"<p>The letters {', '.join(reserved)} are set aside for features that aren't built yet. "
                    "No code uses them.</p>")
    return page("Diagnostic codes · Loom", "lang", site.sidebar_for("diagnostics"), "\n".join(body)), sum(len(c) for _, c in groups)


# ---------------------------------------------------------------- landing page

LANDING = """<h1>Loom</h1>
<p class="summary">Data is the warp. Behavior is the weft. Programs are woven.</p>
<p>Loom is a statically typed language with no garbage collector, built around one idea:
data and behavior are separate threads, and you weave them together. Today a Rust
compiler turns Loom into Lua 5.4 and runs it. These pages describe what works now.</p>
<div class="cards two">
<a class="card" href="lang/index.html"><span class="name">Language</span>
<span class="desc">What the compiler runs today, with samples that are run against it: types, errors, ownership, shuttles, and compile-time code. Also what doesn't work yet.</span>
<span class="count">{npages} pages</span></a>
<a class="card" href="std/index.html"><span class="name">Standard library</span>
<span class="desc">Every <code>std</code> module and item, copied from the doc comments in <code>runtime/std</code>.</span>
<span class="count">{nmods} modules</span></a>
<a class="card" href="lang/diagnostics.html"><span class="name">Diagnostic codes</span>
<span class="desc">Every error code the compiler reports, in one table, with what it means.</span>
<span class="count">{ncodes} codes</span></a>
<a class="card" href="lang/not-yet.html"><span class="name">Not supported yet</span>
<span class="desc">Reflection, threads, spools, <code>given</code>, <code>unsafe</code>, and the rest of the list.</span>
<span class="count">plainly listed</span></a>
</div>
<footer>Generated by <code>python docs/gen.py</code>. Everything here is static HTML with relative links: it works offline, and from <code>file://</code>.</footer>"""


def render_landing(npages, nmods, ncodes):
    sb = sidebar("", None, None, None)
    return page("Loom docs", "", sb, LANDING.format(npages=npages, nmods=nmods, ncodes=ncodes))


def generate(std_modules, writer):
    pages = load_pages()
    site = Site(pages, std_modules)
    for i, p in enumerate(pages):
        writer.write(f'lang/{p["slug"]}.html', site.render_page(i))
    unused = set(samples.sample_names()) - set(site.sample_page)
    if unused:
        raise PageError(f"samples no page shows: {sorted(unused)}")
    diag_html, ncodes = render_diagnostics(site, parse_codes())
    writer.write("lang/diagnostics.html", diag_html)
    writer.write("index.html", render_landing(len(pages), len(std_modules), ncodes))
    return site
