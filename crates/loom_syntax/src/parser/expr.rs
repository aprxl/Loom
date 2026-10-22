//! Expressions: a Pratt parser.
//!
//! Binding powers, loosest to tightest (see the table in `design/01-syntax.md`):
//!
//! | bp | operators |
//! |----|-----------|
//! | 2  | `mend` |
//! | 4  | `or` |
//! | 6  | `and` |
//! | 8  | prefix `not` (its operand is parsed at the comparison level) |
//! | 10 | `== != < <= > >= is` (comparisons chain) |
//! | 12 | `..` `..=` |
//! | 14 | `\|` |
//! | 16 | `~` (xor) |
//! | 18 | `&` |
//! | 20 | `<< >>` |
//! | 22 | `+ -` and their `% \| ?` variants |
//! | 24 | `* / %` and their variants |
//! | 26 | `as as% as\| as?` |
//! | 28 | prefix `- ~ ^` |
//! | —  | postfix: calls, `[…]`, `.`, `?`, `^`, `.{ }` |

use super::{NameCase, Parser};
use crate::ast::{
    Arg, Arm, BinaryOp, CastKind, Expr, ExprKind, FieldName, GenericArg, Ident, LambdaParam, Lit, PlaceMode,
    TemplatePart, UnaryOp,
};
use crate::diag::{Diagnostic, codes};
use crate::source::Span;
use crate::token::{Keyword, Punct, TokenKind};

const BP_MEND: u8 = 2;
const BP_OR: u8 = 4;
const BP_AND: u8 = 6;
const BP_CMP: u8 = 10;
const BP_RANGE: u8 = 12;
const BP_BITOR: u8 = 14;
const BP_XOR: u8 = 16;
const BP_BITAND: u8 = 18;
const BP_SHIFT: u8 = 20;
const BP_ADD: u8 = 22;
const BP_MUL: u8 = 24;
const BP_AS: u8 = 26;
const BP_PREFIX: u8 = 28;

enum Infix {
    Binary(BinaryOp, u8),
    Cmp(BinaryOp),
    Is,
    Range { inclusive: bool },
    As(CastKind),
    Mend,
}

impl Infix {
    fn bp(&self) -> u8 {
        match self {
            Infix::Binary(_, bp) => *bp,
            Infix::Cmp(_) | Infix::Is => BP_CMP,
            Infix::Range { .. } => BP_RANGE,
            Infix::As(_) => BP_AS,
            Infix::Mend => BP_MEND,
        }
    }
}

fn infix_of(kind: &TokenKind) -> Option<Infix> {
    use BinaryOp as B;
    Some(match kind {
        TokenKind::Punct(p) => match p {
            Punct::Plus => Infix::Binary(B::Add, BP_ADD),
            Punct::Minus => Infix::Binary(B::Sub, BP_ADD),
            Punct::PlusPercent => Infix::Binary(B::AddWrap, BP_ADD),
            Punct::MinusPercent => Infix::Binary(B::SubWrap, BP_ADD),
            Punct::PlusPipe => Infix::Binary(B::AddSat, BP_ADD),
            Punct::MinusPipe => Infix::Binary(B::SubSat, BP_ADD),
            Punct::PlusQuestion => Infix::Binary(B::AddChecked, BP_ADD),
            Punct::MinusQuestion => Infix::Binary(B::SubChecked, BP_ADD),
            Punct::Star => Infix::Binary(B::Mul, BP_MUL),
            Punct::Slash => Infix::Binary(B::Div, BP_MUL),
            Punct::Percent => Infix::Binary(B::Rem, BP_MUL),
            Punct::StarPercent => Infix::Binary(B::MulWrap, BP_MUL),
            Punct::StarPipe => Infix::Binary(B::MulSat, BP_MUL),
            Punct::StarQuestion => Infix::Binary(B::MulChecked, BP_MUL),
            Punct::Shl => Infix::Binary(B::Shl, BP_SHIFT),
            Punct::Shr => Infix::Binary(B::Shr, BP_SHIFT),
            Punct::Amp => Infix::Binary(B::BitAnd, BP_BITAND),
            Punct::Tilde => Infix::Binary(B::BitXor, BP_XOR),
            Punct::Pipe => Infix::Binary(B::BitOr, BP_BITOR),
            Punct::EqEq => Infix::Cmp(B::Eq),
            Punct::NotEq => Infix::Cmp(B::Ne),
            Punct::Lt => Infix::Cmp(B::Lt),
            Punct::LtEq => Infix::Cmp(B::Le),
            Punct::Gt => Infix::Cmp(B::Gt),
            Punct::GtEq => Infix::Cmp(B::Ge),
            Punct::DotDot => Infix::Range { inclusive: false },
            Punct::DotDotEq => Infix::Range { inclusive: true },
            _ => return None,
        },
        TokenKind::Keyword(k) => match k {
            Keyword::And => Infix::Binary(B::And, BP_AND),
            Keyword::Or => Infix::Binary(B::Or, BP_OR),
            Keyword::Is => Infix::Is,
            Keyword::As => Infix::As(CastKind::Checked),
            Keyword::AsWrap => Infix::As(CastKind::Wrap),
            Keyword::AsSat => Infix::As(CastKind::Sat),
            Keyword::AsChecked => Infix::As(CastKind::Opt),
            Keyword::Mend => Infix::Mend,
            _ => return None,
        },
        _ => return None,
    })
}

