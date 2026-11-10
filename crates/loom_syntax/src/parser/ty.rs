//! Types.
//!
//! Loosest to tightest: `A | B` (union), `T ! E` (result), `A => B`
//! (function, right-associative), `T@region`, prefix `? ^ * any`, atoms.

use super::{NameCase, Parser};
use crate::ast::{GenericArg, Ident, Ty, TyKind};
use crate::token::{Keyword, Punct, TokenKind};

impl Parser<'_> {
    /// A full type.
    pub(super) fn ty(&mut self) -> Ty {
        self.ty_union()
    }

    /// A type without unions, results or function arrows: after `as`, in
    /// type-test patterns, and in bounds, where `|`, `!` and `=>` mean
    /// something else.
    pub(super) fn ty_cast(&mut self) -> Ty {
        self.ty_region()
    }

    fn ty_union(&mut self) -> Ty {
        let first = self.ty_result();
        if !self.at_punct(Punct::Pipe) || self.at_line_start() {
            return first;
        }
        let start = first.span;
        let mut members = vec![first];
        while !self.at_line_start() && self.eat_punct(Punct::Pipe).is_some() {
            members.push(self.ty_result());
        }
        Ty { kind: TyKind::Union(members), span: self.span_from(start) }
    }

    fn ty_result(&mut self) -> Ty {
        let ok = self.ty_fn();
        if !self.at_punct(Punct::Bang) || self.at_line_start() {
            return ok;
        }
        self.bump();
        let err = self.err_ty();
        let span = ok.span.to(err.span);
        Ty { kind: TyKind::Result { ok: Box::new(ok), err: Box::new(err) }, span }
    }

    /// The error side of `T ! E`: a single type, `(A | B)`, or `_`.
    pub(super) fn err_ty(&mut self) -> Ty {
        self.ty_region()
    }

    fn ty_fn(&mut self) -> Ty {
        // `(A, B) => C`: parentheses right before `=>` hold the parameter
        // list, so `((A, B)) => C` takes one tuple and `(()) => C` takes a unit.
        if self.at_punct(Punct::LParen) && self.paren_is_lambda() {
            let open = self.bump();
            let params = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::ty));
            self.expect_close(Punct::RParen, open);
            self.expect_punct(Punct::FatArrow);
            let ret = self.ty();
            return Ty { kind: TyKind::Fn { params, ret: Box::new(ret) }, span: self.span_from(open) };
        }
        let lhs = self.ty_region();
        if !self.at_punct(Punct::FatArrow) || self.at_line_start() {
            return lhs;
        }
        self.bump();
        let start = lhs.span;
        let ret = self.ty();
        Ty { kind: TyKind::Fn { params: vec![lhs], ret: Box::new(ret) }, span: self.span_from(start) }
    }

    fn ty_region(&mut self) -> Ty {
        let base = self.ty_prefix();
        if !self.at_punct(Punct::At) || self.at_line_start() {
            return base;
        }
        self.bump();
        // `^T@self`: the spool a method is called on.
        let region = match self.eat_kw(Keyword::SelfValue) {
            Some(span) => Ident { name: "self".into(), span },
            None => self.ident(NameCase::Value, "region name"),
        };
        let span = base.span.to(region.span);
        Ty { kind: TyKind::Region { ty: Box::new(base), region }, span }
    }

    fn ty_prefix(&mut self) -> Ty {
        self.nested(|span| Ty { kind: TyKind::Error, span }, Self::ty_prefix_inner)
    }

    fn ty_prefix_inner(&mut self) -> Ty {
        let start = self.peek().span;
        let wrap: fn(Box<Ty>) -> TyKind = match self.kind() {
            TokenKind::Punct(Punct::Question) => TyKind::Optional,
            TokenKind::Punct(Punct::Caret) => TyKind::Pointer,
            TokenKind::Punct(Punct::Star) => TyKind::RawPointer,
            TokenKind::Keyword(Keyword::Any) => {
                self.bump();
                let wefts = if let Some(open) = self.eat_punct(Punct::LParen) {
                    let wefts = self.with_nl(false, |p| p.bounds());
                    self.expect_close(Punct::RParen, open);
                    wefts
                } else {
                    vec![self.ty_atom()]
                };
                return Ty { kind: TyKind::Any(wefts), span: self.span_from(start) };
            }
            _ => return self.ty_atom(),
        };
        self.bump();
        let inner = self.ty_prefix();
        Ty { kind: wrap(Box::new(inner)), span: self.span_from(start) }
    }

    fn ty_atom(&mut self) -> Ty {
        let tok = self.peek().clone();
        let simple = |kind| Ty { kind, span: tok.span };
        match tok.kind {
            TokenKind::Prim(p) => {
                self.bump();
                simple(TyKind::Prim(p))
            }
            TokenKind::Keyword(Keyword::Type) => {
                self.bump();
                simple(TyKind::TypeType)
            }
            TokenKind::Keyword(Keyword::SelfType) => {
                if self.nth_is_punct(1, Punct::Dot) {
                    self.ty_path()
                } else {
                    self.bump();
                    simple(TyKind::SelfType)
                }
            }
            TokenKind::Ident | TokenKind::TypeIdent | TokenKind::ConstIdent => self.ty_path(),
            TokenKind::Underscore => {
                self.bump();
                simple(TyKind::Infer)
            }
            TokenKind::Punct(Punct::LParen) => {
                let open = self.bump();
                let kind = self.with_nl(false, |p| {
                    if p.at_punct(Punct::RParen) {
                        return TyKind::Unit;
                    }
                    let first = p.ty();
                    if !p.at_punct(Punct::Comma) {
                        return first.kind;
                    }
                    let mut elems = vec![first];
                    while p.eat_punct(Punct::Comma).is_some() {
                        if p.at_punct(Punct::RParen) {
                            break;
                        }
                        elems.push(p.ty());
                    }
                    TyKind::Tuple(elems)
                });
                self.expect_close(Punct::RParen, open);
                Ty { kind, span: self.span_from(open) }
            }
            TokenKind::Punct(Punct::LBracket) => {
                let open = self.bump();
                let kind = self.with_nl(false, |p| {
                    let elem = Box::new(p.ty());
                    if p.eat_punct(Punct::Semi).is_some() {
                        TyKind::Array { elem, len: Box::new(p.expr()) }
                    } else {
                        TyKind::Slice(elem)
                    }
                });
                self.expect_close(Punct::RBracket, open);
                Ty { kind, span: self.span_from(open) }
            }
            _ => {
                self.error_expected("a type");
                Ty { kind: TyKind::Error, span: self.here() }
            }
        }
    }

    /// `a.b.Name`, `Name[args]`, or a compile-time call like `uint(12)`.
    fn ty_path(&mut self) -> Ty {
        let start = self.peek().span;
        let mut segments = vec![self.path_segment()];
        while self.at_punct(Punct::Dot)
            && !self.at_line_start()
            && matches!(self.nth_kind(1), TokenKind::Ident | TokenKind::TypeIdent | TokenKind::ConstIdent)
        {
            self.bump();
            segments.push(self.path_segment());
        }
        if self.at_punct(Punct::LParen) && !self.at_line_start() {
            let open = self.bump();
            let args = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::expr));
            self.expect_close(Punct::RParen, open);
            return Ty { kind: TyKind::ConstCall { segments, args }, span: self.span_from(start) };
        }
        let args = if self.at_punct(Punct::LBracket) && !self.at_line_start() { self.type_args() } else { Vec::new() };
        Ty { kind: TyKind::Path { segments, args }, span: self.span_from(start) }
    }

    fn path_segment(&mut self) -> crate::ast::Ident {
        if self.at_kw(Keyword::SelfType) {
            let span = self.bump();
            return crate::ast::Ident { name: "Self".into(), span };
        }
        self.ident(NameCase::Any, "type name")
    }

    /// `[T, 4]`: generic arguments in a type.
    pub(super) fn type_args(&mut self) -> Vec<GenericArg> {
        let open = self.bump();
        let args = self.with_nl(false, |p| {
            p.comma_list(Punct::RBracket, |p| match p.kind() {
                TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Char(_)
                | TokenKind::Str(_)
                | TokenKind::Keyword(Keyword::True | Keyword::False)
                | TokenKind::Punct(Punct::Minus | Punct::LBrace) => GenericArg::Expr(p.expr()),
                _ => GenericArg::Type(p.ty()),
            })
        });
        self.expect_close(Punct::RBracket, open);
        args
    }

    /// `A + B + C`: weft bounds.
    pub(super) fn bounds(&mut self) -> Vec<Ty> {
        let mut bounds = vec![self.ty_cast()];
        while self.eat_punct(Punct::Plus).is_some() {
            bounds.push(self.ty_cast());
        }
        bounds
    }
}
