//! Generics: instances of generic functions and warps.
//!
//! Generic code is checked per instance, the way Zig does it: `largest[T]`
//! is checked once for each `T` it's called with, with `T` standing for that
//! type, and a generic function nobody calls isn't checked at all. The same
//! goes for the methods of a generic warp (`Pair[A, B].swap`) and for a
//! function whose parameter's type is a weft: `fn area(w: Widget)` is
//! `fn area[W: Widget](w: W)`.
//!
//! At a call, each generic argument the call doesn't give is a type
//! variable, and the arguments are checked against a signature with those
//! variables in it. The function is instantiated when the body the call is
//! in is finished and every variable is known. That's also when each
//! argument is checked against its parameter's bounds (`T: Ord`).

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::infer::VarKind;
use super::{BodyId, Checker, FnSig, Instance};
use crate::hir::{self, DefId, DefKind, HirId, TyKind as HirTy};
use crate::ty::{ConstVal, Ty, TyKind};

/// Stands in for an instance that isn't known yet, until the body is
/// finished. Nothing reads it when checking fails.
pub(super) const UNKNOWN: BodyId = BodyId(u32::MAX);

/// How a weft used as a parameter's type, or a `const` parameter's value,
/// becomes a generic argument.
#[derive(Debug, Default)]
pub(super) enum Implicit {
    /// Not in a parameter's type: a weft there isn't a type.
    #[default]
    Off,
    /// At a call: each one gets a fresh variable, recorded with its weft
    /// (none for a `const` parameter).
    Fresh(Vec<(Ty, Option<DefId>)>),
    /// In an instance: the arguments, in the order the wefts and `const`
    /// parameters appear.
    Given { args: Vec<Ty>, next: usize },
}

/// A generic instance a body needs, waiting for its variables.
#[derive(Debug)]
pub(super) struct Pending {
    pub inst: Instance,
    /// Where to record the instance once it's known.
    pub patch: Patch,
    pub span: Span,
    pub bounds: Vec<Bound>,
    /// The body whose tables to patch: a `const for`'s items each have their own.
    pub body: Option<BodyId>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Patch {
    /// The callee of the call with this id.
    Call(HirId),
    /// The field defaults of the construction with this id.
    Construct(HirId),
}

/// A generic argument, and the wefts its parameter needs it to weave.
#[derive(Clone, Debug)]
pub(super) struct Bound {
    pub ty: Ty,
    pub wefts: Vec<DefId>,
    /// The parameter, for messages: `T`, or the weft for an implicit one.
    pub param: String,
    /// For a `const` parameter, its type, which the value inferred for it
    /// must fit (`N` of `[i32; N]` inferred from a length).
    pub value: Option<Ty>,
}

impl Checker<'_> {
    /// The generic parameters of `def`, its parent's first: a method of
    /// `Pair[A, B]` has `A` and `B`, then any of its own.
    pub(super) fn generic_params(&self, def: DefId) -> Vec<DefId> {
        let p = self.p;
        let own = |d: DefId| -> Vec<DefId> {
            match &p.def(d).kind {
                DefKind::Fn(f) => f.generics.clone(),
                DefKind::Warp(w) => w.generics.clone(),
                DefKind::Weave(w) => w.generics.clone(),
                _ => Vec::new(),
            }
        };
        let mut params = match p.def(def).parent {
            Some(parent) if matches!(p.def(def).kind, DefKind::Fn(_) | DefKind::ConstBlock(_)) => own(parent),
            _ => Vec::new(),
        };
        params.extend(own(def));
        params
    }

    /// Whether `def` is only checked for particular types (or values): it,
    /// or the warp it's in, has generic parameters, a parameter's type is a
    /// weft, or a parameter is `const`.
    pub(super) fn is_generic(&self, def: DefId) -> bool {
        !self.generic_params(def).is_empty() || self.implicit_count(def) > 0
    }

    /// How many parameter types of function `def` are wefts, plus how many
    /// of its parameters are `const`.
    fn implicit_count(&self, def: DefId) -> usize {
        let DefKind::Fn(f) = &self.p.def(def).kind else { return 0 };
        let wefts: usize = f.params.iter().filter_map(|p| p.ty.as_ref()).map(|t| self.wefts_in(t)).sum();
        wefts + f.params.iter().filter(|p| p.mode == loom_syntax::ast::ParamMode::Const).count() + f.givens.len()
    }

