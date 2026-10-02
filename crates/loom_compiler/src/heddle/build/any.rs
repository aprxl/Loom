//! `any W` (design/vm.md §10.6): a value of some type that weaves `W`.
//!
//! On Treadle it's 24 bytes: a box on the heap holding the value (always,
//! for now; keeping a small value inline is for V8), a word left at 0, and
//! the value's vtable, which lives in the program's data. A vtable holds
//! the type's number (a type test compares it), its size and alignment,
//! functions to drop, copy and show a value at an address, and a slot for
//! each weft method the program calls through `any`, numbered across the
//! whole program so every vtable has the same shape. An `any` of more wefts
//! is an `any` of fewer, with the same vtable.

use loom_syntax::source::Span;

use super::text::Sink;
use super::value::Val;
use super::{Builder, FnKey, Glue, ParamAbi};
use crate::heddle::{Cmp, InstKind, Mem, Term, VTy, Value};
use crate::hir::DefId;
use crate::ty::{Ty, TyKind};
use crate::typeck::BodyId;

const VT_ID: i64 = 0;
const VT_SIZE: i64 = 8;
const VT_ALIGN: i64 = 16;
const VT_DROP: i64 = 24;
const VT_COPY: i64 = 32;
const VT_SHOW: i64 = 40;
const VT_DEBUG: i64 = 48;
const VT_SLOTS: i64 = 56;
/// A slot with no function.
const NO_FN: u64 = u64::MAX;

impl<'a> Builder<'a> {
    /// The slot of weft method `method` in every vtable.
    fn dyn_slot(&mut self, method: DefId) -> u32 {
        if self.dyn_slots.is_empty() {
            let mut all: Vec<DefId> = self.typed.results.vtables.values().flatten().map(|&(m, _)| m).collect();
            all.sort();
            all.dedup();
            for m in all {
                let n = self.dyn_slots.len() as u32;
                self.dyn_slots.insert(m, n);
            }
        }
        match self.dyn_slots.get(&method) {
            Some(&n) => n,
            None => {
                let n = self.dyn_slots.len() as u32;
                self.dyn_slots.insert(method, n);
                n
            }
        }
    }

    /// The number type tests know `ty` by.
    fn type_id(&mut self, ty: Ty) -> u64 {
        let ty = self.tcx().degauge(ty);
        let n = self.type_ids.len() as u64 + 1;
        *self.type_ids.entry(ty).or_insert(n)
    }

    /// The offset in the data of the vtable of `ty` made into `any` of
    /// `wefts`.
    fn vtable(&mut self, ty: Ty, wefts: &[DefId], span: Span) -> Option<u32> {
        let ty = self.tcx().degauge(ty);
        let key = (ty, {
            let mut w = wefts.to_vec();
            w.sort();
            w
        });
        if let Some(&off) = self.vtable_at.get(&key) {
            return Some(off);
        }
        // Typeck's table for it: the methods called through `any`.
        let entries: Vec<(DefId, BodyId)> = self
            .typed
            .results
            .vtables
            .iter()
            .filter(|((t, closure), _)| *t == ty && wefts.iter().all(|w| closure.contains(w)))
            .flat_map(|(_, es)| es.iter().copied())
            .collect();
        let shows = self
            .typed
            .results
            .vtables
            .keys()
            .filter(|(t, _)| *t == ty)
            .any(|(_, closure)| closure.iter().any(|&w| self.p.def_path(w) == "std.fmt.Show"))
            || wefts.iter().any(|&w| self.p.def_path(w) == "std.fmt.Show");
        let (size, align) = self.size_align(ty)?;
        let id = self.type_id(ty);
        let drop = if self.needs_drop(ty) { u64::from(self.glue(Glue::Drop, ty).0) } else { NO_FN };
        let copy = if self.needs_drop(ty) { u64::from(self.glue(Glue::Copy, ty).0) } else { NO_FN };
        let (show, debug) = if shows {
            (u64::from(self.func_id(FnKey::AnyShow(ty, false)).0), u64::from(self.func_id(FnKey::AnyShow(ty, true)).0))
        } else {
            (NO_FN, NO_FN)
        };
        let mut words = vec![id, size, align, drop, copy, show, debug];
        for (method, inst) in entries {
            let slot = self.dyn_slot(method) as usize;
            let f = u64::from(self.func_id(FnKey::Dyn(inst)).0);
            while words.len() <= 7 + slot {
                words.push(NO_FN);
            }
            words[7 + slot] = f;
        }
        let _ = span;
        let off = (self.data.len() * 8) as u32;
        self.data.extend(words);
        self.vtable_at.insert(key, off);
        Some(off)
    }

