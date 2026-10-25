//! The Loom parser: tokens → [`ast::Module`](crate::ast::Module).
//!
//! A hand-written recursive-descent parser with a Pratt loop for
//! expressions. Like the lexer it never stops at the first error: it reports
//! a diagnostic, inserts an `Error` node, skips to a sensible boundary (the
//! end of the statement, or the next item), and carries on.
//!
//! Newlines end statements. Two pieces of state implement the continuation
//! rule from `design/01-syntax.md`:
//! - `nl_stack`: inside `(...)` and `[...]` newlines don't matter; inside
//!   `{...}` (and at the top level) they do.
//! - `stmt_indent`: the indentation of the line where the current statement
//!   began. A token that starts a new line continues the expression only if
//!   it's `.`, `?` or a binary operator *and* it is indented deeper.

mod expr;
mod items;
mod pat;
mod stmt;
#[cfg(test)]
mod tests;
mod ty;

use std::collections::HashMap;

use crate::ast::{Expr, Ident, Module};
use crate::diag::{Code, Diagnostic, codes};
use crate::lexer::lex;
use crate::source::{FileId, SourceDb, Span};
use crate::token::{Keyword, Punct, Token, TokenKind, describe};

/// The result of parsing one file: the AST plus lexer and parser diagnostics.
#[derive(Debug)]
pub struct Parsed {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

impl Parsed {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Lexes and parses one file of `db`.
pub fn parse(db: &SourceDb, file: FileId) -> Parsed {
    let lexed = lex(db, file);
    let mut parser = Parser::new(db, file, lexed.tokens);
    let module = parser.module();
    let mut diagnostics = lexed.diagnostics;
    diagnostics.extend(parser.diags);
    Parsed { module, diagnostics }
}

/// What a `#` means where it's parsed: the length of the value that an
/// enclosing `x[…]` indexes.
enum LenBase {
    Outside,
    Path(Expr),
    NotPath,
}

/// Which identifier case a declaration expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameCase {
    /// `snake_case`: values, functions, fields, parameters.
    Value,
    /// `PascalCase`: types, wefts, cases.
    Type,
    /// `SCREAMING_CASE`: constants.
    Const,
    /// Anything goes (extern declarations keep their foreign names).
    Any,
}

pub(crate) struct Parser<'a> {
    db: &'a SourceDb,
    file: FileId,
    /// Doc comments are removed from the stream and kept in `docs`.
    tokens: Vec<Token>,
    /// Doc comment lines that precede the token at each index.
    docs: HashMap<usize, Vec<String>>,
    pos: usize,
    diags: Vec<Diagnostic>,
    nl_stack: Vec<bool>,
    stmt_indent: u32,
    /// In `if`/`while`/`for`/`match` heads: `a..` doesn't take `{` as its end.
    no_block_range: bool,
    /// In match guards: `x => …` isn't a lambda.
    no_lambda: bool,
    /// In `x[…]`: what a `#` there stands for.
    len_base: LenBase,
    /// In the elements of `[…]`: `k: loop { … }` is a map entry, not a
    /// labeled loop.
    no_label: bool,
    /// Labels of the enclosing loops, innermost last.
    labels: Vec<String>,
    /// Token index of the last reported error, to avoid cascades.
    last_error_at: Option<usize>,
    /// How deeply expressions, blocks, patterns and types are nested right now.
    depth: u32,
    /// Set once the input nests deeper than [`MAX_DEPTH`]: the rest of the
    /// file is skipped and no further diagnostics are reported.
    bailed: bool,
}

/// The deepest nesting of expressions, blocks, patterns and types the parser
/// accepts. Recursive descent uses the native stack, so without a limit a
/// few hundred `(` would overflow it; past the limit the parser reports
/// S0211 and gives up on the rest of the file instead.
const MAX_DEPTH: u32 = 128;

impl<'a> Parser<'a> {
    fn new(db: &'a SourceDb, file: FileId, raw: Vec<Token>) -> Self {
        let mut tokens = Vec::with_capacity(raw.len());
        let mut docs: HashMap<usize, Vec<String>> = HashMap::new();
        let mut pending: Vec<String> = Vec::new();
        for tok in raw {
            match tok.kind {
                TokenKind::DocComment(text) => pending.push(text),
                _ => {
                    if !pending.is_empty() {
                        docs.insert(tokens.len(), std::mem::take(&mut pending));
                    }
                    tokens.push(tok);
                }
            }
        }
        debug_assert!(matches!(tokens.last(), Some(Token { kind: TokenKind::Eof, .. })));
        Self {
            db,
            file,
            tokens,
            docs,
            pos: 0,
            diags: Vec::new(),
            nl_stack: vec![true],
            stmt_indent: 0,
            no_block_range: false,
            no_lambda: false,
            len_base: LenBase::Outside,
            no_label: false,
            labels: Vec::new(),
            last_error_at: None,
            depth: 0,
            bailed: false,
        }
    }

