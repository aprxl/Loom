//! Optionals and results: `?`, `mend`, `fail`, and `some`/`ok`/`err`.
//!
//! A postfix `?` unwraps a `?T` or a `T ! E`. When there's nothing to
//! unwrap, it jumps to the nearest `mend` whose left operand contains it, or
//! else leaves the function: with `none` from a function that returns an
//! optional, or with the error, converted to the function's error type.
//! A `?` on an optional that has no value fails with `Missing`.
//!
//! A `mend`'s left operand unwraps too when it's an optional or a result, so
//! `s.parse[i32]() mend 0` needs no `?`. In `mend e => …`, `e` is the union
//! of every error that can reach the `mend`.
//!
//! Leaving a function converts the error (see 04-errors, "Conversions"): a
//! type widens into a union that has it, a warp's `from` case absorbs the
//! type it names, and `any W` takes anything that weaves `W`.

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;
use loom_syntax::token::Keyword;

use super::warp::Key;
use super::{Checker, Coercion};
use crate::hir::{Arg, DefId, DefKind, Expr, HirId, LocalId};
use crate::ty::{Ty, TyKind};

/// What a `?` or a `mend` unwraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unwrap {
    /// A `?T`: `none` has no value, and fails with `Missing`.
    Optional,
    /// A `T ! E`: an `err` carries the error.
    Result,
}

/// How an error becomes the type it's going to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrConv {
    /// It already is that type.
    Same,
    /// Into a union that has its type (see `Coercion::ToUnion`).
    ToUnion,
    /// Into `any W`: the wefts, with their super-wefts.
    ToAny(Vec<DefId>),
    /// Wrapped in the `from` case of an error warp: `case Journal(from IoError)`.
    Absorb { warp: DefId, case: DefId, key: Key },
}

/// Where a `?` goes when there's nothing to unwrap.
#[derive(Clone, Debug)]
pub enum Exit {
    /// To the `mend` with this id; with a conversion when it binds the error.
    Mend(HirId, Option<ErrConv>),
    /// Out of a function that returns an optional, with `none`.
    ReturnNone,
    /// Out of a function that returns a result, with the error converted.
    ReturnErr(ErrConv),
}

/// A `?`.
#[derive(Clone, Debug)]
pub struct TryInfo {
    pub unwrap: Unwrap,
    /// The error it fails with: `Missing` for an optional.
    pub err_ty: Ty,
    pub exit: Exit,
}

/// A `mend`.
#[derive(Clone, Debug)]
pub struct MendInfo {
    /// When the left operand is itself an optional or a result: how it
    /// unwraps, its error type, and the conversion into the bound error.
    pub unwrap: Option<(Unwrap, Ty, Option<ErrConv>)>,
    /// The error `mend e =>` binds, if it does.
    pub binding: Option<LocalId>,
    /// A conversion for the left operand's value, when the handler's type
    /// is wider: `a mend b` with `b: ?T` is a `?T`.
    pub wrap: Option<Coercion>,
}

/// A `mend` whose left operand is being checked.
#[derive(Debug)]
pub(super) struct MendCtx {
    id: HirId,
    errors: Vec<Ty>,
    /// The `?`s that jump to it.
    tries: Vec<HirId>,
}

