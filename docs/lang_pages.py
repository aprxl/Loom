# Renders the book (docs/lang/*.html), the diagnostic codes page, the landing
# page (docs/index.html) and the playground (docs/playground.html).
#
# The prose is hand-kept in docs/lang/pages/NN-slug.md, in a tiny markup this
# file parses (see PAGE FORMAT below). Every Loom code sample lives in
# docs/lang/samples/ and is run by `python docs/gen.py --check`; a page can only
# show Loom code by naming a sample, so nothing on these pages is untested. In
# the browser each sample is an editor you can run (play/play.js).
#
# PAGE FORMAT
#   title: Page title              header lines, then a line with only ---
#   summary: One sentence.
#   kind: chapter                  chapter (numbered; the default), preface or appendix
#   ---
#   ## Heading / ### Heading       h2 / h3 (both get anchors; an h2 is a bead on the thread)
#   Paragraph text with `code`, **bold**, *italics* and [links](page:slug).
#   - bullet                       (an indented line continues it)
#   1. numbered item
#   | a | b |                      a table; the second line is |---|---|
#   > A note, shown in a box.
#   @try Change this and run it.   a "try it" box (continues to the next blank line)
#   @sample name                   a sample from docs/lang/samples, with its output
#   @compare                       an "Elsewhere" box: how other languages say it,
#   -- Rust                          one tab per `-- Language` line, each with prose
#   Prose about Rust.                and ```lang fences (not Loom: those are samples)
#   ```rust
#   fn main() {}
#   ```
#   @end
#   ```text ... ```                a block that isn't Loom (commands, layouts)
#
# Link targets: page:slug, page:slug#anchor, std:module, std:module#Item,
# diag:K0101, play: (the playground), or an ordinary relative or absolute URL.
import json
import os
import re

import samples
from common import (HERE, Links, attr, esc, highlight, highlight_foreign, highlight_output, inline, page,
                    sidebar)

PAGES_DIR = os.path.join(HERE, "lang", "pages")
CODES_RS = os.path.join(os.path.dirname(HERE), "crates", "loom_syntax", "src", "diag", "codes.rs")
SKIP_TXT = os.path.join(HERE, "play", "skip.txt")

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

# What the playground does with a sample of each mode.
PLAY_MODE = {"run": "run", "unravel": "run", "error": "run", "check": "check",
             "check-fail": "check", "test": "test", "test-fail": "test"}

# Samples shown off the book's pages: the landing page's and the playground's.
LANDING_SAMPLE = "landing_hello"
PLAYGROUND_SAMPLE = "playground_start"


class PageError(Exception):
    pass


def play_skipped():
    """Samples the playground can't run (docs/play/skip.txt)."""
    if not os.path.exists(SKIP_TXT):
        return set()
    with open(SKIP_TXT, encoding="utf-8") as f:
        return {ln.split()[0] for ln in f if ln.strip() and not ln.startswith("#")}


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
        kind = meta.get("kind", "chapter")
        if kind not in ("chapter", "preface", "appendix"):
            raise PageError(f"{fname}: kind must be chapter, preface or appendix")
        pages.append({"slug": m.group(2), "file": fname, "title": meta["title"], "kind": kind,
                      "summary": meta["summary"], "body": body.strip("\n")})
    chapter, appendix = 0, 0
    for p in pages:
        if p["kind"] == "chapter":
            chapter += 1
            p["number"] = str(chapter)
            p["label"] = f"Chapter {chapter}"
        elif p["kind"] == "appendix":
            p["number"] = chr(ord("A") + appendix)
            p["label"] = f"Appendix {p['number']}"
            appendix += 1
        else:
            p["number"] = ""
            p["label"] = "Before we start"
    return pages


BLOCK_START = re.compile(r"(```|@sample |@try |@compare\s*$|#{2,3} |- |\d+\. |\||> )")


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
        elif ln.startswith("@try "):
            para = [ln[len("@try "):].strip()]
            i += 1
            while i < len(lines) and lines[i].strip() and not BLOCK_START.match(lines[i]):
                para.append(lines[i].strip())
                i += 1
            blocks.append(("try", " ".join(para)))
        elif ln.strip() == "@compare":
            j = i + 1
            while j < len(lines) and lines[j].strip() != "@end":
                j += 1
            if j == len(lines):
                raise PageError(f"{fname}: @compare without @end")
            blocks.append(("compare", parse_compare(fname, lines[i + 1:j])))
            i = j + 1
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
            while i < len(lines) and lines[i].strip() and not BLOCK_START.match(lines[i]):
                para.append(lines[i].strip())
                i += 1
            blocks.append(("p", " ".join(para)))
    return blocks


