# Renders the standard library pages (docs/std/index.html and
# docs/std/modules/*.html) from docs/std/data.py, which mirrors
# runtime/std/*.loom exactly: every item with a doc comment carries it, every
# item without one is marked TODO.
#
# The HTML files are generated, never hand-edited: `python docs/gen.py`.
import re

from common import esc, inline, page, sidebar

# ---------------------------------------------------------------- helpers


def slug(name):
    return re.sub(r"[^A-Za-z0-9_.-]", "_", name)


def doc_html(doc, empty_text="undocumented in source"):
    if doc:
        return f'<p class="item-doc">{inline(doc)}</p>'
    return f'<p class="item-doc empty">{empty_text}</p>'


def member_doc_html(doc):
    if doc:
        return f'<p class="member-doc">{inline(doc)}</p>'
    return '<p class="member-doc empty">undocumented</p>'


def badges(item):
    out = []
    if item.get("builtin"):
        out.append('<span class="badge builtin">builtin</span>')
    if item.get("extern"):
        out.append('<span class="badge extern">extern</span>')
    if not item.get("doc"):
        out.append('<span class="badge todo">TODO</span>')
    return "".join(out)


def member_badges(m):
    out = []
    if m.get("builtin"):
        out.append('<span class="badge builtin">builtin</span>')
    if not m.get("doc"):
        out.append('<span class="badge todo">TODO</span>')
    return "".join(out)


# ---------------------------------------------------------------- render


def render_type(item):
    tid = slug(item["name"])
    parts = [
        f'<div class="item" id="{tid}">',
        '<div class="item-head">',
        f'<span class="sig">{esc(item["kind"])} <strong>{esc(item["name"])}</strong>{esc(item.get("suffix", ""))}</span>',
        badges(item),
        "</div>",
        doc_html(item.get("doc")),
    ]
    if item.get("example"):
        parts.append(f'<pre><code>{esc(item["example"])}</code></pre>')
    if item.get("fields"):
        parts.append('<ul class="fields">')
        for f in item["fields"]:
            parts.append(f'<li><span class="sig">{esc(f)}</span></li>')
        parts.append("</ul>")
    if item.get("cases"):
        parts.append('<ul class="cases">')
        for c in item["cases"]:
            parts.append(f'<li><span class="sig">case {esc(c)}</span></li>')
        parts.append("</ul>")
    if item.get("methods"):
        parts.append('<div class="members">')
        for m in item["methods"]:
            m_name = re.match(r"(?:fun|type) (\w+)", m["sig"])
            mid = f' id="{tid}.{slug(m_name.group(1))}"' if m_name else ""
            parts.append(f'<div class="member"{mid}>')
            parts.append('<div class="member-head">')
            parts.append(f'<span class="sig">{esc(m["sig"])}</span>')
            parts.append(member_badges(m))
            parts.append("</div>")
            parts.append(member_doc_html(m.get("doc")))
            parts.append("</div>")
        parts.append("</div>")
    parts.append("</div>")
    return "\n".join(parts)


def render_fn(item):
    return "\n".join([
        f'<div class="item" id="{slug(item["name"])}">',
        '<div class="item-head">',
        f'<span class="sig">fun <strong>{esc(item["name"])}</strong>{esc(item["suffix"])}</span>',
        badges(item),
        "</div>",
        doc_html(item.get("doc")),
        "</div>",
    ])


def render_const(item):
    return "\n".join([
        f'<div class="item" id="{slug(item["name"])}">',
        '<div class="item-head">',
        f'<span class="sig">const <strong>{esc(item["name"])}</strong>{esc(item["suffix"])}</span>',
        badges(item),
        "</div>",
        doc_html(item.get("doc")),
        "</div>",
    ])


def render_reexports(reexports):
    parts = ['<ul class="reexports">']
    for r in reexports:
        parts.append(
            f'<li><a href="{r["from"]}.html#{slug(r["name"])}"><code>{esc(r["name"])}</code></a>'
            f'<span class="from">from <code>std.{esc(r["from"])}</code></span></li>'
        )
    parts.append("</ul>")
    return "\n".join(parts)


def module_count(mod):
    return (len(mod.get("reexports", [])) + len(mod.get("types", []))
            + len(mod.get("functions", [])) + len(mod.get("consts", [])))


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


def render_module(mod, modules):
    body = [
        f'<div class="crumb"><a href="../index.html">std</a> / {esc(mod["name"])}</div>',
        f'<h1>std.{esc(mod["name"])}</h1>',
        f'<p class="summary">{inline(mod["summary"])}</p>',
    ]
    if mod.get("reexports"):
        body.append("<h2>Re-exports</h2>")
        body.append(render_reexports(mod["reexports"]))
    if mod.get("types"):
        body.append("<h2>Types</h2>")
        body += [render_type(t) for t in mod["types"]]
    if mod.get("consts"):
        body.append("<h2>Constants</h2>")
        body += [render_const(c) for c in mod["consts"]]
    if mod.get("functions"):
        body.append("<h2>Functions</h2>")
        body += [render_fn(f) for f in mod["functions"]]
    return page(f'std.{mod["name"]} · Loom', "std/modules",
                std_sidebar(modules, mod["name"]), "\n".join(body))


INDEX_BODY = """<h1>The Loom standard library</h1>
<p class="summary">What's implemented in <code>runtime/std</code>, one namespace
per module. Every item's description is copied from its doc comment in the
source; an item with none is marked <span class="badge todo">TODO</span>
rather than given one here.</p>
<div class="cards">
{cards}
</div>
<div class="legend">
<div><span class="badge builtin">builtin</span> implemented directly by the compiler or the runtime, not in Loom.</div>
<div><span class="badge extern">extern</span> declared as <code>extern "host"</code>, calling straight into the host.</div>
<div><span class="badge todo">TODO</span> no doc comment in the source yet.</div>
</div>
<footer>Generated from <code>runtime/std/*.loom</code>. Run <code>python docs/gen.py</code> after changing the standard library.</footer>"""


def render_index(modules):
    cards = []
    for m in modules:
        n = module_count(m)
        cards.append(
            f'<a class="card" href="modules/{m["name"]}.html">'
            f'<span class="name">std.{esc(m["name"])}</span>'
            f'<span class="desc">{inline(m["summary"])}</span>'
            f'<span class="count">{n} item{"s" if n != 1 else ""}</span>'
            f'</a>'
        )
    return page("Loom standard library", "std", std_sidebar(modules, None, index=True),
                INDEX_BODY.format(cards="\n".join(cards)))


def generate(modules, writer):
    writer.write("std/index.html", render_index(modules))
    for m in modules:
        writer.write(f'std/modules/{m["name"]}.html', render_module(m, modules))
