//! Lambdas and function values (design/vm.md §10.9).
//!
//! A function value is a function's number and an environment. A lambda is
//! a function of its own whose environment is a run of pointers, in the
//! frame that made it, to the locals it captures (which live in memory: see
//! `scan.rs`), so `xs.each(x => total += x)` changes `total` itself. The
//! escape rule (M0106) keeps a capturing lambda from outliving that frame.
//! A named function used as a value goes through a thunk that takes an
//! environment and ignores it.
//!
//! Every function value of type `(A, B) => R` is called the same way: the
//! result's address first when it's in memory, then the environment, then
//! each argument as a read parameter is passed.

use loom_syntax::ast::ParamMode;
use loom_syntax::source::Span;

use super::value::{Storage, Val};
use super::{Abi, Builder, FnKey, ParamAbi, Vals};
use crate::heddle::{InstKind, Mem, Term, VTy, Value};
use crate::hir::{self, LambdaParam, LocalId};
use crate::ty::{Ty, TyKind};
use crate::typeck::BodyId;

/// A lambda waiting to be built: its expression, and what it captures.
#[derive(Clone, Debug)]
pub(super) struct LambdaInfo<'a> {
    pub expr: &'a hir::Expr,
    pub captures: Vec<LocalId>,
}

impl<'a> Builder<'a> {
    /// The calling convention of function values of type `ty`.
    pub(super) fn fn_abi(&mut self, ty: Ty, span: Span) -> Option<Abi> {
        let TyKind::Fn(params, ret) = self.tcx().kind(self.tcx().degauge(ty)).clone() else {
            return self.internal(span, "calling something that isn't a function");
        };
        let params: Vec<(ParamMode, Ty)> = params.iter().map(|&t| (ParamMode::Read, self.tcx().degauge(t))).collect();
        self.abi(&params, self.tcx().degauge(ret), true, span)
    }

    /// `x => body`: its function, and an environment pointing at what it
    /// captures.
    pub(super) fn lambda(&mut self, e: &'a hir::Expr) -> Option<Val> {
        let captures = self.f.scan.captures.get(&e.id).cloned().unwrap_or_default();
        let env = if captures.is_empty() {
            self.emit1(InstKind::Iconst(0), VTy::Ptr)
        } else {
            let slot = self.raw_slot(8 * captures.len() as u64, 8);
            let env = self.slot_addr(slot);
            for (k, &l) in captures.iter().enumerate() {
                let place = self.local_place(l, e.span)?;
                let at = self.place_addr(&place, e.span)?;
                self.store(Mem::W64, env, 8 * k as i64, at);
            }
            env
        };
        let key = FnKey::Lambda(self.f.body, e.id);
        self.lambdas.insert((self.f.body, e.id), LambdaInfo { expr: e, captures });
        let func = self.func_id(key);
        let f = self.emit1(InstKind::FuncRef(func), VTy::Int);
        Some(Val::Regs(vec![f, env]))
    }

    /// Builds lambda `id` of body `body`; its result types.
    pub(super) fn lambda_func(&mut self, body: BodyId, id: hir::HirId) -> Option<Vec<VTy>> {
        let info = self.lambdas.get(&(body, id)).cloned()?;
        let e = info.expr;
        let span = e.span;
        let hir::ExprKind::Lambda { params, body: inner } = &e.kind else {
            return self.internal(span, "a lambda that isn't one");
        };
        let ty = self.ty_of(e);
        let TyKind::Fn(_, ret) = self.tcx().kind(ty).clone() else {
            return self.internal(span, "a lambda whose type isn't a function's");
        };
        let ret = self.tcx().degauge(ret);
        let abi = self.fn_abi(ty, span)?;
        self.f.scan = self.scan(body, inner);
        let (sret, env, vals) = self.entry_params(&abi);
        self.f.ret = Some(ret);
        self.f.sret = sret;
        let env = env?;
        for (k, &l) in info.captures.iter().enumerate() {
            let at = self.load(Mem::W64, env, 8 * k as i64, VTy::Ptr);
            self.f.locals.insert(l, Storage::At(at));
        }
        let depth = self.open_scope();
        for ((param, pabi), vals) in params.iter().zip(&abi.params).zip(vals) {
            let LambdaParam { local, .. } = param;
            self.bind_param(*local, ParamMode::Read, pabi, vals, span)?;
        }
        let valueless = matches!(self.tcx().kind(ret), TyKind::Unit | TyKind::Never);
        let val = self.expr(inner);
        if self.live() {
            let val = val?;
            let val = if valueless { Val::Regs(Vec::new()) } else { val };
            self.ret_val(val, span)?;
        }
        self.close_scope(depth);
        Some(abi.results)
    }