def parse_compare(fname, lines):
    """An @compare body -> [(label, [("p", text) | ("code", lang, text)])]."""
    tabs = []
    i = 0
    while i < len(lines):
        ln = lines[i]
        if ln.startswith("-- "):
            tabs.append((ln[3:].strip(), []))
            i += 1
        elif not ln.strip():
            i += 1
        elif not tabs:
            raise PageError(f"{fname}: @compare must start with a `-- Language` line")
        elif ln.startswith("```"):
            lang = ln[3:].strip()
            if not lang or lang == "loom":
                raise PageError(f"{fname}: a fence in @compare names its (non-Loom) language")
            j = i + 1
            while j < len(lines) and not lines[j].startswith("```"):
                j += 1
            tabs[-1][1].append(("code", lang, "\n".join(lines[i + 1:j])))
            i = j + 1
        else:
            para = []
            while i < len(lines) and lines[i].strip() and not lines[i].startswith(("```", "-- ")):
                para.append(lines[i].strip())
                i += 1
            tabs[-1][1].append(("p", " ".join(para)))
    if not tabs:
        raise PageError(f"{fname}: an empty @compare")
    return tabs


# ---------------------------------------------------------------- rendering

def _italics(html):
    """*italics* in already-rendered inline HTML (outside tags and code)."""
    parts = re.split(r"(<code>.*?</code>|<[^>]+>)", html)
    for k in range(0, len(parts), 2):
        parts[k] = re.sub(r"(?<![\w*])\*([^*\s][^*]*?)\*(?![\w*])", r"<em>\1</em>", parts[k])
    return "".join(parts)


