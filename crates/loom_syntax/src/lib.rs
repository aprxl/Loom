//! Loom's front door: source files, spans, diagnostics, the lexer, the
//! parser and the AST.
//!
//! This crate has no C dependencies, so it builds fast.

pub mod ast;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod pretty;
pub mod source;
pub mod token;
pub mod twill;

pub use diag::{Diagnostic, Severity};
pub use lexer::{Lexed, lex};
pub use parser::{Parsed, parse, parse_reusing};
pub use source::{FileId, LineCol, SourceDb, Span};
pub use token::{Token, TokenKind};
