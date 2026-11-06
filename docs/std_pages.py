# Renders the standard library pages (docs/std/index.html and
# docs/std/modules/*.html) straight from runtime/std/*.loom (std_source.py):
# every `pub` item with its doc comment. An item without one is marked TODO
# rather than described here; the fix for a thin page is a doc comment in the
# `.loom` file.
#
# Links are made for you: a `code` span in a doc comment that names a std
# type (`List`, `Stream[T]`), a method (`List.push`, or `push` on List's own
# page), a module (`std.iter`) or a function of the same module becomes a
# link, and so does every std type in a signature. Explicit links work too:
# [text](std:module#Item), [text](page:slug), [text](diag:K0101).
#
# The HTML files are generated, never hand-edited: `python docs/gen.py`.
import json
import os
import re

from common import Links, attr, esc, highlight, page, sidebar
from std_source import listed_methods

# Where the book teaches what a module holds.
MODULE_CHAPTER = {
    "iter": "collections", "list": "collections", "slice": "collections", "map": "collections",
    "set": "collections", "deque": "collections", "priority": "collections", "bitset": "collections",
    "option": "errors", "error": "errors",
    "fmt": "types", "ops": "types", "cmp": "types", "hash": "types", "default": "types",
    "num": "values", "string": "values", "math": "values",
    "mem": "memory", "meta": "reflection", "sync": "threads", "target": "const-params",
    "io": "start", "env": "start", "process": "start",
}

# The index groups modules by what they're for.
GROUPS = [
    ("The basics", "Always nearby: the prelude, the core wefts, numbers, text, optionals and errors.",
     ["prelude", "ops", "cmp", "default", "fmt", "hash", "num", "string", "option", "error"]),
    ("Collections and streams", "Ways to hold many things, and lazy pipelines over them.",
     ["list", "slice", "map", "set", "deque", "priority", "bitset", "iter"]),
    ("Memory, types and threads", "Spools and boxes, reflection, and what threads share.",
     ["mem", "meta", "sync", "target"]),
    ("The machine", "Output and input, files, the environment, time, randomness and the terminal.",
     ["io", "fs", "path", "env", "process", "time", "rand", "term", "math"]),
]

KEYWORDS = {"fun", "mut", "take", "self", "Self", "const", "type", "where", "once", "any", "ref", "given",
            "warp", "weft", "pub", "case", "never"}


def slug(name):
    return re.sub(r"[^A-Za-z0-9_.-]", "_", name)


def first_sentence(text):
    m = re.match(r"(.+?[.!?])(\s|$)", text or "")
    return m.group(1) if m else (text or "")


