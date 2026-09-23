//! The Loom lexer.
//!
//! Turns one source file into a flat token stream plus diagnostics. It never
//! stops at the first error: bad input becomes a [`TokenKind::Error`] token
//! (or a best-effort token) and lexing continues.
//!
//! Things worth knowing:
//! - Newlines aren't tokens. Each token records whether a newline came before
//!   it and the indentation of its line, which is what the parser's
//!   continuation rule needs (see [`Token`]).
//! - Interpolated strings become a flat run of template tokens with ordinary
//!   tokens for the embedded expressions. A mode stack tracks nesting.
//! - Radix literals with capture letters or `?` become twill tokens.

mod number;
mod string;
#[cfg(test)]
mod tests;

use crate::diag::{Diagnostic, codes};
use crate::source::{FileId, SourceDb, Span};
use crate::token::{Keyword, PrimType, Punct, Token, TokenKind};

/// The result of lexing one file.
#[derive(Debug)]
pub struct Lexed {
    /// Always ends with exactly one [`TokenKind::Eof`].
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Lexed {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Lexes one file of `db`.
pub fn lex(db: &SourceDb, file: FileId) -> Lexed {
    let mut lexer = Lexer::new(file, db.file(file).text());
    lexer.run();
    Lexed {
        tokens: lexer.tokens,
        diagnostics: lexer.diags,
    }
}

/// Where the text of a string literal ends.
#[derive(Clone, Copy, Debug)]
enum StrCtx {
    /// `"…"`: ends at the next unescaped `"` and can't contain a newline.
    Single,
    /// `"""…"""`: the content ends at byte `end` and the closing quotes start
    /// at `close`. `indent` columns are stripped after every newline.
    Multi { indent: usize, end: usize, close: usize },
}

impl StrCtx {
    fn quote_len(self) -> usize {
        match self {
            StrCtx::Single => 1,
            StrCtx::Multi { .. } => 3,
        }
    }
}

/// An open `{…}` inside a string template.
#[derive(Debug)]
struct Interp {
    /// Brackets opened inside the interpolation and not yet closed.
    depth: u32,
    ctx: StrCtx,
    /// Byte offset of the string's opening quote.
    start: usize,
}

struct Lexer<'a> {
    file: FileId,
    src: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    diags: Vec<Diagnostic>,
    /// A newline was crossed since the last token.
    nl_pending: bool,
    /// Indentation of the current line, in columns.
    line_indent: u32,
    interps: Vec<Interp>,
}

/// Columns of leading whitespace (space = 1, tab = 4).
fn measure_indent(line: &str) -> u32 {
    let mut cols = 0;
    for b in line.bytes() {
        match b {
            b' ' => cols += 1,
            b'\t' => cols += 4,
            _ => break,
        }
    }
    cols
}

fn is_ident_start(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_start(c)
}

fn is_ident_continue(c: char) -> bool {
    unicode_ident::is_xid_continue(c)
}

/// Classifies an identifier-shaped word.
fn classify_word(text: &str) -> TokenKind {
    if text == "_" {
        return TokenKind::Underscore;
    }
    if let Some(kw) = Keyword::from_ident(text) {
        return TokenKind::Keyword(kw);
    }
    if let Some(prim) = PrimType::parse(text) {
        return TokenKind::Prim(prim);
    }
    let first = text.chars().next().unwrap_or('_');
    if !first.is_uppercase() {
        return TokenKind::Ident;
    }
    let screaming =
        text.chars().count() >= 2 && text.chars().all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_');
    if screaming {
        TokenKind::ConstIdent
    } else {
        TokenKind::TypeIdent
    }
}

impl<'a> Lexer<'a> {
    fn new(file: FileId, src: &'a str) -> Self {
        Self {
            file,
            src,
            pos: 0,
            tokens: Vec::new(),
            diags: Vec::new(),
            nl_pending: false,
            line_indent: measure_indent(src),
            interps: Vec::new(),
        }
    }

    // ------------------------------------------------------------ helpers