/// Whether a token can begin an expression.
pub(super) fn can_start_expr(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::Ident
        | TokenKind::TypeIdent
        | TokenKind::ConstIdent
        | TokenKind::Prim(_)
        | TokenKind::Underscore
        | TokenKind::Int(_)
        | TokenKind::Float(_)
        | TokenKind::Twill(_)
        | TokenKind::Char(_)
        | TokenKind::Str(_)
        | TokenKind::Bytes(_)
        | TokenKind::TemplateStart => true,
        TokenKind::Keyword(k) => matches!(
            k,
            Keyword::True
                | Keyword::False
                | Keyword::None
                | Keyword::Some
                | Keyword::Ok
                | Keyword::Err
                | Keyword::SelfValue
                | Keyword::SelfType
                | Keyword::If
                | Keyword::Match
                | Keyword::For
                | Keyword::While
                | Keyword::Loop
                | Keyword::Loom
                | Keyword::Thread
                | Keyword::Break
                | Keyword::Continue
                | Keyword::Return
                | Keyword::Fail
                | Keyword::Yield
                | Keyword::Pull
                | Keyword::Unsafe
                | Keyword::Const
                | Keyword::Not
        ),
        TokenKind::Punct(p) => matches!(
            p,
            Punct::Minus
                | Punct::Tilde
                | Punct::Caret
                | Punct::Dot
                | Punct::LParen
                | Punct::LBracket
                | Punct::LBrace
                | Punct::DotDot
                | Punct::DotDotEq
        ),
        _ => false,
    }
}