class StdLinks:
    """Finds what a `code` span or a signature's type names refer to."""

    def __init__(self, modules, book):
        self.modules = {m["name"] for m in modules}
        self.types = {}       # type name -> module
        self.members = {}     # (type, member) -> module
        self.functions = {}   # (module, fn) -> True
        self.consts = {}      # (module, const) -> True
        self.book = book      # common.Links, for page: and diag: targets
        for m in modules:
            for t in m["types"]:
                self.types.setdefault(t.name, m["name"])
                for x in listed_methods(t):
                    self.members[(t.name, x.name)] = m["name"]
            for f in m["functions"]:
                self.functions[(m["name"], f.name)] = True
            for c in m["consts"]:
                self.consts[(m["name"], c.name)] = True

    def href(self, module, anchor, here_dir):
        base = "modules/" if here_dir == "std" else ""
        return f"{base}{module}.html" + (f"#{slug(anchor)}" if anchor else "")

    def code(self, text, ctx):
        """The link for a `code` span, or None. `ctx` is (module, type or None, here_dir)."""
        module, typ, here = ctx
        t = text.strip()
        # std.module, std.module.item
        if m := re.fullmatch(r"std\.(\w+)(?:\.(\w+))?", t):
            mod, item = m.group(1), m.group(2)
            if mod in self.modules:
                if item is None:
                    return self.href(mod, None, here)
                if self.types.get(item) == mod or (mod, item) in self.functions or (mod, item) in self.consts:
                    return self.href(mod, item, here)
            return None
        # Type, Type[...]
        if m := re.fullmatch(r"([A-Z]\w*)(\[[^\]]*\])?", t):
            name = m.group(1)
            if name in self.types and name != typ:
                return self.href(self.types[name], name, here)
            return None
        # Type.member, Type.member(...)
        if m := re.fullmatch(r"([A-Z]\w*)\.(\w+)(\(.*\))?", t):
            key = (m.group(1), m.group(2))
            if key in self.members:
                return self.href(self.members[key], f"{key[0]}.{key[1]}", here)
            return None
        # member or member(...) of the type whose page this is, or a function of this module
        if m := re.fullmatch(r"\.?([a-z_]\w*)(\(.*\))?", t):
            name = m.group(1)
            if typ and (typ, name) in self.members:
                return self.href(self.members[(typ, name)], f"{typ}.{name}", here)
            if (module, name) in self.functions:
                return self.href(module, name, here)
        return None

    def sig(self, text, ctx, strong=None):
        """A signature, escaped, with keywords marked, std types linked and
        the item's own name (`strong`) in bold."""
        out, pos = [], 0
        for m in re.finditer(r"[A-Za-z_]\w*", text):
            out.append(esc(text[pos:m.start()]))
            w = m.group(0)
            prev = text[m.start() - 1] if m.start() else ""
            if w == strong and not out_has_strong(out):
                out.append(f"<strong>{esc(w)}</strong>")
            elif w in KEYWORDS and prev != ".":
                out.append(f'<span class="kw">{w}</span>')
            elif w[0].isupper() and w in self.types and w != ctx[1]:
                out.append(f'<a href="{self.href(self.types[w], w, ctx[2])}">{esc(w)}</a>')
            elif w[0].isupper() and any(c.islower() for c in w):
                out.append(f'<span class="ty">{esc(w)}</span>')
            else:
                out.append(esc(w))
            pos = m.end()
        out.append(esc(text[pos:]))
        return "".join(out)


def out_has_strong(parts):
    return any(p.startswith("<strong>") for p in parts)


_INLINE = re.compile(r"\[((?:[^\]`]|`[^`]*`)+)\]\(([^)\s]+)\)|`([^`]+)`")


def doc_inline(text, links, ctx):
    """Doc text -> HTML: `code` (linked where it names something), [label](target), **bold**."""
    out, pos = [], 0
    plain = lambda s: re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", esc(s))
    for m in _INLINE.finditer(text):
        out.append(plain(text[pos:m.start()]))
        if m.group(3) is not None:
            code = m.group(3)
            href = links.code(code, ctx)
            html = f"<code>{esc(code)}</code>"
            out.append(f'<a class="code-link" href="{attr(href)}">{html}</a>' if href else html)
        else:
            target = m.group(2)
            here = ctx[2]
            href = links.book.resolve(target, here)
            out.append(f'<a href="{attr(href)}">{doc_inline(m.group(1), links, ctx)}</a>')
        pos = m.end()
    out.append(plain(text[pos:]))
    return "".join(out)


def doc_html(item, links, ctx, cls="item-doc", empty="No description in the source yet."):
    if not item.doc:
        return f'<div class="{cls} empty"><p>{empty}</p></div>'
    parts = [f'<div class="{cls}">']
    for kind, text in item.doc:
        if kind == "p":
            parts.append(f"<p>{doc_inline(text, links, ctx)}</p>")
        else:
            parts.append(f'<pre><code>{highlight(text)}</code></pre>')
    parts.append("</div>")
    return "".join(parts)


def badges(item):
    out = []
    if item.builtin:
        out.append('<span class="badge builtin" title="Implemented by the compiler or the runtime, not in Loom">builtin</span>')
    if item.extern:
        out.append('<span class="badge extern" title="Declared extern &quot;host&quot;: it calls straight into the host">host</span>')
    if not item.doc:
        out.append('<span class="badge todo" title="No doc comment in the source yet">todo</span>')
    return "".join(out)


# ---------------------------------------------------------------- items

