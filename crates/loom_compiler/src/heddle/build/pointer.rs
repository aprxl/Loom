//! Pointers (design/v6-spools.md): `^e` puts a value in a box on the heap,
//! `p^` and a pointer used as its target read it, and dropping the pointer
//! drops the target and frees the box.
//!
//! A pointer is one word in memory (a value that owns memory lives there, so
//! a moved-from pointer is a zero word, and dropping it does nothing).

use loom_syntax::source::Span;

use super::Builder;
use super::value::{Place, Val};
use crate::heddle::{Cmp, InstKind, Mem, VTy, Value};
use crate::hir;
use crate::ty::{Ty, TyKind};

impl<'a> Builder<'a> {
    /// What a pointer type points at.
    pub(super) fn target_ty(&self, ptr: Ty) -> Option<Ty> {
        match *self.tcx().kind(self.tcx().degauge(ptr)) {
            TyKind::Ptr(t, _) => Some(self.tcx().degauge(t)),
            _ => None,
        }
    }

    /// The size and alignment of a box for `target`: never empty, so every
    /// box is an object of its own.
    fn ptr_box(&mut self, target: Ty) -> Option<(u64, u64)> {
        let (size, align) = self.size_align(target)?;
        Some((size.max(1), align.max(1)))
    }

    /// The address the pointer value `v` holds.
    pub(super) fn ptr_addr(&mut self, v: &Val, span: Span) -> Option<Value> {
        match v {
            &Val::Mem(at) => Some(self.load(Mem::W64, at, 0, VTy::Ptr)),
            Val::Regs(_) => self.internal(span, "a pointer in registers"),
        }
    }

    /// The value a pointer value points at.
    pub(super) fn pointee(&mut self, v: &Val, ptr: Ty, span: Span) -> Option<(Val, Ty)> {
        let Some(target) = self.target_ty(ptr) else { return self.internal(span, "dereferencing a non-pointer") };
        let at = self.ptr_addr(v, span)?;
        Some((self.load_val(at, 0, target, span)?, target))
    }

    /// The place a pointer value points at.
    pub(super) fn pointee_place(&mut self, v: &Val, ptr: Ty, span: Span) -> Option<(Place, Ty)> {
        let Some(target) = self.target_ty(ptr) else { return self.internal(span, "dereferencing a non-pointer") };
        let at = self.ptr_addr(v, span)?;
        Some((Place::At { ptr: at, off: 0 }, target))
    }

    /// `^operand`: a box holding the operand's value, and a pointer to it.
    pub(super) fn alloc_expr(&mut self, operand: &'a hir::Expr, ptr: Ty, span: Span) -> Option<Val> {
        let Some(target) = self.target_ty(ptr) else { return self.internal(span, "allocating a non-pointer") };
        let v = self.expr(operand)?;
        let from = self.coerced_ty(operand);
        let (size, align) = self.ptr_box(target)?;
        let (n, a) = (self.iconst(size), self.iconst(align));
        let b = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr);
        self.move_to(b, 0, v, from, span)?;
        let dst = self.temp(ptr, span)?;
        self.store(Mem::W64, dst, 0, b);
        self.own_temp(dst, ptr);
        Some(Val::Mem(dst))
    }

    /// Drop glue for the pointer at `at`: its target, then its box (nothing
    /// for a zero word).
    pub(super) fn drop_ptr(&mut self, at: Value, ptr: Ty, span: Span) -> Option<()> {
        let Some(target) = self.target_ty(ptr) else { return self.internal(span, "dropping a non-pointer") };
        let b = self.load(Mem::W64, at, 0, VTy::Ptr);
        let zero = self.iconst(0);
        let some = self.ucmp(Cmp::Ne, b, zero);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(some, yes, done);
        self.enter(yes);
        self.drop_at(b, target);
        let (size, align) = self.ptr_box(target)?;
        let (n, a) = (self.iconst(size), self.iconst(align));
        self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![b, n, a] }, &[]);
        self.jump(done);
        self.enter(done);
        Some(())
    }

    /// Copy glue for a pointer (its word is at `dst` already): a box of its
    /// own, with a copy of the target.
    pub(super) fn copy_ptr(&mut self, dst: Value, src: Value, ptr: Ty, span: Span) -> Option<()> {
        let Some(target) = self.target_ty(ptr) else { return self.internal(span, "copying a non-pointer") };
        let old = self.load(Mem::W64, src, 0, VTy::Ptr);
        let (size, align) = self.ptr_box(target)?;
        let (n, a) = (self.iconst(size), self.iconst(align));
        let new = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr);
        if self.needs_drop(target) {
            let func = self.glue(super::Glue::Copy, target);
            self.emit(InstKind::Call { func, args: vec![new, old] }, &[]);
        } else {
            self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![new, old, n] }, &[]);
        }
        self.store(Mem::W64, dst, 0, new);
        Some(())
    }
}
