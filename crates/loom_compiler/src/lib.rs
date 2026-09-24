//! Everything semantic in the Loom → Lua pipeline.
//!
//! ```text
//! SourceDb → lex → Tokens → parse → AST → resolve/desugar → HIR
//!   → typeck(Instance) → THIR ⇄ const eval (Rust fold | sandboxed Lua)
//!   → flow::Cfg → analyses → FlowFacts
//!   → lower → LIR → lir_verify → luagen → Bundle { lua, source map }
//! ```
//!
//! The lexer and parser live in `loom_syntax`. This crate has name
//! resolution so far ([`hir`], [`modules`]). Still to come: `ty`, `typeck`,
//! `thir`, `consteval`, `flow`, `patterns`, `lower`, `lir`, `lir_verify` and
//! `luagen`. See `design/lua-backend.md` for the decisions they implement.

pub mod hir;
pub mod modules;

pub use loom_syntax as syntax;