def render_type(t, links, module):
    ctx = (module, t.name, "std/modules")
    tid = slug(t.name)
    parts = [
        f'<section class="item" id="{tid}">',
        '<div class="item-head">',
        f'<span class="sig"><span class="kw">{esc(t.kind)}</span> <strong>{esc(t.name)}</strong>'
        f'{links.sig(t.head, ctx)}</span>',
        badges(t),
        f'<a class="item-anchor" href="#{tid}" aria-label="Link to {attr(t.name)}">#</a>',
        "</div>",
        doc_html(t, links, ctx),
    ]
    fields = [m for m in t.members if m.kind == "field" and m.is_pub]
    cases = [m for m in t.members if m.kind == "case"]
    for label, group, cls in (("Fields", fields, "fields"), ("Cases", cases, "cases")):
        if not group:
            continue
        parts.append(f'<div class="parts"><span class="parts-label">{label}</span><ul class="{cls}">')
        for m in group:
            text = m.head if m.kind == "field" else f"case {m.head}"
            tip = f' title="{attr(m.text)}"' if m.doc else ""
            parts.append(f'<li{tip}><span class="sig">{links.sig(text, ctx)}</span>'
                         + (f'<span class="part-doc">{doc_inline(m.text, links, ctx)}</span>' if m.doc else "")
                         + "</li>")
        parts.append("</ul></div>")
    methods = listed_methods(t)
    if methods:
        parts.append('<div class="members">')
        for m in methods:
            mid = f"{tid}.{slug(m.name)}"
            parts.append(f'<div class="member" id="{mid}">')
            parts.append('<div class="member-head">')
            parts.append(f'<span class="sig">{links.sig(m.head, ctx, strong=m.name)}</span>')
            parts.append(badges(m))
            parts.append(f'<a class="item-anchor" href="#{mid}" aria-label="Link to {attr(t.name)}.{attr(m.name)}">#</a>')
            parts.append("</div>")
            parts.append(doc_html(m, links, ctx, cls="member-doc", empty="No description yet."))
            parts.append("</div>")
        parts.append("</div>")
    parts.append("</section>")
    return "\n".join(parts)


def render_fn(f, links, module):
    ctx = (module, None, "std/modules")
    fid = slug(f.name)
    return "\n".join([
        f'<section class="item" id="{fid}">',
        '<div class="item-head">',
        f'<span class="sig"><span class="kw">fun</span> <strong>{esc(f.name)}</strong>{links.sig(f.head, ctx)}</span>',
        badges(f),
        f'<a class="item-anchor" href="#{fid}" aria-label="Link to {attr(f.name)}">#</a>',
        "</div>",
        doc_html(f, links, ctx),
        "</section>",
    ])


def render_const(c, links, module):
    ctx = (module, None, "std/modules")
    cid = slug(c.name)
    return "\n".join([
        f'<section class="item" id="{cid}">',
        '<div class="item-head">',
        f'<span class="sig"><span class="kw">const</span> <strong>{esc(c.name)}</strong>{links.sig(c.head, ctx)}</span>',
        badges(c),
        "</div>",
        doc_html(c, links, ctx),
        "</section>",
    ])


def render_reexports(reexports, links):
    parts = ['<ul class="reexports">']
    for r in reexports:
        parts.append(
            f'<li><a href="{r["from"]}.html#{slug(r["name"])}"><code>{esc(r["name"])}</code></a>'
            f'<span class="from">from <a href="{r["from"]}.html">std.{esc(r["from"])}</a></span></li>'
        )
    parts.append("</ul>")
    return "\n".join(parts)


def module_count(mod):
    return len(mod["reexports"]) + len(mod["types"]) + len(mod["functions"]) + len(mod["consts"])


def std_sidebar(modules, current, index=False):
    """`index` is True for docs/std/index.html (modules live one level down)."""
    prefix = "modules/" if index else ""
    up = "" if index else "../"
    entries = [("Overview", f"{up}index.html", current is None, None)]
    entries += [(esc(m["name"]), f'{prefix}{m["name"]}.html', m["name"] == current, None) for m in modules]
    book = [("The book", f"{up}../lang/index.html", False, None),
            ("Playground", f"{up}../playground.html", False, None),
            ("Diagnostic codes", f"{up}../lang/diagnostics.html", False, None)]
    return sidebar([("Standard library", entries, "mono"), ("Also", book, "plain")])