    fn wefts_in(&self, t: &hir::Ty) -> usize {
        match &t.kind {
            HirTy::Def { def, args } if matches!(self.p.def(*def).kind, DefKind::Weft(_)) && args.is_empty() => 1,
            HirTy::Def { args, .. } => args
                .iter()
                .map(|a| match a {
                    hir::GenericArg::Type(t) => self.wefts_in(t),
                    hir::GenericArg::Expr(_) => 0,
                })
                .sum(),
            HirTy::Tuple(items) => items.iter().map(|t| self.wefts_in(t)).sum(),
            HirTy::Optional(t) | HirTy::Slice(t) => self.wefts_in(t),
            HirTy::Array { elem, .. } => self.wefts_in(elem),
            _ => 0,
        }
    }

    /// Explicit generic arguments for `params` (`Ring[u32, 4]`): types for
    /// type parameters, values for `const` ones. The counts must match.
    /// `None` if one is wrong (reported).
    pub(super) fn generic_args(&mut self, params: &[DefId], args: &[hir::GenericArg]) -> Option<Vec<Ty>> {
        let mut out = Vec::with_capacity(args.len());
        for (&g, arg) in params.iter().zip(args) {
            let name = self.p.def(g).name.clone();
            let ty = match (self.const_param_decl(g), arg) {
                (None, hir::GenericArg::Type(t)) => self.lower_ty(t),
                (Some(ty), hir::GenericArg::Expr(e)) => self.const_arg(e, ty)?,
                // `Ring[u32, SIZE]`: a constant, or another `const` parameter,
                // reads like a type.
                (Some(ty), hir::GenericArg::Type(t)) => match &t.kind {
                    HirTy::Def { def, args } if args.is_empty() => {
                        let e = hir::Expr {
                            id: HirId(u32::MAX),
                            kind: hir::ExprKind::Def { def: *def, args: Vec::new() },
                            span: t.span,
                        };
                        self.const_arg(&e, ty)?
                    }
                    _ => {
                        self.error(codes::T0104, t.span, format!("`{name}` takes a value, not a type"), "a type");
                        return None;
                    }
                },
                (None, hir::GenericArg::Expr(e)) => {
                    self.error(codes::T0104, e.span, format!("`{name}` takes a type, not a value"), "a value");
                    return None;
                }
            };
            out.push(ty);
        }
        Some(out)
    }

    /// The type of `const` generic parameter `g` (`usize` in
    /// `const N: usize`), or `None` if it's a type parameter.
    pub(super) fn const_param_decl(&mut self, g: DefId) -> Option<Ty> {
        let DefKind::GenericParam(gp) = &self.p.def(g).kind else { return None };
        let hir::GenericParamKind::Const { ty } = &gp.kind else { return None };
        Some(self.lower_ty(ty))
    }

    /// What each generic parameter of `inst` stands for. An instance with
    /// no arguments is the template: each parameter stands for itself.
    pub(super) fn instance_subst(&mut self, inst: &Instance) -> Vec<(DefId, Ty)> {
        let params = self.generic_params(inst.def);
        if inst.args.is_empty() {
            return params.into_iter().map(|g| (g, self.tcx.intern(TyKind::Param(g)))).collect();
        }
        params.into_iter().zip(inst.args.iter().copied()).collect()
    }

    /// The arguments of `inst` for weft-typed parameters: those after its
    /// generic parameters'.
    pub(super) fn implicit_args(&self, inst: &Instance) -> Implicit {
        let n = self.generic_params(inst.def).len();
        Implicit::Given { args: inst.args.iter().skip(n).copied().collect(), next: 0 }
    }

    /// The instance of member function `def` for a value of type `ty`: a
    /// generic warp's methods take the warp's arguments.
    pub(super) fn member_instance(&self, def: DefId, ty: Ty) -> Instance {
        let ty = self.resolve(ty);
        let parent_generic = self.p.def(def).parent.is_some_and(|w| match &self.p.def(w).kind {
            DefKind::Warp(warp) => !warp.generics.is_empty(),
            _ => false,
        });
        match self.tcx.kind(ty) {
            TyKind::Adt(_, args) if parent_generic => Instance { args: args.clone(), ..Instance::of(def) },
            _ => Instance::of(def),
        }
    }

