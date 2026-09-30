//! Inference for literal types and generic arguments.
//!
//! An integer literal with no expected type gets a fresh `{integer}`
//! variable, which becomes the first concrete numeric type it meets:
//! in `for i in 1..=15 { takes_u32(i) }`, `i` is a `u32`. At the end of each
//! function body, variables nothing constrained become `i32` (or `f64` for
//! `{float}`), and literal range checks that had to wait run then.
//!
//! A generic argument a call doesn't give (`largest(a, b)`) is a type
//! variable that can become any type. The function is instantiated once the
//! body is finished and every variable is known (see `generic.rs`).
//!
//! Variables are local to one body; signatures never contain them.

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{Checker, Coercion};
use crate::ty::{Ty, TyKind};

/// What an inference variable stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VarKind {
    Int,
    Float,
    /// Any type.
    Type,
}

#[derive(Debug)]
pub(super) struct Var {
    kind: VarKind,
    value: Option<Ty>,
}

/// A literal whose range can only be checked once its type is known.
#[derive(Debug)]
pub(super) struct PendingLit {
    pub ty: Ty,
    pub value: i128,
    pub span: Span,
}

impl Checker<'_> {
    pub(super) fn new_var(&mut self, kind: VarKind) -> Ty {
        let id = self.vars.len() as u32;
        self.vars.push(Var { kind, value: None });
        self.tcx.intern(match kind {
            VarKind::Int => TyKind::IntVar(id),
            VarKind::Float => TyKind::FloatVar(id),
            VarKind::Type => TyKind::Var(id),
        })
    }

    /// Follows variable bindings until a type that isn't a bound variable.
    pub(super) fn resolve(&self, mut ty: Ty) -> Ty {
        loop {
            match self.tcx.kind(ty) {
                TyKind::IntVar(v) | TyKind::FloatVar(v) | TyKind::Var(v) => match self.vars[*v as usize].value {
                    Some(next) => ty = next,
                    None => return ty,
                },
                _ => return ty,
            }
        }
    }

    /// `resolve`, applied all the way down.
    pub(super) fn deep_resolve(&mut self, ty: Ty) -> Ty {
        let ty = self.resolve(ty);
        let kind = self.tcx.kind(ty).clone();
        let kind = match kind {
            TyKind::Tuple(items) => TyKind::Tuple(items.into_iter().map(|t| self.deep_resolve(t)).collect()),
            TyKind::Optional(t) => TyKind::Optional(self.deep_resolve(t)),
            TyKind::Array(t, n) => TyKind::Array(self.deep_resolve(t), n),
            TyKind::ArrayOf(t, n) => {
                let (t, n) = (self.deep_resolve(t), self.deep_resolve(n));
                return self.tcx.array_of(t, n);
            }
            TyKind::Result(t, e) => TyKind::Result(self.deep_resolve(t), self.deep_resolve(e)),
            TyKind::Range(t) => TyKind::Range(self.deep_resolve(t)),
            TyKind::Fn(params, ret) => {
                TyKind::Fn(params.into_iter().map(|t| self.deep_resolve(t)).collect(), self.deep_resolve(ret))
            }
            TyKind::Adt(def, args) => TyKind::Adt(def, args.into_iter().map(|t| self.deep_resolve(t)).collect()),
            TyKind::Union(items) => {
                let items: Vec<Ty> = items.into_iter().map(|t| self.deep_resolve(t)).collect();
                return self.tcx.union(items);
            }
            _ => return ty,
        };
        self.tcx.intern(kind)
    }

    fn var_of(&self, ty: Ty) -> Option<(u32, VarKind)> {
        match self.tcx.kind(ty) {
            TyKind::IntVar(v) | TyKind::FloatVar(v) | TyKind::Var(v) => Some((*v, self.vars[*v as usize].kind)),
            _ => None,
        }
    }

    fn bind(&mut self, var: u32, ty: Ty) {
        self.vars[var as usize].value = Some(ty);
    }

    /// Makes `a` and `b` the same type if they can be, binding variables.
    /// An `{integer}` may become a float (`let x: f64 = 0`); a `{float}`
    /// may not become an integer.
    pub(super) fn unify(&mut self, a: Ty, b: Ty) -> bool {
        let (a, b) = (self.resolve(a), self.resolve(b));
        if a == b || self.tcx.is_lenient(a) || self.tcx.is_lenient(b) {
            return true;
        }
        match (self.var_of(a), self.var_of(b)) {
            // A type variable takes whatever it meets, a literal's variable included.
            (Some((va, VarKind::Type)), _) => return self.bind_var(va, VarKind::Type, b),
            (_, Some((vb, VarKind::Type))) => return self.bind_var(vb, VarKind::Type, a),
            (Some((va, ka)), Some((vb, kb))) => {
                // Two variables: the float one wins, so `1 + 2.5` is a float.
                if ka == VarKind::Float && kb == VarKind::Int {
                    self.bind(vb, a);
                } else {
                    self.bind(va, b);
                }
                return true;
            }
            (Some((v, kind)), None) => return self.bind_var(v, kind, b),
            (None, Some((v, kind))) => return self.bind_var(v, kind, a),
            (None, None) => {}
        }
        match (self.tcx.kind(a).clone(), self.tcx.kind(b).clone()) {
            (TyKind::Tuple(x), TyKind::Tuple(y)) if x.len() == y.len() => {
                x.into_iter().zip(y).all(|(x, y)| self.unify(x, y))
            }
            (TyKind::Optional(x), TyKind::Optional(y)) | (TyKind::Range(x), TyKind::Range(y)) => self.unify(x, y),
            (TyKind::Array(x, n), TyKind::Array(y, m)) if n == m => self.unify(x, y),
            // A length still being inferred: `fn sum[const N: usize](xs: [i32; N])`.
            (TyKind::Array(x, n), TyKind::ArrayOf(y, m)) | (TyKind::ArrayOf(y, m), TyKind::Array(x, n)) => {
                let n = self.tcx.intern(TyKind::Value(crate::ty::ConstVal::Int(i128::from(n))));
                self.unify(x, y) && self.unify(n, m)
            }
            (TyKind::ArrayOf(x, n), TyKind::ArrayOf(y, m)) => self.unify(x, y) && self.unify(n, m),
            (TyKind::Result(x1, x2), TyKind::Result(y1, y2)) => self.unify(x1, y1) && self.unify(x2, y2),
            (TyKind::Adt(d1, x), TyKind::Adt(d2, y)) if d1 == d2 && x.len() == y.len() => {
                x.into_iter().zip(y).all(|(x, y)| self.unify(x, y))
            }
            (TyKind::Fn(p1, r1), TyKind::Fn(p2, r2)) if p1.len() == p2.len() => {
                p1.into_iter().zip(p2).all(|(x, y)| self.unify(x, y)) && self.unify(r1, r2)
            }
            _ => false,
        }
    }

    fn bind_var(&mut self, var: u32, kind: VarKind, ty: Ty) -> bool {
        let fits = match self.tcx.kind(ty) {
            // A type can't contain itself.
            _ if kind == VarKind::Type => !self.mentions_var(ty, var),
            TyKind::Int(_) => kind == VarKind::Int,
            TyKind::Float(_) => true,
            _ => false,
        };
        if fits {
            self.bind(var, ty);
        }
        fits
    }

    /// Whether `ty` (after resolving) mentions variable `var`.
    fn mentions_var(&self, ty: Ty, var: u32) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty) {
            TyKind::IntVar(v) | TyKind::FloatVar(v) | TyKind::Var(v) => *v == var,
            TyKind::Tuple(items) => items.iter().any(|&t| self.mentions_var(t, var)),
            TyKind::Optional(t) | TyKind::Range(t) | TyKind::Array(t, _) => self.mentions_var(*t, var),
            TyKind::Result(t, e) | TyKind::ArrayOf(t, e) => self.mentions_var(*t, var) || self.mentions_var(*e, var),
            TyKind::Fn(params, ret) => {
                params.iter().any(|&t| self.mentions_var(t, var)) || self.mentions_var(*ret, var)
            }
            TyKind::Adt(_, args) | TyKind::Union(args) => args.iter().any(|&t| self.mentions_var(t, var)),
            _ => false,
        }
    }

    /// Whether `ty` (after resolving) still contains a variable.
    pub(super) fn has_vars(&self, ty: Ty) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty) {
            TyKind::IntVar(_) | TyKind::FloatVar(_) | TyKind::Var(_) => true,
            TyKind::Tuple(items) => items.iter().any(|&t| self.has_vars(t)),
            TyKind::Optional(t) | TyKind::Range(t) | TyKind::Array(t, _) => self.has_vars(*t),
            TyKind::Result(t, e) | TyKind::ArrayOf(t, e) => self.has_vars(*t) || self.has_vars(*e),
            TyKind::Fn(params, ret) => params.iter().any(|&t| self.has_vars(t)) || self.has_vars(*ret),
            TyKind::Adt(_, args) | TyKind::Union(args) => args.iter().any(|&t| self.has_vars(t)),
            _ => false,
        }
    }

    /// Ends a body: unconstrained literal variables made since `first_var`
    /// take their defaults, generic calls are instantiated, and the literal
    /// checks that were waiting run.
    pub(super) fn finish_body(&mut self, first_var: usize) {
        for v in first_var..self.vars.len() {
            if self.vars[v].value.is_none() {
                let default = match self.vars[v].kind {
                    VarKind::Int => self.tcx.i32,
                    VarKind::Float => self.tcx.f64,
                    VarKind::Type => continue,
                };
                self.vars[v].value = Some(default);
            }
        }
        self.solve_projections();
        self.projections.clear();
        self.instantiate_pending();
        // Type variables nothing pinned down were reported by the calls
        // that made them.
        for v in first_var..self.vars.len() {
            if self.vars[v].value.is_none() {
                self.vars[v].value = Some(self.tcx.error);
            }
        }
        for (ty, span) in std::mem::take(&mut self.pending_shows) {
            let ty = self.deep_resolve(ty);
            if !self.tcx.is_lenient(ty)
                && !self.tcx.has_error(ty)
                && self.std.show.is_some_and(|show| !self.weaves(ty, show))
            {
                let shown = self.show(ty);
                self.diags.push(
                    Diagnostic::error(codes::T0108, format!("`{shown}` can't be shown in a template"))
                        .primary(span, format!("this is `{shown}`, which doesn't weave `Show`"))
                        .help("`{x:?}` shows any value in its debug form"),
                );
            }
        }
        for lit in std::mem::take(&mut self.pending_lits) {
            let ty = self.resolve(lit.ty);
            if let Some(int) = self.tcx.as_int(ty)
                && !int.contains(lit.value)
            {
                let shown = self.show(ty);
                self.diags.push(
                    Diagnostic::error(codes::T0102, format!("`{}` doesn't fit in `{shown}`", lit.value))
                        .primary(lit.span, format!("`{shown}` holds {} through {}", int.min(), int.max()))
                        .help("use a wider type, or say what should happen: `as%` wraps, `as|` clamps"),
                );
            }
        }
        for (ty, span) in std::mem::take(&mut self.pending_negations) {
            let ty = self.resolve(ty);
            if let Some(int) = self.tcx.as_int(ty)
                && !int.signed
            {
                let shown = self.show(ty);
                self.error(
                    codes::T0103,
                    span,
                    format!("can't negate a `{shown}`"),
                    "only signed integers and floats have `-`",
                );
            }
        }
    }

    fn zonk_source(&mut self, source: super::IterSource) -> super::IterSource {
        match source {
            super::IterSource::Range(t) => super::IterSource::Range(self.deep_resolve(t)),
            super::IterSource::Seq(t) => super::IterSource::Seq(self.deep_resolve(t)),
            super::IterSource::MapPairs(k, v) => {
                super::IterSource::MapPairs(self.deep_resolve(k), self.deep_resolve(v))
            }
            super::IterSource::SetItems(t) => super::IterSource::SetItems(self.deep_resolve(t)),
            super::IterSource::Shuttle(t) => super::IterSource::Shuttle(self.deep_resolve(t)),
            super::IterSource::Burst(t) => super::IterSource::Burst(self.deep_resolve(t)),
            super::IterSource::Woven { next, item } => super::IterSource::Woven { next, item: self.deep_resolve(item) },
        }
    }

    /// Replaces every variable in the results with its final type.
    pub(super) fn zonk(&mut self) {
        for i in 0..self.r.bodies.len() {
            self.in_body(super::BodyId(i as u32), |this| this.zonk_body());
        }
        let consts: Vec<_> = self.r.const_ty.iter().map(|(&k, &v)| (k, v)).collect();
        for (id, ty) in consts {
            let ty = self.deep_resolve(ty);
            self.r.const_ty.insert(id, ty);
        }
    }

    fn zonk_body(&mut self) {
        let exprs: Vec<_> = self.b.expr_ty.iter().map(|(&k, &v)| (k, v)).collect();
        for (id, ty) in exprs {
            let ty = self.deep_resolve(ty);
            self.b.expr_ty.insert(id, ty);
        }
        let pats: Vec<_> = self.b.pat_ty.iter().map(|(&k, &v)| (k, v)).collect();
        for (id, ty) in pats {
            let ty = self.deep_resolve(ty);
            self.b.pat_ty.insert(id, ty);
        }
        let coerced: Vec<_> = self.b.coerced.iter().map(|(&k, &v)| (k, v)).collect();
        for (id, ty) in coerced {
            let ty = self.deep_resolve(ty);
            self.b.coerced.insert(id, ty);
        }
        let locals: Vec<_> = self.b.local_ty.iter().map(|(&k, &v)| (k, v)).collect();
        for (id, ty) in locals {
            let ty = self.deep_resolve(ty);
            self.b.local_ty.insert(id, ty);
        }
        let tests: Vec<_> = self.b.type_tests.iter().map(|(&k, &v)| (k, v)).collect();
        for (id, ty) in tests {
            let ty = self.deep_resolve(ty);
            self.b.type_tests.insert(id, ty);
        }
        let fails: Vec<_> = self.b.fails.iter().map(|(&k, v)| (k, v.0)).collect();
        for (id, ty) in fails {
            let ty = self.deep_resolve(ty);
            if let Some(f) = self.b.fails.get_mut(&id) {
                f.0 = ty;
            }
        }
        let streams: Vec<_> = self.b.streams.iter().map(|(&k, v)| (k, v.clone())).collect();
        for (id, source) in streams {
            let source = self.zonk_source(source);
            self.b.streams.insert(id, source);
        }
        let to_streams: Vec<_> = self
            .b
            .coercions
            .iter()
            .filter_map(|(&k, c)| match c {
                Coercion::ToStream(source) => Some((k, source.clone())),
                _ => None,
            })
            .collect();
        for (id, source) in to_streams {
            let source = self.zonk_source(source);
            self.b.coercions.insert(id, Coercion::ToStream(source));
        }
        // A widening between two types that turned out equal is no conversion.
        let widenings: Vec<_> =
            self.b.coercions.iter().filter(|(_, c)| **c == Coercion::Widen).map(|(&id, _)| id).collect();
        for id in widenings {
            if let Some(&ty) = self.b.expr_ty.get(&id)
                && self.tcx.as_int(ty).is_none()
            {
                self.b.coercions.remove(&id);
            }
        }
    }
}