def examples(mod):
    """The module's example programs: code blocks in its header with a `main`."""
    return [t for k, t in mod["summary"] if k == "code" and "fun main" in t]


def summary_html(mod, links, here):
    """The header comment: its first paragraph as the summary, the rest as
    prose, and an example program as a live sample."""
    ctx = (mod["name"], None, here)
    out, first = [], True
    for k, t in mod["summary"]:
        if k == "p":
            out.append(f'<p class="{"summary" if first else "intro"}">{doc_inline(t, links, ctx)}</p>')
            first = False
        elif "fun main" in t:
            name = mod["name"]
            out.append(
                f'<div class="sample" id="example" data-sample="std_{name}" data-mode="run" data-file="{name}_example.loom"'
                f' data-store="std.{name}" data-runnable>'
                f'<div class="sample-head"><span class="file"><span class="dot"></span>{esc(name)}_example.loom</span>'
                f'<span class="tag ok">an example to run</span><span class="spacer"></span></div>'
                f'<pre class="code"><code>{highlight(t)}</code></pre></div>')
        else:
            out.append(f'<pre class="plain"><code>{highlight(t)}</code></pre>')
    return "".join(out)


def render_module(mod, modules, links):
    name = mod["name"]
    chips = []
    for t in mod["types"]:
        chips.append(f'<a href="#{slug(t.name)}"><span class="kind">{esc(t.kind)}</span>{esc(t.name)}</a>')
    for f in mod["functions"]:
        chips.append(f'<a href="#{slug(f.name)}"><span class="kind">fun</span>{esc(f.name)}</a>')
    for c in mod["consts"]:
        chips.append(f'<a href="#{slug(c.name)}"><span class="kind">const</span>{esc(c.name)}</a>')
    body = [
        '<div class="chapter-head">',
        f'<div class="crumb"><a href="../index.html">std</a> / {esc(name)}</div>',
        f'<h1>std.{esc(name)}</h1>',
    ]
    blocks = mod["summary"]
    lead = [b for b in blocks[:1] if b[0] == "p"]
    body.append(summary_html({**mod, "summary": lead}, links, "std/modules"))
    meta = [f'<span>source: <code>{esc(mod["path"])}</code></span>']
    if name in MODULE_CHAPTER:
        href = links.book.resolve(f"page:{MODULE_CHAPTER[name]}", "std/modules")
        meta.append(f'<a href="{href}">read about it in the book &rarr;</a>')
    body.append(f'<div class="mod-meta">{"".join(meta)}</div>')
    body.append("</div>")
    rest = blocks[len(lead):]
    if rest:
        body.append(f'<div class="mod-intro">{summary_html({**mod, "summary": rest}, links, "std/modules")}</div>')
    if len(chips) > 1:
        body.append(f'<nav class="toc-chips" aria-label="In this module">{"".join(chips)}</nav>')
    if mod["reexports"]:
        body.append('<h2 id="re-exports">Re-exports</h2>')
        body.append("<p>These names are defined elsewhere in <code>std</code>; this module makes them available.</p>")
        body.append(render_reexports(mod["reexports"], links))
    if mod["types"]:
        body.append('<h2 id="types">Types</h2>')
        body += [render_type(t, links, name) for t in mod["types"]]
    if mod["consts"]:
        body.append('<h2 id="constants">Constants</h2>')
        body += [render_const(c, links, name) for c in mod["consts"]]
    if mod["functions"]:
        body.append('<h2 id="functions">Functions</h2>')
        body += [render_fn(f, links, name) for f in mod["functions"]]
    names = [m["name"] for m in modules]
    i = names.index(name)
    prev = modules[i - 1] if i > 0 else None
    nxt = modules[i + 1] if i + 1 < len(modules) else None
    pager = ['<nav class="pager" aria-label="Modules">']
    pager.append(f'<a class="prev" href="{prev["name"]}.html"><span class="dir">&larr; previous</span>std.{esc(prev["name"])}</a>'
                 if prev else '<a class="prev" href="../index.html"><span class="dir">&larr; back to</span>the library</a>')
    pager.append(f'<a class="next" href="{nxt["name"]}.html"><span class="dir">next &rarr;</span>std.{esc(nxt["name"])}</a>'
                 if nxt else '<a class="next" href="../index.html"><span class="dir">back to &rarr;</span>the library</a>')
    pager.append("</nav>")
    body.append("\n".join(pager))
    return page(f'std.{name} · Loom', "std/modules", std_sidebar(modules, name), "\n".join(body), current="std",
                play=bool(examples(mod)))


