# std spec

A static, offline spec of the standard library: `index.html` lists every
`std` module, each linking to its own page under `modules/`.

Generated from `data.py`, which is a hand-kept transcription of
`runtime/std/*.loom` — every item's doc comment, verbatim, or a `TODO` badge
where the source has none. Nothing here is guessed; if a page looks thin,
the fix is a doc comment in the `.loom` file, not this site.

Regenerate after editing `data.py` or the standard library:

```
python docs/std/gen.py
```

Preview it with `.claude/launch.json`'s `std-spec` server (serves this
directory on `:8878`), or just open `index.html` — everything is plain
links, no JavaScript.
