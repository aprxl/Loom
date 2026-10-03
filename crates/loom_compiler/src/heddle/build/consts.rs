//! Constants with a type (`const LIMIT: u32 = 10 * 1000`), computed the
//! first time they're read, as on Lua (design/vm.md §17 V1).
//!
//! Each one has a place in the program's data: a word saying whether it's
//! computed yet, then its value. Its *init* function computes it there. A
//! read gives the value itself when it's plain data, or a copy of its own
//! when it owns memory; what a constant owns is dropped when `main` is
//! done (`const_exit`), so the run ends with nothing allocated. A constant
//! that's a view (`[T]`, `str`) gets its own copy of what it views, which
//! may be a temporary of its init: freed then too.

use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, FnKey};
use crate::heddle::{Cmp, InstKind, IntOp, Mem, Term, VTy, Value};
use crate::hir::{DefId, DefKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::{BodyId, Instance, Seq};

/// A constant: a typed one, or a `const` expression in a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ConstKey {
    Def(DefId),
    Expr(BodyId, crate::hir::HirId),
}

impl<'a> Builder<'a> {
    /// The body constant `key` was checked in, and its expression.
    fn const_source(&self, key: ConstKey) -> Option<(BodyId, &'a crate::hir::Expr)> {
        match key {
            ConstKey::Def(def) => {
                let DefKind::Const(c) = &self.p.def(def).kind else { return None };
                Some((self.typed.results.find(&Instance::of(def))?, &c.value))
            }
            ConstKey::Expr(body, id) => Some((body, *self.const_exprs.get(&(body, id))?)),
        }
    }

    /// The type constant `key`'s value has.
    fn const_ty(&self, key: ConstKey) -> Option<Ty> {
        let (body, value) = self.const_source(key)?;
        let body = self.typed.results.body(body);
        let ty = body.coerced.get(&value.id).or_else(|| body.expr_ty.get(&value.id)).copied()?;
        Some(self.tcx().degauge(ty))
    }

    /// Where constant `key` is in the data: the offset of its flag word,
    /// with its value 8 bytes on.
    fn const_global(&mut self, key: ConstKey, span: Span) -> Option<(u32, Ty)> {
        if let Some(&g) = self.globals.get(&key) {
            return Some(g);
        }
        let ty = self.const_ty(key)?;
        let Some((size, align)) = self.size_align(ty) else {
            let shown = self.tcx().display(self.p, ty);
            return self.unsupported(span, &format!("constants of type `{shown}`"));
        };
        if align > 8 {
            return self.unsupported(span, "constants aligned past 8 bytes");
        }
        let off = (self.data.len() * 8) as u32;
        let words = 1 + size.div_ceil(8) as usize;
        self.data.extend(std::iter::repeat_n(0, words));
        self.globals.insert(key, (off, ty));
        Some((off, ty))
    }

    /// `const inner` in a body (the expression `e`): computed the first
    /// time it's reached, as on Lua.
    pub(super) fn const_expr(&mut self, e: &'a crate::hir::Expr, inner: &'a crate::hir::Expr) -> Option<Val> {
        self.const_exprs.insert((self.f.body, e.id), inner);
        self.read_const(ConstKey::Expr(self.f.body, e.id), e.span)
    }

    /// Reads constant `key`: computed the first time.
    pub(super) fn read_const(&mut self, key: ConstKey, span: Span) -> Option<Val> {
        let (off, ty) = self.const_global(key, span)?;
        let at = self.emit1(InstKind::Global(off), VTy::Ptr);
        let flag = self.load(Mem::W64, at, 0, VTy::Int);
        let zero = self.iconst(0);
        let fresh = self.ucmp(Cmp::Eq, flag, zero);
        let (init, ready) = (self.new_block(), self.new_block());
        self.br(fresh, init, ready);
        self.enter(init);
        let func = self.func_id(FnKey::ConstInit(key));
        let value = self.offset(at, 8);
        self.emit(InstKind::Call { func, args: vec![value] }, &[]);
        self.jump(ready);
        self.enter(ready);
        let v = self.load_val(at, 8, ty, span)?;
        if self.needs_drop(ty) { self.copy_val(&v, ty, span) } else { Some(v) }
    }

