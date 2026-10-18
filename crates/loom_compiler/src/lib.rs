//! Everything semantic in Loom, between the parser and the two backends.
//!
//! ```text
//! SourceDb → lex → Tokens → parse → AST → resolve/desugar → HIR
//!   → typeck → THIR → analyses (moves, gauges, concurrency) → lower
//!       ├─ lower → luagen → Bundle { lua, source map }      (the Lua backend)
//!       └─ heddle (typed SSA IR) → opt → codegen → image    (Treadle, `loom_vm`)
//! ```
//!
//! The lexer and parser live in `loom_syntax`. This crate has name
//! resolution ([`hir`], [`modules`]), type checking ([`ty`], [`typeck`]),
//! compile-time code ([`consteval`], run as jobs on an in-process Treadle,
//! and [`baked`], the values it computed), lowering to Lua ([`lower`]), and
//! the Treadle pipeline ([`heddle`], [`codegen`], [`treadle`]). See
//! `design/lua-backend.md` and `design/vm.md` for the decisions they
//! implement.

pub mod baked;
pub mod codegen;
pub mod consteval;
mod fxhash;
pub mod heddle;
pub mod hir;
pub mod lower;
pub mod modules;
pub mod target;
pub mod treadle;
pub mod ty;
pub mod typeck;

pub use loom_syntax as syntax;
