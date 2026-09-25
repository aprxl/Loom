//! Compile-time code at the top of a module. So far that's `const assert`
//! whose condition the constant folder can work out: literals, constants,
//! arithmetic and comparisons, and the length of an array. Anything else is
//! "not implemented yet" (it needs the Lua sandbox, see
//! design/06-compile-time.md).

use loom_syntax::Diagnostic;
use loom_syntax::ast::{BinaryOp, Lit, UnaryOp};
use loom_syntax::diag::codes;

use super::gauge::Val;
use super::{BodyId, Checker};
use crate::hir::{DefKind, Expr, ExprKind, StmtKind};

impl Checker<'_> {
    /// The value of an untyped constant made of number literals and other
    /// untyped constants (`const HALF = MAX / 2`), if it has one: the
    /// constant is then a number like a literal, typed where it's used. A
    /// typed constant in it gives it that type instead, so it isn't one.
    pub(super) fn untyped_value(&mut self, e: &Expr) -> Option<Val> {
        if !self.untyped_number(e, 0) {
            return None;
        }
        match self.fold(e, None)? {
            v @ (Val::Int(_) | Val::Float(_)) => Some(v),
            Val::Bool(_) => None,
        }
    }

    fn untyped_number(&self, e: &Expr, depth: u32) -> bool {
        if depth > 64 {
            return false;
        }
        use BinaryOp as B;
        match &e.kind {
            ExprKind::Lit(Lit::Int(i)) => i.suffix.is_none(),
            ExprKind::Lit(Lit::Float(f)) => f.suffix.is_none(),
            ExprKind::Unary { op: UnaryOp::Neg | UnaryOp::BitNot, operand } => self.untyped_number(operand, depth + 1),
            ExprKind::Binary {
                op: B::Add | B::Sub | B::Mul | B::Div | B::Rem | B::BitAnd | B::BitOr | B::BitXor | B::Shl | B::Shr,
                lhs,
                rhs,
            } => self.untyped_number(lhs, depth + 1) && self.untyped_number(rhs, depth + 1),
            ExprKind::Block(block) => match block.stmts.as_slice() {
                [only] => matches!(&only.kind, StmtKind::Expr(e) if self.untyped_number(e, depth + 1)),
                _ => false,
            },
            ExprKind::Def { def, args } if args.is_empty() => match &self.p.def(*def).kind {
                DefKind::Const(c) => c.ty.is_none() && self.untyped_number(&c.value, depth + 1),
                _ => false,
            },
            _ => false,
        }
    }

    /// `const …` as an item: `e` is the whole `const` expression.
    pub(super) fn check_const_block(&mut self, id: BodyId, e: &Expr) {
        let ExprKind::Const(inner) = &e.kind else {
            self.unsupported(e.span, "compile-time code");
            return;
        };
        let assert = match &inner.kind {
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::Def { def, .. } if self.p.def_path(*def) == "std.error.assert" => Some(args),
                _ => None,
            },
            _ => None,
        };
        let Some(args) = assert else {
            self.unsupported(e.span, "compile-time code");
            return;
        };
        self.in_body(id, |this| {
            let first_var = this.vars.len();
            let unit = this.tcx.unit;
            let mark = this.diags.len();
            this.check_coerce(inner, unit);
            this.finish_body(first_var);
            if this.diags.len() > mark {
                return;
            }
            let Some(cond) = args.first() else { return };
            let title = match args.get(1).map(|a| &a.value.kind) {
                None => Some("const assertion failed".to_string()),
                Some(ExprKind::Lit(Lit::Str(s))) => Some(format!("const assertion failed: {s}")),
                Some(_) => None,
            };
            match (this.fold(&cond.value, None), title) {
                (Some(Val::Bool(true)), _) => {}
                (Some(Val::Bool(false)), Some(title)) => {
                    this.diags.push(Diagnostic::error(codes::K0101, title).primary(cond.value.span, "this is false"))
                }
                (Some(Val::Bool(false)), None) => {
                    this.unsupported(args[1].value.span, "a `const assert` message that isn't a string literal");
                }
                _ => {
                    this.unsupported(cond.value.span, "evaluating this at compile time");
                }
            }
        });
    }
}
