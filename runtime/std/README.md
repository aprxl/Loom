# std

The Loom standard library, written in Loom (plus `extern "lua"` declarations
for the primitives that live in `runtime/rt`). Modules are embedded into the
`loom` binary (the list is in `crates/loom_compiler/src/modules.rs`); set
`LOOM_STD_DIR` to load them from disk while developing.

So far each module declares only what the examples use, which is enough for
name resolution. `#builtin` marks types and functions the compiler provides
itself. Bodies arrive with type checking and lowering.

`prelude.loom` lists the names every module sees without a `use`.
