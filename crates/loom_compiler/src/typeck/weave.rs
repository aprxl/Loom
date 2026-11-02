//! Wefts and weaves: which types weave which wefts, and which function
//! implements each weft method for each type.
//!
//! A weave is written as a `weave W into T { … }` item, or inline, by listing
//! the weft in a warp's header and writing its methods in the warp's body.
//! Either way it becomes a [`Weaving`]. A method the weaving doesn't write
//! comes from the weft's default body, checked with `Self` standing for the
//! woven type, or, for std's `#builtin` methods (`Show.show`, `Eq.eq`, …),
//! from the compiler.
//!
//! Weaving a weft means weaving its super-wefts too. Those the program
//! doesn't weave itself are derived when every method they have has a
//! default: `warp ParseError: Error` gets `Show` that way.
//!
//! A generic weave (`weave[T: Show] Show into Boxed[T]`, or a generic warp's
//! header) is written for a pattern. It applies to a type that matches the
//! pattern, when what its parameters stand for meets their bounds.

use loom_syntax::Diagnostic;
use loom_syntax::ast::ParamMode;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{BodyId, Checker, FnSig, Forward, Instance};
use crate::hir::{self, Binding, DefId, DefKind};
use crate::ty::{ConstVal, Ty, TyKind};

/// A weft woven into a type.
#[derive(Clone, Debug)]
pub struct Weaving {
    pub weft: DefId,
    pub target: Ty,
    /// The `weave` item, or the warp whose header lists the weft.
    pub site: DefId,
    /// Where the weft is named, for messages.
    pub span: Span,
    /// Derived because another weaving's weft needs it, not written.
    pub implied: bool,
}

/// A weaving that applies to a type, with what its generic parameters
/// stand for there.
#[derive(Clone, Debug)]
pub(super) struct Match {
    pub w: usize,
    pub bindings: Vec<(DefId, Ty)>,
}

/// The function behind one weft method for one type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Impl {
    /// Written in the weave, or in the warp's body.
    Written(DefId),
    /// The weft's default body.
    Default(DefId),
    /// Made by the compiler: a `#builtin` method of std.
    Builtin(DefId),
    /// Forwarded to a `through` field that has the method.
    Forward(DefId),
}

