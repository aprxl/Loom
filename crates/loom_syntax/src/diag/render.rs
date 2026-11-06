//! Rendering diagnostics rustc-style, through codespan-reporting.

use std::io::{self, IsTerminal};

use codespan_reporting::diagnostic as cs;
use codespan_reporting::term::termcolor::{BufferWriter, ColorChoice};
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
    Config { chars: Chars::ascii(), ..Config::default() }
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
    base.with_code(diag.code.id).with_message(&diag.message).with_labels(labels).with_notes(diag.notes.clone())
}

/// How many of a stage's diagnostics `loom` shows; [`report_tail`] counts the rest.
pub const MAX_SHOWN: usize = 50;

/// What `loom` writes after the diagnostics of one stage (`parse`,
/// `resolve`, `type-check`, `compile`) of the program at `path`: how many
/// past [`MAX_SHOWN`] weren't shown, then, if any is an error, that the
/// stage failed. The `bool` says whether it failed.
pub fn report_tail(path: &str, diagnostics: &[Diagnostic], stage: &str) -> (String, bool) {
    let mut out = String::new();
    let hidden = diagnostics.len().saturating_sub(MAX_SHOWN);
    if hidden > 0 {
        out.push_str(&format!("note: {hidden} more not shown\n\n"));
    }
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    if errors > 0 {
        let plural = if errors == 1 { "" } else { "s" };
        out.push_str(&format!("error: {path} failed to {stage} ({errors} error{plural})\n"));
    }
    (out, errors > 0)
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
    // Each diagnostic is rendered into a buffer and written at once: the
    // renderer writes a character at a time, and unbuffered, a long line
    // took thousands of writes per diagnostic.
    let stderr = BufferWriter::stderr(choice);
    let styles = Styles::default();
    let config = config();
    for diag in diags {
        let mut buffer = stderr.buffer();
        let mut writer = StylesWriter::new(&mut buffer, &styles);
        term::emit_to_write_style(&mut writer, &config, db, &to_codespan(diag)).map_err(io::Error::other)?;
        stderr.print(&buffer)?;
    }
    Ok(())
}
