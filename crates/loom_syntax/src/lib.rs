//! Loom's front door: source files, spans, diagnostics, tokens and the lexer.
//!
//! This crate has no C dependencies, so it builds fast and is the natural
//! place for the parser too (`ast` and `parser` modules, not written yet).

pub mod diag;
pub mod lexer;
pub mod source;
pub mod token;
pub mod twill;

pub use diag::{Diagnostic, Severity};
pub use lexer::{Lexed, lex};
pub use source::{FileId, LineCol, SourceDb, Span};
pub use token::{Token, TokenKind};