    /// The signature of a call to a generic function, with a variable for
    /// each generic argument the call doesn't give. Returns it with the
    /// instance the call needs (in variables) and the bounds to check.
    /// `inst.args` holds the warp's arguments for a method of a generic warp.
    pub(super) fn generic_sig(
        &mut self,
        inst: Instance,
        explicit: &[Ty],
        span: Span,
    ) -> Option<(FnSig, Instance, Vec<Bound>)> {
        let p = self.p;
        let DefKind::Fn(f) = &p.def(inst.def).kind else { return None };
        let parent_params = self.generic_params(inst.def).len() - f.generics.len();
        let mut args = inst.args.clone();
        if args.len() != parent_params {
            // A method of a generic warp called through the type without
            // arguments (`Pair.new(1, 2)`): infer the warp's too.
            args = (0..parent_params).map(|_| self.new_var(VarKind::Type)).collect();
        }
        let own: Vec<Ty> = if explicit.is_empty() {
            f.generics.iter().map(|_| self.new_var(VarKind::Type)).collect()
        } else if explicit.len() == f.generics.len() {
            explicit.to_vec()
        } else {
            let name = &p.def(inst.def).name;
            let n = f.generics.len();
            self.error(
                codes::T0104,
                span,
                format!("`{name}` takes {n} type argument{}", if n == 1 { "" } else { "s" }),
                format!("{} given", explicit.len()),
            );
            return None;
        };
        if let Some(&g) = f.generics.iter().find(|&&g| self.is_region_param(g)) {
            self.unsupported(p.def(g).span, "region parameters");
            return None;
        }
        args.extend(own.iter().copied());
        let template = Instance { args: args.clone(), ..inst };
        let subst = self.instance_subst(&template);
        let saved_subst = std::mem::replace(&mut self.subst, subst.clone());
        let saved_implicit = std::mem::replace(&mut self.implicit, Implicit::Fresh(Vec::new()));
        let this = template.self_ty.or_else(|| self.parent_warp_ty(template.def));
        let saved_self = std::mem::replace(&mut self.self_ty, this);
        let saved_current = self.current;
        let sig = self.lower_sig(f, p.def(inst.def).span);
        self.current = saved_current;
        self.self_ty = saved_self;
        let implicit = std::mem::replace(&mut self.implicit, saved_implicit);
        self.subst = saved_subst;
        let sig = sig?;

        let mut bounds = Vec::new();
        for (&g, &ty) in f.generics.iter().zip(&own) {
            if let Some(decl) = self.const_param_decl(g) {
                bounds.push(Bound { ty, wefts: Vec::new(), param: p.def(g).name.clone(), value: Some(decl) });
            }
            let wefts = self.bounds_of(g, f);
            if !wefts.is_empty() {
                bounds.push(Bound { ty, wefts, param: p.def(g).name.clone(), value: None });
            }
        }
        bounds.extend(self.parent_bounds(inst.def, &args[..parent_params]));
        if let Implicit::Fresh(vars) = implicit {
            for (ty, weft) in vars {
                args.push(ty);
                if let Some(weft) = weft {
                    bounds.push(Bound { ty, wefts: vec![weft], param: p.def(weft).name.clone(), value: None });
                }
            }
        }
        Some((sig, Instance { args, ..template }, bounds))
    }

    /// The bounds a method's `where` clause puts on its warp's parameters
    /// (`fn sum(self) -> T where T: Num`), for the warp's arguments `args`.
    pub(super) fn parent_bounds(&self, def: DefId, args: &[Ty]) -> Vec<Bound> {
        let DefKind::Fn(f) = &self.p.def(def).kind else { return Vec::new() };
        let params = self.generic_params(def);
        let parent = &params[..params.len() - f.generics.len()];
        let mut bounds = Vec::new();
        for (&g, &ty) in parent.iter().zip(args) {
            let mut wefts = Vec::new();
            for pred in &f.where_clause {
                if matches!(&pred.ty.kind, HirTy::Def { def, .. } if *def == g) {
                    for t in &pred.bounds {
                        if let HirTy::Def { def, .. } = &t.kind
                            && matches!(self.p.def(*def).kind, DefKind::Weft(_))
                        {
                            wefts.push(*def);
                        }
                    }
                }
            }
            if !wefts.is_empty() {
                bounds.push(Bound { ty, wefts, param: self.p.def(g).name.clone(), value: None });
            }
        }
        bounds
    }

