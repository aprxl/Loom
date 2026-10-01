//! Optionals and results (design/vm.md §10.5).
//!
//! `?T` is `T` with `none` in one of `T`'s spare bit patterns (a niche) when
//! it has one, and `T` then a flag otherwise; in registers, the same parts.
//! `T ! E` is a tag (0 for `ok`) and the value or the error, in memory.

use loom_syntax::source::Span;

use super::repr::{Repr, loaded};
use super::value::Val;
use super::{Builder, ErrStore, Vals};
use crate::heddle::{Cmp, InstKind, Mem, VTy, Value};
use crate::hir;
use crate::ty::layout::{Niche, Shape};
use crate::ty::{Ty, TyKind};
use crate::typeck::{ErrConv, Exit, Unwrap};

/// Where an optional keeps whether it's `some`.
#[derive(Clone, Copy, Debug)]
enum Presence {
    /// A byte at this offset, after the value: 1 for `some`.
    Flag(u64),
    /// `none` is this pattern of the value's niche.
    Niche(Niche),
}

impl<'a> Builder<'a> {
    /// How `opt` (an optional type) says whether it holds a value.
    fn presence(&mut self, opt: Ty, span: Span) -> Option<Presence> {
        let TyKind::Optional(inner) = *self.tcx().kind(opt) else {
            return self.internal(span, "the presence of something that isn't optional");
        };
        match self.layout(opt)?.shape {
            Shape::Optional { flag: Some(off) } => Some(Presence::Flag(off)),
            Shape::Optional { flag: None } => {
                let niche = self.layout(inner)?.niche?;
                Some(Presence::Niche(niche))
            }
            _ => self.internal(span, "an optional laid out as something else"),
        }
    }

    /// Of a value in registers: the part at `off` with `bytes` bytes.
    fn part_at(&mut self, ty: Ty, off: u64, span: Span) -> Option<(usize, Mem)> {
        let repr = self.repr_of(ty, span)?;
        let i = repr.parts().iter().position(|p| p.off == off)?;
        Some((i, repr.parts()[i].mem))
    }

    /// `some(v)` as a value of `opt`, where `v` is a value of `inner`.
    pub(super) fn some(&mut self, opt: Ty, inner: Ty, v: Val, span: Span) -> Option<Val> {
        let presence = self.presence(opt, span)?;
        match self.repr_of(opt, span)? {
            Repr::Regs(_) => {
                let mut vals = self.regs(v, inner, span)?;
                if let Presence::Flag(_) = presence {
                    vals.push(self.iconst(1));
                }
                Some(Val::Regs(vals))
            }
            Repr::Mem { .. } => {
                let ptr = self.temp(opt, span)?;
                self.move_to(ptr, 0, v, inner, span)?;
                if let Presence::Flag(off) = presence {
                    let one = self.iconst(1);
                    self.store(Mem::U8, ptr, off as i64, one);
                }
                self.own_temp(ptr, opt);
                Some(Val::Mem(ptr))
            }
        }
    }

    /// `none` as a value of `opt`.
    pub(super) fn none(&mut self, opt: Ty, span: Span) -> Option<Val> {
        let presence = self.presence(opt, span)?;
        match self.repr_of(opt, span)? {
            Repr::Regs(parts) => {
                let mut vals: Vec<Value> = parts
                    .iter()
                    .map(|p| match p.vty {
                        VTy::F32 | VTy::F64 => self.emit1(InstKind::Fconst(0), p.vty),
                        vty => self.emit1(InstKind::Iconst(0), vty),
                    })
                    .collect();
                if let Presence::Niche(n) = presence {
                    let (i, mem) = self.part_at(opt, n.offset, span)?;
                    vals[i] = self.emit1(InstKind::Iconst(loaded(mem, n.start)), parts[i].vty);
                }
                Some(Val::Regs(vals))
            }
            Repr::Mem { size, .. } => {
                let ptr = self.temp(opt, span)?;
                self.emit(InstKind::Zero { dst: ptr, size }, &[]);
                if let Presence::Niche(n) = presence {
                    let mem = niche_mem(n.bytes);
                    let k = self.iconst(n.start as u64);
                    self.store(mem, ptr, n.offset as i64, k);
                }
                self.own_temp(ptr, opt);
                Some(Val::Mem(ptr))
            }
        }
    }