    /// The init function of constant `key`: computes its value at the
    /// address it's given, and marks it computed.
    pub(super) fn const_init(&mut self, key: ConstKey) -> Option<Vec<VTy>> {
        let (body, value) = self.const_source(key)?;
        let span = value.span;
        let ty = self.const_ty(key)?;
        self.f.body = body;
        self.f.scan = self.scan(body, value);
        let dst = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.push(dst);
        let depth = self.open_scope();
        let mut v = self.expr(value)?;
        if let Some((size, align)) = self.const_view(ty, span)? {
            // Its own copy of what it views.
            let parts = self.regs(v, ty, span)?;
            let (ptr, len) = (parts[0], parts[1]);
            let k = self.iconst(size);
            let bytes = self.uarith(IntOp::Mul, len, k);
            let a = self.iconst(align);
            let buf = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![bytes, a] }, VTy::Ptr);
            self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![buf, ptr, bytes] }, &[]);
            v = Val::Regs(vec![buf, len]);
        }
        self.move_to(dst, 0, v, ty, span)?;
        let flag = self.offset(dst, -8);
        let one = self.iconst(1);
        self.store(Mem::W64, flag, 0, one);
        self.close_scope(depth);
        if self.live() {
            self.terminate(Term::Ret(Vec::new()));
        }
        Some(Vec::new())
    }

    /// Whether a constant of type `ty` has something to free when `main`
    /// is done.
    pub(super) fn const_owns(&mut self, ty: Ty) -> bool {
        self.needs_drop(ty)
            || matches!(self.tcx().kind(ty), TyKind::Str)
            || matches!(self.seq_of(ty), Some(Seq::Slice(_)))
    }

    /// The size and alignment of the elements of `ty`, when it's a view:
    /// `None` when it isn't.
    fn const_view(&mut self, ty: Ty, span: Span) -> Option<Option<(u64, u64)>> {
        if let TyKind::Str = self.tcx().kind(ty) {
            return Some(Some((1, 1)));
        }
        let Some(Seq::Slice(elem)) = self.seq_of(ty) else { return Some(None) };
        if self.needs_drop(elem) {
            return self.unsupported(span, "constant slices of values that own memory");
        }
        Some(Some(self.size_align(elem)?))
    }

    /// Drops what each computed constant owns: once `main` is done.
    pub(super) fn const_exit(&mut self) -> Option<Vec<VTy>> {
        self.f.span = None;
        let globals: Vec<(u32, Ty)> = self.globals.values().copied().collect();
        for (off, ty) in globals {
            let span = self.here();
            let view = self.const_view(ty, span).flatten();
            if !self.needs_drop(ty) && view.is_none() {
                continue;
            }
            let at = self.emit1(InstKind::Global(off), VTy::Ptr);
            let flag = self.load(Mem::W64, at, 0, VTy::Int);
            let zero = self.iconst(0);
            let made = self.ucmp(Cmp::Ne, flag, zero);
            let (yes, done) = (self.new_block(), self.new_block());
            self.br(made, yes, done);
            self.enter(yes);
            let value: Value = self.offset(at, 8);
            match view {
                Some((size, align)) => {
                    let (buf, len) = (self.load(Mem::W64, value, 0, VTy::Ptr), self.load(Mem::W64, value, 8, VTy::Int));
                    let k = self.iconst(size);
                    let bytes = self.uarith(IntOp::Mul, len, k);
                    let a = self.iconst(align);
                    self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![buf, bytes, a] }, &[]);
                }
                None => self.drop_at(value, ty),
            }
            self.jump(done);
            self.enter(done);
        }
        self.terminate(Term::Ret(Vec::new()));
        Some(Vec::new())
    }
}