    /// Reports each bound that doesn't hold for a call to `def` at `span`.
    /// Returns whether they all do.
    pub(super) fn check_bounds(&mut self, def: DefId, span: Span, bounds: &[Bound]) -> bool {
        let name = self.p.def(def).name.clone();
        let mut ok = true;
        for bound in bounds {
            let ty = self.deep_resolve(bound.ty);
            if self.tcx.is_lenient(ty) {
                continue;
            }
            if let Some(decl) = bound.value {
                ok &= self.check_const_value(ty, decl, &bound.param, span);
            }
            for &weft in &bound.wefts {
                if !self.weaves(ty, weft) {
                    let (shown, weft_name) = (self.show(ty), self.p.def(weft).name.clone());
                    let mut diag = Diagnostic::error(codes::T0108, format!("`{shown}` doesn't weave `{weft_name}`"));
                    // A weft as a parameter's type is its own bound; a
                    // named parameter says what it stands for.
                    diag = if bound.param == weft_name {
                        diag.primary(span, format!("`{name}` needs a `{weft_name}`"))
                    } else {
                        diag.primary(span, format!("`{name}` needs `{}` to weave `{weft_name}`", bound.param))
                            .note(format!("here `{}` is `{shown}`", bound.param))
                    };
                    self.diags.push(diag);
                    ok = false;
                }
            }
        }
        ok
    }

    /// Whether the value `ty` inferred for `const` parameter `param` fits
    /// its type `decl`; reports it if not.
    fn check_const_value(&mut self, ty: Ty, decl: Ty, param: &str, span: Span) -> bool {
        let TyKind::Value(value) = self.tcx.kind(ty).clone() else { return true };
        let decl_kind = self.tcx.kind(self.tcx.degauge(decl)).clone();
        let shown = self.show(decl);
        let (code, message, v) = match (value, decl_kind) {
            (ConstVal::Int(v), TyKind::Int(int)) if int.contains(v) => return true,
            (ConstVal::Bool(_), TyKind::Bool) => return true,
            (ConstVal::Int(v), TyKind::Int(_)) => {
                (codes::T0102, format!("`{v}` doesn't fit in `{shown}`"), v.to_string())
            }
            (ConstVal::Int(v), _) => (codes::T0101, format!("expected `{shown}`, found an integer"), v.to_string()),
            (ConstVal::Bool(b), _) => (codes::T0101, format!("expected `{shown}`, found a `bool`"), b.to_string()),
            _ => (codes::T0101, format!("expected `{shown}`, found a compile-time value"), "that".into()),
        };
        self.diags.push(Diagnostic::error(code, message).primary(span, format!("`{param}` is {v} here")));
        false
    }

    /// Whether an error was already reported inside `span`.
    fn errors_within(&self, span: Span) -> bool {
        self.diags.iter().any(|d| {
            d.severity == loom_syntax::diag::Severity::Error
                && d.labels
                    .iter()
                    .any(|l| l.primary && l.span.file == span.file && l.span.lo >= span.lo && l.span.hi <= span.hi)
        })
    }

    /// The wefts generic parameter `g` of `f` must weave: its bounds and
    /// its `where` clauses.
    fn bounds_of(&self, g: DefId, f: &hir::FnDef) -> Vec<DefId> {
        let mut wefts = Vec::new();
        let mut add = |t: &hir::Ty| {
            if let HirTy::Def { def, .. } = &t.kind
                && matches!(self.p.def(*def).kind, DefKind::Weft(_))
            {
                wefts.push(*def);
            }
        };
        if let DefKind::GenericParam(gp) = &self.p.def(g).kind
            && let hir::GenericParamKind::Type { bounds } = &gp.kind
        {
            bounds.iter().for_each(&mut add);
        }
        for pred in &f.where_clause {
            if matches!(&pred.ty.kind, HirTy::Def { def, .. } if *def == g) {
                pred.bounds.iter().for_each(&mut add);
            }
        }
        wefts
    }