    /// Whether `v`, a value of `opt`, holds a value: a `bool`.
    pub(super) fn is_some(&mut self, v: &Val, opt: Ty, span: Span) -> Option<Value> {
        let presence = self.presence(opt, span)?;
        match (v, presence) {
            (Val::Regs(vals), Presence::Flag(_)) => Some(*vals.last()?),
            (Val::Regs(vals), Presence::Niche(n)) => {
                let (i, mem) = self.part_at(opt, n.offset, span)?;
                let none = self.iconst(loaded(mem, n.start));
                Some(self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: vals[i], b: none }, VTy::Int))
            }
            (&Val::Mem(ptr), Presence::Flag(off)) => Some(self.load(Mem::U8, ptr, off as i64, VTy::Int)),
            (&Val::Mem(ptr), Presence::Niche(n)) => {
                let got = self.load(niche_mem(n.bytes), ptr, n.offset as i64, VTy::Int);
                let none = self.iconst(n.start as u64);
                Some(self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: got, b: none }, VTy::Int))
            }
        }
    }

    /// The value inside `v`, a value of `opt` that holds one.
    pub(super) fn unwrap_some(&mut self, v: &Val, opt: Ty, span: Span) -> Option<Val> {
        let TyKind::Optional(inner) = *self.tcx().kind(opt) else {
            return self.internal(span, "unwrapping something that isn't optional");
        };
        let presence = self.presence(opt, span)?;
        match v {
            Val::Regs(vals) => {
                let n = match presence {
                    Presence::Flag(_) => vals.len() - 1,
                    Presence::Niche(_) => vals.len(),
                };
                Some(Val::Regs(vals[..n].to_vec()))
            }
            &Val::Mem(ptr) => self.load_val(ptr, 0, inner, span),
        }
    }

    // ------------------------------------------------------------ tagged values

    /// A tagged value's layout: where its tag is, and its payload.
    fn tagged(&mut self, ty: Ty, span: Span) -> Option<(crate::ty::layout::Tag, u64)> {
        match self.layout(ty)?.shape {
            Shape::Tagged { tag, payload } => Some((tag, payload)),
            _ => self.internal(span, "a result or a union laid out as something else"),
        }
    }

    /// A new value of `ty` (a result or a union) with tag `tag` and `v`, a
    /// value of `inner`, as its payload.
    pub(super) fn make_tagged(&mut self, ty: Ty, tag: u64, inner: Ty, v: Val, span: Span) -> Option<Val> {
        let (at, payload) = self.tagged(ty, span)?;
        let ptr = self.temp(ty, span)?;
        let t = self.iconst(tag);
        self.store(super::warp::tag_mem(at.bytes), ptr, at.offset as i64, t);
        self.move_to(ptr, payload as i64, v, inner, span)?;
        self.own_temp(ptr, ty);
        Some(Val::Mem(ptr))
    }

    /// The tag of `v`, a result or a union of `ty`.
    pub(super) fn tag_value(&mut self, v: &Val, ty: Ty, span: Span) -> Option<Value> {
        let (at, _) = self.tagged(ty, span)?;
        let Val::Mem(ptr) = *v else { return self.internal(span, "a result in registers") };
        Some(self.load(super::warp::tag_mem(at.bytes), ptr, at.offset as i64, VTy::Int))
    }

    /// The payload of `v`, a result or a union of `ty`, as a value of `inner`.
    pub(super) fn payload(&mut self, v: &Val, ty: Ty, inner: Ty, span: Span) -> Option<Val> {
        let (_, payload) = self.tagged(ty, span)?;
        let Val::Mem(ptr) = *v else { return self.internal(span, "a result in registers") };
        self.load_val(ptr, payload as i64, inner, span)
    }

    /// `ok(v)` or `err(v)` as a value of the result type `ty`.
    pub(super) fn result(&mut self, ty: Ty, is_ok: bool, v: Val, span: Span) -> Option<Val> {
        let TyKind::Result(ok, err) = *self.tcx().kind(ty) else {
            return self.internal(span, "`ok` or `err` of something that isn't a result");
        };
        let inner = if is_ok { ok } else { err };
        self.make_tagged(ty, u64::from(!is_ok), inner, v, span)
    }

    /// The tag of member `member` in union `ty`: its place among the
    /// members.
    fn member_tag(&mut self, union: Ty, member: Ty, span: Span) -> Option<u64> {
        let TyKind::Union(members) = self.tcx().kind(union) else {
            return self.internal(span, "a member of something that isn't a union");
        };
        match members.iter().position(|&m| m == member) {
            Some(i) => Some(i as u64),
            None => self.internal(span, "a type that isn't a member of the union"),
        }
    }

    /// `v`, a value of `from`, as a value of the union `to`.
    pub(super) fn union_of(&mut self, to: Ty, from: Ty, v: Val, span: Span) -> Option<Val> {
        if matches!(self.tcx().kind(from), TyKind::Union(_)) {
            return self.widen_union(to, from, v, span);
        }
        let tag = self.member_tag(to, from, span)?;
        self.make_tagged(to, tag, from, v, span)
    }

    /// `v`, a value of the union `from`, as a value of the union `to`,
    /// which has every member `from` has: the same payload, its tag
    /// renumbered.
    pub(super) fn widen_union(&mut self, to: Ty, from: Ty, v: Val, span: Span) -> Option<Val> {
        let TyKind::Union(have) = self.tcx().kind(from).clone() else {
            return self.internal(span, "widening something that isn't a union");
        };
        let old = self.tag_value(&v, from, span)?;
        let (at, payload) = self.tagged(to, span)?;
        let (_, old_payload) = self.tagged(from, span)?;
        let mut bytes = 0;
        for &m in &have {
            bytes = bytes.max(self.size_align(m)?.0);
        }
        let ptr = self.temp(to, span)?;
        // The payload's bytes, then the new tag for the old one.
        let Val::Mem(src) = v else { return None };
        if bytes > 0 {
            let dst = self.offset(ptr, payload as i64);
            let from_at = self.offset(src, old_payload as i64);
            self.emit(InstKind::Copy { dst, src: from_at, size: bytes }, &[]);
        }
        let new_tag = self.new_var(VTy::Int);
        let done = self.new_block();
        for (i, &m) in have.iter().enumerate() {
            let t = self.member_tag(to, m, span)?;
            let (yes, next) = (self.new_block(), self.new_block());
            let k = self.iconst(i as u64);
            let c = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: old, b: k }, VTy::Int);
            self.br(c, yes, next);
            self.enter(yes);
            let tv = self.iconst(t);
            self.write(new_tag, tv);
            self.jump(done);
            self.enter(next);
        }
        self.terminate(crate::heddle::Term::Crash(0));
        self.enter(done);
        let tv = self.read(new_tag);
        self.store(super::warp::tag_mem(at.bytes), ptr, at.offset as i64, tv);
        // What was in the old one is the new one's now.
        if self.needs_drop(from) {
            self.moved_from(&Val::Mem(src), from);
        }
        self.own_temp(ptr, to);
        Some(Val::Mem(ptr))
    }

    // ------------------------------------------------------------ errors

    /// An error of type `from` as the type `conv` makes it, `to`.
    pub(super) fn convert_err(&mut self, v: Val, from: Ty, to: Ty, conv: &ErrConv, span: Span) -> Option<Val> {
        match conv {
            ErrConv::Same => Some(v),
            ErrConv::ToUnion => self.union_of(to, from, v, span),
            ErrConv::ToAny(_) => self.unsupported(span, "`any` errors"),
            ErrConv::Absorb { case, key, .. } => {
                // The warp's case that holds it: a value of `to`.
                let info = self.warp_info(to)?;
                let tag = super::Builder::case_tag(&info, *case);
                let ptr = self.temp(to, span)?;
                let layout = self.layout(to)?;
                let Shape::Sum { tag: at, .. } = layout.shape else {
                    return self.internal(span, "absorbing an error into something that isn't a sum");
                };
                let t = self.iconst(tag);
                self.store(super::warp::tag_mem(at.bytes), ptr, at.offset as i64, t);
                let pos = self.field_pos(to, Some(*case), key, span)?;
                self.move_to(ptr, pos.off as i64, v, from, span)?;
                self.own_temp(ptr, to);
                Some(Val::Mem(ptr))
            }
        }
    }

    /// The error type of the function being built's result.
    fn ret_err_ty(&self) -> Option<Ty> {
        match self.tcx().kind(self.f.ret?) {
            TyKind::Result(_, e) => Some(*e),
            _ => None,
        }
    }

    /// Leaves with error `err` (of type `ty`), as `exit` says.
    fn exit_with(&mut self, exit: &Exit, err: Val, ty: Ty, span: Span) -> Option<()> {
        match exit {
            Exit::Mend(id, conv) => {
                let i = self.f.mends.iter().rposition(|m| m.id == *id)?;
                let (fail, scopes) = (self.f.mends[i].fail, self.f.mends[i].scopes);
                if let (Some(conv), Some((to, store))) = (conv, self.f.mends[i].err.clone()) {
                    let err = self.convert_err(err, ty, to, conv, span)?;
                    match store {
                        ErrStore::Vars(vars) => {
                            let vals = self.regs(err, to, span)?;
                            self.write_all(&vars, &vals);
                        }
                        ErrStore::Slot(ptr) => self.move_to(ptr, 0, err, to, span)?,
                    }
                }
                self.leave_scopes(scopes);
                if self.live() {
                    self.jump(fail);
                }
            }
            Exit::ReturnNone => {
                let ret = self.f.ret?;
                let none = self.none(ret, span)?;
                self.ret_val(none, span)?;
            }
            Exit::ReturnErr(conv) => {
                let ret = self.f.ret?;
                let to = self.ret_err_ty()?;
                let err = self.convert_err(err, ty, to, conv, span)?;
                let r = self.result(ret, false, err, span)?;
                self.ret_val(r, span)?;
            }
        }
        Some(())
    }

    /// Unwraps `v`, a value of `ty` (an optional or a result): its value, or
    /// on the way out when there's none, its error (`Missing` for an
    /// optional) handed to `on_fail`.
    fn unwrap_or(
        &mut self,
        v: &Val,
        ty: Ty,
        unwrap: Unwrap,
        err_ty: Ty,
        span: Span,
        on_fail: impl FnOnce(&mut Self, Val) -> Option<()>,
    ) -> Option<Val> {
        let present = match unwrap {
            Unwrap::Optional => self.is_some(v, ty, span)?,
            Unwrap::Result => {
                let tag = self.tag_value(v, ty, span)?;
                let zero = self.iconst(0);
                self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: tag, b: zero }, VTy::Int)
            }
        };
        let (ok, fail) = (self.new_block(), self.new_block());
        self.br(present, ok, fail);
        self.enter(fail);
        let err = match unwrap {
            // `Missing`, a record with no fields.
            Unwrap::Optional => self.aggregate(err_ty, &[], Vec::new(), span)?,
            Unwrap::Result => self.payload(v, ty, err_ty, span)?,
        };
        on_fail(self, err)?;
        self.enter(ok);
        match unwrap {
            Unwrap::Optional => self.unwrap_some(v, ty, span),
            Unwrap::Result => {
                let TyKind::Result(t, _) = *self.tcx().kind(ty) else { return None };
                self.payload(v, ty, t, span)
            }
        }
    }

    /// `x?`.
    pub(super) fn try_expr(&mut self, e: &'a hir::Expr, inner: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        let info = self.tables().tries.get(&e.id).cloned()?;
        let v = self.expr(inner)?;
        let ty = self.coerced_ty(inner);
        self.unwrap_or(&v, ty, info.unwrap, info.err_ty, span, |this, err| {
            this.exit_with(&info.exit, err, info.err_ty, span)
        })
    }

    /// `fail e`.
    pub(super) fn fail_expr(&mut self, e: &'a hir::Expr, value: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        let (ty, conv) = self.tables().fails.get(&e.id).cloned()?;
        let v = self.expr(value)?;
        self.exit_with(&Exit::ReturnErr(conv), v, ty, span)?;
        None
    }

    /// `a mend b`, `a mend e => b`.
    pub(super) fn mend_expr(
        &mut self,
        e: &'a hir::Expr,
        left: &'a hir::Expr,
        handler: &'a hir::Expr,
        ty: Ty,
    ) -> Option<Val> {
        let span = e.span;
        let info = self.tables().mends.get(&e.id).cloned()?;
        let join = self.join(ty, span)?;
        // Where the errors that reach it go, when it binds one.
        let err = match info.binding {
            Some(l) => {
                let to = self.local_ty(l);
                let store = match self.repr_of(to, span)? {
                    super::Repr::Regs(parts) => {
                        let tys: Vec<VTy> = parts.iter().map(|p| p.vty).collect();
                        ErrStore::Vars(self.new_vars(&tys))
                    }
                    super::Repr::Mem { .. } => {
                        let ptr = self.temp(to, span)?;
                        self.own_temp(ptr, to);
                        self.f.temps.remove(&ptr);
                        ErrStore::Slot(ptr)
                    }
                };
                Some((to, store))
            }
            None => None,
        };
        let (fail, done) = (self.new_block(), self.new_block());
        let from = self.f.cleanups.len();
        self.f.mends.push(super::MendCtx { id: e.id, fail, err: err.clone(), scopes: self.f.scopes.len() });
        let left_val = self.expr(left);
        let mut value = left_val.and_then(|v| {
            let lty = self.coerced_ty(left);
            match &info.unwrap {
                Some((unwrap, err_ty, conv)) => {
                    let exit = Exit::Mend(e.id, conv.clone());
                    let inner = match self.tcx().kind(lty) {
                        TyKind::Optional(t) | TyKind::Result(t, _) => *t,
                        _ => lty,
                    };
                    let (unwrap, err_ty) = (*unwrap, *err_ty);
                    let v = self.unwrap_or(&v, lty, unwrap, err_ty, span, |this, err| {
                        this.exit_with(&exit, err, err_ty, span)
                    })?;
                    Some((v, inner))
                }
                None => Some((v, lty)),
            }
        });
        self.f.mends.pop();
        // `a mend b` with a wider `b`: `a`'s value, wrapped.
        if let Some((v, vty)) = value.take() {
            value = match info.wrap {
                Some(crate::typeck::Coercion::ToOptional) => self.some(ty, vty, v, span).map(|v| (v, ty)),
                Some(crate::typeck::Coercion::ToResult) => self.result(ty, true, v, span).map(|v| (v, ty)),
                _ => Some((v, vty)),
            };
        }
        if let Some((v, _)) = value
            && self.live()
            && self.join_put(&join, v, from, span).is_some()
        {
            self.jump(done);
        }
        self.enter(fail);
        if self.live() {
            let depth = self.open_scope();
            let mut ok = true;
            if let (Some(l), Some((to, store))) = (info.binding, err) {
                let ev = match store {
                    ErrStore::Vars(vars) => Val::Regs(self.read_all(&vars)),
                    ErrStore::Slot(ptr) => Val::Mem(ptr),
                };
                let _ = to;
                ok = self.bind_owned(l, ev, span).is_some();
            }
            if ok && self.expr_into(handler, &join).is_some() {
                let start = self.scope_start(depth);
                self.run_cleanups(start);
                if self.live() {
                    self.jump(done);
                }
            }
            self.f.cur = None;
            self.close_scope(depth);
        }
        self.enter(done);
        self.live().then(|| self.join_get(&join))
    }

    // ------------------------------------------------------------ checked operations

    /// `a +? b` and friends: `some` of the result, or `none` when it doesn't
    /// fit `int`'s type.
    pub(super) fn checked_arith(
        &mut self,
        name: &'static str,
        a: Value,
        b: Value,
        opt: Ty,
        inner: Ty,
        span: Span,
    ) -> Option<Vals> {
        let Some(int) = self.int_of(inner) else { return self.unsupported(span, "this checked operation") };
        let spec = self.iconst(super::int_spec(int).pack());
        let out = self.emit(InstKind::Native { name, imm: 0, args: vec![a, b, spec] }, &[VTy::Int, VTy::Int]);
        self.opt_from(out[0], out[1], opt, inner, span)
    }

    /// `some(value)` when `ok` is true, else `none`, as a value of `opt` in
    /// registers.
    pub(super) fn opt_from(&mut self, value: Value, ok: Value, opt: Ty, inner: Ty, span: Span) -> Option<Vals> {
        let some = self.some(opt, inner, Val::Regs(vec![value]), span)?;
        let some = self.regs(some, opt, span)?;
        let none = self.none(opt, span)?;
        let none = self.regs(none, opt, span)?;
        let vars = self.new_vars(&some.iter().map(|&v| self.f.types[v.0 as usize]).collect::<Vec<_>>());
        let (yes, no, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(ok, yes, no);
        self.enter(yes);
        self.write_all(&vars, &some);
        self.jump(done);
        self.enter(no);
        self.write_all(&vars, &none);
        self.jump(done);
        self.enter(done);
        Some(self.read_all(&vars))
    }
}

/// How a niche of `bytes` bytes is loaded: zero-extended, as its patterns
/// are counted.
fn niche_mem(bytes: u8) -> Mem {
    match bytes {
        1 => Mem::U8,
        2 => Mem::U16,
        4 => Mem::U32,
        _ => Mem::W64,
    }
}
