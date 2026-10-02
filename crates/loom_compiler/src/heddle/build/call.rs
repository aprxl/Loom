//! Calls: arguments by the callee's ABI (see `Abi` in `mod.rs`).
//!
//! The receiver and the arguments are worked out in source order. A `mut`
//! argument is its place's address, so the callee's writes are the
//! caller's at once, unwinding included (design/vm.md D6). A `take`
//! argument in memory is memory the callee then owns: a temporary it takes
//! over, or a copy of a place, which is zeroed. A result in memory is
//! written at an address the caller gives, a temporary of its own.

use loom_syntax::ast::ParamMode;
use loom_syntax::source::Span;

use super::value::{Place, Val};
use super::{Builder, ParamAbi};
use crate::heddle::{InstKind, Term};
use crate::hir::{self, DefKind, ExprKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::{ArgSource, Call, Receiver};

/// An argument, once worked out: a value, or (for `mut`) a place.
pub(super) enum Arg {
    Val(Val, Ty),
    Place(Place),
}

impl<'a> Builder<'a> {
    pub(super) fn call(
        &mut self,
        e: &'a hir::Expr,
        callee: &'a hir::Expr,
        args: &'a [hir::Arg],
        ty: Ty,
    ) -> Option<Val> {
        let span = e.span;
        let tables = self.tables();
        if tables.closure_calls.contains(&e.id) {
            return self.call_value(e, callee, args);
        }
        if tables.new_lists.contains(&e.id) {
            return self.new_empty(e);
        }
        if let ExprKind::BuiltinCase(kw) = callee.kind {
            return self.builtin_case(e, kw, args);
        }
        let Some(info) = tables.calls.get(&e.id).cloned() else {
            return self.unsupported(span, "this call");
        };
        if info.dynamic.is_some() {
            return self.unsupported(span, "`any` values");
        }
        if tables.streams.contains_key(&e.id) {
            return self.unsupported(span, "streams");
        }
        let p = self.p;
        let def = self.typed.results.instance(info.callee).def;
        let DefKind::Fn(f) = &p.def(def).kind else { return self.unsupported(span, "this call") };
        if f.abi.is_some() {
            return self.extern_call(def, &info, args, span);
        }
        if f.body.is_none()
            && self.typed.results.instance(info.callee).self_ty.is_none()
            && !super::builtin::writes_body(&p.def_path(def))
        {
            return self.builtin_call(e, callee, args, &info, def);
        }
        let sig = self.typed.results.sigs.get(&info.callee)?.clone();
        let abi = self.body_abi(info.callee, span)?;
        let modes: Vec<ParamMode> = sig.params.iter().map(|p| p.mode).collect();
        let recv_ty = info.args.iter().position(|&s| s == ArgSource::Receiver).map(|k| self.plain_ty(sig.params[k].ty));
        let worked = self.call_args(callee, args, &info, &modes, recv_ty)?;
        // Defaults, in parameter order, with the callee's tables.
        let mut by_param: Vec<Option<Arg>> = Vec::with_capacity(info.args.len());
        let mut worked = worked.into_iter().map(Some).collect::<Vec<_>>();
        for (k, source) in info.args.iter().enumerate() {
            let arg = match source {
                ArgSource::Receiver => worked[0].take(),
                ArgSource::Arg(i) => {
                    let shift = usize::from(info.receiver.is_some());
                    worked[i + shift].take()
                }
                ArgSource::Default => {
                    let default = f.params.get(k).and_then(|p| p.default.as_ref())?;
                    if super::expr::mentions_locals(default) {
                        return self.unsupported(default.span, "default values that use other parameters");
                    }
                    // The default was checked with the function's body.
                    let saved = std::mem::replace(&mut self.f.body, info.callee);
                    let v = self.expr(default);
                    let dty = self.coerced_ty(default);
                    self.f.body = saved;
                    Some(Arg::Val(v?, dty))
                }
            };
            by_param.push(arg);
        }
        let func = self.func_id(super::FnKey::Body(info.callee));
        self.emit_call(func, &abi, by_param, sig.ret, &modes, span, ty)
    }

    /// Works out a call's receiver and arguments, in source order: a place
    /// for each `mut` one, a value for the rest. The receiver, if any, is
    /// first, then the arguments by their position in the call.
    fn call_args(
        &mut self,
        callee: &'a hir::Expr,
        args: &'a [hir::Arg],
        info: &Call,
        modes: &[ParamMode],
        recv_ty: Option<Ty>,
    ) -> Option<Vec<Arg>> {
        let mode_of = |source: ArgSource| {
            info.args.iter().position(|&s| s == source).and_then(|k| modes.get(k)).copied().unwrap_or(ParamMode::Read)
        };
        let mut out = Vec::with_capacity(args.len() + 1);
        if let Some(r) = info.receiver {
            // `s.parse[u16]()`: the method is inside the type arguments.
            let method = match &callee.kind {
                ExprKind::Index { base, .. } => base.as_ref(),
                _ => callee,
            };
            let base = match (r, &method.kind) {
                (Receiver::Expr, ExprKind::Field { base, .. }) => Some(base.as_ref()),
                _ => None,
            };
            let mode = mode_of(ArgSource::Receiver);
            out.push(self.receiver_arg(r, base, &info.through, mode, recv_ty, callee.span)?);
        }
        for (i, arg) in args.iter().enumerate() {
            let a = if mode_of(ArgSource::Arg(i)) == ParamMode::Mut {
                let (place, ty) = self.place_or_temp(&arg.value)?;
                Arg::Place(self.addressable(place, ty, arg.value.span)?)
            } else {
                let v = self.expr(&arg.value)?;
                self.moved_away(&arg.value);
                Arg::Val(v, self.coerced_ty(&arg.value))
            };
            out.push(a);
        }
        Some(out)
    }

    /// A method's receiver: the value before the dot (or `self`, or a
    /// dot-block's subject), through the `through` fields to what the
    /// method is called on.
    fn receiver_arg(
        &mut self,
        r: Receiver,
        base: Option<&'a hir::Expr>,
        through: &[crate::typeck::Key],
        mode: ParamMode,
        want: Option<Ty>,
        span: Span,
    ) -> Option<Arg> {
        let (mut place, mut ty) = match r {
            Receiver::Expr => {
                let base = base?;
                if mode != ParamMode::Mut && through.is_empty() {
                    let v = self.expr(base)?;
                    self.moved_away(base);
                    let ty = self.coerced_ty(base);
                    return self.receiver_view(Arg::Val(v, ty), ty, want, span);
                }
                self.place_or_temp(base)?
            }
            Receiver::Local(l) => (self.local_place(l, span)?, self.local_ty(l)),
            Receiver::Subject(id) => self.f.subjects.get(&id).cloned()?,
        };
        for key in through {
            let pos = self.field_pos(ty, None, key, span)?;
            place = self.field_place_of(&place, &pos, span)?;
            ty = pos.ty;
        }
        if mode == ParamMode::Mut {
            let place = self.addressable(place, ty, span)?;
            return self.receiver_view(Arg::Place(place), ty, want, span);
        }
        let v = self.read_place(&place, ty, span)?;
        self.receiver_view(Arg::Val(v, ty), ty, want, span)
    }

    /// A place a `mut` argument can be given by address: a temporary in
    /// registers is put in memory of its own. (A local a callee writes to
    /// lives in memory already: see `scan.rs`.)
    fn addressable(&mut self, place: Place, ty: Ty, span: Span) -> Option<Place> {
        let Place::Vars(vars) = place else { return Some(place) };
        let vals = self.read_all(&vars);
        let ptr = self.addr_of(Val::Regs(vals), ty, span)?;
        Some(Place::At { ptr, off: 0 })
    }

    /// A receiver of type `ty` passed to a method of a slice (`want`), when
    /// it's an array or a list: a view of all of it. For a `mut` receiver,
    /// the view is in memory of its own; the elements are still its.
    fn receiver_view(&mut self, arg: Arg, ty: Ty, want: Option<Ty>, span: Span) -> Option<Arg> {
        let Some(want) = want else { return Some(arg) };
        // A `String` where `str` is wanted: a view of its text.
        if matches!(self.tcx().kind(want), TyKind::Str) && self.is_string(ty) {
            return Some(match arg {
                Arg::Val(v, _) => {
                    let (p, n) = self.text_of(v, ty, span)?;
                    Arg::Val(Val::Regs(vec![p, n]), want)
                }
                place => place,
            });
        }
        let to_slice = matches!(self.seq_of(want), Some(crate::typeck::Seq::Slice(_)))
            && matches!(self.seq_of(ty), Some(crate::typeck::Seq::Array(..) | crate::typeck::Seq::List(_)));
        if !to_slice {
            return Some(arg);
        }
        Some(match arg {
            Arg::Val(v, _) => Arg::Val(self.slice_of(v, ty, span)?, want),
            Arg::Place(place) => {
                let v = self.read_place(&place, ty, span)?;
                let (buf, len) = self.view_of(&v, ty, span)?;
                let slot = self.raw_slot(16, 8);
                let ptr = self.slot_addr(slot);
                self.store(crate::heddle::Mem::W64, ptr, 0, buf);
                self.store(crate::heddle::Mem::W64, ptr, 8, len);
                Arg::Place(Place::At { ptr, off: 0 })
            }
        })
    }

    /// Emits a call of `func` with `args` (by parameter) as `abi` says; its
    /// result, a value of `ret` (`ty` where it's used).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_call(
        &mut self,
        func: crate::heddle::FuncId,
        abi: &super::Abi,
        args: Vec<Option<Arg>>,
        ret: Ty,
        modes: &[ParamMode],
        span: Span,
        ty: Ty,
    ) -> Option<Val> {
        let sret = match abi.sret {
            Some(rt) => Some(self.temp(rt, span)?),
            None => None,
        };
        let mut flat = Vec::new();
        flat.extend(sret);
        for ((arg, pabi), mode) in args.into_iter().zip(&abi.params).zip(modes) {
            match (arg?, pabi) {
                (Arg::Place(place), _) => flat.push(self.place_addr(&place, span)?),
                (Arg::Val(v, vty), ParamAbi::Regs(_)) => flat.extend(self.regs(v, vty, span)?),
                (Arg::Val(v, vty), ParamAbi::Addr) => {
                    let v = if *mode == ParamMode::Take { self.owned(v, vty, span)? } else { v };
                    flat.push(self.addr_of(v, vty, span)?);
                }
            }
        }
        let results = self.emit(InstKind::Call { func, args: flat }, &abi.results);
        if matches!(self.tcx().kind(ty), TyKind::Never) || matches!(self.tcx().kind(ret), TyKind::Never) {
            self.terminate(Term::Crash(1));
            return None;
        }
        match sret {
            Some(ptr) => {
                self.own_temp(ptr, ret);
                Some(Val::Mem(ptr))
            }
            None => Some(Val::Regs(results)),
        }
    }

    /// `some(x)`, `ok(x)`, `err(e)`.
    fn builtin_case(&mut self, e: &'a hir::Expr, kw: loom_syntax::token::Keyword, args: &'a [hir::Arg]) -> Option<Val> {
        let span = e.span;
        let arg = args.first()?;
        let v = self.expr(&arg.value)?;
        let inner = self.coerced_ty(&arg.value);
        let ty = self.ty_of(e);
        match kw {
            loom_syntax::token::Keyword::Some => self.some(ty, inner, v, span),
            loom_syntax::token::Keyword::Ok => self.result(ty, true, v, span),
            _ => self.result(ty, false, v, span),
        }
    }
}

