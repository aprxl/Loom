//! Iteration: `for` loops over streams and `Iter` types, the methods of
//! `Stream` that everything with items has, and associated types.
//!
//! `Stream[T]` is std's lazy sequence of `T`s (the runtime makes it a Lua
//! function). Arrays, lists, slices, ranges and types that weave `Iter` all
//! have its methods: `xs.indexed()` calls `Stream.indexed` on a stream of
//! `xs`'s elements. A `for` loop over an `Iter` type loops over such a
//! stream too.
//!
//! An associated type (`type Item` in `Iter`) is what the weave says it is
//! for each type: `weave Iter into Countdown { type Item = u32 … }`.

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::method::Found;
use super::warp::Member;
use super::{Checker, IterSource};
use crate::hir::{DefId, DefKind, Expr};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    /// `Stream[item]`.
    pub(super) fn stream_of(&mut self, item: Ty) -> Option<Ty> {
        let stream = self.std.stream?;
        Some(self.tcx.intern(TyKind::Adt(stream, vec![item])))
    }

    /// `T`, for a `Stream[T]`.
    pub(super) fn stream_item(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Adt(def, args) if Some(*def) == self.std.stream => args.first().copied(),
            _ => None,
        }
    }

    /// Where the items of a value of `ty` come from, and their type, when
    /// it has items but isn't a `Stream`.
    pub(super) fn iter_source(&mut self, ty: Ty) -> Option<(IterSource, Ty)> {
        let ty = self.resolve(ty);
        if let Some(elem) = self.seq_elem(ty) {
            return Some((IterSource::Seq(elem), elem));
        }
        if let TyKind::Range(elem) = *self.tcx.kind(ty) {
            return Some((IterSource::Range(elem), elem));
        }
        let iter = self.std.iter?;
        if self.stream_item(ty).is_some() || self.weaving(iter, ty).is_none() {
            return None;
        }
        // Its `next`, written in it or in its weave.
        let found = self.methods_of(ty, "next", 0);
        let [found] = found.as_slice() else { return None };
        if !found.path.is_empty() {
            return None;
        }
        let next = self.instance(found.inst.clone());
        let sig = self.fn_sig(next)?;
        let item = match self.tcx.kind(sig.ret) {
            TyKind::Optional(item) => *item,
            _ => return None,
        };
        Some((IterSource::Woven { next, item }, item))
    }

    /// Rule 3 of method lookup: a method of `Stream`, for something with
    /// items. `next` isn't one: each call would start a new stream.
    pub(super) fn stream_method(&mut self, ty: Ty, name: &str) -> Option<Found> {
        if name == "next" {
            return None;
        }
        let stream = self.std.stream?;
        let Some(Member::Method(m)) = self.member(stream, name) else { return None };
        let (source, item) = self.iter_source(ty)?;
        let self_ty = self.stream_of(item)?;
        Some(Found {
            inst: self.member_instance(m, self_ty),
            path: Vec::new(),
            self_ty,
            via: format!("Stream.{name}"),
            stream: Some(source),
        })
    }

    /// The items a `for` loop over a stream, or over a type that weaves
    /// `Iter`, binds. `None` if `ty` has no items.
    pub(super) fn for_items(&mut self, e: &Expr, ty: Ty) -> Option<Ty> {
        if let Some(item) = self.stream_item(ty) {
            return Some(item);
        }
        let (source, item) = self.iter_source(ty)?;
        self.b.streams.insert(e.id, source);
        Some(item)
    }

    // ------------------------------------------------------------ associated types

    /// The associated types weft `weft` declares.
    pub(super) fn assoc_types(&self, weft: DefId) -> Vec<DefId> {
        match &self.p.def(weft).kind {
            DefKind::Weft(w) => {
                w.members.iter().copied().filter(|&m| matches!(self.p.def(m).kind, DefKind::Type(_))).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The `type name = …` a weave (or a warp that weaves inline) gives.
    pub(super) fn assoc_binding(&self, site: DefId, name: &str) -> Option<DefId> {
        let members = match &self.p.def(site).kind {
            DefKind::Weave(w) => &w.members,
            DefKind::Warp(w) => &w.members,
            _ => return None,
        };
        members.iter().copied().find(|&m| self.p.def(m).name == name && matches!(self.p.def(m).kind, DefKind::Type(_)))
    }

    /// Whether type member `member` of a warp says what an associated type
    /// of a weft its header lists is.
    pub(super) fn binds_assoc(&self, w: &crate::hir::WarpDef, member: DefId) -> bool {
        let name = &self.p.def(member).name;
        w.wefts
            .iter()
            .filter_map(|t| self.weft_of(t))
            .any(|weft| self.assoc_types(weft).iter().any(|&a| &self.p.def(a).name == name))
    }

    /// What associated type `name` of `weft` is for `ty`.
    pub(super) fn assoc_ty(&mut self, ty: Ty, weft: DefId, name: &str) -> Option<Ty> {
        let ty = self.resolve(ty);
        if Some(weft) == self.std.iter
            && name == "Item"
            && let Some(item) = self.stream_item(ty)
        {
            return Some(item);
        }
        let m = self.weaving(weft, ty)?;
        let site = self.weavings[m.w].site;
        let member = self.assoc_binding(site, name)?;
        let DefKind::Type(alias) = &self.p.def(member).kind else { return None };
        let value = alias.value.as_ref()?;
        // Read where the weave is: its generic parameters stand for what
        // they matched.
        let saved_subst = std::mem::replace(&mut self.subst, m.bindings);
        let saved_self = self.self_ty.replace(ty);
        let saved_implicit = std::mem::take(&mut self.implicit);
        let out = self.lower_ty(value);
        self.implicit = saved_implicit;
        self.self_ty = saved_self;
        self.subst = saved_subst;
        Some(out)
    }

    /// A weft's associated type used as a type: `Item` inside `Iter`, for
    /// the type `Self` stands for.
    pub(super) fn weft_assoc(&mut self, def: DefId, span: Span) -> Ty {
        let Some(weft) = self.p.def(def).parent.filter(|&w| matches!(self.p.def(w).kind, DefKind::Weft(_))) else {
            return self.unsupported(span, "associated types");
        };
        let name = self.p.def(def).name.clone();
        let Some(ty) = self.self_ty.filter(|&t| !matches!(self.tcx.kind(t), TyKind::Param(_))) else {
            return self.unsupported(span, "associated types outside a weave");
        };
        if let Some(out) = self.assoc_ty(ty, weft, &name) {
            return out;
        }
        let (shown, weft_name) = (self.show(ty), self.p.def(weft).name.clone());
        self.diags.push(
            Diagnostic::error(codes::T0111, format!("`{weft_name}.{name}` isn't known for `{shown}`"))
                .primary(span, format!("`{shown}` doesn't say what `{name}` is")),
        );
        self.tcx.error
    }

    /// `T.Item`: the associated type `name` of whichever weft `ty` weaves
    /// that has one.
    pub(super) fn assoc_named(&mut self, ty: Ty, name: &str, span: Span) -> Ty {
        let ty = self.resolve(ty);
        if self.tcx.is_error(ty) {
            return ty;
        }
        // `I.Item` in the signature of a call to a generic function: known
        // once the arguments say what `I` is.
        if self.has_vars(ty) {
            let var = self.new_var(super::infer::VarKind::Type);
            self.projections.push((var, ty, name.to_string(), span));
            return var;
        }
        let mut wefts: Vec<DefId> = self
            .weavings
            .iter()
            .map(|w| w.weft)
            .filter(|&w| self.assoc_types(w).iter().any(|&a| self.p.def(a).name == name))
            .collect();
        if let Some(iter) = self.std.iter
            && name == "Item"
        {
            wefts.push(iter);
        }
        wefts.sort();
        wefts.dedup();
        let found: Vec<Ty> = wefts.into_iter().filter_map(|w| self.assoc_ty(ty, w, name)).collect();
        match found.as_slice() {
            [one] => *one,
            [] => {
                let shown = self.show(ty);
                self.error(codes::T0111, span, format!("`{shown}` has no associated type `{name}`"), "not found");
                self.tcx.error
            }
            _ => {
                let shown = self.show(ty);
                self.error(
                    codes::T0116,
                    span,
                    format!("`{shown}` has more than one associated type `{name}`"),
                    "from more than one weft",
                );
                self.tcx.error
            }
        }
    }

    /// Works out each `I.Item` whose `I` is now known.
    pub(super) fn solve_projections(&mut self) {
        let mut waiting = Vec::new();
        for (var, base, name, span) in std::mem::take(&mut self.projections) {
            let base = self.deep_resolve(base);
            if self.has_vars(base) {
                waiting.push((var, base, name, span));
                continue;
            }
            let ty = self.assoc_named(base, &name, span);
            if !self.unify(var, ty) {
                self.mismatch(span, var, ty);
            }
        }
        self.projections.extend(waiting);
    }

    /// A weaving must say what each of its weft's associated types is.
    /// Returns whether it does.
    pub(super) fn check_assoc_bindings(&mut self, weft: DefId, site: DefId, target: Ty, span: Span) -> bool {
        let mut ok = true;
        for assoc in self.assoc_types(weft) {
            let name = self.p.def(assoc).name.clone();
            if self.assoc_binding(site, &name).is_none() {
                let (shown, weft_name) = (self.show(target), self.p.def(weft).name.clone());
                self.diags.push(
                    Diagnostic::error(
                        codes::T0113,
                        format!("`{shown}` doesn't say what `{name}` is, which `{weft_name}` needs"),
                    )
                    .primary(span, format!("add `type {name} = …`"))
                    .secondary(self.p.def(assoc).span, "declared here"),
                );
                ok = false;
            }
        }
        ok
    }
}