    fn byte_at(&self, i: usize) -> Option<u8> {
        self.src.as_bytes().get(i).copied()
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn span(&self, lo: usize, hi: usize) -> Span {
        Span::new(self.file, lo as u32, hi as u32)
    }

    /// Pushes a token that may start a line.
    fn push(&mut self, kind: TokenKind, lo: usize, hi: usize) {
        let nl_before = std::mem::take(&mut self.nl_pending);
        self.tokens.push(Token {
            kind,
            span: self.span(lo, hi),
            nl_before,
            line_indent: self.line_indent,
        });
    }

    /// Pushes a token inside a string template (never starts a line).
    fn push_inner(&mut self, kind: TokenKind, lo: usize, hi: usize) {
        self.tokens.push(Token {
            kind,
            span: self.span(lo, hi),
            nl_before: false,
            line_indent: self.line_indent,
        });
    }

    fn report(&mut self, diag: Diagnostic) {
        self.diags.push(diag);
    }

    /// Called with `pos` just past a `\n`.
    fn crossed_newline(&mut self) {
        if self.interps.is_empty() {
            self.nl_pending = true;
        }
        self.line_indent = measure_indent(&self.src[self.pos..]);
    }

    // ------------------------------------------------------------ driver

    fn run(&mut self) {
        loop {
            self.skip_trivia();
            let start = self.pos;
            let Some(c) = self.peek() else {
                self.finish();
                return;
            };

            if let Some(interp) = self.interps.last() {
                match c {
                    // Only reachable for single-line strings: `skip_trivia`
                    // stops at the newline so we can report it here.
                    '\n' => {
                        let start = interp.start;
                        self.report(
                            Diagnostic::error(codes::S0111, "unterminated interpolation")
                                .primary(self.span(start, start + 1), "this string")
                                .secondary(self.span(self.pos, self.pos), "the line ends before the `}`")
                                .help("close the `{…}` before the end of the line, or use a `\"\"\"` string"),
                        );
                        self.abandon_interps();
                        continue;
                    }
                    '}' if interp.depth == 0 => {
                        self.close_interp();
                        continue;
                    }
                    ':' if interp.depth == 0 => {
                        self.interp_spec();
                        continue;
                    }
                    _ => {}
                }
            }

            match c {
                '"' => self.string(start),
                '\'' => self.char_literal(start),
                '0'..='9' => self.number(start),
                'r' if self.byte_at(start + 1) == Some(b'"') => self.raw_string(start),
                'b' if self.byte_at(start + 1) == Some(b'"') => self.byte_string(start),
                c if is_ident_start(c) => self.word(start),
                _ => self.punct(start),
            }
        }
    }

    fn finish(&mut self) {
        if let Some(interp) = self.interps.first() {
            let start = interp.start;
            self.report(
                Diagnostic::error(codes::S0111, "unterminated interpolation")
                    .primary(self.span(start, start + 1), "this string's `{…}` is never closed"),
            );
            self.abandon_interps();
        }
        let end = self.src.len();
        self.push(TokenKind::Eof, end, end);
    }

    /// Closes every open interpolation (after an error) so the token stream
    /// stays well-formed for the parser.
    fn abandon_interps(&mut self) {
        while self.interps.pop().is_some() {
            self.push_inner(TokenKind::InterpClose, self.pos, self.pos);
            self.push_inner(TokenKind::TemplateEnd, self.pos, self.pos);
        }
    }

    // ------------------------------------------------------------ trivia

    fn skip_trivia(&mut self) {
        while let Some(b) = self.byte_at(self.pos) {
            match b {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    if matches!(
                        self.interps.last(),
                        Some(Interp {
                            ctx: StrCtx::Single,
                            ..
                        })
                    ) {
                        return;
                    }
                    self.pos += 1;
                    self.crossed_newline();
                }
                b'/' if self.byte_at(self.pos + 1) == Some(b'/') => self.line_comment(),
                b'/' if self.byte_at(self.pos + 1) == Some(b'*') => self.block_comment(),
                _ => return,
            }
        }
    }

    fn line_comment(&mut self) {
        let start = self.pos;
        let end = self.src[start..].find('\n').map_or(self.src.len(), |i| start + i);
        self.pos = end;
        let src = self.src;
        let text = &src[start..end];
        // `///` is a doc comment; `////…` is an ordinary comment.
        if let Some(doc) = text.strip_prefix("///")
            && !doc.starts_with('/')
        {
            let doc = doc.strip_prefix(' ').unwrap_or(doc).to_string();
            self.push(TokenKind::DocComment(doc), start, end);
        }
    }

    fn block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1u32;
        while depth > 0 {
            match (self.byte_at(self.pos), self.byte_at(self.pos + 1)) {
                (Some(b'/'), Some(b'*')) => {
                    depth += 1;
                    self.pos += 2;
                }
                (Some(b'*'), Some(b'/')) => {
                    depth -= 1;
                    self.pos += 2;
                }
                (Some(b'\n'), _) => {
                    self.pos += 1;
                    self.crossed_newline();
                }
                // Only ASCII bytes are compared, so stepping through the
                // middle of multi-byte characters is harmless.
                (Some(_), _) => self.pos += 1,
                (None, _) => {
                    self.report(
                        Diagnostic::error(codes::S0101, "unterminated block comment")
                            .primary(self.span(start, start + 2), "this comment is never closed")
                            .note("block comments nest, so every `/*` needs its own `*/`"),
                    );
                    return;
                }
            }
        }
    }

    // ------------------------------------------------------------ words & punctuation

    fn word(&mut self, start: usize) {
        self.bump();
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let src = self.src;
        let text = &src[start..self.pos];
        let kind = if text == "as" {
            // `as%`, `as|` and `as?` are single tokens when glued together.
            match self.byte_at(self.pos) {
                Some(b'%') => Some(Keyword::AsWrap),
                Some(b'|') => Some(Keyword::AsSat),
                Some(b'?') => Some(Keyword::AsChecked),
                _ => None,
            }
            .map_or(TokenKind::Keyword(Keyword::As), |kw| {
                self.pos += 1;
                TokenKind::Keyword(kw)
            })
        } else {
            classify_word(text)
        };
        self.push(kind, start, self.pos);
    }

    fn punct(&mut self, start: usize) {
        let src = self.src;
        let rest = &src[start..];
        let Some(&punct) = Punct::BY_LENGTH.iter().find(|p| rest.starts_with(p.as_str())) else {
            let c = self.bump().unwrap_or('\0');
            let mut diag = Diagnostic::error(codes::S0100, format!("unexpected character `{c}`"))
                .primary(self.span(start, self.pos), "not valid here");
            diag = match c {
                '“' | '”' | '‘' | '’' => {
                    diag.help("Loom uses straight quotes: `\"` for strings, `'` for chars")
                }
                '…' => diag.help("ranges are written `..` or `..=`"),
                '$' => diag.help("string interpolation is `{expr}`, with no `$`"),
                _ => diag,
            };
            self.report(diag);
            self.push(TokenKind::Error, start, self.pos);
            return;
        };
        self.pos += punct.as_str().len();
        self.push(TokenKind::Punct(punct), start, self.pos);

        if let Some(interp) = self.interps.last_mut() {
            match punct {
                Punct::LParen | Punct::LBracket | Punct::LBrace => interp.depth += 1,
                Punct::RParen | Punct::RBracket | Punct::RBrace => interp.depth = interp.depth.saturating_sub(1),
                _ => {}
            }
        }
    }
}
