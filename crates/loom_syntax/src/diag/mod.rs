//! Diagnostics: what the compiler reports, independent of how it's rendered.
//!
//! Every diagnostic carries a [`Code`] from [`codes`]. Codes are grouped by
//! letter: S lexer/syntax, N names, T types, M moves, L `once`, R regions,
//! C concurrency, G gauges, P patterns, K const, U unsupported, I interop.

pub mod codes;
mod render;

pub use codes::Code;
pub use render::{ColorMode, emit_to_stderr, render_plain};

use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: Code,
    pub message: String,
    pub labels: Vec<Label>,
    /// Trailing `= note: …` / `= help: …` lines, already prefixed.
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(code: Code, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn warning(code: Code, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    fn new(severity: Severity, code: Code, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Adds the label that points at the problem itself.
    pub fn primary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: true,
        });
        self
    }

    /// Adds a label that points at related context.
    pub fn secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: false,
        });
        self
    }

    pub fn note(mut self, text: impl AsRef<str>) -> Self {
        self.notes.push(format!("note: {}", text.as_ref()));
        self
    }

    pub fn help(mut self, text: impl AsRef<str>) -> Self {
        self.notes.push(format!("help: {}", text.as_ref()));
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}