    /// Instantiates, once the body's variables are known, every generic
    /// instance it needs.
    pub(super) fn instantiate_pending(&mut self) {
        for pending in std::mem::take(&mut self.pending) {
            let args: Vec<Ty> = pending.inst.args.iter().map(|&t| self.deep_resolve(t)).collect();
            let self_ty = pending.inst.self_ty.map(|t| self.deep_resolve(t));
            let name = self.p.def(pending.inst.def).name.clone();
            if args.iter().chain(&self_ty).any(|&t| self.has_vars(t)) {
                // An error in the call's arguments already said why.
                if self.errors_within(pending.span) {
                    continue;
                }
                self.diags.push(
                    Diagnostic::error(codes::T0110, format!("can't tell which types `{name}` is used with here"))
                        .primary(pending.span, "type arguments unknown")
                        .help(format!("give them: `{name}[…](…)`, or annotate what it's assigned to")),
                );
                continue;
            }
            if args.iter().chain(&self_ty).any(|&t| self.tcx.is_error(t)) {
                continue;
            }
            if !self.check_bounds(pending.inst.def, pending.span, &pending.bounds) {
                continue;
            }
            let id = self.instance(Instance { args, self_ty, ..pending.inst });
            let patch = |this: &mut Self| match pending.patch {
                Patch::Call(call) => {
                    if let Some(c) = this.b.calls.get_mut(&call) {
                        c.callee = id;
                    }
                }
                Patch::Construct(e) => {
                    if let Some(c) = this.b.constructs.get_mut(&e) {
                        c.defaults = id;
                    }
                }
            };
            match pending.body {
                Some(body) if Some(body) != self.cur_body => self.in_body(body, patch),
                _ => patch(self),
            }
        }
    }
}

impl Checker<'_> {
    /// Gives every warp type the results mention a layout for its arguments
    /// (lowering describes values by it), and instantiates the hand-written
    /// `show`, `eq` and `lt` of generic warps for them. Returns whether that
    /// queued anything to check.
    pub(super) fn register_types(&mut self) -> bool {
        let mut tys: Vec<Ty> = Vec::new();
        for body in &self.r.bodies {
            tys.extend(body.expr_ty.values().copied());
            tys.extend(body.pat_ty.values().copied());
            tys.extend(body.local_ty.values().copied());
        }
        for sig in self.r.sigs.values() {
            tys.extend(sig.params.iter().map(|p| p.ty));
            tys.push(sig.ret);
        }
        tys.extend(self.r.const_ty.values().copied());
        tys.sort();
        tys.dedup();
        let before = self.queue.len();
        for ty in tys {
            self.register_type(ty);
        }
        self.register_vtables();
        self.queue.len() > before
    }

    fn register_type(&mut self, ty: Ty) {
        if !self.registered.insert(ty) || self.has_vars(ty) || self.tcx.has_params(ty) {
            return;
        }
        match self.tcx.kind(ty).clone() {
            TyKind::Tuple(items) => items.into_iter().for_each(|t| self.register_type(t)),
            TyKind::Optional(t) | TyKind::Range(t) | TyKind::Array(t, _) => self.register_type(t),
            TyKind::Result(t, e) | TyKind::Ptr(t, e) => {
                self.register_type(t);
                self.register_type(e);
            }
            TyKind::Fn(params, ret) => {
                params.into_iter().for_each(|t| self.register_type(t));
                self.register_type(ret);
            }
            TyKind::Adt(_, args) => {
                args.iter().for_each(|&t| self.register_type(t));
                let Some(info) = self.adt_info(ty) else { return };
                for t in info.field_tys() {
                    self.register_type(t);
                }
                self.instantiate_op_fns(ty);
            }
            _ => {}
        }
    }

    /// The hand-written `show`, `eq` and `lt` a generic weave gives `ty`.
    fn instantiate_op_fns(&mut self, ty: Ty) {
        if self.r.op_fns.contains_key(&ty) {
            return;
        }
        let mut fns = super::OpFns::default();
        let mut any = false;
        for (weft, name) in [(self.std.show, "show"), (self.std.eq, "eq"), (self.std.ord, "lt")] {
            let Some(weft) = weft else { continue };
            let Some(m) = self.weaving(weft, ty) else { continue };
            if !self.tcx.has_params(self.weavings[m.w].target) {
                continue;
            }
            let Some(method) = self.weft_methods(weft).into_iter().find(|&d| self.p.def(d).name == name) else {
                continue;
            };
            let imp = match self.impl_of(m.w, method, ty, 0) {
                Some(imp @ (super::Impl::Written(_) | super::Impl::Default(_) | super::Impl::Forward(_))) => imp,
                _ => continue,
            };
            let inst = self.impl_instance(imp, ty, &m.bindings);
            let id = self.instance(inst);
            match name {
                "show" => fns.show = Some(id),
                "eq" => fns.eq = Some(id),
                _ => fns.lt = Some(id),
            }
            any = true;
        }
        if any {
            self.r.op_fns.insert(ty, fns);
        }
    }
}