class Site:
    def __init__(self, pages, std_modules):
        self.pages = pages
        self.links = Links([p["slug"] for p in pages] + ["diagnostics"], [m["name"] for m in std_modules])
        self.sample_page = {}     # sample name -> page slug
        self.skip = play_skipped()

    # -- blocks
    def inl(self, text, here="lang"):
        return _italics(inline(text, self.links, here))

    def render_sample(self, name, slug, here="lang"):
        if name not in samples.sample_names():
            raise PageError(f"{slug}: no sample {name!r}")
        if name in self.sample_page:
            raise PageError(f"sample {name!r} is used by both {self.sample_page[name]} and {slug}")
        self.sample_page[name] = slug
        mode, codes = samples.directive(name)
        tag, tag_cls = MODE_TAG[mode]
        diag_href = "diagnostics.html" if here == "lang" else "lang/diagnostics.html"
        if codes:
            tag += ": " + ", ".join(f'<a href="{diag_href}#{c}">{c}</a>' for c in codes)
        runnable = name not in self.skip
        data = (f' data-sample="{attr(name)}" data-mode="{PLAY_MODE[mode]}" data-file="{attr(name)}.loom"'
                + (" data-runnable" if runnable else ""))
        note = "" if runnable else '<span class="sample-static-note">checked output: this one needs a real machine</span>'
        parts = [
            f'<div class="sample" id="sample-{name}"{data}>',
            f'<div class="sample-head"><span class="file"><span class="dot"></span>{esc(name)}.loom</span>'
            f'<span class="tag {tag_cls}">{tag}</span>{note}<span class="spacer"></span></div>',
            f'<pre class="code"><code>{highlight(samples.shown_source(name))}</code></pre>',
        ]
        out = samples.shown_out(name)
        if out is not None:
            mark = samples.STDERR_MARK.strip()
            if mark in out:
                stdout, _, stderr = out.partition(mark + "\n")
                sections = [("Output", stdout.rstrip("\n"), False), ("Error output", stderr.rstrip("\n"), True)]
            elif mode in ("check-fail", "unravel", "error"):
                sections = [("The compiler says" if mode == "check-fail" else "Error output", out, True)]
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

    def render_compare(self, tabs):
        parts = ['<div class="compare">',
                 '<div class="compare-head"><span class="compare-label">Elsewhere</span>'
                 '<div class="tabs" role="tablist" aria-label="Other languages"></div></div>']
        for label, items in tabs:
            key = slugify(label)
            parts.append(f'<div class="tab-panel" role="tabpanel" data-lang="{attr(key)}" data-label="{attr(label)}">')
            parts.append(f"<h4>{esc(label)}</h4>")
            for item in items:
                if item[0] == "p":
                    parts.append(f"<p>{self.inl(item[1])}</p>")
                else:
                    _, lang, code = item
                    parts.append(f'<pre class="foreign" data-lang="{attr(lang)}"><code>{highlight_foreign(code, lang)}</code></pre>')
            parts.append("</div>")
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
                tag = kind
                out.append(f'<{tag} id="{sid}">{self.inl(payload)}<a class="anchor" href="#{sid}" aria-label="Link to this section">#</a></{tag}>')
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
                out.append(f'<div class="callout"><p>{self.inl(payload)}</p></div>')
            elif kind == "try":
                out.append(f'<div class="try"><p>{self.inl(payload)}</p></div>')
            elif kind == "pre":
                out.append(f'<pre class="plain"><code>{esc(payload)}</code></pre>')
            elif kind == "sample":
                out.append(self.render_sample(payload, slug))
            elif kind == "compare":
                out.append(self.render_compare(payload))
        return "\n".join(out)

    # -- pages
    def sidebar_for(self, current, here="lang"):
        prefix = "" if here == "lang" else "lang/"
        book = [(inline(p["title"]), f'{prefix}{p["slug"]}.html', p["slug"] == current, (p["number"], p["slug"]))
                for p in self.pages]
        more = [("Diagnostic codes", f"{prefix}diagnostics.html", current == "diagnostics", None),
                ("Playground", "../playground.html" if here == "lang" else "playground.html", current == "playground", None),
                ("Standard library", "../std/index.html" if here == "lang" else "std/index.html", False, None)]
        return sidebar([("The book", book, "beads"), ("Also", more, "plain")])

    def render_page(self, idx):
        p = self.pages[idx]
        blocks = parse_blocks(p["file"], p["body"])
        body_html = self.render_blocks(p, blocks)
        has_samples = any(k == "sample" for k, _ in blocks)
        body = [
            '<div class="chapter-head">',
            f'<span class="chapter-no">{esc(p["label"])}</span>',
            f'<h1>{inline(p["title"])}</h1>',
            f'<p class="summary">{self.inl(p["summary"])}</p>',
            "</div>",
            body_html,
        ]
        prev = self.pages[idx - 1] if idx > 0 else None
        nxt = self.pages[idx + 1] if idx + 1 < len(self.pages) else None
        if nxt is None:
            nxt = {"slug": "diagnostics", "title": "Diagnostic codes", "label": "Reference"}
        pager = ['<nav class="pager" aria-label="Chapters">']
        pager.append(f'<a class="prev" href="{prev["slug"]}.html"><span class="dir">&larr; {esc(prev["label"])}</span>{inline(prev["title"])}</a>'
                     if prev else "<span></span>")
        pager.append(f'<a class="next" href="{nxt["slug"]}.html"><span class="dir">{esc(nxt["label"])} &rarr;</span>{inline(nxt["title"])}</a>')
        pager.append("</nav>")
        body.append("\n".join(pager))
        title = p["title"].replace(chr(96), "")
        return page(f"{title} · Loom", "lang", self.sidebar_for(p["slug"]), "\n".join(body), current="book",
                    play=has_samples, body_attrs=f'data-chapter="{attr(p["slug"])}"')


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
        '<div class="chapter-head"><span class="chapter-no">Reference</span>',
        "<h1>Diagnostic codes</h1>",
        '<p class="summary">Every code the compiler can report, with its one-line meaning. '
        "Generated from <code>crates/loom_syntax/src/diag/codes.rs</code>.</p></div>",
        "<p>A diagnostic looks like <code>error[K0101]: const assertion failed</code>. "
        "The letter says which part of the compiler found it, and the digits are stable, "
        "so a code is safe to search for. Where a chapter shows the error happening, "
        "the code links to that sample, and you can run it there and poke at it.</p>",
        "<p>Anything the compiler doesn't do yet fails with a <code>U</code> code. "
        "It never compiles it wrongly. See "
        '<a href="not-yet.html">Not supported yet</a>.</p>',
    ]
    letters = []
    for head, _ in groups:
        if head[0] not in letters:
            letters.append(head[0])
    body.append('<div class="toc-chips">' + "".join(
        f'<a href="#group-{next(h for h, _ in groups if h[0] == l)[:5]}"><span class="kind">{l}</span>{esc(LETTERS.get(l, ""))}</a>'
        for l in letters) + "</div>")
    for head, codes in groups:
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
    reserved = [f"{l} ({LETTERS[l]})" for l in "RCI" if l not in letters]
    if reserved:
        body.append(f"<p>The letters {', '.join(reserved)} are set aside for features that aren't built yet. "
                    "No code uses them.</p>")
    html = page("Diagnostic codes · Loom", "lang", site.sidebar_for("diagnostics"), "\n".join(body),
                current="diag", main_class="")
    return html, sum(len(c) for _, c in groups)