    /// `v`, a value of `from`, as an `any` (of type `to`) of `wefts`: in a
    /// box of its own.
    pub(super) fn any_of(&mut self, v: Val, from: Ty, to: Ty, wefts: &[DefId], span: Span) -> Option<Val> {
        let from = self.tcx().degauge(from);
        // Already an `any`: it carries its vtable.
        if matches!(self.tcx().kind(from), TyKind::Any(_)) {
            return Some(v);
        }
        let vt = self.vtable(from, wefts, span)?;
        let (size, align) = self.size_align(from)?;
        let (n, a) = (self.iconst(size), self.iconst(align));
        let b = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr);
        self.move_to(b, 0, v, from, span)?;
        let dst = self.temp(to, span)?;
        self.store(Mem::W64, dst, 0, b);
        let zero = self.iconst(0);
        self.store(Mem::W64, dst, 8, zero);
        let vt = self.emit1(InstKind::Global(vt), VTy::Ptr);
        self.store(Mem::W64, dst, 16, vt);
        self.own_temp(dst, to);
        Some(Val::Mem(dst))
    }

    /// The box and the vtable of the `any` at `at`.
    fn any_parts(&mut self, at: Value) -> (Value, Value) {
        (self.load(Mem::W64, at, 0, VTy::Ptr), self.load(Mem::W64, at, 16, VTy::Ptr))
    }

    /// Drop glue for an `any`: its value, then its box.
    pub(super) fn drop_any(&mut self, at: Value) {
        let (b, vt) = self.any_parts(at);
        let zero = self.iconst(0);
        let some = self.ucmp(Cmp::Ne, b, zero);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(some, yes, done);
        self.enter(yes);
        let f = self.load(Mem::W64, vt, VT_DROP, VTy::Int);
        let none = self.iconst(NO_FN);
        let has = self.ucmp(Cmp::Ne, f, none);
        let (call, free) = (self.new_block(), self.new_block());
        self.br(has, call, free);
        self.enter(call);
        self.emit(InstKind::CallInd { callee: f, args: vec![b] }, &[]);
        self.jump(free);
        self.enter(free);
        let size = self.load(Mem::W64, vt, VT_SIZE, VTy::Int);
        let align = self.load(Mem::W64, vt, VT_ALIGN, VTy::Int);
        self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![b, size, align] }, &[]);
        self.jump(done);
        self.enter(done);
    }

    /// Copy glue for an `any` (its bytes are at `dst` already): a box of
    /// its own, with a copy of the value.
    pub(super) fn copy_any(&mut self, dst: Value, src: Value) {
        let (b, vt) = self.any_parts(src);
        let size = self.load(Mem::W64, vt, VT_SIZE, VTy::Int);
        let align = self.load(Mem::W64, vt, VT_ALIGN, VTy::Int);
        let new = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![size, align] }, VTy::Ptr);
        let f = self.load(Mem::W64, vt, VT_COPY, VTy::Int);
        let none = self.iconst(NO_FN);
        let deep = self.ucmp(Cmp::Ne, f, none);
        let (call, bytes, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(deep, call, bytes);
        self.enter(call);
        self.emit(InstKind::CallInd { callee: f, args: vec![new, b] }, &[]);
        self.jump(done);
        self.enter(bytes);
        self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![new, b, size] }, &[]);
        self.jump(done);
        self.enter(done);
        self.store(Mem::W64, dst, 0, new);
    }

    /// Writes the value an `any` at `at` holds, as `{x}` (or `{x:?}`).
    pub(super) fn show_any(&mut self, sink: Sink, at: Value, debug: bool) {
        let (b, vt) = self.any_parts(at);
        let f = self.load(Mem::W64, vt, if debug { VT_DEBUG } else { VT_SHOW }, VTy::Int);
        self.emit(InstKind::CallInd { callee: f, args: vec![b, sink.0] }, &[]);
    }

    /// Whether the `any` at `at` holds a `ty`.
    pub(super) fn any_is(&mut self, at: Value, ty: Ty) -> Value {
        let (_, vt) = self.any_parts(at);
        let id = self.load(Mem::W64, vt, VT_ID, VTy::Int);
        let want = self.type_id(ty);
        let want = self.iconst(want);
        self.ucmp(Cmp::Eq, id, want)
    }

    /// The value of type `ty` the `any` at `at` holds (a view of it).
    pub(super) fn any_value(&mut self, at: Value, ty: Ty, span: Span) -> Option<Val> {
        let b = self.load(Mem::W64, at, 0, VTy::Ptr);
        self.load_val(b, 0, ty, span)
    }

    /// `show(value's address, sink)` for a value of `ty` in an `any`.
    pub(super) fn any_show_func(&mut self, ty: Ty, debug: bool) -> Option<Vec<VTy>> {
        self.f.span = None;
        let span = self.here();
        let ptr = self.new_value(VTy::Ptr);
        let h = self.new_value(VTy::Int);
        self.f.blocks[0].params.extend([ptr, h]);
        let v = self.load_val(ptr, 0, ty, span)?;
        self.write_value(Sink(h), v, ty, None, debug, span)?;
        self.terminate(Term::Ret(Vec::new()));
        Some(Vec::new())
    }

    /// A weft method instance as a vtable slot calls it: `self` by
    /// address (the box), the rest as the method takes them.
    pub(super) fn dyn_func(&mut self, id: BodyId, span: Span) -> Option<Vec<VTy>> {
        self.f.span = None;
        let target = self.body_abi(id, span)?;
        let sret = target.sret.map(|_| {
            let v = self.new_value(VTy::Ptr);
            self.f.blocks[0].params.push(v);
            v
        });
        let this = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.push(this);
        let mut args: Vec<Value> = sret.into_iter().collect();
        let sig = self.typed.results.sigs.get(&id)?.clone();
        let self_ty = self.plain_ty(sig.params.first()?.ty);
        match &target.params[0] {
            ParamAbi::Addr => args.push(this),
            ParamAbi::Regs(parts) => {
                let parts = parts.clone();
                args.extend(self.load_parts(this, 0, &parts));
            }
        }
        let _ = self_ty;
        for p in &target.params[1..] {
            match p {
                ParamAbi::Addr => {
                    let v = self.new_value(VTy::Ptr);
                    self.f.blocks[0].params.push(v);
                    args.push(v);
                }
                ParamAbi::Regs(parts) => {
                    for part in parts.iter() {
                        let v = self.new_value(part.vty);
                        self.f.blocks[0].params.push(v);
                        args.push(v);
                    }
                }
            }
        }
        let func = self.func_id(FnKey::Body(id));
        let results = self.emit(InstKind::Call { func, args }, &target.results);
        self.terminate(Term::Ret(results));
        Some(target.results)
    }

    /// A call of weft method `method` on an `any` (the receiver `base`).
    pub(super) fn dyn_call(
        &mut self,
        e: &'a crate::hir::Expr,
        callee: &'a crate::hir::Expr,
        args: &'a [crate::hir::Arg],
        info: &crate::typeck::Call,
        method: DefId,
    ) -> Option<Val> {
        let span = e.span;
        let crate::hir::ExprKind::Field { base, .. } = &callee.kind else {
            return self.unsupported(span, "this call through `any`");
        };
        // The weft method's own parameters say how each is passed; the
        // arguments' types are what was checked.
        let crate::hir::DefKind::Fn(fdef) = &self.p.def(method).kind else {
            return self.internal(span, "a method through `any` that isn't a function");
        };
        let modes: Vec<loom_syntax::ast::ParamMode> = fdef.params.iter().map(|p| p.mode).collect();
        let (place, _) = self.place_or_temp(base)?;
        let any = self.place_addr(&place, span)?;
        let mut flat_args: Vec<(Val, Ty, loom_syntax::ast::ParamMode)> = Vec::new();
        for (k, source) in info.args.iter().enumerate().skip(1) {
            let crate::typeck::ArgSource::Arg(i) = source else {
                return self.unsupported(span, "default arguments through `any`");
            };
            let mode = modes.get(k).copied().unwrap_or(loom_syntax::ast::ParamMode::Read);
            let ty = self.plain_ty(self.coerced_ty(&args[*i].value));
            let v = if mode == loom_syntax::ast::ParamMode::Mut {
                let (pl, _) = self.place_or_temp(&args[*i].value)?;
                Val::Mem(self.place_addr(&pl, span)?)
            } else {
                self.expr(&args[*i].value)?
            };
            flat_args.push((v, ty, mode));
        }
        let params: Vec<(loom_syntax::ast::ParamMode, Ty)> = flat_args.iter().map(|(_, t, m)| (*m, *t)).collect();
        let ret = self.plain_ty(self.ty_of(e));
        let abi = self.abi(&params, ret, false, span)?;
        let sret = match abi.sret {
            Some(rt) => Some(self.temp(rt, span)?),
            None => None,
        };
        let (b, vt) = self.any_parts(any);
        let slot = self.dyn_slot(method);
        let f = self.load(Mem::W64, vt, VT_SLOTS + 8 * i64::from(slot), VTy::Int);
        let mut flat: Vec<Value> = sret.into_iter().collect();
        flat.push(b);
        for ((v, ty, mode), pabi) in flat_args.into_iter().zip(&abi.params) {
            match (pabi, mode) {
                (_, loom_syntax::ast::ParamMode::Mut) => {
                    let Val::Mem(p) = v else { return None };
                    flat.push(p);
                }
                (ParamAbi::Regs(_), _) => flat.extend(self.regs(v, ty, span)?),
                (ParamAbi::Addr, m) => {
                    let v = if m == loom_syntax::ast::ParamMode::Take { self.owned(v, ty, span)? } else { v };
                    flat.push(self.addr_of(v, ty, span)?);
                }
            }
        }
        let results = self.emit(InstKind::CallInd { callee: f, args: flat }, &abi.results);
        let _ = e;
        match sret {
            Some(p) => {
                self.own_temp(p, ret);
                Some(Val::Mem(p))
            }
            None => Some(Val::Regs(results)),
        }
    }
}
