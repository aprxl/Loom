# Loom docs

A static, offline site: `index.html` opens the book (`lang/`), the standard
library (`std/`), the diagnostic codes and the playground. `loom docs` opens
it; it's built into the binary, so it always matches that `loom`.

- `lang/` is the book, generated from `lang/pages/*.md` (the page format is
  at the top of `lang_pages.py`). Each `@sample NAME` there is
  `lang/samples/NAME.loom`, a program that really runs; its last line says
  what it should do, and `NAME.out` holds what it prints (see `samples.py`).
  `@compare` boxes show what other languages do instead, and `@try` boxes
  suggest an edit to make.
- `std/` is generated from `std/data.py`, a hand-kept transcription of
  `runtime/std/*.loom`: every item's doc comment, verbatim, or a `TODO`
  badge where the source has none. Nothing here is guessed; if a page looks
  thin, the fix is a doc comment in the `.loom` file, not this site.
- `style.css` is the theme, `weave.js` draws the thread that runs down every
  page, and `play/play.js` turns each sample into an editor with a Run
  button. The pages read fine without JavaScript.

## The playground

Samples run in the browser on the real compiler and Treadle:
`crates/loom_play` built for WebAssembly (`wasm32-wasip1`), packed into
`play/loom-wasm.js`, run in a Web Worker under a small WASI shim. It works
from `file://`, which is how `loom docs` opens the pages: nothing is fetched,
the wasm comes in a classic `<script>`. Rebuild it when the compiler or the
VM changes what a program does (`gen.py --check` notes when it's older than
the sources):

```
rustup target add wasm32-wasip1     # once
python docs/play/build.py           # build, pack, and run every sample through it (with Node)
```

`crates/loom_play/tests/samples.rs` checks, in `cargo test`, that the
playground says exactly what the CLI says for every sample.
`play/skip.txt` lists any sample the sandbox would change (no environment,
files in memory, one worker); those show their checked output and no Run
button.

## Regenerating

Regenerate after editing any of it, then check every sample against a
`loom` binary:

```
python docs/gen.py
python docs/gen.py --check --loom target/debug/loom.exe
```

`--bless` rewrites the `.out` files instead. Preview the site with
`.claude/launch.json`'s `std-spec` server (serves `docs/` on `:8878`), or
open `index.html`.

Fonts (Fraunces, Alegreya, Recursive, Caveat) come from Google Fonts; offline,
each falls back to a font the machine has.
