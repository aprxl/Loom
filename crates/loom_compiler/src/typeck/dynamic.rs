//! `any W`: values of some type that weaves `W`, with dynamic dispatch.
//!
//! An `any` value carries its type's vtable: its runtime descriptor, and a
//! function for each weft method the program calls through an `any`
//! (`w.draw(…)` with `w: any Widget`). Vtables are filled in once every body
//! is checked, since a call anywhere can add a method to all of them.
//!
//! A method that takes or returns `Self` besides its receiver can't be
//! called through `any`: there's no one `Self` to check it against.

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{Checker, Coercion, FnSig};
use crate::hir::{self, DefId, DefKind, Pat, TyKind as HirTy};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    /// `wefts` and all their super-wefts, sorted.
    pub(super) fn weft_closure(&self, wefts: &[DefId]) -> Vec<DefId> {
        let mut out: Vec<DefId> = Vec::new();
        let mut stack = wefts.to_vec();
        while let Some(w) = stack.pop() {
            if !out.contains(&w) {
                out.push(w);
                stack.extend(self.super_heads(w));
            }
        }
        out.sort();
        out
    }

    /// The weft method `name` a value of `any wefts` has, and its signature
    /// with `Self` as that `any` type. `None` if there's no such method, or
    /// it can't be called dynamically (reported).
    pub(super) fn dyn_method(&mut self, any: Ty, wefts: &[DefId], name: &str, span: Span) -> Option<(DefId, FnSig)> {
        let found: Vec<DefId> = self
            .weft_closure(wefts)
            .into_iter()
            .flat_map(|w| self.weft_methods(w))
            .filter(|&m| self.p.def(m).name == name)
            .collect();
        let shown = self.show(any);
        let method = match found.as_slice() {
            [] => {
                self.error(codes::T0111, span, format!("`{shown}` has no method `{name}`"), "no such method");
                return None;
            }
            [m] => *m,
            _ => {
                let vias: Vec<String> = found
                    .iter()
                    .map(|&m| format!("`{}.{name}`", self.p.def(self.p.def(m).parent.unwrap_or(m)).name))
                    .collect();
                self.diags.push(
                    Diagnostic::error(codes::T0116, format!("`{shown}` has more than one method `{name}`"))
                        .primary(span, format!("could be {}", vias.join(" or "))),
                );
                return None;
            }
        };
        let DefKind::Fn(f) = &self.p.def(method).kind else { return None };
        let mentions_self = f.params.iter().skip(1).filter_map(|p| p.ty.as_ref()).any(mentions_self_ty)
            || matches!(&f.ret, hir::FnRet::Value(t) if mentions_self_ty(t));
        if !f.has_self() || mentions_self {
            let weft = self.p.def(self.p.def(method).parent.unwrap_or(method)).name.clone();
            self.diags.push(
                Diagnostic::error(codes::T0111, format!("`{weft}.{name}` can't be called on `{shown}`"))
                    .primary(span, "not through `any`")
                    .note("a method that takes or returns `Self` needs to know which type `Self` is"),
            );
            return None;
        }
        self.dyn_methods.insert(method);
        let sig = self.sig_with_self(method, any, &[])?;
        Some((method, sig))
    }

    /// `x is b: T`, where `x` is an `any` value or a union: whether it holds
    /// a `T`.
    pub(super) fn check_type_test(&mut self, pat: &Pat, local: hir::LocalId, t: &hir::Ty, scrutinee: Ty) {
        let tested = self.lower_ty(t);
        self.b.local_ty.insert(local, tested);
        let scrutinee = self.resolve(scrutinee);
        if self.tcx.is_error(tested) || self.tcx.is_error(scrutinee) {
            return;
        }
        if matches!(self.tcx.kind(tested), TyKind::Gauge(..)) {
            self.check_gauge_test(pat, tested, scrutinee);
            return;
        }
        if let TyKind::Union(members) = self.tcx.kind(scrutinee).clone() {
            if !members.contains(&tested) {
                let (shown, union) = (self.show(tested), self.show(scrutinee));
                self.diags.push(
                    Diagnostic::error(codes::T0101, format!("`{shown}` isn't one of `{union}`"))
                        .primary(t.span, "so this can never match"),
                );
                return;
            }
            self.b.type_tests.insert(pat.id, tested);
            return;
        }
        let TyKind::Any(wefts) = self.tcx.kind(scrutinee).clone() else {
            let shown = self.show(scrutinee);
            self.error(
                codes::T0101,
                pat.span,
                format!("a type test needs an `any` value or a union, and this is `{shown}`"),
                "its type is already known",
            );
            return;
        };
        for weft in wefts {
            if !self.weaves(tested, weft, &[]) {
                let (shown, weft_name) = (self.show(tested), self.p.def(weft).name.clone());
                self.diags.push(
                    Diagnostic::error(codes::T0108, format!("`{shown}` doesn't weave `{weft_name}`"))
                        .primary(t.span, "so this can never match"),
                );
                return;
            }
        }
        self.b.type_tests.insert(pat.id, tested);
    }

    /// Fills in the vtable of every type made into an `any`: a function for
    /// each weft method called through `any`.
    pub(super) fn register_vtables(&mut self) {
        let mut sites: Vec<(Ty, Vec<DefId>)> = Vec::new();
        for body in &self.r.bodies {
            for (id, coercion) in &body.coercions {
                if let Coercion::ToAny(wefts) = coercion
                    && let Some(&ty) = body.expr_ty.get(id)
                {
                    sites.push((ty, wefts.clone()));
                }
            }
        }
        sites.sort();
        sites.dedup();
        let mut methods: Vec<DefId> = self.dyn_methods.iter().copied().collect();
        methods.sort();
        for (ty, wefts) in sites {
            let closure = self.weft_closure(&wefts);
            let mut entries = Vec::new();
            for &weft in &closure {
                for method in self.weft_methods(weft) {
                    if !methods.contains(&method) {
                        continue;
                    }
                    let imp = match self.weaving(weft, Some(&[]), ty) {
                        Some(m) => self.impl_of(m.w, method, ty, 0).map(|imp| (imp, Some(m.w), m.bindings)),
                        // Built-in weaves (`Show` for numbers) have only
                        // compiler-made methods.
                        None => self.builtin_default(method).map(|imp| (imp, None, Vec::new())),
                    };
                    let Some((imp, w, bindings)) = imp else { continue };
                    // A scalar changed through `any` can't be copied back to
                    // where it came from: the `any` holds its own copy.
                    let mut_self = matches!(&self.p.def(method).kind,
                        DefKind::Fn(f) if f.params.first().is_some_and(|p| p.is_self && p.mode == loom_syntax::ast::ParamMode::Mut));
                    if mut_self && !self.is_table_backed(ty) {
                        let shown = self.show(ty);
                        self.unsupported(
                            self.p.def(method).span,
                            &format!("`mut self` methods of `{shown}` through `any`"),
                        );
                        continue;
                    }
                    let inst = self.impl_instance(imp, ty, w, &bindings);
                    entries.push((method, self.instance(inst)));
                }
            }
            self.r.vtables.insert((ty, closure), entries);
        }
    }
}

/// Whether a HIR type mentions `Self`.
fn mentions_self_ty(t: &hir::Ty) -> bool {
    match &t.kind {
        HirTy::SelfType => true,
        HirTy::Def { args, .. } => args.iter().any(|a| matches!(a, hir::GenericArg::Type(t) if mentions_self_ty(t))),
        HirTy::Tuple(items) => items.iter().any(mentions_self_ty),
        HirTy::Optional(t) | HirTy::Slice(t) | HirTy::Pointer(t) => mentions_self_ty(t),
        HirTy::Result { ok, err } => mentions_self_ty(ok) || mentions_self_ty(err),
        HirTy::Array { elem, .. } => mentions_self_ty(elem),
        HirTy::Fn { params, ret } => params.iter().any(mentions_self_ty) || mentions_self_ty(ret),
        _ => false,
    }
}
