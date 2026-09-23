# std

The Loom standard library, written in Loom (plus `extern "lua"` declarations
for the primitives that live in `runtime/rt`). Modules are embedded into the
`loom` binary; set `LOOM_STD_DIR` to load them from disk while developing.

Planned modules: prelude, io, fmt, env, fs, mem, num, math, iter, list, map,
set, string, rand, time, term, sync. Nothing here yet: std starts with the
M1 milestone, after the parser and type checker exist.
