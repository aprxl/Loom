//! Blocks and statements.

use super::{NameCase, Parser};
use crate::ast::{AssignOp, BinaryOp, Block, Expr, ExprKind, FnQualifier, ItemKind, Stmt, StmtKind};
use crate::diag::{Diagnostic, codes};
use crate::token::{Keyword, Punct, TokenKind};

impl Parser<'_> {
    /// `{ stmt* }`. Without a `{`, reports it and returns an empty block, so
    /// statement recovery (not this block) decides what to skip.
    pub(super) fn block(&mut self) -> Block {
        match self.eat_punct(Punct::LBrace) {
            Some(open) => self.block_rest(open),
            None => {
                self.error_expected("`{`");
                Block { stmts: Vec::new(), span: self.here() }
            }
        }
    }

    /// The statements and closing brace of a block whose `{` is already consumed.
    pub(super) fn block_rest(&mut self, open: crate::source::Span) -> Block {
        self.nested(|span| Block { stmts: Vec::new(), span }, |p| p.block_rest_inner(open))
    }

    fn block_rest_inner(&mut self, open: crate::source::Span) -> Block {
        let stmts = self.with_nl(true, |p| {
            let mut stmts = Vec::new();
            loop {
                while p.eat_punct(Punct::Semi).is_some() {}
                if p.at_punct(Punct::RBrace) || p.at_eof() {
                    break;
                }
                let before = p.pos;
                stmts.push(p.stmt());
                p.stmt_terminator();
                if p.pos == before {
                    p.skip_bad_stmt();
                }
            }
            stmts
        });
        self.expect_close(Punct::RBrace, open);
        Block { stmts, span: self.span_from(open) }
    }

    pub(super) fn stmt(&mut self) -> Stmt {
        let saved = self.stmt_indent;
        self.stmt_indent = self.peek().line_indent;
        let stmt = self.stmt_inner();
        self.stmt_indent = saved;
        stmt
    }

    fn stmt_inner(&mut self) -> Stmt {
        let start = self.peek().span;
        let kind = match self.kind() {
            TokenKind::Keyword(Keyword::Let | Keyword::Var) => self.let_stmt(),
            TokenKind::Keyword(Keyword::Given) => {
                self.bump();
                if matches!(self.kind(), TokenKind::Ident) && self.nth_is_punct(1, Punct::Eq) {
                    let name = self.ident(NameCase::Value, "name");
                    self.bump();
                    StmtKind::Given { name: Some(name), value: self.expr() }
                } else {
                    StmtKind::Given { name: None, value: self.expr() }
                }
            }
            TokenKind::Keyword(Keyword::Spool) => {
                self.bump();
                let name = self.ident(NameCase::Value, "spool name");
                self.expect_punct(Punct::Eq);
                StmtKind::Spool { name, init: self.expr() }
            }
            TokenKind::Keyword(Keyword::Defer) => {
                self.bump();
                if matches!(self.kind(), TokenKind::Ident) && self.nth_is_punct(1, Punct::FatArrow) {
                    let binding = self.ident(NameCase::Value, "name");
                    self.bump();
                    StmtKind::Defer { binding: Some(binding), body: self.stmt_expr() }
                } else {
                    StmtKind::Defer { binding: None, body: self.stmt_expr() }
                }
            }
            // Labeled loops (`outer: for …`) are expressions: see `primary`.
            _ if self.at_item_start() => match self.item() {
                Some(item) => {
                    if let ItemKind::Fn(f) = &item.kind
                        && f.qualifier == FnQualifier::Test
                    {
                        let span = f.name.span;
                        self.error_at(
                            codes::S0212,
                            span,
                            "a test can't be inside a function",
                            "move it to the top level",
                        );
                    }
                    StmtKind::Item(Box::new(item))
                }
                None => StmtKind::Expr(Expr { kind: ExprKind::Error, span: start }),
            },
            _ => StmtKind::Expr(self.stmt_expr()),
        };
        Stmt { kind, span: self.span_from(start) }
    }

    fn let_stmt(&mut self) -> StmtKind {
        let mutable = self.bump().kind == TokenKind::Keyword(Keyword::Var);
        let pat = self.pat_top(false);
        let ty = self.eat_punct(Punct::Colon).map(|_| self.ty());
        let init = if self.eat_punct(Punct::Eq).is_some() {
            self.expr()
        } else {
            let span = self.here();
            let keyword = if mutable { "var" } else { "let" };
            self.error_at(
                codes::S0210,
                span,
                format!("`{keyword}` needs an initializer"),
                "expected `=` and a value here",
            );
            Expr { kind: ExprKind::Error, span }
        };
        StmtKind::Let { mutable, pat, ty, init }
    }

    /// An expression that may also be an assignment: statements, match-arm
    /// bodies and lambda bodies.
    pub(super) fn stmt_expr(&mut self) -> Expr {
        let target = self.expr_bp(0);
        let Some(op) = assign_op(self.kind()) else {
            return target;
        };
        if self.at_line_start() {
            return target;
        }
        self.bump();
        let value = self.expr();
        let span = target.span.to(value.span);
        Expr { kind: ExprKind::Assign { target: Box::new(target), op, value: Box::new(value) }, span }
    }

    /// Reports an assignment operator where only an expression is allowed.
    pub(super) fn reject_assignment(&mut self) {
        if assign_op(self.kind()).is_none() || self.at_line_start() || self.last_error_at == Some(self.pos) {
            return;
        }
        let span = self.peek().span;
        self.last_error_at = Some(self.pos);
        self.report(
            Diagnostic::error(codes::S0205, "assignments are statements, not expressions")
                .primary(span, "can't assign here")
                .help("use `==` to compare, or give the assignment its own statement"),
        );
    }
}

pub(super) fn assign_op(kind: &TokenKind) -> Option<AssignOp> {
    let TokenKind::Punct(p) = kind else { return None };
    Some(match p {
        Punct::Eq => AssignOp::Assign,
        Punct::PlusEq => AssignOp::Compound(BinaryOp::Add),
        Punct::MinusEq => AssignOp::Compound(BinaryOp::Sub),
        Punct::StarEq => AssignOp::Compound(BinaryOp::Mul),
        Punct::SlashEq => AssignOp::Compound(BinaryOp::Div),
        Punct::PercentEq => AssignOp::Compound(BinaryOp::Rem),
        Punct::AmpEq => AssignOp::Compound(BinaryOp::BitAnd),
        Punct::PipeEq => AssignOp::Compound(BinaryOp::BitOr),
        Punct::TildeEq => AssignOp::Compound(BinaryOp::BitXor),
        Punct::ShlEq => AssignOp::Compound(BinaryOp::Shl),
        Punct::ShrEq => AssignOp::Compound(BinaryOp::Shr),
        Punct::PlusPercentEq => AssignOp::Compound(BinaryOp::AddWrap),
        Punct::MinusPercentEq => AssignOp::Compound(BinaryOp::SubWrap),
        Punct::StarPercentEq => AssignOp::Compound(BinaryOp::MulWrap),
        Punct::PlusPipeEq => AssignOp::Compound(BinaryOp::AddSat),
        Punct::MinusPipeEq => AssignOp::Compound(BinaryOp::SubSat),
        Punct::StarPipeEq => AssignOp::Compound(BinaryOp::MulSat),
        _ => return None,
    })
}