# ---------------------------------------------------------------- landing page

# A little loom: six warp threads, and a weft that goes over and under them.
HERO_WEAVE = """<svg class="hero-weave" viewBox="0 0 560 120" role="img" aria-label="A thread woven over and under six warp threads">
<defs><linearGradient id="weft" x1="0" x2="1"><stop offset="0" stop-color="#ee7a5f"/><stop offset=".5" stop-color="#f3bd55"/><stop offset="1" stop-color="#b7c56f"/></linearGradient></defs>
<g stroke-linecap="round">
{warps}
<path class="weft-under" d="M10 60 C 60 30, 80 30, 120 60 S 180 90, 220 60 S 280 30, 320 60 S 380 90, 420 60 S 480 30, 550 60" fill="none" stroke="url(#weft)" stroke-width="7"/>
{overs}
</g>
</svg>"""


def hero_weave():
    warps, overs = [], []
    colors = ["#8fb6d8", "#ec86ad", "#79c6b0", "#a7a1ec", "#f59d57", "#b7c56f"]
    for k, x in enumerate(range(70, 560, 80)):
        c = colors[k % len(colors)]
        warps.append(f'<line x1="{x}" y1="8" x2="{x}" y2="112" stroke="{c}" stroke-width="9" opacity=".9"/>')
        # every other crossing, the warp passes over the weft: draw a short piece of it again on top
        if k % 2 == 1:
            overs.append(f'<line x1="{x}" y1="44" x2="{x}" y2="76" stroke="#1b1511" stroke-width="13"/>'
                         f'<line x1="{x}" y1="40" x2="{x}" y2="80" stroke="{c}" stroke-width="9"/>')
    return HERO_WEAVE.format(warps="\n".join(warps), overs="\n".join(overs))


def render_landing(site, nmods, ncodes):
    chapters = []
    for p in site.pages:
        num = p["number"] or "·"
        chapters.append(
            f'<a class="card" href="lang/{p["slug"]}.html"><span class="name">{esc(num)} &middot; {inline(p["title"])}</span>'
            f'<span class="desc">{site.inl(p["summary"], "")}</span></a>')
    body = f"""<section class="hero">
<span class="chapter-no">a language for weaving programs</span>
<h1>Loom</h1>
<p class="tagline">Data is the <em>warp</em>. Behavior is the <span class="w2">weft</span>. Programs are <span class="w3">woven</span>.</p>
{hero_weave()}
<p>Loom is a statically typed language with no garbage collector. You describe data with
<strong>warps</strong>, behavior with <strong>wefts</strong>, and join them with <strong>weaves</strong>.
It compiles to Treadle, its own fast virtual machine, and to Lua.</p>
<div class="cta"><a class="primary" href="lang/index.html">Start reading the book</a>
<a class="secondary" href="playground.html">Open the playground</a>
<a class="secondary" href="std/index.html">Browse the library</a></div>
</section>
<h2 id="try-it">Go on, change it</h2>
<p>Every program in these docs runs right here in your browser, on the real compiler built for
WebAssembly. Edit this one and press <strong>Run</strong> (or <code>Ctrl</code>+<code>Enter</code>).</p>
{site.render_sample(LANDING_SAMPLE, "index (landing)", here="")}
<h2 id="the-book">The book</h2>
<p>A tour of the language from the first <code>print</code> to threads and compile-time code,
one feature per section, each next to what Rust, Go, Swift, Python and friends do instead.</p>
<div class="cards two">
{chr(10).join(chapters)}
</div>
<h2 id="reference">Reference</h2>
<div class="cards two">
<a class="card" href="std/index.html"><span class="name">The standard library</span>
<span class="desc">Every <code>std</code> module, type and function, from the doc comments in <code>runtime/std</code>.</span>
<span class="count">{nmods} modules</span></a>
<a class="card" href="lang/diagnostics.html"><span class="name">Diagnostic codes</span>
<span class="desc">Every error code the compiler reports, what it means, and where a chapter shows it.</span>
<span class="count">{ncodes} codes</span></a>
</div>
<footer>Generated by <code>python docs/gen.py</code>. Everything here is static and works offline, from
<code>file://</code>; <code>loom docs</code> opens it.</footer>"""
    sb = site.sidebar_for(None, here="")
    return page("Loom", "", sb, body, current=None, play=True)


