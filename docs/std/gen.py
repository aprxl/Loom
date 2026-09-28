# Generates docs/std/*.html from data.py, which mirrors runtime/std/*.loom
# exactly: every item with a doc comment carries it, every item without one
# is marked TODO. Re-run after editing data.py or runtime/std itself; the
# HTML files are not hand-edited.
#
#   python docs/std/gen.py
import html
import os
import re

OUT = os.path.dirname(os.path.abspath(__file__))

# ---------------------------------------------------------------- helpers

def esc(s):
    return html.escape(s, quote=False)

def inline(doc):
    """Doc text -> HTML: escape, then turn `code` spans into <code>."""
    if doc is None:
        return None
    out = esc(doc)
    out = re.sub(r"`([^`]+)`", lambda m: f"<code>{m.group(1)}</code>", out)
    return out

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
    parts = [
        '<div class="item">',
        '<div class="item-head">',
        f'<span class="sig">{esc(item["kind"])} <strong>{esc(item["name"])}</strong>{esc(item.get("suffix",""))}</span>',
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
            parts.append('<div class="member">')
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
    parts = [
        '<div class="item">',
        '<div class="item-head">',
        f'<span class="sig">fn <strong>{esc(item["name"])}</strong>{esc(item["suffix"])}</span>',
        badges(item),
        "</div>",
        doc_html(item.get("doc")),
        "</div>",
    ]
    return "\n".join(parts)

def render_const(item):
    parts = [
        '<div class="item">',
        '<div class="item-head">',
        f'<span class="sig">const <strong>{esc(item["name"])}</strong>{esc(item["suffix"])}</span>',
        badges(item),
        "</div>",
        doc_html(item.get("doc")),
        "</div>",
    ]
    return "\n".join(parts)

def render_reexports(reexports):
    parts = ['<ul class="reexports">']
    for r in reexports:
        parts.append(
            f'<li><a href="{r["from"]}.html"><code>{esc(r["name"])}</code></a>'
            f'<span class="from">from <code>std.{esc(r["from"])}</code></span></li>'
        )
    parts.append("</ul>")
    return "\n".join(parts)

def sidebar(modules, current):
    parts = ['<nav class="sidebar">', '<a class="brand" href="../index.html">Loom <span class="dim">std</span></a>', "<ul>"]
    for m in modules:
        cls = ' class="current"' if m["name"] == current else ""
        parts.append(f'<li><a{cls} href="{m["name"]}.html">{esc(m["name"])}</a></li>')
    parts.append("</ul></nav>")
    return "\n".join(parts)

def index_sidebar(modules):
    parts = ['<nav class="sidebar">', '<a class="brand" href="index.html">Loom <span class="dim">std</span></a>', "<ul>"]
    for m in modules:
        parts.append(f'<li><a href="modules/{m["name"]}.html">{esc(m["name"])}</a></li>')
    parts.append("</ul></nav>")
    return "\n".join(parts)

PAGE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>std.{name} · Loom</title>
<link rel="stylesheet" href="../style.css">
</head>
<body>
<div class="layout">
{sidebar}
<main>
<div class="crumb"><a href="../index.html">std</a> / {name}</div>
<h1>std.{name}</h1>
<p class="summary">{summary}</p>
{body}
</main>
</div>
</body>
</html>
"""

def module_count(mod):
    n = 0
    n += len(mod.get("reexports", []))
    n += len(mod.get("types", []))
    n += len(mod.get("functions", []))
    n += len(mod.get("consts", []))
    return n

def render_module(mod, modules):
    body = []
    if mod.get("reexports"):
        body.append("<h2>Re-exports</h2>")
        body.append(render_reexports(mod["reexports"]))
    if mod.get("types"):
        body.append("<h2>Types</h2>")
        for t in mod["types"]:
            body.append(render_type(t))
    if mod.get("consts"):
        body.append("<h2>Constants</h2>")
        for c in mod["consts"]:
            body.append(render_const(c))
    if mod.get("functions"):
        body.append("<h2>Functions</h2>")
        for f in mod["functions"]:
            body.append(render_fn(f))
    html_out = PAGE.format(
        name=esc(mod["name"]),
        summary=inline(mod["summary"]),
        sidebar=sidebar(modules, mod["name"]),
        body="\n".join(body),
    )
    path = os.path.join(OUT, "modules", f'{mod["name"]}.html')
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(html_out)

INDEX = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Loom std</title>
<link rel="stylesheet" href="style.css">
</head>
<body>
<div class="layout">
{sidebar}
<main>
<h1>The Loom standard library</h1>
<p class="summary">What's implemented in <code>runtime/std</code>, one namespace
per module. Every item's description is copied from its doc comment in the
source; an item with none is marked <span class="badge todo">TODO</span>
rather than given one here.</p>
<div class="cards">
{cards}
</div>
<div class="legend">
<div><span class="badge builtin">builtin</span> implemented directly by the compiler or the runtime, not in Loom.</div>
<div><span class="badge extern">extern</span> declared as <code>extern "lua"</code>, calling straight into the host.</div>
<div><span class="badge todo">TODO</span> no doc comment in the source yet.</div>
</div>
<footer>Generated from <code>runtime/std/*.loom</code>. Re-run <code>docs/std/gen.py</code> after changing the standard library.</footer>
</main>
</div>
</body>
</html>
"""

def render_index(modules):
    cards = []
    for m in modules:
        cards.append(
            f'<a class="card" href="modules/{m["name"]}.html">'
            f'<span class="name">std.{esc(m["name"])}</span>'
            f'<span class="desc">{inline(m["summary"])}</span>'
            f'<span class="count">{module_count(m)} item{"s" if module_count(m) != 1 else ""}</span>'
            f'</a>'
        )
    html_out = INDEX.format(sidebar=index_sidebar(modules), cards="\n".join(cards))
    with open(os.path.join(OUT, "index.html"), "w", encoding="utf-8", newline="\n") as f:
        f.write(html_out)

if __name__ == "__main__":
    from data import MODULES

    os.makedirs(os.path.join(OUT, "modules"), exist_ok=True)
    render_index(MODULES)
    for m in MODULES:
        render_module(m, MODULES)
    print(f"wrote index.html + {len(MODULES)} module pages")
