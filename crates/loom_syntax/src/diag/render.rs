//! Rendering diagnostics rustc-style, through codespan-reporting.

use std::io::{self, IsTerminal};

use codespan_reporting::diagnostic as cs;
use codespan_reporting::term::termcolor::{ColorChoice, StandardStream};
use codespan_reporting::term::{self, Chars, Config, Styles, StylesWriter};

use super::{Diagnostic, Severity};
use crate::source::{FileId, SourceDb};

/// Whether colored output is used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Color when stderr is a terminal and `NO_COLOR` isn't set.
    #[default]
    Auto,
    Always,
    Never,
}

fn config() -> Config {
    Config {
        chars: Chars::ascii(),
        ..Config::default()
    }
}

fn to_codespan(diag: &Diagnostic) -> cs::Diagnostic<FileId> {
    let base = match diag.severity {
        Severity::Error => cs::Diagnostic::error(),
        Severity::Warning => cs::Diagnostic::warning(),
    };
    let labels = diag
        .labels
        .iter()
        .map(|l| {
            let label = if l.primary {
                cs::Label::primary(l.span.file, l.span.range())
            } else {
                cs::Label::secondary(l.span.file, l.span.range())
            };
            label.with_message(&l.message)
        })
        .collect();
    base.with_code(diag.code.id)
        .with_message(&diag.message)
        .with_labels(labels)
        .with_notes(diag.notes.clone())
}

/// Renders one diagnostic without color, e.g. for tests and golden files.
pub fn render_plain(db: &SourceDb, diag: &Diagnostic) -> String {
    term::emit_into_string(&config(), db, &to_codespan(diag))
        .unwrap_or_else(|e| format!("error[{}]: {} (rendering failed: {e})\n", diag.code, diag.message))
}

/// Writes diagnostics to stderr.
pub fn emit_to_stderr(db: &SourceDb, diags: &[Diagnostic], mode: ColorMode) -> io::Result<()> {
    let choice = match mode {
        ColorMode::Always => ColorChoice::Always,
        ColorMode::Never => ColorChoice::Never,
        ColorMode::Auto if io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() => ColorChoice::Auto,
        ColorMode::Auto => ColorChoice::Never,
    };
    let stream = StandardStream::stderr(choice);
    let mut lock = stream.lock();
    let styles = Styles::default();
    let mut writer = StylesWriter::new(&mut lock, &styles);
    let config = config();
    for diag in diags {
        term::emit_to_write_style(&mut writer, &config, db, &to_codespan(diag)).map_err(io::Error::other)?;
    }
    Ok(())
}
