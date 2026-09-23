//! Patterns.

use super::{NameCase, Parser};
use crate::ast::{CasePath, FieldPat, Lit, Pat, PatKind};
use crate::diag::{Diagnostic, codes};
use crate::source::Span;
use crate::token::{Keyword, Punct, Radix, TokenKind};
use crate::twill::TwillPattern;

impl Parser<'_> {
    /// A pattern, including `a | b` alternatives. `allow_type_test` enables
    /// `name: Type` at the top level (match arms and `is`; not `let`/`for`,
    /// where `: Type` is an annotation).
    pub(super) fn pat_top(&mut self, allow_type_test: bool) -> Pat {
        let first = self.pat_single(allow_type_test);
        if !self.at_punct(Punct::Pipe) || self.at_line_start() {
            return first;
        }
        let start = first.span;
        let mut alts = vec![first];
        while !self.at_line_start() && self.eat_punct(Punct::Pipe).is_some() {
            alts.push(self.pat_single(allow_type_test));
        }
        Pat { kind: PatKind::Or(alts), span: self.span_from(start) }
    }

    fn pat_single(&mut self, allow_type_test: bool) -> Pat {
        let tok = self.peek().clone();
        let start = tok.span;
        let simple = |kind| Pat { kind, span: start };
        match tok.kind {
            TokenKind::Underscore => {
                self.bump();
                simple(PatKind::Wild)
            }
            TokenKind::Keyword(Keyword::Mut) => {
                self.bump();
                let name = self.ident(NameCase::Value, "binding");
                Pat { kind: PatKind::Bind { name, mutable: true }, span: self.span_from(start) }
            }
            TokenKind::Ident => {
                let name = self.ident(NameCase::Value, "binding");
                if allow_type_test && self.eat_punct(Punct::Colon).is_some() {
                    let ty = self.ty_cast();
                    return Pat { kind: PatKind::TypeTest { name, ty }, span: self.span_from(start) };
                }
                simple(PatKind::Bind { name, mutable: false })
            }
            TokenKind::TypeIdent | TokenKind::ConstIdent | TokenKind::Keyword(Keyword::SelfType) => {
                let mut segments = vec![self.pat_path_segment()];
                while self.at_punct(Punct::Dot)
                    && !self.at_line_start()
                    && matches!(self.nth_kind(1), TokenKind::TypeIdent | TokenKind::ConstIdent)
                {
                    self.bump();
                    segments.push(self.pat_path_segment());
                }
                self.case_pat(start, CasePath::Path(segments))
            }
            TokenKind::Punct(Punct::Dot) => {
                self.bump();
                if let Some(open) = self.eat_punct(Punct::LParen) {
                    let expr = self.with_nl(false, |p| p.expr());
                    self.expect_close(Punct::RParen, open);
                    return Pat { kind: PatKind::ConstCase(Box::new(expr)), span: self.span_from(start) };
                }
                let name = self.ident(NameCase::Type, "case");
                self.case_pat(start, CasePath::Dot(name))
            }
            TokenKind::Keyword(k @ (Keyword::Some | Keyword::Ok | Keyword::Err)) => {
                self.bump();
                let open = self.expect_punct(Punct::LParen);
                let inner = Box::new(self.with_nl(false, |p| p.pat_top(true)));
                self.expect_close(Punct::RParen, open);
                let kind = match k {
                    Keyword::Some => PatKind::Some(inner),
                    Keyword::Ok => PatKind::Ok(inner),
                    _ => PatKind::Err(inner),
                };
                Pat { kind, span: self.span_from(start) }
            }
            TokenKind::Keyword(Keyword::None) => {
                self.bump();
                simple(PatKind::None)
            }
            TokenKind::Twill(pattern) => {
                self.bump();
                self.warn_hex_captures(&pattern, start);
                simple(PatKind::Twill(pattern))
            }
            TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Char(_)
            | TokenKind::Str(_)
            | TokenKind::Bytes(_)
            | TokenKind::Keyword(Keyword::True | Keyword::False)
            | TokenKind::Punct(Punct::Minus) => {
                let lo = self.lit_pat();
                if matches!(lo.kind, PatKind::Twill(_)) {
                    return lo;
                }
                self.maybe_range(lo)
            }
            TokenKind::Punct(p @ (Punct::DotDot | Punct::DotDotEq)) => {
                self.bump();
                let hi = Box::new(self.lit_pat());
                Pat {
                    kind: PatKind::Range { lo: None, hi: Some(hi), inclusive: p == Punct::DotDotEq },
                    span: self.span_from(start),
                }
            }
            TokenKind::Punct(Punct::LParen) => {
                let open = self.bump().span;
                let kind = self.with_nl(false, |p| {
                    if p.at_punct(Punct::RParen) {
                        return PatKind::Tuple(Vec::new());
                    }
                    let first = p.pat_top(true);
                    if !p.at_punct(Punct::Comma) {
                        return first.kind;
                    }
                    let mut elems = vec![first];
                    while p.eat_punct(Punct::Comma).is_some() {
                        if p.at_punct(Punct::RParen) {
                            break;
                        }
                        elems.push(p.pat_top(true));
                    }
                    PatKind::Tuple(elems)
                });
                self.expect_close(Punct::RParen, open);
                Pat { kind, span: self.span_from(open) }
            }
            TokenKind::Punct(Punct::LBracket) => {
                let open = self.bump().span;
                let elems = self.with_nl(false, |p| {
                    p.comma_list(Punct::RBracket, |p| {
                        if p.at_punct(Punct::DotDot) {
                            let start = p.bump().span;
                            let name =
                                matches!(p.kind(), TokenKind::Ident).then(|| p.ident(NameCase::Value, "binding"));
                            return Pat { kind: PatKind::Rest(name), span: p.span_from(start) };
                        }
                        p.pat_top(true)
                    })
                });
                self.expect_close(Punct::RBracket, open);
                Pat { kind: PatKind::Slice(elems), span: self.span_from(open) }
            }
            _ => {
                self.error_expected("a pattern");
                Pat { kind: PatKind::Error, span: self.here() }
            }
        }
    }

    fn pat_path_segment(&mut self) -> crate::ast::Ident {
        if self.at_kw(Keyword::SelfType) {
            let span = self.bump().span;
            return crate::ast::Ident { name: "Self".into(), span };
        }
        self.ident(NameCase::Any, "name")
    }

    /// A case or record pattern, with an optional `(fields)` payload.
    fn case_pat(&mut self, start: Span, path: CasePath) -> Pat {
        if !self.at_punct(Punct::LParen) || self.at_line_start() {
            return Pat { kind: PatKind::Case { path, fields: None, rest: false }, span: self.span_from(start) };
        }
        let open = self.bump().span;
        let mut rest = false;
        let fields = self.with_nl(false, |p| {
            p.comma_list(Punct::RParen, |p| {
                let field_start = p.peek().span;
                if p.at_punct(Punct::DotDot) {
                    p.bump();
                    rest = true;
                    return None;
                }
                let label = if matches!(p.kind(), TokenKind::Ident) && p.nth_is_punct(1, Punct::Colon) {
                    let label = p.ident(NameCase::Value, "field");
                    p.bump();
                    Some(label)
                } else {
                    None
                };
                let pat = p.pat_top(true);
                Some(FieldPat { label, pat, span: p.span_from(field_start) })
            })
        });
        self.expect_close(Punct::RParen, open);
        let fields = fields.into_iter().flatten().collect();
        Pat { kind: PatKind::Case { path, fields: Some(fields), rest }, span: self.span_from(start) }
    }

    /// A literal pattern: `42`, `-1`, `'a'`, `"GET"`, `true`. A hex literal
    /// with lowercase digit letters becomes a twill pattern here (with a warning).
    fn lit_pat(&mut self) -> Pat {
        let start = self.peek().span;
        let negative = self.eat_punct(Punct::Minus).is_some();
        let tok = self.peek().clone();
        let lit = match tok.kind {
            TokenKind::Int(i) => {
                let text = self.text(tok.span);
                if !negative
                    && i.radix == Radix::Hex
                    && i.suffix.is_none()
                    && text[2..].bytes().any(|b| b.is_ascii_lowercase())
                    && let Ok(pattern) = TwillPattern::parse(&text[2..], Radix::Hex)
                {
                    self.bump();
                    self.warn_hex_captures(&pattern, tok.span);
                    return Pat { kind: PatKind::Twill(Box::new(pattern)), span: tok.span };
                }
                Lit::Int(i)
            }
            TokenKind::Float(f) => Lit::Float(f),
            TokenKind::Char(c) if !negative => Lit::Char(c),
            TokenKind::Str(s) if !negative => Lit::Str(s),
            TokenKind::Bytes(b) if !negative => Lit::Bytes(b),
            TokenKind::Keyword(Keyword::True) if !negative => Lit::Bool(true),
            TokenKind::Keyword(Keyword::False) if !negative => Lit::Bool(false),
            _ => {
                self.error_expected(if negative { "a number after `-`" } else { "a literal" });
                return Pat { kind: PatKind::Error, span: self.span_from(start) };
            }
        };
        self.bump();
        Pat { kind: PatKind::Lit { lit, negative }, span: self.span_from(start) }
    }

    fn maybe_range(&mut self, lo: Pat) -> Pat {
        let inclusive = match self.kind() {
            TokenKind::Punct(Punct::DotDot) if !self.at_line_start() => false,
            TokenKind::Punct(Punct::DotDotEq) if !self.at_line_start() => true,
            _ => return lo,
        };
        self.bump();
        let hi_follows = matches!(
            self.kind(),
            TokenKind::Int(_) | TokenKind::Float(_) | TokenKind::Char(_) | TokenKind::Punct(Punct::Minus)
        );
        let hi = hi_follows.then(|| Box::new(self.lit_pat()));
        let start = lo.span;
        Pat { kind: PatKind::Range { lo: Some(Box::new(lo)), hi, inclusive }, span: self.span_from(start) }
    }

    fn warn_hex_captures(&mut self, pattern: &TwillPattern, span: Span) {
        let letters = pattern.hex_digit_captures();
        if letters.is_empty() {
            return;
        }
        let list: Vec<String> = letters.iter().map(|c| format!("`{c}`")).collect();
        let text = self.text(span);
        let suggestion: String = text[..2]
            .chars()
            .chain(text[2..].chars().map(|c| if letters.contains(&c) { c.to_ascii_uppercase() } else { c }))
            .collect();
        let which = if letters.len() == 1 { "which is also a hex digit" } else { "which are also hex digits" };
        self.report(
            Diagnostic::warning(codes::S0206, format!("this twill pattern captures {}, {which}", list.join(", ")))
                .primary(span, "lowercase letters in a pattern are captures")
                .help(format!("to match the digits literally, write them in uppercase: `{suggestion}`")),
        );
    }
}
