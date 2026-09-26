//! Compile-time code: `const` expressions (`const fib(20)`,
//! `const { … }`), `const assert`, and `const` items at the top of a module
//! or in a warp.
//!
//! The checker makes sure compile-time code can run while compiling: it
//! can't read the locals around it (K0105), which have no values yet, and
//! control flow can't leave it (K0106). Lowering then runs it in the
//! sandbox (see `lower::consteval`). A `const assert` at the top of a
//! module whose condition the constant folder can work out is decided
//! here, without the sandbox.

use std::collections::HashSet;

use loom_syntax::Diagnostic;
use loom_syntax::ast::{BinaryOp, Lit, UnaryOp};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::gauge::Val;
use super::{BodyId, Checker, Fence};
use crate::hir::{Arg, DefKind, Expr, ExprKind, LocalId, Program, StmtKind};
use crate::ty::{Ty, TyKind};

/// The arguments of `assert(…)` when `e` is a call of std's `assert`:
/// `const assert(cond, message)` checks `cond` while compiling.
pub(crate) fn assert_args<'e>(p: &Program, e: &'e Expr) -> Option<&'e [Arg]> {
    let ExprKind::Call { callee, args } = &e.kind else { return None };
    match &callee.kind {
        ExprKind::Def { def, .. } if p.def_path(*def) == "std.error.assert" => Some(args),
        _ => None,
    }
}

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

    /// `const inner`, at `span`: `inner` runs while compiling. A `const
    /// assert(…)` is `()`; anything else has the type of its value.
    pub(super) fn check_const_expr(&mut self, span: Span, inner: &Expr, expected: Option<Ty>) -> Ty {
        let outer: HashSet<LocalId> = self.b.local_ty.keys().copied().collect();
        let saved_locals = self.runtime_locals.replace(outer);
        let saved_fence = self.fence.replace(Fence::Const(span));
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_mends = std::mem::take(&mut self.mends);
        let saved_subjects = std::mem::take(&mut self.subjects);
        let saved_self = self.self_local.take();
        let saved_shuttle = self.shuttle.take();
        let ty = match assert_args(self.p, inner) {
            Some(_) => self.check_coerce(inner, self.tcx.unit),
            None => {
                let ty = self.check_expr(inner, expected);
                self.check_const_holds(ty, span);
                ty
            }
        };
        self.shuttle = saved_shuttle;
        self.self_local = saved_self;
        self.subjects = saved_subjects;
        self.mends = saved_mends;
        self.loops = saved_loops;
        self.fence = saved_fence;
        self.runtime_locals = saved_locals;
        ty
    }

    /// Reports a constant of type `ty`, at `span`, that holds something
    /// that changes as it's used: every use of a constant gets the same value.
    pub(super) fn check_const_holds(&mut self, ty: Ty, span: Span) {
        let Some(what) = self.stateful(ty, 0) else { return };
        let shown = self.show(ty);
        self.diags.push(
            Diagnostic::error(codes::K0108, format!("a constant can't hold {what}"))
                .primary(span, format!("this is `{shown}`"))
                .note(format!("every use of a constant gets the same value, and {what} changes as it's used"))
                .help("make it in a function, and call that where it's needed"),
        );
    }

    /// What in `ty` a constant can't hold, if anything.
    fn stateful(&mut self, ty: Ty, depth: u32) -> Option<&'static str> {
        let ty = self.resolve(ty);
        if depth > 8 {
            return None;
        }
        if let TyKind::Adt(def, _) = self.tcx.kind(ty) {
            match self.p.def_path(*def).as_str() {
                "std.iter.Stream" => return Some("a stream"),
                "std.iter.Shuttle" | "std.iter.Burst" => return Some("a shuttle"),
                _ => {}
            }
        }
        if self.is_once(ty) {
            return Some("a `once` value");
        }
        let parts: Vec<Ty> = match self.tcx.kind(ty).clone() {
            TyKind::Tuple(items) | TyKind::Union(items) => items,
            TyKind::Optional(t) | TyKind::Array(t, _) | TyKind::Range(t) => vec![t],
            TyKind::Result(t, e) => vec![t, e],
            TyKind::Adt(_, args) if self.is_builtin_adt(ty) => args,
            TyKind::Adt(..) => {
                let info = self.adt_info(ty)?;
                let fields = info.fields.iter().map(|f| f.ty);
                let cases = info.cases.iter().flat_map(|c| c.fields.iter().flatten().map(|f| f.ty));
                fields.chain(cases).collect()
            }
            _ => Vec::new(),
        };
        parts.into_iter().find_map(|t| self.stateful(t, depth + 1))
    }

    /// Reports `local`, declared outside the compile-time code it's used in.
    pub(super) fn runtime_value(&mut self, local: LocalId, span: Span) {
        let l = self.p.local(local);
        self.diags.push(
            Diagnostic::error(codes::K0105, format!("`{}` has no value while compiling", l.name))
                .primary(span, "read by compile-time code")
                .secondary(l.span, "declared outside it, so it gets its value when the program runs")
                .help("compile-time code can use constants, functions and its own locals"),
        );
    }

    /// `const …` as an item: `e` is the whole `const` expression. A
    /// `const assert` the constant folder can decide is decided here; the
    /// rest is left to the sandbox.
    pub(super) fn check_const_block(&mut self, id: BodyId, e: &Expr) {
        let ExprKind::Const(inner) = &e.kind else {
            self.unsupported(e.span, "compile-time code");
            return;
        };
        self.in_body(id, |this| {
            let first_var = this.vars.len();
            let mark = this.diags.len();
            this.check_const_expr(e.span, inner, None);
            this.finish_body(first_var);
            if this.diags.len() > mark {
                return;
            }
            this.check_value_moves(e);
            if this.diags.len() > mark || this.folds_assert(inner) {
                return;
            }
            this.r.const_blocks.push(id);
        });
    }

    /// Decides a `const assert` with the constant folder, when it can:
    /// whether it did.
    fn folds_assert(&mut self, inner: &Expr) -> bool {
        let Some(args) = assert_args(self.p, inner) else { return false };
        let Some(cond) = args.first() else { return false };
        match self.fold(&cond.value, None) {
            Some(Val::Bool(true)) => true,
            Some(Val::Bool(false)) => {
                let title = match args.get(1).map(|a| &a.value.kind) {
                    None => "const assertion failed".to_string(),
                    Some(ExprKind::Lit(Lit::Str(s))) => format!("const assertion failed: {s}"),
                    // The message needs computing: the sandbox does it.
                    Some(_) => return false,
                };
                self.diags.push(Diagnostic::error(codes::K0101, title).primary(cond.value.span, "this is false"));
                true
            }
            _ => false,
        }
    }
}
