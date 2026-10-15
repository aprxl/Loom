//! Lambdas, and functions as values.
//!
//! A lambda gets its parameter types from where it's used: passed where
//! `T => U` is expected (`xs.map(x => x * 2)`), `x` is a `T`. A lambda with
//! nothing expected needs its parameters annotated: `(x: i32) => x * 2`.
//! Its body is a function of its own: `return` leaves the lambda, and a `?`
//! can't reach a `mend` outside it.
//!
//! A value of a function type is called like a function, with its
//! arguments in order. A function's name is a value too, when it isn't
//! generic: `xs.filter(is_prime)`.

use loom_syntax::Diagnostic;
use loom_syntax::ast::ParamMode;
use loom_syntax::diag::codes;

use super::infer::VarKind;
use super::{Checker, Instance};
use crate::hir::{Arg, DefId, Expr, LambdaParam};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    pub(super) fn check_lambda(&mut self, e: &Expr, params: &[LambdaParam], body: &Expr, expected: Option<Ty>) -> Ty {
        let expected = expected.map(|t| self.resolve(t));
        let mut want = match expected.map(|t| self.tcx.kind(t).clone()) {
            Some(TyKind::Fn(ps, ret)) => Some((ps, ret)),
            _ => None,
        };
        // Reported once: a lambda that doesn't fit where it is. Its body is
        // still checked, on its own.
        let mut mismatched = false;
        if let Some((ps, _)) = &want
            && ps.len() != params.len()
        {
            let shown = self.show(expected.expect("a function type"));
            let n = ps.len();
            self.error(
                codes::T0104,
                e.span,
                format!(
                    "this lambda takes {} parameter{}, but `{shown}` takes {n}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" }
                ),
                format!("expected {n} parameter{}", if n == 1 { "" } else { "s" }),
            );
            want = None;
            mismatched = true;
        }
        let mut tys = Vec::with_capacity(params.len());
        for (i, param) in params.iter().enumerate() {
            let given = want.as_ref().map(|(ps, _)| ps[i]);
            let ty = match (&param.ty, given) {
                (Some(t), given) => {
                    let ty = self.lower_ty(t);
                    if let Some(given) = given
                        && !self.unify(ty, given)
                    {
                        self.mismatch(t.span, given, ty);
                        mismatched = true;
                    }
                    ty
                }
                (None, Some(given)) => given,
                (None, None) => {
                    let local = self.p.local(param.local);
                    let name = local.name.clone();
                    if !mismatched && expected.is_none_or(|t| !self.tcx.is_error(t)) {
                        self.diags.push(
                            Diagnostic::error(codes::T0110, format!("can't tell the type of `{name}`"))
                                .primary(local.span, "nothing says what this parameter is")
                                .help(format!("annotate it: `({name}: T) => …`")),
                        );
                    }
                    self.tcx.error
                }
            };
            self.b.local_ty.insert(param.local, ty);
            tys.push(ty);
        }
        let ret = match &want {
            _ if mismatched => self.tcx.error,
            Some((_, ret)) => *ret,
            None => self.new_var(VarKind::Type),
        };
        // The body is a function of its own.
        let saved_ret = self.ret.replace(ret);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_mends = std::mem::take(&mut self.mends);
        let saved_argument = std::mem::replace(&mut self.in_argument, false);
        let saved_defer = self.fence.take();
        let saved_item = std::mem::replace(&mut self.in_item, false);
        self.check_coerce(body, ret);
        self.in_item = saved_item;
        self.fence = saved_defer;
        self.in_argument = saved_argument;
        self.mends = saved_mends;
        self.loops = saved_loops;
        self.ret = saved_ret;
        if mismatched {
            return self.tcx.error;
        }
        self.tcx.intern(TyKind::Fn(tys, ret))
    }

    /// `f(x)` where `f` is a value of a function type.
    pub(super) fn call_value(&mut self, call: &Expr, callee: &Expr, args: &[Arg]) -> Ty {
        let ty = self.check_expr(callee, None);
        let ty = self.resolve(ty);
        let (params, ret) = match self.tcx.kind(ty).clone() {
            TyKind::Fn(params, ret) => (params, ret),
            TyKind::Error => return self.check_args_alone(args),
            _ => {
                let shown = self.show(ty);
                self.error(codes::T0106, callee.span, format!("`{shown}` can't be called"), "not a function");
                return self.check_args_alone(args);
            }
        };
        if args.len() != params.len() {
            let n = params.len();
            let shown = self.show(ty);
            self.error(
                codes::T0104,
                call.span,
                format!("`{shown}` takes {n} argument{}", if n == 1 { "" } else { "s" }),
                format!("{} given", args.len()),
            );
            self.check_args_alone(args);
            return ret;
        }
        for (arg, &param) in args.iter().zip(&params) {
            if let Some(label) = &arg.label {
                self.error(
                    codes::T0105,
                    label.span,
                    "a function value's arguments have no names",
                    "pass it by position",
                );
            }
            if arg.mutable {
                self.error(codes::T0112, arg.span, "a function value doesn't change its arguments", "remove `mut`");
            }
            if arg.is_given {
                self.unsupported(arg.span, "`given:` arguments");
            }
            let saved = std::mem::replace(&mut self.in_argument, true);
            self.check_coerce(&arg.value, param);
            self.in_argument = saved;
        }
        self.b.closure_calls.insert(call.id);
        ret
    }

    /// A function's name used as a value: its instance, typed as a function.
    pub(super) fn fn_value(&mut self, e: &Expr, def: DefId) -> Ty {
        if self.is_generic(def) {
            return self.unsupported(e.span, "generic functions used as values");
        }
        let id = self.instance(Instance::of(def));
        let Some(sig) = self.fn_sig(id) else { return self.tcx.error };
        if sig.params.iter().any(|p| p.mode == ParamMode::Mut) {
            return self.unsupported(e.span, "functions with `mut` parameters used as values");
        }
        self.b.fn_values.insert(e.id, id);
        let params = sig.params.iter().map(|p| p.ty).collect();
        self.tcx.intern(TyKind::Fn(params, sig.ret))
    }
}
