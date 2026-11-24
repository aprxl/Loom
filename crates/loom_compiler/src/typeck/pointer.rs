//! Pointers: `^e` allocates, `p^` reads through one, and a pointer stands
//! for its target where a field, a method or a pattern needs one
//! (design/v6-spools.md).

use loom_syntax::diag::codes;

use loom_syntax::source::Span;

use super::{Checker, Coercion};
use crate::hir::{Arg, DefId, DefKind, Expr, ExprKind, LocalId, LocalKind, Region};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    /// The region type `@name` stands for.
    pub(super) fn region_ty(&mut self, region: &Region, span: Span) -> Ty {
        match region {
            Region::Heap => self.tcx.heap,
            // Literals: nothing owns them, nothing frees them.
            Region::Static => self.tcx.spool,
            Region::Local(l) => self.region_of_local(*l, span),
            Region::Param(_) => self.unsupported(span, "region parameters"),
            Region::Error => self.tcx.error,
        }
    }

    /// The region of the spool the local `l` is.
    fn region_of_local(&mut self, l: LocalId, span: Span) -> Ty {
        let ty = match self.p.local(l).kind {
            LocalKind::Spool => return self.tcx.spool,
            LocalKind::SelfParam => self.self_ty,
            // A `given` of the signature being lowered, or of the body.
            _ => self
                .given_tys
                .iter()
                .find(|(g, _)| *g == l)
                .map(|&(_, t)| t)
                .or_else(|| self.b.local_ty.get(&l).copied()),
        };
        match ty.map(|t| self.resolve(t)).and_then(|t| self.region_of_spool_ty(t)) {
            Some(region) => region,
            None => {
                let name = self.p.local(l).name.clone();
                self.error(codes::T0101, span, format!("`{name}` isn't a spool"), "a region is the name of a spool");
                self.tcx.error
            }
        }
    }

    /// The region of values allocated in a spool of type `ty`.
    pub(super) fn region_of_spool_ty(&self, ty: Ty) -> Option<Ty> {
        match *self.tcx.kind(ty) {
            TyKind::Adt(def, _) if Some(def) == self.std.heap => Some(self.tcx.heap),
            TyKind::Adt(def, _)
                if Some(def) == self.std.arena || Some(def) == self.std.pool || Some(def) == self.std.shared_arena =>
            {
                Some(self.tcx.spool)
            }
            _ => None,
        }
    }

    /// `^operand`: a new pointer to `operand`'s value, which moves into it.
    pub(super) fn check_alloc(&mut self, e: &Expr, operand: &Expr, expected: Option<Ty>) -> Ty {
        let want = expected.and_then(|t| match *self.tcx.kind(self.resolve(t)) {
            TyKind::Ptr(inner, _) => Some(inner),
            _ => None,
        });
        let (region, from) = self.ambient_spool();
        if let Some(local) = from {
            self.b.allocs.insert(e.id, local);
        }
        let inner = self.check_expr(operand, want);
        if let Some(want) = want {
            self.coerce(operand, inner, want);
            self.adopt_region(want, region, operand.span);
            return self.tcx.intern(TyKind::Ptr(want, region));
        }
        self.adopt_region(inner, region, operand.span);
        self.tcx.intern(TyKind::Ptr(inner, region))
    }

    /// Whether `actual` is `target` but for regions, which `target` leaves
    /// open (`Any`): the same values, whichever spool they are in.
    pub(super) fn erases_regions(&mut self, actual: Ty, target: Ty) -> bool {
        if actual == target {
            return true;
        }
        let any = self.tcx.any_region;
        match (self.tcx.kind(actual).clone(), self.tcx.kind(target).clone()) {
            (TyKind::Region(_), TyKind::Region(_)) => target == any,
            (TyKind::Ptr(a, ra), TyKind::Ptr(b, rb)) => self.erases_regions(ra, rb) && self.erases_regions(a, b),
            (TyKind::Adt(d1, x), TyKind::Adt(d2, y)) if d1 == d2 && x.len() == y.len() => {
                x.into_iter().zip(y).all(|(x, y)| self.erases_regions(x, y))
            }
            (TyKind::Optional(a), TyKind::Optional(b)) => self.erases_regions(a, b),
            (TyKind::Tuple(a), TyKind::Tuple(b)) if a.len() == b.len() => {
                a.into_iter().zip(b).all(|(x, y)| self.erases_regions(x, y))
            }
            _ => false,
        }
    }

    /// Whether `actual` and `target` are the same type but for regions.
    pub(super) fn regions_only(&mut self, actual: Ty, target: Ty) -> bool {
        if actual == target {
            return true;
        }
        match (self.tcx.kind(actual).clone(), self.tcx.kind(target).clone()) {
            (TyKind::Region(_), TyKind::Region(_)) => true,
            (TyKind::Ptr(a, ra), TyKind::Ptr(b, rb)) => self.regions_only(ra, rb) && self.regions_only(a, b),
            (TyKind::Adt(d1, x), TyKind::Adt(d2, y)) if d1 == d2 && x.len() == y.len() => {
                x.into_iter().zip(y).all(|(x, y)| self.regions_only(x, y))
            }
            (TyKind::Optional(a), TyKind::Optional(b)) => self.regions_only(a, b),
            _ => false,
        }
    }

    /// Settles the region of a warp value that holds pointers: a value
    /// built in a spool is in it.
    pub(super) fn adopt_region(&mut self, ty: Ty, region: Ty, span: Span) {
        let ty = self.resolve(ty);
        let TyKind::Adt(def, args) = self.tcx.kind(ty).clone() else { return };
        let hidden = matches!(&self.p.def(def).kind, crate::hir::DefKind::Warp(w)
            if crate::hir::hidden_param(self.p, w).is_some());
        let Some(&last) = args.last().filter(|_| hidden) else { return };
        if !self.unify(last, region) {
            let (want, shown) = (self.show(region), self.show(ty));
            self.error(
                codes::T0101,
                span,
                format!("`{shown}` can't be allocated in `{want}`"),
                "what it holds is in another region",
            );
        }
    }

    /// `spool name = init`: a spool of its own, ended with its scope.
    pub(super) fn check_spool(&mut self, local: LocalId, init: &Expr) {
        self.b.spools = true;
        let ty = self.check_expr(init, None);
        let ty = self.resolve(ty);
        if !self.tcx.is_error(ty)
            && !matches!(*self.tcx.kind(ty), TyKind::Adt(def, _)
                if Some(def) == self.std.arena || Some(def) == self.std.pool || Some(def) == self.std.shared_arena)
        {
            let shown = self.show(ty);
            self.error(
                codes::T0101,
                init.span,
                format!("`{shown}` isn't a spool that can be made here"),
                "try `Arena(…)`",
            );
            self.b.local_ty.insert(local, self.tcx.error);
            return;
        }
        self.b.local_ty.insert(local, ty);
    }

    /// The type of `heap`.
    pub(super) fn heap_ty(&mut self, span: Span) -> Ty {
        match self.std.heap {
            Some(def) => self.tcx.intern(TyKind::Adt(def, Vec::new())),
            None => self.unsupported(span, "`heap`"),
        }
    }

    /// The type a weft's `#given_default(Type)` names: what a `given` of it
    /// is when nothing in scope gives one.
    pub(super) fn given_default(&mut self, weft: DefId) -> Option<Ty> {
        let d = self.p.def(weft);
        let attr = d.attrs.iter().find(|a| a.name == "given_default")?;
        let loom_syntax::ast::ExprKind::Name(name) = &attr.args.first()?.value.kind else { return None };
        match self.p.module_name(d.module, &name.name)? {
            crate::hir::Binding::Def(def) if matches!(self.p.def(def).kind, DefKind::Warp(_)) => {
                Some(self.tcx.intern(TyKind::Adt(def, Vec::new())))
            }
            _ => None,
        }
    }

    /// The instance of `ty`'s `default`, if it weaves `Default`.
    pub(super) fn default_instance(&mut self, ty: Ty) -> Option<super::BodyId> {
        let weft = self.std.default?;
        let method = self.weft_methods(weft).into_iter().find(|&m| self.p.def(m).name == "default")?;
        let m = self.weaving(weft, Some(&[]), ty)?;
        let imp = self.impl_of(m.w, method, ty, 0)?;
        let inst = self.impl_instance(imp, ty, Some(m.w), &m.bindings);
        Some(self.instance(inst))
    }

    /// The innermost `given` in scope that weaves `weft`.
    pub(super) fn ambient(&mut self, weft: DefId) -> Option<(LocalId, Ty)> {
        let found = self.given_scope.iter().rev().copied().collect::<Vec<_>>();
        found.into_iter().find(|&(_, ty)| {
            let ty = self.resolve(ty);
            !self.has_vars(ty) && self.weaves(ty, weft, &[])
        })
    }

    /// The spool `^e` allocates in: the innermost `given` spool in scope,
    /// else the heap. Its region, and the local when it is an arena.
    pub(super) fn ambient_spool(&mut self) -> (Ty, Option<LocalId>) {
        let Some(weft) = self.std.spool else { return (self.tcx.heap, None) };
        match self.ambient(weft) {
            Some((local, ty)) => match self.region_of_spool_ty(self.resolve(ty)) {
                Some(r) if r == self.tcx.heap => (r, None),
                Some(r) => (r, Some(local)),
                None => (self.tcx.heap, None),
            },
            None => (self.tcx.heap, None),
        }
    }

    /// The types a call gives the `given` parameters of function `def`, in
    /// order: those of its `given:` arguments, else of the `given`s in
    /// scope, else the heap for a spool. Arguments are checked here, once.
    pub(super) fn given_hints_for(&mut self, def: DefId, args: &[Arg]) -> Vec<Option<Ty>> {
        let DefKind::Fn(f) = &self.p.def(def).kind else { return Vec::new() };
        let mut explicit = args.iter().filter(|a| a.is_given);
        let mut hints = Vec::with_capacity(f.givens.len());
        for g in &f.givens {
            let weft = self.plain_weft(&g.ty);
            let hint = if let Some(arg) = explicit.next() {
                let ty = self.check_expr(&arg.value, None);
                self.prechecked.insert(arg.value.id, ty);
                Some(ty)
            } else if let Some(weft) = weft {
                match self.ambient(weft) {
                    Some((_, ty)) => Some(ty),
                    None if Some(weft) == self.std.spool => {
                        self.std.heap.map(|d| self.tcx.intern(TyKind::Adt(d, Vec::new())))
                    }
                    None => self.given_default(weft),
                }
            } else {
                None
            };
            hints.push(hint);
        }
        hints
    }

    /// `given name = value`: from here to the end of the block, calls that
    /// need a spool take this one.
    pub(super) fn check_given(&mut self, local: Option<LocalId>, value: &Expr) {
        self.b.spools = true;
        let ty = self.check_expr(value, None);
        let ty = self.resolve(ty);
        if self.tcx.is_error(ty) {
            return;
        }
        let Some(local) = local else { return };
        self.b.local_ty.insert(local, ty);
        self.given_scope.push((local, ty));
    }

    /// `pointer^`: the target, read.
    pub(super) fn check_deref(&mut self, pointer: &Expr) -> Ty {
        let ty = self.check_expr(pointer, None);
        let ty = self.resolve(ty);
        match *self.tcx.kind(ty) {
            TyKind::Ptr(inner, _) => inner,
            TyKind::Error => ty,
            _ => {
                let shown = self.show(ty);
                self.error(codes::T0101, pointer.span, format!("`{shown}` isn't a pointer"), "can't be read with `^`");
                self.tcx.error
            }
        }
    }

    /// The type `e`, of type `ty`, stands for in a field access, a method
    /// call or a pattern: its target, if it's a pointer (recorded as a
    /// [`Coercion::Deref`] on `e`), else `ty`.
    pub(super) fn auto_deref(&mut self, e: &Expr, ty: Ty) -> Ty {
        let ty = self.resolve(ty);
        match *self.tcx.kind(ty) {
            TyKind::Ptr(inner, _) if !self.b.coercions.contains_key(&e.id) => {
                let inner = self.resolve(inner);
                self.b.coerced.insert(e.id, inner);
                self.b.coercions.insert(e.id, Coercion::Deref);
                inner
            }
            _ => ty,
        }
    }

    /// Writing to `target` goes through a pointer into a spool whose target
    /// isn't plain data (open question 1, option (c)): two copies of the
    /// pointer could then see a case change under a borrow.
    pub(super) fn check_arena_write(&mut self, target: &Expr) {
        let mut e = target;
        loop {
            let (inner, ptr_expr) = match &e.kind {
                ExprKind::Field { base, .. } | ExprKind::ConstField { base, .. } | ExprKind::Index { base, .. } => {
                    (base.as_ref(), self.b.coercions.get(&base.id) == Some(&Coercion::Deref))
                }
                ExprKind::Deref(inner) => (inner.as_ref(), true),
                _ => return,
            };
            if ptr_expr && let Some(&t) = self.b.expr_ty.get(&inner.id) {
                let t = self.resolve(t);
                if let TyKind::Ptr(target_ty, r) = *self.tcx.kind(t)
                    && r == self.tcx.spool
                    && !self.is_copy(target_ty)
                {
                    self.error(
                        codes::T0107,
                        target.span,
                        "can't change what a pointer into a spool points at",
                        "it holds more than plain data",
                    );
                    return;
                }
            }
            e = inner;
        }
    }

    /// Whether `e` is the target of a pointer: `p^`, a field of one, or
    /// something a pointer is dereferenced to for a field access.
    pub(super) fn derefs(&self, e: &Expr) -> bool {
        matches!(e.kind, ExprKind::Deref(_)) || self.b.coercions.get(&e.id).is_some_and(|c| *c == Coercion::Deref)
    }
}
