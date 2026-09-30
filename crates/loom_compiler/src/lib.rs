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
//! resolution ([`hir`], [`modules`]), type checking ([`ty`], [`typeck`]) and
//! lowering to Lua ([`lower`]), each covering the same first slice of the
//! language. Still to come: `consteval`, `flow` (the analyses) and
//! `lir_verify`. See `design/lua-backend.md` for the decisions they implement.

pub mod codegen;
pub mod heddle;
pub mod hir;
pub mod lower;
pub mod modules;
pub mod target;
pub mod treadle;
pub mod ty;
pub mod typeck;

pub use loom_syntax as syntax;