impl<'a> Builder<'a> {
    /// A call of a method of a warp std leaves to the runtime (`#builtin`).
    fn builtin_call(
        &mut self,
        e: &'a hir::Expr,
        callee: &'a hir::Expr,
        args: &'a [hir::Arg],
        info: &Call,
        def: crate::hir::DefId,
    ) -> Option<Val> {
        let span = e.span;
        let path = self.p.def_path(def);
        match path.as_str() {
            "std.fmt.Fmt.put" => {
                let Some(receiver) = info.receiver else {
                    return self.unsupported(span, "`Fmt.put` called this way");
                };
                let method = match &callee.kind {
                    ExprKind::Index { base, .. } => base.as_ref(),
                    _ => callee,
                };
                let base = match (info.receiver, &method.kind) {
                    (Some(Receiver::Expr), ExprKind::Field { base, .. }) => Some(base.as_ref()),
                    _ => None,
                };
                let Some(Arg::Place(place)) =
                    self.receiver_arg(receiver, base, &info.through, ParamMode::Mut, None, span)
                else {
                    return None;
                };
                let f = self.place_addr(&place, span)?;
                let x = match info.args.get(1) {
                    Some(ArgSource::Arg(i)) => &args[*i].value,
                    _ => return self.unsupported(span, "`Fmt.put` called this way"),
                };
                self.fmt_put(f, x)
            }
            _ => self.unsupported(span, &format!("`{path}` on Treadle")),
        }
    }
}
