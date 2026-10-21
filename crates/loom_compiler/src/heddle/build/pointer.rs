//! Pointers (design/v6-spools.md): `^e` puts a value in a box on the heap,
//! `p^` and a pointer used as its target read it, and dropping the pointer
//! drops the target and frees the box.
//!
//! A pointer is one word in memory (a value that owns memory lives there, so
//! a moved-from pointer is a zero word, and dropping it does nothing).

use loom_syntax::source::Span;

use super::Builder;
use super::value::{Place, Val};
use crate::heddle::{Cmp, InstKind, Mem, Term, VTy, Value};
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

    /// The address the pointer value `v` holds: in a register for a pointer
    /// into a spool, in a word of memory for one that owns its box.
    pub(super) fn ptr_addr(&mut self, v: &Val, _span: Span) -> Option<Value> {
        match v {
            &Val::Mem(at) => Some(self.load(Mem::W64, at, 0, VTy::Ptr)),
            Val::Regs(vals) => vals.first().copied(),
        }
    }

    pub(super) fn is_arena(&self, ty: Ty) -> bool {
        matches!(self.tcx().kind(self.tcx().degauge(ty)), TyKind::Adt(def, _)
            if self.p.def_path(*def) == "std.mem.Arena")
    }

    /// `size` bytes in the arena whose handle is `h`: its bump as one op
    /// when the size is small, else a call.
    pub(super) fn arena_alloc(&mut self, h: Value, size: u64) -> Value {
        let rounded = size.max(1).div_ceil(16) * 16;
        if rounded <= 0xFFF0 {
            return self
                .emit1(InstKind::Native { name: "arena.alloc.k", imm: rounded as u16, args: vec![h] }, VTy::Ptr);
        }
        let n = self.iconst(size);
        self.emit1(InstKind::Native { name: "arena.alloc", imm: 0, args: vec![h, n] }, VTy::Ptr)
    }

    /// `Arena(capacity)`: the handle, in a temporary that owns it.
    pub(super) fn arena_new(&mut self, args: &'a [hir::Arg], ty: Ty, span: Span) -> Option<Val> {
        let Some(arg) = args.first() else { return self.internal(span, "an arena made from nothing") };
        let cap = self.expr(&arg.value)?;
        let cap = self.regs(cap, self.tcx().usize, span)?[0];
        let h = self.emit1(InstKind::Native { name: "arena.new", imm: 0, args: vec![cap] }, VTy::Int);
        let at = self.temp(ty, span)?;
        self.store(Mem::W64, at, 0, h);
        self.own_temp(at, ty);
        Some(Val::Mem(at))
    }

    /// The bodies of `Arena`'s methods: `reset` and `put`.
    pub(super) fn arena_method(&mut self, path: &str, args: &[Val], tys: &[Ty], span: Span) -> Option<()> {
        let Val::Mem(at) = args[0] else { return self.internal(span, "an arena in registers") };
        let h = self.load(Mem::W64, at, 0, VTy::Int);
        match path.rsplit('.').next().unwrap_or_default() {
            "reset" => {
                self.emit(InstKind::Native { name: "arena.reset", imm: 0, args: vec![h] }, &[]);
                self.terminate(Term::Ret(Vec::new()));
            }
            "put" => {
                let t = self.tcx().degauge(tys[1]);
                let (size, align) = self.size_align(t)?;
                if align > 16 {
                    return self.unsupported(span, "putting a value aligned past 16 bytes in an arena");
                }
                let p = self.arena_alloc(h, size);
                self.move_to(p, 0, args[1].clone(), t, span)?;
                self.terminate(Term::Ret(vec![p]));
            }
            name => return self.internal(span, &format!("no body for `Arena.{name}`")),
        }
        Some(())
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
    pub(super) fn alloc_expr(&mut self, id: hir::HirId, operand: &'a hir::Expr, ptr: Ty, span: Span) -> Option<Val> {
        let Some(target) = self.target_ty(ptr) else { return self.internal(span, "allocating a non-pointer") };
        let v = self.expr(operand)?;
        let from = self.coerced_ty(operand);
        // In an arena: its next bytes, and a pointer that owns nothing.
        if let Some(&arena) = self.tables().allocs.get(&id) {
            let (size, align) = self.size_align(target)?;
            if align > 16 {
                return self.unsupported(span, "allocating a value aligned past 16 bytes in an arena");
            }
            let ty = self.local_ty(arena);
            let place = self.local_place(arena, span)?;
            let at = match self.read_place(&place, ty, span)? {
                Val::Mem(at) => at,
                Val::Regs(_) => return self.internal(span, "a spool in registers"),
            };
            let h = self.load(Mem::W64, at, 0, VTy::Int);
            let p = self.arena_alloc(h, size);
            self.move_to(p, 0, v, from, span)?;
            return Some(Val::Regs(vec![p]));
        }
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
