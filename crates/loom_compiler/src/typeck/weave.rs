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

use loom_syntax::Diagnostic;
use loom_syntax::ast::ParamMode;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{BodyId, Checker, FnSig, Instance};
use crate::hir::{self, Binding, DefId, DefKind};
use crate::ty::{Ty, TyKind};

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

/// The function behind one weft method for one type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Impl {
    /// Written in the weave, or in the warp's body.
    Written(DefId),
    /// The weft's default body.
    Default(DefId),
    /// Made by the compiler: a `#builtin` method of std.
    Builtin(DefId),
}

impl Impl {
    pub fn def(self) -> DefId {
        match self {
            Impl::Written(d) | Impl::Default(d) | Impl::Builtin(d) => d,
        }
    }
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
                DefKind::Weave(w) if w.generics.is_empty() => {
                    let quiet = Some(d.module) != p.root;
                    let mark = self.diags.len();
                    let saved = self.current.replace(site);
                    let weft = self.weft_of(&w.weft);
                    let target = self.lower_ty(&w.target);
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
                DefKind::Warp(w) if w.generics.is_empty() => {
                    let target = self.tcx.intern(TyKind::Adt(site, Vec::new()));
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
    fn weft_of(&self, t: &hir::Ty) -> Option<DefId> {
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

    /// The weaving of `weft` into `ty`, if there is one.
    pub(super) fn weaving(&self, weft: DefId, ty: Ty) -> Option<usize> {
        self.weave_index.get(&(weft, self.resolve(ty))).copied()
    }

    /// What implements weft method `method` in weaving `w`: the function
    /// with its name in the weave (or the warp's body), or else the default.
    pub(super) fn impl_of(&self, w: usize, method: DefId) -> Option<Impl> {
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
        match found {
            Some(def) => Some(Impl::Written(def)),
            None => self.default_of(method),
        }
    }

    /// The instance a call to `imp` for type `ty` runs.
    pub(super) fn impl_instance(&self, imp: Impl, ty: Ty) -> Instance {
        match imp {
            Impl::Written(def) => Instance::of(def),
            Impl::Default(def) | Impl::Builtin(def) => Instance { def, self_ty: Some(ty) },
        }
    }

    /// Methods named `name` that weaves give `ty`, from wefts in scope here.
    pub(super) fn weave_methods(&self, ty: Ty, name: &str) -> Vec<(DefId, Impl)> {
        let ty = self.resolve(ty);
        let mut found = Vec::new();
        for (w, weaving) in self.weavings.iter().enumerate() {
            if weaving.target != ty || !self.weft_in_scope(weaving.weft) {
                continue;
            }
            for method in self.weft_methods(weaving.weft) {
                if self.p.def(method).name == name
                    && let Some(imp) = self.impl_of(w, method)
                {
                    found.push((weaving.weft, imp));
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

    /// Reports that several wefts give `ty` a method `name`.
    pub(super) fn ambiguous_method(&mut self, span: Span, ty: Ty, name: &str, wefts: &[DefId]) {
        let shown = self.show(ty);
        let names: Vec<String> = wefts.iter().map(|w| format!("`{}.{name}`", self.p.def(*w).name)).collect();
        let first = self.p.def(wefts[0]).name.clone();
        self.diags.push(
            Diagnostic::error(codes::T0116, format!("`{shown}` has more than one method `{name}`"))
                .primary(span, format!("could be {}", names.join(" or ")))
                .help(format!("say which one: `{first}.{name}(x)`")),
        );
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
            for &member in &weave.members {
                let md = p.def(member);
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
        for method in self.weft_methods(w.weft) {
            let name = p.def(method).name.clone();
            match self.impl_of(i, method) {
                // `through` fields may forward it; they aren't supported yet
                // and are reported where they're declared.
                None if self.site_has_through(w.site) => {}
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
                    if self.check_impl_sig(id, method, w.target, &weft_name) {
                        self.record_op_fn(w.weft, w.target, id);
                    }
                }
                Some(imp @ Impl::Default(_)) => {
                    let inst = self.impl_instance(imp, w.target);
                    let id = self.instance(inst);
                    self.record_op_fn(w.weft, w.target, id);
                }
                Some(imp @ Impl::Builtin(_)) => {
                    if self.check_derive(&w, &name) {
                        let inst = self.impl_instance(imp, w.target);
                        self.instance(inst);
                    }
                }
            }
        }
    }

    fn site_has_through(&self, site: DefId) -> bool {
        let p = self.p;
        let warp = match &p.def(site).kind {
            DefKind::Warp(_) => Some(site),
            DefKind::Weave(_) => match self
                .tcx
                .kind(self.weavings.iter().find(|w| w.site == site).map(|w| w.target).unwrap_or(self.tcx.error))
            {
                TyKind::Adt(def, _) => Some(*def),
                _ => None,
            },
            _ => None,
        };
        let Some(DefKind::Warp(w)) = warp.map(|d| &p.def(d).kind) else { return false };
        w.fields.iter().any(|&f| matches!(&p.def(f).kind, DefKind::Field(f) if f.through))
    }

    /// The orphan rule: a weave must be defined next to its weft or its
    /// type, so two programs can't weave the same thing differently.
    fn orphan_ok(&self, w: &Weaving) -> bool {
        let local = |def: DefId| !self.is_std_module(self.p.def(def).module);
        if local(w.weft) {
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
    fn sig_with_self(&mut self, method: DefId, ty: Ty) -> Option<FnSig> {
        let DefKind::Fn(f) = &self.p.def(method).kind else { return None };
        let saved_self = self.self_ty.replace(ty);
        let saved = self.current.replace(method);
        let sig = self.lower_sig(f, self.p.def(method).span);
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
        format!("fn({}) -> {ret}", params.join(", "))
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