impl Checker<'_> {
    /// Finds every weave in the program, once. Generic weaves and weaves of
    /// generic wefts aren't supported yet; they're reported where they're
    /// written, when that's in the root module.
    pub(super) fn collect_weaves(&mut self) {
        let p = self.p;
        for (i, d) in p.defs.iter().enumerate() {
            let site = DefId(i as u32);
            match &d.kind {
                DefKind::Weave(w) => {
                    let quiet = Some(d.module) != p.root;
                    let mark = self.diags.len();
                    let saved = self.current.replace(site);
                    let params = w.generics.iter().map(|&g| (g, self.tcx.intern(TyKind::Param(g)))).collect();
                    let saved_subst = std::mem::replace(&mut self.subst, params);
                    let weft = self.weft_of(&w.weft);
                    let target = self.lower_ty(&w.target);
                    self.subst = saved_subst;
                    self.current = saved;
                    if quiet {
                        self.diags.truncate(mark);
                    }
                    if let Some(weft) = weft
                        && !self.tcx.is_error(target)
                    {
                        self.add_weaving(weft, target, site, w.weft.span, false);
                    }
                }
                DefKind::Warp(w) => {
                    // A generic warp's weaves are written for its template,
                    // `Pair[A, B]`, and hold for every `Pair`.
                    let params = w.generics.iter().map(|&g| self.tcx.intern(TyKind::Param(g))).collect();
                    let target = self.tcx.intern(TyKind::Adt(site, params));
                    for t in &w.wefts {
                        if let Some(weft) = self.weft_of(t) {
                            self.add_weaving(weft, target, site, t.span, false);
                        }
                    }
                }
                _ => {}
            }
        }
        // Super-wefts nobody wove are derived, if they can be.
        let mut i = 0;
        while i < self.weavings.len() {
            let w = self.weavings[i].clone();
            for sup in self.supers(w.weft) {
                if self.weaving(sup, w.target).is_none() && self.derivable(sup) {
                    self.add_weaving(sup, w.target, w.site, w.span, true);
                }
            }
            i += 1;
        }
    }

    fn add_weaving(&mut self, weft: DefId, target: Ty, site: DefId, span: Span, implied: bool) {
        let index = self.weavings.len();
        self.weave_index.entry((weft, target)).or_insert(index);
        self.weavings.push(Weaving { weft, target, site, span, implied });
    }

    /// The weft a type names, if it names a weft without arguments.
    pub(super) fn weft_of(&self, t: &hir::Ty) -> Option<DefId> {
        match &t.kind {
            hir::TyKind::Def { def, args } if args.is_empty() && matches!(self.p.def(*def).kind, DefKind::Weft(_)) => {
                Some(*def)
            }
            _ => None,
        }
    }

    /// The super-wefts of `weft`: `Eq` for `Ord: Eq`.
    pub(super) fn supers(&self, weft: DefId) -> Vec<DefId> {
        match &self.p.def(weft).kind {
            DefKind::Weft(w) => w.supers.iter().filter_map(|t| self.weft_of(t)).collect(),
            _ => Vec::new(),
        }
    }

    /// The methods `weft` declares (not its super-wefts').
    pub(super) fn weft_methods(&self, weft: DefId) -> Vec<DefId> {
        match &self.p.def(weft).kind {
            DefKind::Weft(w) => {
                w.members.iter().copied().filter(|&m| matches!(self.p.def(m).kind, DefKind::Fn(_))).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether every method of `weft` has a default, so weaving it needs
    /// nothing written.
    fn derivable(&self, weft: DefId) -> bool {
        self.weft_methods(weft).into_iter().all(|m| self.default_of(m).is_some())
    }

    /// The compiler-made default of a std weft method, if it has one.
    pub(super) fn builtin_default(&self, method: DefId) -> Option<Impl> {
        self.default_of(method).filter(|imp| matches!(imp, Impl::Builtin(_)))
    }

    /// The default a weft method falls back on, if it has one.
    fn default_of(&self, method: DefId) -> Option<Impl> {
        let d = self.p.def(method);
        let DefKind::Fn(f) = &d.kind else { return None };
        if f.body.is_some() {
            Some(Impl::Default(method))
        } else if d.attrs.iter().any(|a| a.name == "builtin") {
            Some(Impl::Builtin(method))
        } else {
            None
        }
    }

    /// The weaving of `weft` into `ty`, if there is one: for `ty` itself, or
    /// a generic one whose pattern `ty` matches.
    pub(super) fn weaving(&self, weft: DefId, ty: Ty) -> Option<Match> {
        let ty = self.resolve(ty);
        if let Some(&w) = self.weave_index.get(&(weft, ty)) {
            return Some(Match { w, bindings: Vec::new() });
        }
        (0..self.weavings.len()).filter(|&w| self.weavings[w].weft == weft).find_map(|w| self.matches(w, ty))
    }

    /// Whether weaving `w` applies to `ty`, and what its parameters stand
    /// for if so.
    fn matches(&self, w: usize, ty: Ty) -> Option<Match> {
        let target = self.weavings[w].target;
        if target == ty {
            return Some(Match { w, bindings: Vec::new() });
        }
        if !self.tcx.has_params(target) {
            return None;
        }
        let mut bindings = Vec::new();
        if !self.match_pattern(target, ty, &mut bindings) {
            return None;
        }
        // Each parameter's bounds must hold for what it stands for.
        for &(param, arg) in &bindings {
            for weft in self.param_bounds(self.weavings[w].site, param) {
                if !self.weaves(arg, weft) {
                    return None;
                }
            }
        }
        Some(Match { w, bindings })
    }

    /// Matches `ty` against `pattern`, a type with generic parameters in it,
    /// recording what each parameter stands for.
    fn match_pattern(&self, pattern: Ty, ty: Ty, bindings: &mut Vec<(DefId, Ty)>) -> bool {
        let ty = self.resolve(ty);
        match (self.tcx.kind(pattern), self.tcx.kind(ty)) {
            (TyKind::Param(p), _) => match bindings.iter().find(|(q, _)| q == p) {
                Some(&(_, bound)) => bound == ty,
                None => {
                    bindings.push((*p, ty));
                    true
                }
            },
            (TyKind::Adt(d1, a1), TyKind::Adt(d2, a2)) if d1 == d2 && a1.len() == a2.len() => {
                a1.clone().into_iter().zip(a2.clone()).all(|(p, t)| self.match_pattern(p, t, bindings))
            }
            (TyKind::Tuple(a1), TyKind::Tuple(a2)) if a1.len() == a2.len() => {
                a1.clone().into_iter().zip(a2.clone()).all(|(p, t)| self.match_pattern(p, t, bindings))
            }
            (TyKind::Optional(p), TyKind::Optional(t)) => self.match_pattern(*p, *t, bindings),
            (TyKind::Array(p, a), TyKind::Array(t, b)) if a == b => self.match_pattern(*p, *t, bindings),
            (TyKind::ArrayOf(p, n), TyKind::Array(t, len)) => {
                let Some(len) = self.tcx.find(&TyKind::Value(ConstVal::Int(i128::from(*len)))) else { return false };
                self.match_pattern(*p, *t, bindings) && self.match_pattern(*n, len, bindings)
            }
            _ => pattern == ty,
        }
    }

    /// The wefts generic parameter `param` of `site` (a weave, or a generic
    /// warp) must weave.
    fn param_bounds(&self, site: DefId, param: DefId) -> Vec<DefId> {
        let mut wefts = Vec::new();
        if let DefKind::GenericParam(g) = &self.p.def(param).kind
            && let hir::GenericParamKind::Type { bounds } = &g.kind
        {
            wefts.extend(bounds.iter().filter_map(|t| self.weft_of(t)));
        }
        let preds = match &self.p.def(site).kind {
            DefKind::Weave(w) => w.where_clause.as_slice(),
            DefKind::Warp(w) => w.where_clause.as_slice(),
            _ => &[],
        };
        for pred in preds {
            if matches!(&pred.ty.kind, hir::TyKind::Def { def, .. } if *def == param) {
                wefts.extend(pred.bounds.iter().filter_map(|t| self.weft_of(t)));
            }
        }
        wefts
    }

    /// What implements weft method `method` in weaving `w`, for type `ty`:
    /// the function with its name in the weave (or the warp's body), or one
    /// a `through` field has, or else the default.
    pub(super) fn impl_of(&mut self, w: usize, method: DefId, ty: Ty, depth: u32) -> Option<Impl> {
        let p = self.p;
        let name = &p.def(method).name;
        let site = self.weavings[w].site;
        let written = match &p.def(site).kind {
            DefKind::Weave(weave) if !self.weavings[w].implied => weave.members.as_slice(),
            DefKind::Warp(warp) => warp.members.as_slice(),
            _ => &[],
        };
        let found =
            written.iter().copied().find(|&m| &p.def(m).name == name && matches!(p.def(m).kind, DefKind::Fn(_)));
        if let Some(def) = found {
            return Some(Impl::Written(def));
        }
        if !self.tcx.has_params(ty) && !self.forwarded(ty, &name.clone(), depth).is_empty() {
            return Some(Impl::Forward(method));
        }
        self.default_of(method)
    }

    /// The instance a call to `imp` for type `ty` runs. `bindings` says what
    /// a generic weave's parameters stand for.
    pub(super) fn impl_instance(&self, imp: Impl, ty: Ty, bindings: &[(DefId, Ty)]) -> Instance {
        match imp {
            Impl::Written(def) => {
                let params = self.generic_params(def);
                let args: Option<Vec<Ty>> =
                    params.iter().map(|g| bindings.iter().find(|(p, _)| p == g).map(|(_, t)| *t)).collect();
                match args {
                    Some(args) if !params.is_empty() => Instance { args, ..Instance::of(def) },
                    _ => self.member_instance(def, ty),
                }
            }
            Impl::Default(def) | Impl::Builtin(def) => Instance { self_ty: Some(ty), ..Instance::of(def) },
            Impl::Forward(def) => Instance { self_ty: Some(ty), forward: true, ..Instance::of(def) },
        }
    }

    /// Methods named `name` that weaves give `ty`, from wefts in scope here,
    /// with the instance each would run.
    pub(super) fn weave_methods(&mut self, ty: Ty, name: &str, depth: u32) -> Vec<(DefId, Instance)> {
        let ty = self.resolve(ty);
        let mut found = Vec::new();
        for w in 0..self.weavings.len() {
            let weft = self.weavings[w].weft;
            if !self.weft_in_scope(weft) {
                continue;
            }
            let Some(m) = self.matches(w, ty) else { continue };
            for method in self.weft_methods(weft) {
                if self.p.def(method).name == name
                    && let Some(imp) = self.impl_of(w, method, ty, depth)
                {
                    found.push((weft, self.impl_instance(imp, ty, &m.bindings)));
                }
            }
        }
        found.dedup_by_key(|(weft, _)| *weft);
        found
    }

    /// Whether the code being checked can name `weft`: a weave's methods are
    /// only found through a weft that's in scope.
    fn weft_in_scope(&self, weft: DefId) -> bool {
        let Some(current) = self.current else { return false };
        let module = self.p.def(current).module;
        if self.p.def(weft).module == module {
            return true;
        }
        self.p.module_name(module, &self.p.def(weft).name) == Some(Binding::Def(weft))
    }

    // ------------------------------------------------------------ conformance

    /// Checks the weaves written at `site` (a `weave` item, or a warp with
    /// wefts in its header), and queues the functions they use.
    pub(super) fn check_weaves_at(&mut self, site: DefId) {
        let indices: Vec<usize> =
            self.weavings.iter().enumerate().filter(|(_, w)| w.site == site).map(|(i, _)| i).collect();
        for i in indices {
            self.check_weaving(i);
        }
        // Functions in a weave that aren't methods of its weft.
        let p = self.p;
        if let DefKind::Weave(weave) = &p.def(site).kind
            && let Some(weft) = self.weft_of(&weave.weft)
        {
            let methods: Vec<String> = self.weft_methods(weft).iter().map(|&m| p.def(m).name.clone()).collect();
            let assoc: Vec<String> = self.assoc_types(weft).iter().map(|&a| p.def(a).name.clone()).collect();
            for &member in &weave.members {
                let md = p.def(member);
                if matches!(md.kind, DefKind::Type(_)) && !assoc.contains(&md.name) {
                    let weft_name = p.def(weft).name.clone();
                    self.diags.push(
                        Diagnostic::error(
                            codes::T0114,
                            format!("`{}` isn't an associated type of `{weft_name}`", md.name),
                        )
                        .primary(md.span, "not in the weft"),
                    );
                }
                if matches!(md.kind, DefKind::Fn(_)) && !methods.contains(&md.name) {
                    let weft_name = p.def(weft).name.clone();
                    self.diags.push(
                        Diagnostic::error(codes::T0114, format!("`{}` isn't a method of `{weft_name}`", md.name))
                            .primary(md.span, "not in the weft")
                            .help("a weave only holds the weft's methods; put other functions in the warp"),
                    );
                }
            }
        }
    }

    fn check_weaving(&mut self, i: usize) {
        let w = self.weavings[i].clone();
        let p = self.p;
        let weft_name = p.def(w.weft).name.clone();
        let target = self.show(w.target);
        if let Some(&first) = self.weave_index.get(&(w.weft, w.target))
            && first != i
        {
            if !w.implied {
                let first_span = self.weavings[first].span;
                self.diags.push(
                    Diagnostic::error(codes::T0115, format!("`{target}` weaves `{weft_name}` twice"))
                        .primary(w.span, "woven again here")
                        .secondary(first_span, "first woven here")
                        .help("a type weaves each weft once"),
                );
            }
            return;
        }
        if !w.implied && !self.orphan_ok(&w) {
            self.diags.push(
                Diagnostic::error(
                    codes::T0115,
                    format!("can't weave `{weft_name}` into `{target}` here: neither is defined in this program"),
                )
                .primary(w.span, "both come from elsewhere")
                .help(format!("wrap it in a warp of your own, like `warp Wrapped({target})`, and weave that")),
            );
            return;
        }
        for sup in self.supers(w.weft) {
            if !self.weaves(w.target, sup) {
                let sup_name = p.def(sup).name.clone();
                self.diags.push(
                    Diagnostic::error(
                        codes::T0108,
                        format!("`{target}` weaves `{weft_name}`, which needs `{sup_name}` too"),
                    )
                    .primary(w.span, format!("`{target}` doesn't weave `{sup_name}`"))
                    .help(format!("weave it: `weave {sup_name} into {target} {{ … }}`")),
                );
            }
        }
        if Some(w.weft) == self.std.copy && !self.is_copy(w.target) {
            self.diags.push(
                Diagnostic::error(codes::T0108, format!("`{target}` can't be `Copy`"))
                    .primary(w.span, "some of its fields are moved, not copied")
                    .note("a warp is `Copy` when every field is"),
            );
        }
        // Without its associated types, the weft's methods can't be read.
        if !w.implied && !self.check_assoc_bindings(w.weft, w.site, w.target, w.span) {
            return;
        }
        // A generic weave's methods are instantiated for each type it's used
        // with; only what's written is checked here, for the pattern.
        let generic = self.tcx.has_params(w.target);
        for method in self.weft_methods(w.weft) {
            let name = p.def(method).name.clone();
            match self.impl_of(i, method, w.target, 0) {
                None => {
                    self.diags.push(
                        Diagnostic::error(
                            codes::T0113,
                            format!("`{target}` doesn't have `{name}`, which `{weft_name}` needs"),
                        )
                        .primary(w.span, format!("`{name}` is missing"))
                        .secondary(p.def(method).span, "declared here"),
                    );
                }
                Some(Impl::Written(def)) => {
                    let id = self.queue_def(def);
                    if self.check_impl_sig(id, method, w.target, &weft_name) && !generic {
                        self.record_op_fn(w.weft, w.target, id);
                    }
                }
                Some(_) if generic => {}
                Some(imp @ Impl::Default(_)) => {
                    let inst = self.impl_instance(imp, w.target, &[]);
                    let id = self.instance(inst);
                    self.record_op_fn(w.weft, w.target, id);
                }
                Some(imp @ Impl::Builtin(_)) => {
                    if self.check_derive(&w, &name) {
                        let inst = self.impl_instance(imp, w.target, &[]);
                        self.instance(inst);
                    }
                }
                Some(imp @ Impl::Forward(_)) => {
                    if self.check_forward(&w, method, &weft_name) {
                        let inst = self.impl_instance(imp, w.target, &[]);
                        let id = self.instance(inst);
                        self.record_op_fn(w.weft, w.target, id);
                    }
                }
            }
        }
    }

    /// A weft method a `through` field provides must be the only one, and
    /// must take the same arguments and return the same type as the weft's.
    fn check_forward(&mut self, w: &Weaving, method: DefId, weft_name: &str) -> bool {
        let name = self.p.def(method).name.clone();
        let target = self.show(w.target);
        let mut found = self.forwarded(w.target, &name, 0);
        if found.len() > 1 {
            let vias: Vec<String> = found.iter().map(|f| format!("`{}`", f.via)).collect();
            self.diags.push(
                Diagnostic::error(codes::T0116, format!("`{target}` gets `{name}` through more than one field"))
                    .primary(w.span, format!("could be {}", vias.join(" or ")))
                    .help(format!("write `{name}` in `{target}` itself to choose")),
            );
            return false;
        }
        let Some(found) = found.pop() else { return false };
        let inner = self.instance(found.inst);
        let (Some(actual), Some(expected)) = (self.fn_sig(inner), self.sig_with_self(method, w.target)) else {
            return false;
        };
        let same = actual.params.len() == expected.params.len()
            && actual.params.first().map(|p| p.mode) == expected.params.first().map(|p| p.mode)
            && actual.params.iter().zip(&expected.params).skip(1).all(|(a, e)| a.mode == e.mode && a.ty == e.ty)
            && actual.ret == expected.ret;
        if !same {
            let (want, got) = (self.show_sig(&expected), self.show_sig(&actual));
            self.diags.push(
                Diagnostic::error(codes::T0114, format!("`{}` doesn't match `{weft_name}.{name}`", found.via))
                    .primary(w.span, format!("`{}` is `{got}`", found.via))
                    .secondary(self.p.def(method).span, format!("`{weft_name}` needs `{want}`")),
            );
            return false;
        }
        true
    }

    /// Records what a forwarding instance calls: the method a `through`
    /// field has, and the fields to follow to reach it.
    pub(super) fn check_forward_instance(&mut self, id: BodyId) {
        let inst = self.r.instance(id).clone();
        let Some(ty) = inst.self_ty else { return };
        let name = self.p.def(inst.def).name.clone();
        let mut found = self.forwarded(ty, &name, 0);
        if found.len() != 1 {
            return;
        }
        let found = found.remove(0);
        let target = self.instance(found.inst);
        self.r.forwards.insert(id, Forward { path: found.path, target });
    }

    /// The orphan rule: a weave must be defined next to its weft or its
    /// type, so two programs can't weave the same thing differently.
    fn orphan_ok(&self, w: &Weaving) -> bool {
        let local = |def: DefId| !self.is_std_module(self.p.def(def).module);
        // std weaves its own wefts into its own types.
        if !local(w.site) || local(w.weft) {
            return true;
        }
        matches!(self.tcx.kind(w.target), TyKind::Adt(def, _) if local(*def))
    }

    pub(super) fn is_std_module(&self, module: hir::ModuleId) -> bool {
        let path = &self.p.module(module).path;
        path == "std" || path.starts_with("std.")
    }

    /// Remembers a hand-written method that a built-in operation uses.
    fn record_op_fn(&mut self, weft: DefId, ty: Ty, id: BodyId) {
        let slot = self.r.op_fns.entry(ty).or_default();
        if Some(weft) == self.std.show {
            slot.show = Some(id);
        } else if Some(weft) == self.std.eq {
            slot.eq = Some(id);
        } else if Some(weft) == self.std.ord {
            slot.lt = Some(id);
        }
    }

    /// A written method must have the weft's signature, with `Self` as the
    /// woven type. Returns whether it does.
    fn check_impl_sig(&mut self, id: BodyId, method: DefId, target: Ty, weft_name: &str) -> bool {
        let Some(actual) = self.fn_sig(id) else { return false };
        let Some(expected) = self.sig_with_self(method, target) else { return false };
        let same = actual.params.len() == expected.params.len()
            && actual.params.iter().zip(&expected.params).all(|(a, e)| a.mode == e.mode && a.ty == e.ty)
            && actual.ret == expected.ret;
        if same {
            return true;
        }
        let def = self.r.instance(id).def;
        let name = self.p.def(def).name.clone();
        let (want, got) = (self.show_sig(&expected), self.show_sig(&actual));
        self.diags.push(
            Diagnostic::error(codes::T0114, format!("`{name}` doesn't match `{weft_name}.{name}`"))
                .primary(self.p.def(def).span, format!("this is `{got}`"))
                .secondary(self.p.def(method).span, format!("`{weft_name}` needs `{want}`")),
        );
        false
    }

    /// The signature of weft method `method` with `Self` standing for `ty`,
    /// without checking its body.
    pub(super) fn sig_with_self(&mut self, method: DefId, ty: Ty) -> Option<FnSig> {
        let DefKind::Fn(f) = &self.p.def(method).kind else { return None };
        let saved_self = self.self_ty.replace(ty);
        let saved = self.current.replace(method);
        let saved_subst = std::mem::take(&mut self.subst);
        let sig = self.lower_sig(f, self.p.def(method).span);
        self.subst = saved_subst;
        self.current = saved;
        self.self_ty = saved_self;
        sig
    }

    fn show_sig(&mut self, sig: &FnSig) -> String {
        let mut params = Vec::with_capacity(sig.params.len());
        for p in &sig.params {
            let ty = if p.name == "self" { "self".to_string() } else { self.show(p.ty) };
            params.push(match p.mode {
                ParamMode::Mut => format!("mut {ty}"),
                ParamMode::Take => format!("take {ty}"),
                _ => ty,
            });
        }
        let ret = self.show(sig.ret);
        format!("fun({}) -> {ret}", params.join(", "))
    }

    /// A derived method works field by field, so every field must weave
    /// the weft too. Returns whether it can be derived.
    fn check_derive(&mut self, w: &Weaving, method: &str) -> bool {
        let needs = |this: &Self| {
            if Some(w.weft) == this.std.eq || Some(w.weft) == this.std.ord || Some(w.weft) == this.std.hash {
                Some(w.weft)
            } else {
                None
            }
        };
        let Some(weft) = needs(self) else { return true };
        // A generic warp's fields are checked for each type it's used with.
        if self.tcx.has_params(w.target) {
            return true;
        }
        let Some(info) = self.adt_info(w.target) else {
            // Only warps can be derived; other types weave these wefts
            // built in, or by hand.
            let (target, weft_name) = (self.show(w.target), self.p.def(w.weft).name.clone());
            self.diags.push(
                Diagnostic::error(
                    codes::T0113,
                    format!("`{target}` doesn't have `{method}`, which `{weft_name}` needs"),
                )
                .primary(w.span, "only warps can derive it"),
            );
            return false;
        };
        let fields: Vec<(String, Ty)> = info
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty))
            .chain(info.cases.iter().flat_map(|c| {
                c.fields.iter().flatten().enumerate().map(move |(i, f)| {
                    let name = f.name.clone().unwrap_or_else(|| i.to_string());
                    (format!("{}.{name}", c.name), f.ty)
                })
            }))
            .collect();
        let mut ok = true;
        for (name, ty) in fields {
            if !self.weaves(ty, weft) {
                let (shown, target, weft_name) = (self.show(ty), self.show(w.target), self.p.def(weft).name.clone());
                self.diags.push(
                    Diagnostic::error(codes::T0108, format!("can't derive `{weft_name}` for `{target}`"))
                        .primary(w.span, format!("`{name}` is a `{shown}`, which isn't `{weft_name}`"))
                        .help(format!("write `{method}` by hand, or weave `{weft_name}` into `{shown}`")),
                );
                ok = false;
            }
        }
        ok
    }
}
