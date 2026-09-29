# Loom docs

A static, offline site: `index.html` links the language guide (`lang/`) and
the standard library spec (`std/`). `loom docs` opens it; it's built into
the binary, so it always matches that `loom`.

- `std/` is generated from `std/data.py`, a hand-kept transcription of
  `runtime/std/*.loom`: every item's doc comment, verbatim, or a `TODO`
  badge where the source has none. Nothing here is guessed; if a page looks
  thin, the fix is a doc comment in the `.loom` file, not this site.
- `lang/` is generated from `lang/pages/*.md`. Each `@sample NAME` there is
  `lang/samples/NAME.loom`, a program that really runs; its last line says
  what it should do, and `NAME.out` holds what it prints (see `samples.py`).

Regenerate after editing any of it, then check every sample against a
`loom` binary:

```
python docs/gen.py
python docs/gen.py --check --loom target/debug/loom.exe
```

`--bless` rewrites the `.out` files instead. Preview the site with
`.claude/launch.json`'s `std-spec` server (serves `docs/` on `:8878`), or
open `index.html`: everything is plain links, no JavaScript.
