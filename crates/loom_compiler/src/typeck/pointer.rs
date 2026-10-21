//! Pointers: `^e` allocates, `p^` reads through one, and a pointer stands
//! for its target where a field, a method or a pattern needs one
//! (design/v6-spools.md).

use loom_syntax::diag::codes;

use super::{Checker, Coercion};
use crate::hir::{Expr, ExprKind};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    /// `^operand`: a new pointer to `operand`'s value, which moves into it.
    pub(super) fn check_alloc(&mut self, operand: &Expr, expected: Option<Ty>) -> Ty {
        let want = expected.and_then(|t| match *self.tcx.kind(self.resolve(t)) {
            TyKind::Ptr(inner, _) => Some(inner),
            _ => None,
        });
        let inner = self.check_expr(operand, want);
        if let Some(want) = want {
            self.coerce(operand, inner, want);
            return self.tcx.intern(TyKind::Ptr(want, self.tcx.heap));
        }
        self.tcx.intern(TyKind::Ptr(inner, self.tcx.heap))
    }

    /// `pointer^`: the target, read.
    pub(super) fn check_deref(&mut self, pointer: &Expr) -> Ty {
        let ty = self.check_expr(pointer, None);
        let ty = self.resolve(ty);
        match *self.tcx.kind(ty) {
            TyKind::Ptr(inner, _) => inner,
            TyKind::Error => ty,
            _ => {
                let shown = self.show(ty);
                self.error(codes::T0101, pointer.span, format!("`{shown}` isn't a pointer"), "can't be read with `^`");
                self.tcx.error
            }
        }
    }

    /// The type `e`, of type `ty`, stands for in a field access, a method
    /// call or a pattern: its target, if it's a pointer (recorded as a
    /// [`Coercion::Deref`] on `e`), else `ty`.
    pub(super) fn auto_deref(&mut self, e: &Expr, ty: Ty) -> Ty {
        let ty = self.resolve(ty);
        match *self.tcx.kind(ty) {
            TyKind::Ptr(inner, _) if !self.b.coercions.contains_key(&e.id) => {
                let inner = self.resolve(inner);
                self.b.coerced.insert(e.id, inner);
                self.b.coercions.insert(e.id, Coercion::Deref);
                inner
            }
            _ => ty,
        }
    }

    /// Whether `e` is the target of a pointer: `p^`, a field of one, or
    /// something a pointer is dereferenced to for a field access.
    pub(super) fn derefs(&self, e: &Expr) -> bool {
        matches!(e.kind, ExprKind::Deref(_)) || self.b.coercions.get(&e.id).is_some_and(|c| *c == Coercion::Deref)
    }
}