    /// Runs `f` one nesting level deeper. At [`MAX_DEPTH`] it reports S0211
    /// once, skips to the end of the file and returns `on_limit(span)` instead.
    fn nested<T>(&mut self, on_limit: impl FnOnce(Span) -> T, f: impl FnOnce(&mut Self) -> T) -> T {
        if self.depth >= MAX_DEPTH {
            let span = self.here();
            if !self.bailed {
                self.report(
                    Diagnostic::error(codes::S0211, "this is nested too deeply")
                        .primary(self.peek().span, format!("more than {MAX_DEPTH} levels of nesting"))
                        .help("split the expression up with `let`, or move the inner part into a function"),
                );
                self.bailed = true;
                self.pos = self.tokens.len() - 1;
                self.last_error_at = Some(self.pos);
            }
            return on_limit(span);
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    // ------------------------------------------------------------ cursor

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn nth(&self, n: usize) -> &Token {
        let last = self.tokens.len() - 1;
        &self.tokens[(self.pos + n).min(last)]
    }

    fn kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn nth_kind(&self, n: usize) -> &TokenKind {
        &self.nth(n).kind
    }

    fn at_eof(&self) -> bool {
        matches!(self.kind(), TokenKind::Eof)
    }

    fn at_punct(&self, p: Punct) -> bool {
        *self.kind() == TokenKind::Punct(p)
    }

    fn nth_is_punct(&self, n: usize, p: Punct) -> bool {
        *self.nth_kind(n) == TokenKind::Punct(p)
    }

    fn at_kw(&self, k: Keyword) -> bool {
        *self.kind() == TokenKind::Keyword(k)
    }

    fn nth_is_kw(&self, n: usize, k: Keyword) -> bool {
        *self.nth_kind(n) == TokenKind::Keyword(k)
    }

    /// The current token is the contextual keyword `word` (lexed as an identifier).
    fn at_contextual(&self, word: &str) -> bool {
        self.nth_is_contextual(0, word)
    }

    fn nth_is_contextual(&self, n: usize, word: &str) -> bool {
        let tok = self.nth(n);
        tok.kind == TokenKind::Ident && self.text(tok.span) == word
    }

    fn text(&self, span: Span) -> &'a str {
        let db: &'a SourceDb = self.db;
        db.slice(span)
    }

    /// Consumes the current token (never moves past `Eof`).
    fn bump(&mut self) -> Token {
        let tok = self.peek().clone();
        if !matches!(tok.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        tok
    }

    fn eat_punct(&mut self, p: Punct) -> Option<Span> {
        self.at_punct(p).then(|| self.bump().span)
    }

    fn eat_kw(&mut self, k: Keyword) -> Option<Span> {
        self.at_kw(k).then(|| self.bump().span)
    }

    /// Span of the most recently consumed token.
    fn prev_span(&self) -> Span {
        if self.pos == 0 { self.peek().span } else { self.tokens[self.pos - 1].span }
    }

    /// A zero-width span at the start of the current token.
    fn here(&self) -> Span {
        let s = self.peek().span;
        Span::new(s.file, s.lo, s.lo)
    }

    /// From `start` to the end of the last consumed token.
    fn span_from(&self, start: Span) -> Span {
        let end = self.prev_span();
        if end.hi < start.lo { start } else { Span::new(start.file, start.lo, end.hi) }
    }

    fn take_docs(&mut self) -> Vec<String> {
        self.docs.remove(&self.pos).unwrap_or_default()
    }

    // ------------------------------------------------------------ diagnostics

    fn describe_current(&self) -> String {
        let tok = self.peek();
        match &tok.kind {
            TokenKind::Eof => "the end of the file".into(),
            TokenKind::Punct(p) => format!("`{p}`"),
            TokenKind::Keyword(k) => format!("keyword `{k}`"),
            TokenKind::Ident | TokenKind::TypeIdent | TokenKind::ConstIdent => {
                format!("`{}`", self.text(tok.span))
            }
            TokenKind::TemplateStart | TokenKind::Str(_) => "a string".into(),
            TokenKind::InterpClose => "`}`".into(),
            TokenKind::InterpSpec(spec) => format!("the format spec `:{spec}`"),
            TokenKind::TemplateText(_) | TokenKind::TemplateEnd => "the rest of the string".into(),
            _ => describe(self.db, tok),
        }
    }

    fn report(&mut self, diag: Diagnostic) {
        if !self.bailed {
            self.diags.push(diag);
        }
    }

    /// Whether the current token is one the lexer already reported.
    fn at_lex_error(&self) -> bool {
        matches!(self.kind(), TokenKind::Error)
    }

    /// Reports "expected X, found Y" at the current token, at most once per
    /// position, and never at a token the lexer already complained about.
    fn error_expected(&mut self, what: &str) {
        if self.last_error_at == Some(self.pos) || self.at_lex_error() {
            self.last_error_at = Some(self.pos);
            return;
        }
        self.last_error_at = Some(self.pos);
        let found = self.describe_current();
        let span = self.peek().span;
        let label = if matches!(self.kind(), TokenKind::Eof) { "the file ends here" } else { "unexpected here" };
        let mut diag = Diagnostic::error(codes::S0200, format!("expected {what}, found {found}")).primary(span, label);
        if self.peek().nl_before && is_continuation(self.kind()) {
            diag = diag.help(format!(
                "a line starting with {found} continues the statement above only when it's indented deeper than \
                 that statement's first line"
            ));
        }
        self.report(diag);
    }

    fn error_at(&mut self, code: Code, span: Span, message: impl Into<String>, label: impl Into<String>) {
        self.last_error_at = Some(self.pos);
        self.report(Diagnostic::error(code, message).primary(span, label));
    }

    fn expect_punct(&mut self, p: Punct) -> Span {
        match self.eat_punct(p) {
            Some(span) => span,
            None => {
                self.error_expected(&format!("`{p}`"));
                self.here()
            }
        }
    }

    fn expect_kw(&mut self, k: Keyword) -> Span {
        match self.eat_kw(k) {
            Some(span) => span,
            None => {
                self.error_expected(&format!("`{k}`"));
                self.here()
            }
        }
    }

    /// Expects `close` for a delimiter opened at `open`, pointing at the
    /// opener when the file ends first.
    fn expect_close(&mut self, close: Punct, open: Span) -> Span {
        if let Some(span) = self.eat_punct(close) {
            return span;
        }
        if self.at_eof() {
            self.report_unclosed(open, close);
        } else if self.last_error_at == Some(self.pos) || self.at_lex_error() {
            self.last_error_at = Some(self.pos);
        } else {
            self.last_error_at = Some(self.pos);
            let found = self.describe_current();
            let opener = self.text(open).to_string();
            self.report(
                Diagnostic::error(codes::S0200, format!("expected `{close}`, found {found}"))
                    .primary(self.peek().span, "unexpected here")
                    .secondary(open, format!("to close this `{opener}`")),
            );
        }
        self.here()
    }

    /// Reports S0201 for `open`, which should have been closed with `close`
    /// before the current token (the end of the file, or a line that can't
    /// belong inside it). At most once per position.
    fn report_unclosed(&mut self, open: Span, close: Punct) {
        if self.last_error_at == Some(self.pos) {
            return;
        }
        self.last_error_at = Some(self.pos);
        let opener = self.text(open).to_string();
        let expected = if self.at_eof() {
            format!("expected `{close}` before the end of the file")
        } else {
            format!("expected `{close}` before this line")
        };
        self.report(
            Diagnostic::error(codes::S0201, format!("unclosed `{opener}`"))
                .primary(open, format!("this `{opener}` is never closed"))
                .secondary(self.here(), expected),
        );
    }

    /// Reads an identifier of any case class, checking it against `case`.
    fn ident(&mut self, case: NameCase, what: &str) -> Ident {
        let tok = self.peek().clone();
        let actual = match tok.kind {
            TokenKind::Ident => NameCase::Value,
            TokenKind::TypeIdent => NameCase::Type,
            TokenKind::ConstIdent => NameCase::Const,
            _ => {
                self.error_expected(what);
                return Ident { name: String::new(), span: self.here() };
            }
        };
        self.bump();
        let name = self.text(tok.span).to_string();
        if case != NameCase::Any && case != actual {
            let (rule, example) = match case {
                NameCase::Value => ("snake_case", "my_name"),
                NameCase::Type => ("PascalCase", "MyName"),
                NameCase::Const => ("SCREAMING_CASE", "MY_NAME"),
                NameCase::Any => unreachable!(),
            };
            self.error_at(
                codes::S0207,
                tok.span,
                format!("{what} `{name}` should be {rule}"),
                format!("{what}s are written like `{example}`"),
            );
        }
        Ident { name, span: tok.span }
    }

    // ------------------------------------------------------------ newlines

    fn nl_significant(&self) -> bool {
        *self.nl_stack.last().unwrap_or(&true)
    }

    /// Runs `f` with newlines significant (`true`, inside braces) or not
    /// (`false`, inside parens and brackets). Restrictions reset inside.
    fn with_nl<T>(&mut self, significant: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        self.nl_stack.push(significant);
        let saved = (self.no_block_range, self.no_lambda, self.no_label);
        self.no_block_range = false;
        self.no_lambda = false;
        self.no_label = false;
        let result = f(self);
        (self.no_block_range, self.no_lambda, self.no_label) = saved;
        self.nl_stack.pop();
        result
    }

    /// Whether the current token starts a new line, in a context where that matters.
    fn at_line_start(&self) -> bool {
        self.peek().nl_before && self.nl_significant()
    }

    /// Whether the current token may continue the expression being parsed.
    fn continues(&self) -> bool {
        if !self.at_line_start() {
            return true;
        }
        self.peek().line_indent > self.stmt_indent && is_continuation(self.kind())
    }

    /// Whether the current token ends a statement: `;`, `}`, the end of the
    /// file, or the start of a new line.
    fn at_stmt_end(&self) -> bool {
        matches!(self.kind(), TokenKind::Eof | TokenKind::Punct(Punct::Semi | Punct::RBrace)) || self.at_line_start()
    }

    /// After a statement or item: requires a newline, `;` or `}`.
    fn stmt_terminator(&mut self) {
        if self.eat_punct(Punct::Semi).is_some() || self.at_stmt_end() {
            return;
        }
        if self.at_lex_error() {
            self.last_error_at = Some(self.pos);
        } else if self.last_error_at.is_none_or(|at| at < self.pos.saturating_sub(8)) {
            let found = self.describe_current();
            let span = self.peek().span;
            self.last_error_at = Some(self.pos);
            self.report(
                Diagnostic::error(codes::S0202, format!("expected a new line or `;` after this, found {found}"))
                    .primary(span, "this should start on its own line"),
            );
        }
        self.recover_to_stmt_end();
    }

    /// After a statement, arm or member that consumed nothing: skips the
    /// offending token and the rest of its statement, so one bad line gives
    /// one error rather than one per token.
    fn skip_bad_stmt(&mut self) {
        self.bump();
        self.recover_to_stmt_end();
    }

    /// Skips tokens until the end of the current statement, keeping nested
    /// brackets balanced. Always consumes at least one token if not at an end.
    fn recover_to_stmt_end(&mut self) {
        let mut depth = 0usize;
        loop {
            match self.kind() {
                TokenKind::Eof => return,
                TokenKind::Punct(Punct::Semi | Punct::RBrace) if depth == 0 => return,
                _ if depth == 0 && self.at_line_start() => return,
                TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.bump();
        }
    }
}

/// Tokens that may start a continuation line: `.`, `?`, and binary operators.
fn is_continuation(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::Punct(p) => matches!(
            p,
            Punct::Dot
                | Punct::Question
                | Punct::DotDot
                | Punct::DotDotEq
                | Punct::EqEq
                | Punct::NotEq
                | Punct::Lt
                | Punct::LtEq
                | Punct::Gt
                | Punct::GtEq
                | Punct::Shl
                | Punct::Shr
                | Punct::Plus
                | Punct::Minus
                | Punct::Star
                | Punct::Slash
                | Punct::Percent
                | Punct::PlusPercent
                | Punct::MinusPercent
                | Punct::StarPercent
                | Punct::PlusPipe
                | Punct::MinusPipe
                | Punct::StarPipe
                | Punct::PlusQuestion
                | Punct::MinusQuestion
                | Punct::StarQuestion
                | Punct::Amp
                | Punct::Pipe
                | Punct::Tilde
        ),
        TokenKind::Keyword(k) => matches!(
            k,
            Keyword::And
                | Keyword::Or
                | Keyword::Mend
                | Keyword::Is
                | Keyword::As
                | Keyword::AsWrap
                | Keyword::AsSat
                | Keyword::AsChecked
        ),
        _ => false,
    }
}

impl Parser<'_> {
    fn file_span(&self) -> Span {
        let len = self.db.file(self.file).text().len() as u32;
        Span::new(self.file, 0, len)
    }
}
