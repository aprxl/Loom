//! Constants with a type (`const LIMIT: u32 = 10 * 1000`), computed the
//! first time they're read, as on Lua (design/vm.md §17 V1).
//!
//! Each one has a place in the program's data: a word saying whether it's
//! computed yet, then its value. Its *init* function computes it there. A
//! read gives the value itself when it's plain data, or a copy of its own
//! when it owns memory; what a constant owns is dropped when `main` is
//! done (`const_exit`), so the run ends with nothing allocated.

use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, FnKey};
use crate::heddle::{Cmp, InstKind, Mem, Term, VTy, Value};
use crate::hir::{DefId, DefKind};
use crate::ty::Ty;
use crate::typeck::{BodyId, Instance};

impl<'a> Builder<'a> {
    /// The body constant `def` was checked in.
    fn const_body(&self, def: DefId) -> Option<BodyId> {
        self.typed.results.find(&Instance::of(def))
    }

    /// The type constant `def`'s value has.
    fn const_ty(&self, def: DefId) -> Option<Ty> {
        let DefKind::Const(c) = &self.p.def(def).kind else { return None };
        let body = self.typed.results.body(self.const_body(def)?);
        let ty = body.coerced.get(&c.value.id).or_else(|| body.expr_ty.get(&c.value.id)).copied()?;
        Some(self.tcx().degauge(ty))
    }

    /// Where constant `def` is in the data: the offset of its flag word,
    /// with its value 8 bytes on.
    fn const_global(&mut self, def: DefId, span: Span) -> Option<(u32, Ty)> {
        if let Some(&g) = self.globals.get(&def) {
            return Some(g);
        }
        let ty = self.const_ty(def)?;
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
        self.globals.insert(def, (off, ty));
        Some((off, ty))
    }

    /// Reads constant `def`: computed the first time.
    pub(super) fn read_const(&mut self, def: DefId, span: Span) -> Option<Val> {
        let (off, ty) = self.const_global(def, span)?;
        let at = self.emit1(InstKind::Global(off), VTy::Ptr);
        let flag = self.load(Mem::W64, at, 0, VTy::Int);
        let zero = self.iconst(0);
        let fresh = self.ucmp(Cmp::Eq, flag, zero);
        let (init, ready) = (self.new_block(), self.new_block());
        self.br(fresh, init, ready);
        self.enter(init);
        let func = self.func_id(FnKey::ConstInit(def));
        let value = self.offset(at, 8);
        self.emit(InstKind::Call { func, args: vec![value] }, &[]);
        self.jump(ready);
        self.enter(ready);
        let v = self.load_val(at, 8, ty, span)?;
        if self.needs_drop(ty) { self.copy_val(&v, ty, span) } else { Some(v) }
    }

    /// The init function of constant `def`: computes its value at the
    /// address it's given, and marks it computed.
    pub(super) fn const_init(&mut self, def: DefId) -> Option<Vec<VTy>> {
        let span = self.p.def(def).span;
        let DefKind::Const(c) = &self.p.def(def).kind else { return self.internal(span, "a constant that isn't one") };
        let body = self.const_body(def)?;
        let ty = self.const_ty(def)?;
        self.f.body = body;
        self.f.scan = self.scan(body, &c.value);
        let dst = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.push(dst);
        let depth = self.open_scope();
        let v = self.expr(&c.value)?;
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

    /// Drops what each computed constant owns: once `main` is done.
    pub(super) fn const_exit(&mut self) -> Option<Vec<VTy>> {
        self.f.span = None;
        let globals: Vec<(u32, Ty)> = self.globals.values().copied().collect();
        for (off, ty) in globals {
            if !self.needs_drop(ty) {
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
            self.drop_at(value, ty);
            self.jump(done);
            self.enter(done);
        }
        self.terminate(Term::Ret(Vec::new()));
        Some(Vec::new())
    }
}