# ---------------------------------------------------------------- playground

def render_playground(site):
    """A big editor, and every sample of the book to start from."""
    menu = []
    for p in site.pages:
        names = [n for n, slug in site.sample_page.items() if slug == p["slug"] and n not in site.skip]
        if not names:
            continue
        # in the order the page shows them
        order = {n: i for i, (k, n) in enumerate(b for b in parse_blocks(p["file"], p["body"]) if b[0] == "sample")}
        names.sort(key=lambda n: order.get(n, 0))
        menu.append({"chapter": f'{p["number"] + ". " if p["number"] else ""}{p["title"].replace("`", "")}',
                     "samples": [{"name": n, "mode": PLAY_MODE[samples.mode_of(n)],
                                  "source": samples.shown_source(n)} for n in names]})
    data = json.dumps(menu, ensure_ascii=False).replace("</", "<\\/")
    sample_html = site.render_sample(PLAYGROUND_SAMPLE, "playground", here="")
    # the playground's editor keeps its own edits apart from the book's
    sample_html = sample_html.replace(' data-runnable', ' data-runnable data-store="playground"', 1)
    body = f"""<div class="chapter-head playground">
<span class="chapter-no">a scrap of cloth to practise on</span>
<h1>Playground</h1>
<p class="summary">Write any Loom program and run it, right here: the compiler and Treadle run in your browser.</p>
</div>
<div class="play-examples"><label for="play-pick">Start from a sample of the book:</label>
<select id="play-pick"><option value="">choose one…</option></select></div>
<div class="playground">
{sample_html}
</div>
<p>Programs here are sandboxed: no environment variables, files live in memory and start empty,
there's no terminal, and <code>time.sleep</code> doesn't wait. Threads run one at a time, in the
reference schedule. A run stops after ten seconds.</p>
<script type="application/json" id="play-samples">{data}</script>
<script>
document.addEventListener("DOMContentLoaded", function () {{
  var pick = document.getElementById("play-pick");
  var menu = JSON.parse(document.getElementById("play-samples").textContent);
  menu.forEach(function (ch) {{
    var group = document.createElement("optgroup");
    group.label = ch.chapter;
    ch.samples.forEach(function (s) {{
      var o = document.createElement("option");
      o.value = s.name;
      o.textContent = s.name;
      o.dataset.mode = s.mode;
      group.appendChild(o);
    }});
    pick.appendChild(group);
  }});
  pick.addEventListener("change", function () {{
    var name = pick.value;
    if (!name) return;
    var found = null;
    menu.forEach(function (ch) {{ ch.samples.forEach(function (s) {{ if (s.name === name) found = s; }}); }});
    var sample = document.querySelector(".playground .sample");
    var ta = sample && sample.querySelector("textarea");
    if (!found || !ta) return;
    sample.dataset.mode = found.mode;
    sample.dispatchEvent(new Event("loom:mode"));
    ta.value = found.source;
    ta.dispatchEvent(new Event("input"));
    ta.focus();
  }});
}});
</script>"""
    return page("Playground · Loom", "", site.sidebar_for("playground", here=""), body, current="play", play=True)


def generate(std_modules, writer):
    pages = load_pages()
    site = Site(pages, std_modules)
    for i, p in enumerate(pages):
        writer.write(f'lang/{p["slug"]}.html', site.render_page(i))
    diag_html, ncodes = render_diagnostics(site, parse_codes())
    writer.write("lang/diagnostics.html", diag_html)
    writer.write("index.html", render_landing(site, len(std_modules), ncodes))
    writer.write("playground.html", render_playground(site))
    unused = set(samples.sample_names()) - set(site.sample_page)
    if unused:
        raise PageError(f"samples no page shows: {sorted(unused)}")
    for name in site.skip:
        if name not in site.sample_page:
            raise PageError(f"docs/play/skip.txt names {name}, which no page shows")
    return site
