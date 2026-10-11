//! `const` expressions in Lua bundles.
//!
//! Compile-time code is run before lowering, on Treadle (see
//! `crate::consteval`). The bundle still computes each constant when the
//! program first reads it (`rt.lazy`): a `const` expression in a function is
//! a constant of its own, so `const fib(20)` in a loop is computed once.
//! Writing the values the compiler found into the bundle instead is V4's
//! next step (design/vm.md §17).

use loom_syntax::source::Span;

use super::lir::{Expr, Stmt, StmtKind};
use super::{Dest, Lowerer};
use crate::hir;
use crate::typeck::assert_args;

impl Lowerer<'_> {
    /// `const inner`, the expression `e`: its value, read from its constant.
    /// A `const assert` has none, and nothing of it is left to run.
    pub(super) fn lower_const(&mut self, e: &hir::Expr, inner: &hir::Expr) -> Expr {
        if assert_args(self.p, inner).is_some() {
            return Expr::Nil;
        }
        let id = crate::heddle::ConstKey::Expr(self.cur, e.id);
        // A number computed while compiling is written where it's read.
        if let Some(lit) = self.baked_scalar(id) {
            return lit;
        }
        let key = format!("K{}_{}", self.cur.0, e.id.0);
        if self.const_keys.insert(key.clone()) {
            match self.baked_literal(id) {
                Some(Expr::Nil) => {}
                Some(lit) => self.const_value_stmt(&key, lit, e.span),
                None => {
                    let body = self.body(e.span, |this| this.lower_into(inner, &Dest::Return));
                    self.const_init(&key, body, e.span);
                }
            }
        }
        Expr::field(Expr::name("C"), &key)
    }

    /// `C.key = literal`: constant `key`, computed while compiling.
    pub(super) fn const_value_stmt(&mut self, key: &str, value: Expr, span: Span) {
        let target = Expr::field(Expr::name("C"), key);
        self.consts.push(Stmt { kind: StmtKind::Assign(vec![target], vec![value]), span: Some(span) });
    }

    /// `CI.key = function() … end`: how constant `key` is computed.
    pub(super) fn const_init(&mut self, key: &str, body: Vec<Stmt>, span: Span) {
        let target = Expr::field(Expr::name("CI"), key);
        let init = Expr::Function(Vec::new(), body);
        self.consts.push(Stmt { kind: StmtKind::Assign(vec![target], vec![init]), span: Some(span) });
    }
}