    /// A named function used as a value: its thunk, and no environment.
    pub(super) fn fn_value(&mut self, id: BodyId) -> Option<Val> {
        let func = self.func_id(FnKey::Thunk(id));
        let f = self.emit1(InstKind::FuncRef(func), VTy::Int);
        let env = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        Some(Val::Regs(vec![f, env]))
    }

    /// The thunk of function instance `id`: takes an environment, which it
    /// ignores, and calls the function with the rest.
    pub(super) fn thunk_func(&mut self, id: BodyId, span: Span) -> Option<Vec<VTy>> {
        self.f.span = None;
        let sig = self.typed.results.sigs.get(&id)?.clone();
        if sig.params.iter().any(|p| p.mode != ParamMode::Read) {
            return self.unsupported(span, "functions with `mut` or `take` parameters as values");
        }
        let params: Vec<Ty> = sig.params.iter().map(|p| p.ty).collect();
        let fn_ty = self.tcx().find(&TyKind::Fn(params, sig.ret));
        let Some(fn_ty) = fn_ty else { return self.internal(span, "a function value with no type") };
        let abi = self.fn_abi(fn_ty, span)?;
        let target = self.body_abi(id, span)?;
        let (sret, _, vals) = self.entry_params(&abi);
        let mut args: Vals = sret.into_iter().collect();
        for v in vals {
            args.extend(v);
        }
        let func = self.func_id(FnKey::Body(id));
        let results = self.emit(InstKind::Call { func, args }, &target.results);
        self.terminate(Term::Ret(results));
        Some(abi.results)
    }

    /// `f(a, b)`, where `f` is a function value.
    pub(super) fn call_value(&mut self, e: &'a hir::Expr, callee: &'a hir::Expr, args: &'a [hir::Arg]) -> Option<Val> {
        let fty = self.coerced_ty(callee);
        let f = self.value(callee)?;
        let mut vals = Vec::with_capacity(args.len());
        for arg in args {
            vals.push(self.expr(&arg.value)?);
        }
        self.call_fn(&f, fty, vals, e.span)
    }

    /// Calls `f`, a function value of type `fty`, with `args`.
    pub(super) fn call_fn(&mut self, f: &[Value], fty: Ty, args: Vec<Val>, span: Span) -> Option<Val> {
        let abi = self.fn_abi(fty, span)?;
        let TyKind::Fn(ptys, ret) = self.tcx().kind(self.tcx().degauge(fty)).clone() else { return None };
        let ret = self.tcx().degauge(ret);
        let mut flat: Vec<Value> = Vec::new();
        let sret = match abi.sret {
            Some(rt) => Some(self.temp(rt, span)?),
            None => None,
        };
        flat.extend(sret);
        flat.push(f[1]);
        for ((v, pabi), &pty) in args.into_iter().zip(&abi.params).zip(&ptys) {
            let pty = self.tcx().degauge(pty);
            match pabi {
                ParamAbi::Regs(_) => flat.extend(self.regs(v, pty, span)?),
                ParamAbi::Addr => flat.push(self.addr_of(v, pty, span)?),
            }
        }
        let results = self.emit(InstKind::CallInd { callee: f[0], args: flat }, &abi.results);
        if matches!(self.tcx().kind(ret), TyKind::Never) {
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
}
