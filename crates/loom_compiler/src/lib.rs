//! Everything semantic in the Loom → Lua pipeline.
//!
//! ```text
//! SourceDb → lex → Tokens → parse → AST → resolve/desugar → HIR
//!   → typeck(Instance) → THIR ⇄ const eval (Rust fold | sandboxed Lua)
//!   → flow::Cfg → analyses → FlowFacts
//!   → lower → LIR → lir_verify → luagen → Bundle { lua, source map }
//! ```
//!
//! Only the lexer exists so far (in `loom_syntax`). The modules planned here
//! are: `session`, `modules`, `hir`, `ty`, `typeck`, `thir`, `consteval`,
//! `flow`, `patterns`, `lower`, `lir`, `lir_verify` and `luagen`. See
//! `design/lua-backend.md` for the decisions they implement.

pub use loom_syntax as syntax;