impl Parser<'_> {
    /// A full expression (no assignment).
    pub(super) fn expr(&mut self) -> Expr {
        let e = self.expr_bp(0);
        self.reject_assignment();
        e
    }

    /// An expression in a head position (`if`, `while`, `for … in`, `match`):
    /// an open-ended range there stops before `{`.
    pub(super) fn head_expr(&mut self) -> Expr {
        let saved = self.no_block_range;
        self.no_block_range = true;
        let e = self.expr();
        self.no_block_range = saved;
        e
    }

    pub(super) fn expr_bp(&mut self, min_bp: u8) -> Expr {
        self.nested(|span| Expr { kind: ExprKind::Error, span }, |p| p.expr_bp_inner(min_bp))
    }

    fn expr_bp_inner(&mut self, min_bp: u8) -> Expr {
        let mut lhs = self.prefix_expr();
        loop {
            if !self.continues() {
                break;
            }
            if self.at_punct(Punct::PullArrow) && BP_CMP >= min_bp {
                lhs = self.pull_arrow_in_expr(lhs);
                continue;
            }
            let Some(infix) = infix_of(self.kind()) else { break };
            if infix.bp() < min_bp {
                break;
            }
            lhs = match infix {
                Infix::Binary(op, bp) => {
                    self.bump();
                    let rhs = self.expr_bp(bp + 1);
                    binary(op, lhs, rhs)
                }
                Infix::Cmp(_) => self.comparison(lhs),
                Infix::Is => {
                    self.bump();
                    let pat = self.pat_top(true);
                    let span = lhs.span.to(pat.span);
                    Expr { kind: ExprKind::Is { expr: Box::new(lhs), pat: Box::new(pat) }, span }
                }
                Infix::Range { inclusive } => {
                    let op = self.bump().span;
                    let hi = self.range_hi_follows().then(|| Box::new(self.expr_bp(BP_RANGE + 1)));
                    let end = hi.as_ref().map_or(op, |h| h.span);
                    let span = lhs.span.to(end);
                    self.reject_chained_range();
                    Expr { kind: ExprKind::Range { lo: Some(Box::new(lhs)), hi, inclusive }, span }
                }
                Infix::As(kind) => {
                    self.bump();
                    let ty = self.ty_cast();
                    let span = lhs.span.to(ty.span);
                    Expr { kind: ExprKind::Cast { expr: Box::new(lhs), kind, ty: Box::new(ty) }, span }
                }
                Infix::Mend => {
                    self.bump();
                    let binding = if matches!(self.kind(), TokenKind::Ident) && self.nth_is_punct(1, Punct::FatArrow) {
                        let name = self.ident(NameCase::Value, "error name");
                        self.bump();
                        Some(name)
                    } else {
                        None
                    };
                    let handler = self.expr_bp(BP_MEND + 1);
                    let span = lhs.span.to(handler.span);
                    Expr { kind: ExprKind::Mend { expr: Box::new(lhs), binding, handler: Box::new(handler) }, span }
                }
            };
        }
        lhs
    }

    /// One or more comparisons. Two or more make a chain: `a < b <= c`.
    fn comparison(&mut self, first: Expr) -> Expr {
        let mut rest: Vec<(BinaryOp, Expr, Span)> = Vec::new();
        while self.continues()
            && let Some(Infix::Cmp(op)) = infix_of(self.kind())
        {
            let op_span = self.bump().span;
            let rhs = self.expr_bp(BP_CMP + 1);
            rest.push((op, rhs, op_span));
        }
        if rest.len() == 1 {
            let (op, rhs, _) = rest.pop().expect("one comparison");
            return binary(op, first, rhs);
        }
        let less = rest.iter().any(|(op, ..)| matches!(op, BinaryOp::Lt | BinaryOp::Le));
        let greater = rest.iter().any(|(op, ..)| matches!(op, BinaryOp::Gt | BinaryOp::Ge));
        if less && greater {
            let span = rest[1].2;
            self.report(
                Diagnostic::error(codes::S0203, "comparison chain mixes `<` and `>`")
                    .primary(span, "this goes the other way")
                    .help("a chain like `a < b > c` is never what you meant; split it with `and`"),
            );
        }
        let end = rest.last().map_or(first.span, |(_, e, _)| e.span);
        let span = first.span.to(end);
        let rest = rest.into_iter().map(|(op, e, _)| (op, e)).collect();
        Expr { kind: ExprKind::Chain { first: Box::new(first), rest }, span }
    }

    /// Ranges don't associate: `a..b..c` is an error (parsed as `(a..b)..c`
    /// to carry on).
    fn reject_chained_range(&mut self) {
        if !self.continues()
            || !matches!(self.kind(), TokenKind::Punct(Punct::DotDot | Punct::DotDotEq))
            || self.last_error_at == Some(self.pos)
        {
            return;
        }
        let span = self.peek().span;
        self.last_error_at = Some(self.pos);
        self.report(
            Diagnostic::error(codes::S0200, "ranges can't be chained")
                .primary(span, "a second range operator")
                .help("put parentheses around the range you mean, like `(a..b)`"),
        );
    }

    /// `a <~ b` in an expression: `<~` is always one token (the pull arrow of
    /// a shuttle signature). Reports it and carries on as if it were `a < ~b`.
    fn pull_arrow_in_expr(&mut self, lhs: Expr) -> Expr {
        self.last_error_at = Some(self.pos);
        let arrow = self.bump().span;
        self.report(
            Diagnostic::error(codes::S0200, "`<~` is the pull arrow, not `<` followed by `~`")
                .primary(arrow, "one token, only used in shuttle signatures")
                .help("to compare with a bitwise not, write `a < ~b` with a space"),
        );
        let operand = self.expr_bp(BP_PREFIX);
        let span = arrow.to(operand.span);
        let rhs = Expr { kind: ExprKind::Unary { op: UnaryOp::BitNot, operand: Box::new(operand) }, span };
        binary(BinaryOp::Lt, lhs, rhs)
    }

    fn range_hi_follows(&self) -> bool {
        !self.at_line_start() && can_start_expr(self.kind()) && !(self.no_block_range && self.at_punct(Punct::LBrace))
    }

    fn prefix_expr(&mut self) -> Expr {
        let start = self.peek().span;
        let op = match self.kind() {
            TokenKind::Punct(Punct::Minus) => Some((UnaryOp::Neg, BP_PREFIX)),
            TokenKind::Punct(Punct::Tilde) => Some((UnaryOp::BitNot, BP_PREFIX)),
            TokenKind::Punct(Punct::Caret) => Some((UnaryOp::Alloc, BP_PREFIX)),
            TokenKind::Keyword(Keyword::Not) => Some((UnaryOp::Not, BP_CMP)),
            _ => None,
        };
        if let Some((op, bp)) = op {
            self.bump();
            let operand = self.expr_bp(bp);
            let span = start.to(operand.span);
            return Expr { kind: ExprKind::Unary { op, operand: Box::new(operand) }, span };
        }
        if let TokenKind::Punct(p @ (Punct::DotDot | Punct::DotDotEq)) = *self.kind() {
            self.bump();
            let hi = self.range_hi_follows().then(|| Box::new(self.expr_bp(BP_RANGE + 1)));
            let span = hi.as_ref().map_or(start, |h| start.to(h.span));
            self.reject_chained_range();
            return Expr { kind: ExprKind::Range { lo: None, hi, inclusive: p == Punct::DotDotEq }, span };
        }
        let primary = self.primary();
        self.postfix(primary)
    }

    fn postfix(&mut self, mut e: Expr) -> Expr {
        loop {
            if !self.continues() {
                break;
            }
            match self.kind() {
                TokenKind::Punct(Punct::LParen) => {
                    let (args, close) = self.call_args();
                    let span = e.span.to(close);
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), args }, span };
                }
                TokenKind::Punct(Punct::LBracket) => {
                    let (args, close) = self.index_args();
                    let span = e.span.to(close);
                    e = Expr { kind: ExprKind::Index { base: Box::new(e), args }, span };
                }
                TokenKind::Punct(Punct::Dot) => e = self.dot_suffix(e),
                TokenKind::Punct(Punct::Question) => {
                    let q = self.bump().span;
                    let span = e.span.to(q);
                    e = Expr { kind: ExprKind::Try(Box::new(e)), span };
                }
                TokenKind::Punct(Punct::Caret) => {
                    let c = self.bump().span;
                    let span = e.span.to(c);
                    e = Expr { kind: ExprKind::Deref(Box::new(e)), span };
                }
                _ => break,
            }
        }
        e
    }

    /// Reads the current integer token, whose value is `value`, as a tuple
    /// index after a `.`: `t.0`, `.1`.
    fn tuple_index(&mut self, value: u128) -> Option<FieldName> {
        let tok = self.bump();
        match u32::try_from(value) {
            Ok(index) => Some(FieldName::Index(index, tok.span)),
            Err(_) => {
                self.error_at(codes::S0200, tok.span, "tuple index is too large", "not a tuple index");
                None
            }
        }
    }

    /// `.name`, `.0`, `.{ … }` or `.(field)` after an expression.
    fn dot_suffix(&mut self, base: Expr) -> Expr {
        let base_span = base.span;
        self.bump();
        let tok = self.peek().clone();
        let kind = match tok.kind {
            TokenKind::Punct(Punct::LBrace) => {
                let open = self.bump().span;
                let body = self.block_rest(open);
                ExprKind::DotBlock { subject: Box::new(base), body }
            }
            TokenKind::Punct(Punct::LParen) => {
                let open = self.bump().span;
                let field = self.with_nl(false, |p| p.expr());
                self.expect_close(Punct::RParen, open);
                ExprKind::ConstField { base: Box::new(base), field: Box::new(field) }
            }
            TokenKind::Ident | TokenKind::TypeIdent | TokenKind::ConstIdent => {
                let name = self.ident(NameCase::Any, "field name");
                ExprKind::Field { base: Box::new(base), field: FieldName::Named(name) }
            }
            TokenKind::Int(lit) => match self.tuple_index(lit.value) {
                Some(field) => ExprKind::Field { base: Box::new(base), field },
                None => ExprKind::Error,
            },
            _ => {
                self.error_expected("a field name after `.`");
                return Expr { kind: ExprKind::Error, span: base_span };
            }
        };
        let span = self.span_from(base_span);
        Expr { kind, span }
    }

    /// `(args)` of a call.
    pub(super) fn call_args(&mut self) -> (Vec<Arg>, Span) {
        let open = self.expect_punct(Punct::LParen);
        let args = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::arg));
        let close = self.expect_close(Punct::RParen, open);
        (args, close)
    }

    fn arg(&mut self) -> Arg {
        let start = self.peek().span;
        if self.at_kw(Keyword::Given) && self.nth_is_punct(1, Punct::Colon) {
            self.bump();
            self.bump();
            let value = self.expr();
            return Arg { label: None, is_given: true, mutable: false, value, span: self.span_from(start) };
        }
        let label =
            if self.nth_is_punct(1, Punct::Colon) && matches!(self.kind(), TokenKind::Ident | TokenKind::Keyword(_)) {
                let tok = self.bump();
                self.bump();
                Some(Ident { name: self.text(tok.span).to_string(), span: tok.span })
            } else {
                None
            };
        let mutable = self.eat_kw(Keyword::Mut).is_some();
        let value = self.expr();
        Arg { label, is_given: false, mutable, value, span: self.span_from(start) }
    }

    /// `[args]` after an expression: an index, or explicit generic arguments.
    fn index_args(&mut self) -> (Vec<GenericArg>, Span) {
        let open = self.expect_punct(Punct::LBracket);
        let args = self.with_nl(false, |p| p.comma_list(Punct::RBracket, Self::bracket_arg));
        let close = self.expect_close(Punct::RBracket, open);
        (args, close)
    }

    /// Decides whether an entry of `x[...]` is a type or an expression by its
    /// first token. Name resolution sorts out the rest.
    fn bracket_arg(&mut self) -> GenericArg {
        let is_type = match self.kind() {
            TokenKind::Prim(_) => !self.nth_is_punct(1, Punct::Dot),
            TokenKind::Keyword(Keyword::Any) | TokenKind::Punct(Punct::Question | Punct::Caret | Punct::Star) => true,
            TokenKind::TypeIdent => !(self.nth_is_punct(1, Punct::LParen) || self.nth_is_punct(1, Punct::Dot)),
            // `[T; N]`: an array type, when what follows the `[` starts a type.
            TokenKind::Punct(Punct::LBracket) => match self.nth_kind(1) {
                TokenKind::Prim(_) => !self.nth_is_punct(2, Punct::Dot),
                TokenKind::TypeIdent => !(self.nth_is_punct(2, Punct::LParen) || self.nth_is_punct(2, Punct::Dot)),
                TokenKind::Punct(Punct::LBracket) => true,
                _ => false,
            },
            _ => false,
        };
        if is_type { GenericArg::Type(self.ty()) } else { GenericArg::Expr(self.expr()) }
    }

    /// Parses `item (, item)* ,?` up to (not including) `close`.
    pub(super) fn comma_list<T>(&mut self, close: Punct, mut item: impl FnMut(&mut Self) -> T) -> Vec<T> {
        let mut items = Vec::new();
        loop {
            if self.at_punct(close) || self.at_eof() {
                break;
            }
            let before = self.pos;
            items.push(item(self));
            if self.eat_punct(Punct::Comma).is_none() || self.pos == before {
                break;
            }
        }
        items
    }

    // ------------------------------------------------------------ primaries

    fn primary(&mut self) -> Expr {
        let tok = self.peek().clone();
        let span = tok.span;
        let simple = |kind| Expr { kind, span };
        match tok.kind {
            TokenKind::Int(i) => {
                self.bump();
                simple(ExprKind::Lit(Lit::Int(i)))
            }
            TokenKind::Float(f) => {
                self.bump();
                simple(ExprKind::Lit(Lit::Float(f)))
            }
            TokenKind::Char(c) => {
                self.bump();
                simple(ExprKind::Lit(Lit::Char(c)))
            }
            TokenKind::Str(s) => {
                self.bump();
                simple(ExprKind::Lit(Lit::Str(s)))
            }
            TokenKind::Bytes(b) => {
                self.bump();
                simple(ExprKind::Lit(Lit::Bytes(b)))
            }
            TokenKind::Twill(_) => {
                self.last_error_at = Some(self.pos);
                self.bump();
                self.report(
                    Diagnostic::error(codes::S0204, "twill patterns only work in patterns")
                        .primary(span, "capture letters and `?` digits need a pattern")
                        .help("use it in a `match` arm, a `let`, or after `is`"),
                );
                simple(ExprKind::Error)
            }
            TokenKind::TemplateStart => self.template(),
            TokenKind::Ident => {
                // `outer: for …`, `outer: loop { … }`: a labeled loop.
                if !self.no_label
                    && self.nth_is_punct(1, Punct::Colon)
                    && matches!(
                        self.nth_kind(2),
                        TokenKind::Keyword(Keyword::For | Keyword::While | Keyword::Loop | Keyword::Loom)
                    )
                {
                    let label = self.ident(NameCase::Value, "label");
                    self.bump();
                    return self.labeled_loop(label);
                }
                if !self.no_lambda && self.nth_is_punct(1, Punct::FatArrow) {
                    return self.lambda_single(false);
                }
                if !self.no_lambda
                    && self.text(span) == "take"
                    && matches!(self.nth_kind(1), TokenKind::Ident)
                    && self.nth_is_punct(2, Punct::FatArrow)
                {
                    self.bump();
                    return self.lambda_single(true);
                }
                let name = self.ident(NameCase::Any, "name");
                simple(ExprKind::Name(name))
            }
            TokenKind::TypeIdent | TokenKind::ConstIdent => {
                let name = self.ident(NameCase::Any, "name");
                simple(ExprKind::Name(name))
            }
            TokenKind::Prim(p) => {
                self.bump();
                simple(ExprKind::Prim(p))
            }
            TokenKind::Underscore => {
                self.bump();
                simple(ExprKind::Hole)
            }
            TokenKind::Keyword(k) => self.keyword_expr(k),
            TokenKind::Punct(Punct::Dot) => {
                self.bump();
                let kind = if let TokenKind::Int(lit) = self.kind() {
                    // `.0` is `self.0` (or the subject's `.0` in a dot-block).
                    let value = lit.value;
                    self.tuple_index(value).map_or(ExprKind::Error, ExprKind::LeadingDot)
                } else {
                    ExprKind::LeadingDot(FieldName::Named(self.ident(NameCase::Any, "name after `.`")))
                };
                Expr { kind, span: self.span_from(span) }
            }
            TokenKind::Punct(Punct::LParen) => self.paren_or_lambda(),
            TokenKind::Punct(Punct::LBracket) => self.array_like(),
            TokenKind::Punct(Punct::LBrace) => {
                let block = self.block();
                let span = block.span;
                Expr { kind: ExprKind::Block(block), span }
            }
            _ => {
                self.error_expected("an expression");
                Expr { kind: ExprKind::Error, span: self.here() }
            }
        }
    }

    fn keyword_expr(&mut self, k: Keyword) -> Expr {
        let start = self.peek().span;
        let simple = |kind| Expr { kind, span: start };
        match k {
            Keyword::True | Keyword::False => {
                self.bump();
                simple(ExprKind::Lit(Lit::Bool(k == Keyword::True)))
            }
            Keyword::None => {
                self.bump();
                simple(ExprKind::Lit(Lit::None))
            }
            Keyword::Some | Keyword::Ok | Keyword::Err => {
                self.bump();
                simple(ExprKind::BuiltinCase(k))
            }
            Keyword::SelfValue => {
                self.bump();
                simple(ExprKind::SelfValue)
            }
            Keyword::SelfType => {
                self.bump();
                simple(ExprKind::SelfType)
            }
            Keyword::If => {
                self.bump();
                self.if_rest(start, false)
            }
            Keyword::Match => self.match_expr(),
            Keyword::For => {
                self.bump();
                self.for_rest(start, None, false, false)
            }
            Keyword::While => self.while_expr(None),
            Keyword::Loop => self.loop_expr(None),
            Keyword::Loom => self.loom_expr(None),
            Keyword::Thread => {
                self.bump();
                let e = self.expr_bp(BP_MEND + 1);
                let span = start.to(e.span);
                Expr { kind: ExprKind::Thread(Box::new(e)), span }
            }
            Keyword::Break => {
                self.bump();
                let label = self.loop_label_ref();
                let value = self.optional_operand();
                Expr { kind: ExprKind::Break { label, value }, span: self.span_from(start) }
            }
            Keyword::Continue => {
                self.bump();
                let mut label = self.loop_label_ref();
                // `continue` carries no value, so a name after it can only be a label.
                if label.is_none() && !self.at_line_start() && matches!(self.kind(), TokenKind::Ident) {
                    let name = self.ident(NameCase::Value, "label");
                    self.error_at(
                        codes::S0200,
                        name.span,
                        format!("no enclosing loop is labeled `{}`", name.name),
                        "not a loop label",
                    );
                    label = Some(name);
                }
                Expr { kind: ExprKind::Continue { label }, span: self.span_from(start) }
            }
            Keyword::Return => {
                self.bump();
                let value = self.optional_operand();
                Expr { kind: ExprKind::Return(value), span: self.span_from(start) }
            }
            Keyword::Fail => {
                self.bump();
                let e = self.expr_bp(BP_MEND + 1);
                let span = start.to(e.span);
                Expr { kind: ExprKind::Fail(Box::new(e)), span }
            }
            Keyword::Yield => {
                self.bump();
                let place = if self.eat_kw(Keyword::Mut).is_some() {
                    Some(PlaceMode::Mut)
                } else if self.eat_kw(Keyword::Ref).is_some() {
                    Some(PlaceMode::Ref)
                } else {
                    None
                };
                let value = self.expr_bp(BP_MEND + 1);
                let span = start.to(value.span);
                Expr { kind: ExprKind::Yield { place, value: Box::new(value) }, span }
            }
            Keyword::Pull => {
                self.bump();
                simple(ExprKind::Pull)
            }
            Keyword::Unsafe => self.unsafe_block(),
            Keyword::Const => self.const_expr(),
            Keyword::Warp if self.nth_is_punct(1, Punct::LBrace) => {
                // `const fn fixed(…) -> type = warp { … }`: in the design
                // (06-compile-time), but not in v1. Skip it as one unit.
                self.recover_to_stmt_end();
                let span = self.span_from(start);
                self.error_at(
                    codes::U0101,
                    span,
                    "anonymous `warp { … }` types aren't supported yet",
                    "not supported in v1",
                );
                Expr { kind: ExprKind::Error, span }
            }
            _ => {
                self.error_expected("an expression");
                Expr { kind: ExprKind::Error, span: self.here() }
            }
        }
    }

    /// The value after `break` / `return`, if one follows on the same line.
    fn optional_operand(&mut self) -> Option<Box<Expr>> {
        let follows = !self.at_line_start()
            && can_start_expr(self.kind())
            && !(self.no_block_range && self.at_punct(Punct::LBrace));
        follows.then(|| Box::new(self.expr_bp(BP_MEND + 1)))
    }

    /// `break outer`: an identifier right after `break`/`continue` is a label
    /// only if an enclosing loop has that label.
    fn loop_label_ref(&mut self) -> Option<Ident> {
        if self.at_line_start() || !matches!(self.kind(), TokenKind::Ident) {
            return None;
        }
        let text = self.text(self.peek().span);
        if self.labels.iter().any(|l| l == text) { Some(self.ident(NameCase::Value, "label")) } else { None }
    }

    fn template(&mut self) -> Expr {
        let start = self.bump().span;
        let mut parts = Vec::new();
        loop {
            let tok = self.peek().clone();
            match tok.kind {
                TokenKind::TemplateText(text) => {
                    self.bump();
                    parts.push(TemplatePart::Text(text, tok.span));
                }
                TokenKind::InterpOpen => {
                    self.bump();
                    if matches!(self.kind(), TokenKind::InterpClose | TokenKind::InterpSpec(_)) {
                        let span = tok.span.to(self.peek().span);
                        self.last_error_at = Some(self.pos);
                        self.report(
                            Diagnostic::error(codes::S0200, "empty interpolation")
                                .primary(span, "expected an expression inside `{…}`")
                                .help("for literal braces in a string, write `{{` and `}}`"),
                        );
                    }
                    let (expr, spec) = self.with_nl(false, |p| {
                        let expr = p.expr();
                        let spec = match p.kind().clone() {
                            TokenKind::InterpSpec(spec) => {
                                p.bump();
                                Some(spec)
                            }
                            _ => None,
                        };
                        (expr, spec)
                    });
                    if !matches!(self.kind(), TokenKind::InterpClose) {
                        self.error_expected("`}` to end the interpolation");
                        // Skip to this interpolation's `}`, stepping over nested templates.
                        let mut nested = 0usize;
                        loop {
                            match self.kind() {
                                TokenKind::Eof => break,
                                TokenKind::InterpClose | TokenKind::TemplateEnd if nested == 0 => break,
                                TokenKind::TemplateStart => nested += 1,
                                TokenKind::TemplateEnd => nested -= 1,
                                _ => {}
                            }
                            self.bump();
                        }
                    }
                    if matches!(self.kind(), TokenKind::InterpClose) {
                        self.bump();
                    }
                    parts.push(TemplatePart::Interp { expr, spec });
                }
                TokenKind::TemplateEnd => {
                    self.bump();
                    break;
                }
                _ => break,
            }
        }
        Expr { kind: ExprKind::Template(parts), span: self.span_from(start) }
    }

    // ------------------------------------------------------------ lambdas & groups

    /// `x => body` (the current token is the parameter).
    fn lambda_single(&mut self, take: bool) -> Expr {
        let start = self.peek().span;
        let name = self.ident(NameCase::Value, "parameter");
        self.expect_punct(Punct::FatArrow);
        let body = self.stmt_expr();
        let span = start.to(body.span);
        let params = vec![LambdaParam { take, name, ty: None }];
        Expr { kind: ExprKind::Lambda { params, body: Box::new(body) }, span }
    }

    /// Whether the `(` at the cursor opens a lambda's (or a function type's)
    /// parameter list: its matching `)` is followed by `=>`.
    pub(super) fn paren_is_lambda(&self) -> bool {
        let mut depth = 0usize;
        let mut i = 0;
        loop {
            match self.nth_kind(i) {
                TokenKind::Eof => return false,
                TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        return self.nth_is_punct(i + 1, Punct::FatArrow);
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn paren_or_lambda(&mut self) -> Expr {
        if !self.no_lambda && self.paren_is_lambda() {
            return self.lambda_parens();
        }
        let open = self.bump().span;
        let result = self.with_nl(false, |p| {
            if p.at_punct(Punct::RParen) {
                return ExprKind::Tuple(Vec::new());
            }
            let first = p.expr();
            if !p.at_punct(Punct::Comma) {
                return first.kind;
            }
            let mut elems = vec![first];
            while p.eat_punct(Punct::Comma).is_some() {
                if p.at_punct(Punct::RParen) {
                    break;
                }
                elems.push(p.expr());
            }
            ExprKind::Tuple(elems)
        });
        self.expect_close(Punct::RParen, open);
        Expr { kind: result, span: self.span_from(open) }
    }

    fn lambda_parens(&mut self) -> Expr {
        let open = self.bump().span;
        let params = self.with_nl(false, |p| {
            p.comma_list(Punct::RParen, |p| {
                let take = if p.at_contextual("take") && matches!(p.nth_kind(1), TokenKind::Ident) {
                    p.bump();
                    true
                } else {
                    false
                };
                let name = p.ident(NameCase::Value, "parameter");
                let ty = p.eat_punct(Punct::Colon).map(|_| p.ty());
                LambdaParam { take, name, ty }
            })
        });
        self.expect_close(Punct::RParen, open);
        self.expect_punct(Punct::FatArrow);
        let body = self.stmt_expr();
        let span = open.to(body.span);
        Expr { kind: ExprKind::Lambda { params, body: Box::new(body) }, span }
    }

    /// `[a, b]`, `[v; n]`, `[k: v, …]`, `[]`, `[:]`.
    fn array_like(&mut self) -> Expr {
        let open = self.bump().span;
        let kind = self.with_nl(false, |p| {
            // `[k: loop { … }]` is a map entry, like `f(k: loop { … })` is a named argument.
            p.no_label = true;
            if p.at_punct(Punct::RBracket) {
                return ExprKind::Array(Vec::new());
            }
            if p.at_punct(Punct::Colon) && p.nth_is_punct(1, Punct::RBracket) {
                p.bump();
                return ExprKind::Map(Vec::new());
            }
            let first = p.expr();
            if p.eat_punct(Punct::Semi).is_some() {
                let count = p.expr();
                return ExprKind::ArrayRepeat { value: Box::new(first), count: Box::new(count) };
            }
            if p.eat_punct(Punct::Colon).is_some() {
                let value = p.expr();
                let mut pairs = vec![(first, value)];
                while p.eat_punct(Punct::Comma).is_some() {
                    if p.at_punct(Punct::RBracket) {
                        break;
                    }
                    let key = p.expr();
                    p.expect_punct(Punct::Colon);
                    let value = p.expr();
                    pairs.push((key, value));
                }
                return ExprKind::Map(pairs);
            }
            let mut elems = vec![first];
            while p.eat_punct(Punct::Comma).is_some() {
                if p.at_punct(Punct::RBracket) {
                    break;
                }
                elems.push(p.expr());
            }
            ExprKind::Array(elems)
        });
        self.expect_close(Punct::RBracket, open);
        Expr { kind, span: self.span_from(open) }
    }

    // ------------------------------------------------------------ control flow

    /// After `if` (already consumed): `cond { … } [else …]`.
    fn if_rest(&mut self, start: Span, is_const: bool) -> Expr {
        let cond = self.head_expr();
        let then = self.block();
        let else_ = if self.eat_kw(Keyword::Else).is_some() {
            let else_start = self.peek().span;
            if self.eat_kw(Keyword::If).is_some() {
                Some(Box::new(self.if_rest(else_start, is_const)))
            } else {
                let block = self.block();
                let span = block.span;
                Some(Box::new(Expr { kind: ExprKind::Block(block), span }))
            }
        } else {
            None
        };
        Expr { kind: ExprKind::If { cond: Box::new(cond), then, else_, is_const }, span: self.span_from(start) }
    }

    fn match_expr(&mut self) -> Expr {
        let start = self.bump().span;
        let scrutinee = self.head_expr();
        let open = self.expect_punct(Punct::LBrace);
        let outer_indent = self.stmt_indent;
        let mut closed = true;
        let arms = self.with_nl(true, |p| {
            let mut arms = Vec::new();
            loop {
                while p.eat_punct(Punct::Comma).is_some() || p.eat_punct(Punct::Semi).is_some() {}
                if p.at_punct(Punct::RBrace) || p.at_eof() {
                    break;
                }
                // A statement keyword no deeper than the `match` itself can't
                // be an arm: the `}` is missing. Stop here, so the rest of
                // the block parses normally instead of as broken arms.
                if p.at_line_start() && p.peek().line_indent <= outer_indent && p.at_stmt_keyword() {
                    p.report_unclosed(open, Punct::RBrace);
                    closed = false;
                    break;
                }
                let before = p.pos;
                arms.push(p.arm());
                if p.eat_punct(Punct::Comma).is_none() {
                    p.stmt_terminator();
                }
                if p.pos == before {
                    p.skip_bad_stmt();
                }
            }
            arms
        });
        if closed {
            self.expect_close(Punct::RBrace, open);
        }
        Expr { kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, span: self.span_from(start) }
    }

    /// Whether the current token starts a statement and can't start a
    /// pattern (or a member of a warp, weft or weave).
    pub(super) fn at_stmt_keyword(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::Keyword(
                Keyword::Let
                    | Keyword::Var
                    | Keyword::Given
                    | Keyword::Spool
                    | Keyword::Defer
                    | Keyword::Return
                    | Keyword::For
                    | Keyword::While
                    | Keyword::Loop
                    | Keyword::Use
                    | Keyword::Warp
                    | Keyword::Weft
                    | Keyword::Weave
                    | Keyword::Extern
            )
        )
    }

    fn arm(&mut self) -> Arm {
        let saved = self.stmt_indent;
        self.stmt_indent = self.peek().line_indent;
        let start = self.peek().span;
        let pat = self.pat_top(true);
        let guard = if self.eat_kw(Keyword::If).is_some() {
            let saved_lambda = self.no_lambda;
            self.no_lambda = true;
            let guard = self.expr();
            self.no_lambda = saved_lambda;
            Some(guard)
        } else {
            None
        };
        self.expect_punct(Punct::FatArrow);
        let body = self.stmt_expr();
        self.stmt_indent = saved;
        Arm { pat, guard, body, span: self.span_from(start) }
    }

    /// After `for` (already consumed): `pat in iter { … }`.
    fn for_rest(&mut self, start: Span, label: Option<Ident>, is_const: bool, is_loom: bool) -> Expr {
        let pat = self.pat_top(false);
        self.expect_kw(Keyword::In);
        let iter = self.head_expr();
        let body = self.block();
        Expr {
            kind: ExprKind::For { label, pat: Box::new(pat), iter: Box::new(iter), body, is_const, is_loom },
            span: self.span_from(start),
        }
    }

    fn while_expr(&mut self, label: Option<Ident>) -> Expr {
        let start = self.bump().span;
        let cond = self.head_expr();
        let body = self.block();
        Expr { kind: ExprKind::While { label, cond: Box::new(cond), body }, span: self.span_from(start) }
    }

    fn loop_expr(&mut self, label: Option<Ident>) -> Expr {
        let start = self.bump().span;
        let body = self.block();
        Expr { kind: ExprKind::Loop { label, body }, span: self.span_from(start) }
    }

    /// `loom { … }`, `loom(args) { … }` or `loom for …`.
    fn loom_expr(&mut self, label: Option<Ident>) -> Expr {
        let start = self.bump().span;
        if self.eat_kw(Keyword::For).is_some() {
            return self.for_rest(start, label, false, true);
        }
        let args = if self.at_punct(Punct::LParen) && !self.at_line_start() { self.call_args().0 } else { Vec::new() };
        let body = self.block();
        Expr { kind: ExprKind::Loom { args, body }, span: self.span_from(start) }
    }

    /// A loop after `label:` (already consumed). The label is in scope for
    /// `break`/`continue` inside it.
    fn labeled_loop(&mut self, label: Ident) -> Expr {
        let start = label.span;
        self.labels.push(label.name.clone());
        let mut e = match self.kind() {
            TokenKind::Keyword(Keyword::For) => {
                let start = self.bump().span;
                self.for_rest(start, Some(label), false, false)
            }
            TokenKind::Keyword(Keyword::While) => self.while_expr(Some(label)),
            TokenKind::Keyword(Keyword::Loom) => self.loom_expr(Some(label)),
            _ => self.loop_expr(Some(label)),
        };
        self.labels.pop();
        e.span = start.to(e.span);
        e
    }

    fn unsafe_block(&mut self) -> Expr {
        let start = self.bump().span;
        let reason = match self.kind().clone() {
            TokenKind::Str(reason) => {
                self.bump();
                reason
            }
            _ => {
                let span = self.here();
                self.report(
                    Diagnostic::error(codes::S0200, "`unsafe` blocks need a reason")
                        .primary(span, "expected a string saying why this is sound")
                        .help("write `unsafe \"why this is sound\" { … }`"),
                );
                String::new()
            }
        };
        let body = self.block();
        Expr { kind: ExprKind::Unsafe { reason, body }, span: self.span_from(start) }
    }

    /// `const { … }`, `const if …`, `const for …`, or `const expr`.
    fn const_expr(&mut self) -> Expr {
        let start = self.bump().span;
        match self.kind() {
            TokenKind::Keyword(Keyword::If) => {
                self.bump();
                self.if_rest(start, true)
            }
            TokenKind::Keyword(Keyword::For) => {
                self.bump();
                self.for_rest(start, None, true, false)
            }
            _ => {
                let inner = self.expr_bp(BP_PREFIX);
                let span = start.to(inner.span);
                Expr { kind: ExprKind::Const(Box::new(inner)), span }
            }
        }
    }
}

fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    let span = lhs.span.to(rhs.span);
    Expr { kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span }
}