impl Checker<'_> {
    /// `Missing`, the error of a `?` on an empty optional.
    fn missing_ty(&mut self) -> Ty {
        match self.std.missing {
            Some(def) => {
                let ty = self.tcx.intern(TyKind::Adt(def, Vec::new()));
                self.adt_info(ty);
                ty
            }
            None => self.tcx.error,
        }
    }

    /// `x?`.
    pub(super) fn check_try(&mut self, e: &Expr, inner: &Expr) -> Ty {
        let ty = self.check_expr(inner, None);
        let ty = self.resolve(ty);
        let (value, err_ty, unwrap) = match self.tcx.kind(ty).clone() {
            TyKind::Optional(t) => (t, self.missing_ty(), Unwrap::Optional),
            TyKind::Result(t, err) => (t, err, Unwrap::Result),
            _ if self.tcx.is_lenient(ty) => {
                // Already reported; it may still fail, as far as a `mend` knows.
                if let Some(ctx) = self.mends.last_mut() {
                    ctx.errors.push(self.tcx.error);
                }
                return self.tcx.error;
            }
            _ => {
                let shown = self.show(ty);
                self.error(
                    codes::T0101,
                    e.span,
                    format!("`?` unwraps an optional or a result, and this is `{shown}`"),
                    "nothing to unwrap",
                );
                return self.tcx.error;
            }
        };
        let exit = match self.mends.last_mut() {
            Some(ctx) => {
                ctx.errors.push(err_ty);
                ctx.tries.push(e.id);
                Some(Exit::Mend(ctx.id, None))
            }
            None => self.exit_function(unwrap, err_ty, e.span),
        };
        if let Some(exit) = exit {
            self.b.tries.insert(e.id, TryInfo { unwrap, err_ty, exit });
        }
        value
    }

    /// Where a `?` with no `mend` around it goes: out of the function, which
    /// must be able to carry what it failed with.
    fn exit_function(&mut self, unwrap: Unwrap, err_ty: Ty, span: Span) -> Option<Exit> {
        if self.fence.is_some() {
            self.leaves_fence(span, "?");
            return None;
        }
        // A lambda whose return type nothing gave yet: a `?` in it makes it
        // return what the `?` passes on (`xs.map(s => s.first()? + 1)`).
        if let Some(ret) = self.ret
            && matches!(self.tcx.kind(self.resolve(ret)), TyKind::Var(_))
        {
            let value = self.new_var(super::infer::VarKind::Type);
            let fallible = match unwrap {
                Unwrap::Optional => self.tcx.optional(value),
                Unwrap::Result => self.tcx.intern(TyKind::Result(value, err_ty)),
            };
            self.unify(ret, fallible);
        }
        let ret = self.ret.map(|t| self.resolve(t));
        match ret.map(|t| self.tcx.kind(t).clone()) {
            Some(TyKind::Optional(_)) if unwrap == Unwrap::Optional => Some(Exit::ReturnNone),
            Some(TyKind::Optional(_)) => {
                self.diags.push(
                    Diagnostic::error(codes::T0101, "this `?` would lose the error: the function returns an optional")
                        .primary(span, "fails with an error")
                        .help("say what to do with it: `mend return none`, or return a result"),
                );
                None
            }
            Some(TyKind::Result(_, target)) => match self.error_conversion(err_ty, target) {
                Some(conv) => Some(Exit::ReturnErr(conv)),
                None => {
                    self.cant_convert(span, err_ty, target, "`?` passes on");
                    None
                }
            },
            Some(TyKind::Error) => None,
            _ if self.in_item => {
                self.diags.push(
                    Diagnostic::error(codes::C0406, "`?` can't leave an iteration of a `loom for`")
                        .primary(span, "each iteration is a thread of its own, which ends with its body")
                        .help("handle it in the body: put it inside the left of a `mend`"),
                );
                None
            }
            _ => {
                self.diags.push(
                    Diagnostic::error(codes::T0101, "`?` has nowhere to go")
                        .primary(span, "no `mend` around it, and the function can't fail")
                        .help("put it inside the left of a `mend`, or return `?T` or `T ! E`"),
                );
                None
            }
        }
    }

    fn cant_convert(&mut self, span: Span, from: Ty, to: Ty, what: &str) {
        let (from, to) = (self.show(from), self.show(to));
        self.diags.push(
            Diagnostic::error(
                codes::T0108,
                format!("{what} an error of type `{from}`, but the function's errors are `{to}`"),
            )
            .primary(span, format!("`{from}` can't become `{to}`"))
            .help(format!(
                "widen the function's errors to `{to} | {from}`, or give `{to}` a case like `Wrapped(from {from})`"
            )),
        );
    }

    /// How an error of type `from` becomes a `to`, if it can.
    pub(super) fn error_conversion(&mut self, from: Ty, to: Ty) -> Option<ErrConv> {
        let (from, to) = (self.deep_resolve(from), self.deep_resolve(to));
        if from == to || self.tcx.is_lenient(from) || self.tcx.is_error(to) {
            return Some(ErrConv::Same);
        }
        match (self.tcx.kind(from).clone(), self.tcx.kind(to).clone()) {
            (TyKind::Union(have), TyKind::Union(want)) => {
                have.iter().all(|t| want.contains(t)).then_some(ErrConv::Same)
            }
            (_, TyKind::Union(want)) => want.contains(&from).then_some(ErrConv::ToUnion),
            (TyKind::Any(have), TyKind::Any(want)) => {
                let closure = self.weft_closure(&have);
                want.iter().all(|w| closure.contains(w)).then_some(ErrConv::Same)
            }
            (TyKind::Union(_), TyKind::Any(_)) => None,
            (_, TyKind::Any(wefts)) => {
                wefts.iter().all(|&w| self.weaves(from, w)).then(|| ErrConv::ToAny(self.weft_closure(&wefts)))
            }
            (_, TyKind::Adt(def, _)) => self.absorbing_case(def, to, from),
            _ => None,
        }
    }

    /// The case of error warp `to` that absorbs `from` (`case Journal(from
    /// IoError)`), if it has one that can: its only field, and no fields
    /// shared by every case.
    fn absorbing_case(&mut self, warp: DefId, to: Ty, from: Ty) -> Option<ErrConv> {
        let info = self.adt_info(to)?;
        if !info.fields.is_empty() {
            return None;
        }
        for case in &info.cases {
            let DefKind::Case(c) = &self.p.def(case.def).kind else { continue };
            let (Some(hir_fields), Some(fields)) = (&c.fields, &case.fields) else { continue };
            if let ([hir_field], [field]) = (hir_fields.as_slice(), fields.as_slice())
                && hir_field.absorbs
                && field.ty == from
            {
                return Some(ErrConv::Absorb { warp, case: case.def, key: field.key.clone() });
            }
        }
        None
    }

    /// `a mend b`, `a mend e => b`.
    pub(super) fn check_mend(&mut self, e: &Expr, left: &Expr, binding: Option<LocalId>, handler: &Expr) -> Ty {
        self.mends.push(MendCtx { id: e.id, errors: Vec::new(), tries: Vec::new() });
        let left_ty = self.check_expr(left, None);
        let ctx = self.mends.pop().expect("pushed above");
        let left_ty = self.resolve(left_ty);
        let mut errors = ctx.errors;
        let (success, unwrap) = match self.tcx.kind(left_ty).clone() {
            TyKind::Optional(t) => {
                let missing = self.missing_ty();
                errors.push(missing);
                (t, Some((Unwrap::Optional, missing)))
            }
            TyKind::Result(t, err) => {
                errors.push(err);
                (t, Some((Unwrap::Result, err)))
            }
            _ => (left_ty, None),
        };
        if unwrap.is_none() && errors.is_empty() && !self.tcx.is_lenient(left_ty) {
            let shown = self.show(left_ty);
            self.diags.push(
                Diagnostic::error(codes::T0101, "nothing on the left of this `mend` can fail")
                    .primary(left.span, format!("this is `{shown}`, and there's no `?` in it"))
                    .help("`mend` recovers from an optional or a result, or from a `?` on its left"),
            );
        }
        // What `mend e =>` binds: every error that can get here.
        let mut unwrap_conv = None;
        if let Some(local) = binding {
            let errors: Vec<Ty> = errors.iter().map(|&t| self.deep_resolve(t)).collect();
            let err_ty = self.tcx.union(errors);
            self.b.local_ty.insert(local, err_ty);
            for id in ctx.tries {
                let Some(from) = self.b.tries.get(&id).map(|t| t.err_ty) else { continue };
                let conv = self.error_conversion(from, err_ty);
                if let Some(info) = self.b.tries.get_mut(&id) {
                    info.exit = Exit::Mend(e.id, conv);
                }
            }
            if let Some((_, from)) = unwrap {
                unwrap_conv = self.error_conversion(from, err_ty);
            }
        }
        // The handler gives the value when the left doesn't.
        let handler_ty = self.check_expr(handler, Some(success));
        let handler_ty = self.resolve(handler_ty);
        let success_plain = !matches!(self.tcx.kind(self.resolve(success)), TyKind::Optional(_) | TyKind::Result(..));
        let (result, wrap) = match self.tcx.kind(handler_ty).clone() {
            _ if self.tcx.is_lenient(handler_ty) => (success, None),
            // `flag() mend env_port() mend 8080`: a fallback that may be
            // absent too makes the whole an optional.
            TyKind::Optional(inner) if success_plain && self.unify(inner, success) => {
                (handler_ty, Some(Coercion::ToOptional))
            }
            TyKind::Result(inner, _) if success_plain && self.unify(inner, success) => {
                (handler_ty, Some(Coercion::ToResult))
            }
            _ => {
                self.coerce(handler, handler_ty, success);
                (success, None)
            }
        };
        let unwrap = unwrap.map(|(u, err)| (u, err, unwrap_conv));
        self.b.mends.insert(e.id, MendInfo { unwrap, binding, wrap });
        result
    }

    /// `fail e`: leaves the function with an error.
    pub(super) fn check_fail(&mut self, e: &Expr, value: &Expr) -> Ty {
        let ret = self.ret.map(|t| self.resolve(t));
        let Some(TyKind::Result(_, target)) = ret.map(|t| self.tcx.kind(t).clone()) else {
            // The value isn't checked: without an error type, a leading dot
            // (`fail .Negative`) would only add noise.
            if ret.is_some_and(|t| !self.tcx.is_error(t)) || ret.is_none() {
                self.diags.push(
                    Diagnostic::error(codes::T0101, "`fail` needs a function that can fail")
                        .primary(e.span, "this function doesn't return `T ! E`")
                        .help("declare the error type: `fn name(…) -> T ! E`, or `fn name(…) ! E`"),
                );
            }
            return self.tcx.never;
        };
        let ty = self.check_expr(value, Some(target));
        match self.error_conversion(ty, target) {
            Some(conv) => {
                let ty = self.deep_resolve(ty);
                self.b.fails.insert(e.id, (ty, conv));
            }
            None => self.cant_convert(value.span, ty, target, "`fail` gives"),
        }
        self.tcx.never
    }

    /// `some(x)`, `ok(x)`, `err(e)` as values.
    pub(super) fn check_builtin_case(&mut self, call: &Expr, kw: Keyword, args: &[Arg], expected: Option<Ty>) -> Ty {
        let name = match kw {
            Keyword::Some => "some",
            Keyword::Ok => "ok",
            Keyword::Err => "err",
            _ => "none",
        };
        let [arg] = args else {
            self.error(codes::T0104, call.span, format!("`{name}` takes one value"), "here");
            for arg in args {
                self.check_expr(&arg.value, None);
            }
            return self.tcx.error;
        };
        let expected = expected.map(|t| self.resolve(t)).map(|t| self.tcx.kind(t).clone());
        match (kw, expected) {
            (Keyword::Some, Some(TyKind::Optional(inner))) => {
                self.check_coerce(&arg.value, inner);
                self.tcx.optional(inner)
            }
            (Keyword::Some, _) => {
                let ty = self.check_expr(&arg.value, None);
                self.tcx.optional(ty)
            }
            (Keyword::Ok, Some(TyKind::Result(ok, err))) => {
                self.check_coerce(&arg.value, ok);
                self.tcx.intern(TyKind::Result(ok, err))
            }
            (Keyword::Err, Some(TyKind::Result(ok, err))) => {
                self.check_coerce(&arg.value, err);
                self.tcx.intern(TyKind::Result(ok, err))
            }
            _ => {
                self.check_expr(&arg.value, None);
                self.diags.push(
                    Diagnostic::error(codes::T0110, format!("can't tell which result type `{name}(…)` makes here"))
                        .primary(call.span, "needs a known `T ! E`")
                        .help("annotate what it's assigned to, or let a return type say it"),
                );
                self.tcx.error
            }
        }
    }
}