def search_index(modules):
    """[{n: name, k: kind, m: module, a: anchor, d: first sentence}] for the index's search box."""
    out = []
    for m in modules:
        out.append({"n": f"std.{m['name']}", "k": "module", "m": m["name"], "a": "",
                    "d": first_sentence(" ".join(t for k, t in m["summary"] if k == "p"))})
        for t in m["types"]:
            out.append({"n": t.name, "k": t.kind, "m": m["name"], "a": slug(t.name), "d": first_sentence(t.text)})
            for x in listed_methods(t):
                out.append({"n": f"{t.name}.{x.name}", "k": "method" if x.kind == "fn" else "type",
                            "m": m["name"], "a": f"{slug(t.name)}.{slug(x.name)}", "d": first_sentence(x.text)})
        for f in m["functions"]:
            out.append({"n": f.name, "k": "fun", "m": m["name"], "a": slug(f.name), "d": first_sentence(f.text)})
        for c in m["consts"]:
            out.append({"n": c.name, "k": "const", "m": m["name"], "a": slug(c.name), "d": first_sentence(c.text)})
    return out


SEARCH_SCRIPT = """<script>
(function () {
  var box = document.getElementById("std-search");
  var list = document.getElementById("std-results");
  var data = JSON.parse(document.getElementById("std-index").textContent);
  function esc(s) { return s.replace(/&/g, "&amp;").replace(/</g, "&lt;"); }
  function prose(s) { return esc(s).replace(/`([^`]+)`/g, "<code>$1</code>"); }
  function show() {
    var q = box.value.trim().toLowerCase();
    if (!q) { list.hidden = true; list.innerHTML = ""; return; }
    var hits = data.map(function (it) {
      var n = it.n.toLowerCase(), d = (it.d || "").toLowerCase();
      var score = n === q ? 0 : n.indexOf(q) === 0 ? 1 : n.indexOf("." + q) >= 0 ? 2 : n.indexOf(q) >= 0 ? 3 : d.indexOf(q) >= 0 ? 5 : -1;
      return [score, it];
    }).filter(function (x) { return x[0] >= 0; }).sort(function (a, b) { return a[0] - b[0] || a[1].n.length - b[1].n.length; }).slice(0, 40);
    list.hidden = false;
    list.textContent = "";
    if (!hits.length) {
      list.innerHTML = '<li class="none">Nothing by that name. Try a method (<code>push</code>) or a type (<code>Map</code>).</li>';
      return;
    }
    hits.forEach(function (x) {
      var it = x[1];
      var li = document.createElement("li");
      var a = document.createElement("a");
      a.setAttribute("href", "modules/" + it.m + ".html" + (it.a ? "#" + it.a : ""));
      a.innerHTML = '<span class="kind">' + it.k + '</span><code>' + esc(it.n) + '</code><span class="in">std.' + it.m +
        '</span><span class="d">' + prose(it.d || "") + '</span>';
      li.appendChild(a);
      list.appendChild(li);
    });
  }
  box.addEventListener("input", show);
  document.addEventListener("keydown", function (e) {
    if (e.key === "/" && document.activeElement !== box) { e.preventDefault(); box.focus(); }
  });
  box.addEventListener("keydown", function (e) {
    if (e.key === "Enter") { var a = list.querySelector("a"); if (a) location.href = a.href; }
  });
})();
</script>"""