impl Checker<'_> {
    /// Notes which instance the diagnostics from `mark` on came from, and
    /// drops those another instance already reported: a mistake in a
    /// generic body is reported once, not once per type it's used with.
    pub(super) fn note_instance(&mut self, mark: usize, inst: &Instance) {
        if inst.args.is_empty() && inst.self_ty.is_none() {
            return;
        }
        let what = self.describe_instance(inst);
        let fresh: Vec<Diagnostic> = self.diags.drain(mark..).collect();
        for diag in fresh {
            let span = diag.labels.iter().find(|l| l.primary).map(|l| l.span);
            if self.instance_diags.insert((diag.code.id, span)) {
                self.diags.push(diag.note(format!("in {what}")));
            }
        }
    }

    /// `largest` with `T = u8`, or `Widget.area` for `Button`.
    pub(super) fn describe_instance(&mut self, inst: &Instance) -> String {
        let p = self.p;
        // `const assert(…)` in a warp: the warp, with its arguments.
        if let (DefKind::ConstBlock(_), Some(parent)) = (&p.def(inst.def).kind, p.def(inst.def).parent) {
            let ty = self.tcx.intern(TyKind::Adt(parent, inst.args.clone()));
            return format!("`{}`", self.show(ty));
        }
        let name = match p.def(inst.def).parent {
            Some(parent) if !p.def(parent).name.is_empty() => {
                format!("{}.{}", p.def(parent).name, p.def(inst.def).name)
            }
            _ => p.def(inst.def).name.clone(),
        };
        // The generic parameters' names, then the `const` parameters' (a
        // weft as a parameter's type has none).
        let mut names: Vec<Option<String>> =
            self.generic_params(inst.def).iter().map(|&g| Some(p.def(g).name.clone())).collect();
        if let DefKind::Fn(f) = &p.def(inst.def).kind {
            for param in &f.params {
                let wefts = param.ty.as_ref().map_or(0, |t| self.wefts_in(t));
                names.extend(std::iter::repeat_n(None, wefts));
                if param.mode == loom_syntax::ast::ParamMode::Const {
                    names.push(Some(p.local(param.local).name.clone()));
                }
            }
        }
        let mut parts = Vec::new();
        for (i, &arg) in inst.args.iter().enumerate() {
            let mut shown = self.show(arg);
            // A type can be thousands of parts long (see `MAX_TYPE_SIZE`).
            if let Some((cut, _)) = shown.char_indices().nth(80) {
                shown.truncate(cut);
                shown.push('…');
            }
            match names.get(i) {
                Some(Some(name)) => parts.push(format!("{name} = {shown}")),
                _ => parts.push(shown),
            }
        }
        let mut out = format!("`{name}`");
        if !parts.is_empty() {
            out.push_str(&format!(" with `{}`", parts.join("`, `")));
        }
        if let Some(ty) = inst.self_ty {
            let shown = self.show(ty);
            out.push_str(&format!(" for `{shown}`"));
        }
        out
    }
}