def render_index(modules, links):
    by = {m["name"]: m for m in modules}
    sections = []
    seen = set()
    for title, blurb, names in GROUPS:
        cards = []
        for n in names:
            if n not in by:
                continue
            seen.add(n)
            cards.append(module_card(by[n], links))
        sections.append(f'<h2 id="{slug(title.lower())}">{esc(title)}</h2><p>{esc(blurb)}</p>'
                        f'<div class="cards">{"".join(cards)}</div>')
    rest = [m for m in modules if m["name"] not in seen]
    if rest:
        sections.append('<h2 id="more">More</h2><div class="cards">'
                        + "".join(module_card(m, links) for m in rest) + "</div>")
    total = sum(1 + len(listed_methods(t)) for m in modules for t in m["types"]) + \
        sum(len(m["functions"]) + len(m["consts"]) for m in modules)
    undocumented = sum(1 for m in modules for t in m["types"] for x in [t] + listed_methods(t) if not x.doc) + \
        sum(1 for m in modules for f in m["functions"] + m["consts"] if not f.doc)
    data = json.dumps(search_index(modules), ensure_ascii=False).replace("</", "<\\/")
    body = f"""<div class="chapter-head">
<span class="chapter-no">the library</span>
<h1>The standard library</h1>
<p class="summary">Everything <code>std</code> gives you, module by module: {total} types, methods,
functions and constants, each described by the doc comment in its source.</p>
</div>
<div class="std-search"><input id="std-search" type="search" placeholder="Find a type, method or function (press /)"
 aria-label="Search the standard library" autocomplete="off" spellcheck="false">
<ul id="std-results" class="std-results" hidden></ul></div>
<p>Only the <a href="modules/prelude.html">prelude</a> is in scope without a <code>use</code>.
Everything else is <code>use std.module.name</code> away, or written out in full: <code>std.io.print("hi")</code>.
Names in code are links: follow <a class="code-link" href="modules/iter.html#Stream"><code>Stream</code></a> or
<a class="code-link" href="modules/list.html#List.push"><code>List.push</code></a> wherever you meet them.</p>
{"".join(sections)}
<div class="legend">
<div><span class="badge builtin">builtin</span> implemented directly by the compiler or the runtime, not in Loom.</div>
<div><span class="badge extern">host</span> declared <code>extern "host"</code>: it calls straight into the machine running the program.</div>
<div><span class="badge todo">todo</span> no doc comment in the source yet{f' ({undocumented} left)' if undocumented else ''}.</div>
</div>
<footer>Generated from <code>runtime/std/*.loom</code> by <code>python docs/gen.py</code>: change a doc comment there and run it again.</footer>
<script type="application/json" id="std-index">{data}</script>
{SEARCH_SCRIPT}"""
    return page("The standard library · Loom", "std", std_sidebar(modules, None, index=True), body, current="std")


def module_card(m, links):
    n = module_count(m)
    summary = " ".join(t for k, t in m["summary"] if k == "p")
    return (f'<a class="card" href="modules/{m["name"]}.html">'
            f'<span class="name">std.{esc(m["name"])}</span>'
            f'<span class="desc">{doc_inline_plain(first_sentence(summary))}</span>'
            f'<span class="count">{n} item{"s" if n != 1 else ""}</span></a>')


def doc_inline_plain(text):
    """Inline code only: a card is one big link, so nothing inside it links."""
    out, pos = [], 0
    for m in re.finditer(r"`([^`]+)`", text):
        out.append(esc(text[pos:m.start()]))
        out.append(f"<code>{esc(m.group(1))}</code>")
        pos = m.end()
    out.append(esc(text[pos:]))
    return "".join(out)


def generate(modules, writer, lang_slugs):
    book = Links(lang_slugs + ["diagnostics"], [m["name"] for m in modules])
    links = StdLinks(modules, book)
    writer.write("std/index.html", render_index(modules, links))
    for m in modules:
        writer.write(f'std/modules/{m["name"]}.html', render_module(m, modules, links))
