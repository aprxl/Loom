//! Shuttles: functions that yield and pull (`fun f() ~> T <~ U`).
//!
//! Calling one makes a `Shuttle[T, U]` without running its body, which is
//! checked as if it returned `()`: `yield v` takes a `T`, `pull` gives a
//! `?U`, and `return` ends it. A generator (`~> T`) pulls nothing, a sink
//! (`<~ U`) yields nothing.
//!
//! A shuttle keeps its parameters for as long as it lives, which can be
//! longer than the call that made it. Until the region checker can follow
//! a borrow that long, a shuttle only takes parameters it owns: `take`
//! ones, and `Copy` ones, which it gets a copy of.

use loom_syntax::Diagnostic;
use loom_syntax::ast::{ParamMode, PlaceMode};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{Checker, FnSig, FrameQuery, Instance};
use loom_syntax::ast::FieldName;

use crate::hir::{self, DefId, DefKind, Expr, ExprKind, FnRet};
use crate::ty::{Ty, TyKind};

/// The shuttle whose body is being checked: what it yields and pulls
/// (`None` for a generator's input, or a sink's output).
#[derive(Clone, Copy, Debug)]
pub(super) struct ShuttleCtx {
    pub yields: Option<Ty>,
    pub pulls: Option<Ty>,
    /// A projection's: it yields places.
    pub lends: Option<PlaceMode>,
}

impl Checker<'_> {
    /// `Shuttle[T, U]` for a shuttle's `~> T <~ U`, where a missing side is `()`.
    pub(super) fn shuttle_ty(&mut self, yields: Option<Ty>, pulls: Option<Ty>) -> Ty {
        let Some(shuttle) = self.std.shuttle else { return self.tcx.error };
        let unit = self.tcx.unit;
        self.tcx.intern(TyKind::Adt(shuttle, vec![yields.unwrap_or(unit), pulls.unwrap_or(unit)]))
    }

    /// `T` and `U` of `Shuttle[T, U]`.
    pub(super) fn shuttle_parts(&self, ty: Ty) -> Option<(Ty, Ty)> {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Adt(def, args) if Some(*def) == self.std.shuttle && args.len() == 2 => Some((args[0], args[1])),
            _ => None,
        }
    }

    /// `T` of `Burst[T]`.
    pub(super) fn burst_item(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Adt(def, args) if Some(*def) == self.std.burst => args.first().copied(),
            _ => None,
        }
    }

    /// For a shuttle's body: what it yields and pulls. Also checks that it
    /// only takes parameters it can keep.
    pub(super) fn shuttle_ctx(&mut self, f: &hir::FnDef, sig: &FnSig) -> Option<ShuttleCtx> {
        let FnRet::Shuttle { yields, pulls } = &f.ret else { return None };
        let (t, u) = self.shuttle_parts(sig.ret)?;
        let lends = yields.as_ref().and_then(|(mode, _)| *mode);
        // A projection lends places for one statement of its caller's: it
        // can borrow what it's given for that long.
        for (param, sig_param) in f.params.iter().zip(&sig.params).filter(|_| lends.is_none()) {
            let kept = match sig_param.mode {
                ParamMode::Take => true,
                ParamMode::Read => self.is_copy(sig_param.ty),
                ParamMode::Mut | ParamMode::Const => false,
            };
            if !kept {
                let what =
                    if param.is_self { "a shuttle that borrows `self`" } else { "a shuttle that borrows a parameter" };
                self.unsupported(param.span, &format!("{what} (take it with `take`, or make it `Copy`)"));
            }
        }
        Some(ShuttleCtx { yields: yields.as_ref().map(|_| t), pulls: pulls.as_ref().map(|_| u), lends })
    }

    /// Whether `def` is a projection, and how it lends.
    pub(super) fn lends(&self, def: DefId) -> Option<PlaceMode> {
        match &self.p.def(def).kind {
            DefKind::Fn(f) => match &f.ret {
                FnRet::Shuttle { yields: Some((mode, _)), .. } => *mode,
                _ => None,
            },
            _ => None,
        }
    }

    /// `yield v`, or a projection's `yield mut place`.
    pub(super) fn check_yield(&mut self, value: &Expr, place: Option<PlaceMode>, span: Span) -> Ty {
        let lends = self.shuttle.and_then(|s| s.lends);
        match (self.shuttle.and_then(|s| s.yields), place, lends) {
            (Some(t), None, None) => {
                self.check_coerce(value, t);
            }
            (Some(t), Some(mode), Some(lends)) => {
                let ty = self.check_expr(value, Some(t));
                self.coerce(value, ty, t);
                match self.place(value) {
                    None => self.error(codes::T0107, value.span, "a projection lends a place", "not a place"),
                    Some(false) if mode == PlaceMode::Mut => self.error(
                        codes::T0107,
                        value.span,
                        "can't lend this to change",
                        "part of something that can't be changed",
                    ),
                    Some(_) if mode != lends => {
                        let (want, got) = if lends == PlaceMode::Mut { ("mut", "ref") } else { ("ref", "mut") };
                        self.wrong_side(
                            span,
                            &format!("yield {got}"),
                            &format!("this projection lends with `~> {want}`"),
                        );
                    }
                    Some(_) => {}
                }
            }
            (Some(_), None, Some(_)) => {
                self.check_expr(value, None);
                self.wrong_side(span, "yield", "a projection lends places: `yield mut place` or `yield ref place`");
            }
            (Some(_), Some(_), None) => {
                self.check_expr(value, None);
                self.wrong_side(span, "yield mut", "only a projection (`~> mut T`, `~> ref T`) lends places");
            }
            (None, ..) => {
                self.check_expr(value, None);
                self.wrong_side(span, "yield", "this shuttle only pulls: it has no `~> T`");
            }
        }
        if self.fence.is_some() {
            self.leaves_fence(span, "yield");
        }
        self.tcx.unit
    }

    /// `pull`: `?U`.
    pub(super) fn check_pull(&mut self, span: Span) -> Ty {
        if self.fence.is_some() {
            self.leaves_fence(span, "pull");
        }
        match self.shuttle.and_then(|s| s.pulls) {
            Some(u) => self.tcx.intern(TyKind::Optional(u)),
            None => {
                self.wrong_side(span, "pull", "this shuttle only yields: it has no `<~ U`");
                self.tcx.error
            }
        }
    }

    /// A shuttle's frame is part of the shuttle value, so a shuttle that
    /// makes another of itself would contain itself.
    pub(super) fn check_shuttle_self_call(&mut self, callee: DefId, span: Span) {
        if self.shuttle.is_none() || self.current != Some(callee) {
            return;
        }
        let name = self.p.def(callee).name.clone();
        self.diags.push(
            Diagnostic::error(codes::T0121, format!("shuttle `{name}` can't make another `{name}` inside itself"))
                .primary(span, "this shuttle would be part of its own frame")
                .note("a shuttle's frame is stored inside the shuttle value, so it has a size known when compiling")
                .help("allocate the inner one, `^name(…)`, which needs spools (not implemented yet)"),
        );
    }

    /// `f.Frame.size` or `f.Frame.align`, where `f` is a shuttle function:
    /// a `usize` known once the program is built, not while checking (see
    /// `heddle::frame`). `None` for anything else.
    pub(super) fn check_frame_query(&mut self, e: &Expr, base: &Expr, field: &FieldName) -> Option<Ty> {
        let FieldName::Named(name) = field else { return None };
        let align = match name.name.as_str() {
            "size" => false,
            "align" => true,
            _ => return None,
        };
        let ExprKind::Field { base: inner, field: FieldName::Named(frame) } = &base.kind else { return None };
        if frame.name != "Frame" {
            return None;
        }
        let ExprKind::Def { def, args } = &inner.kind else { return None };
        let DefKind::Fn(f) = &self.p.def(*def).kind else { return None };
        if !matches!(f.ret, FnRet::Shuttle { .. }) {
            return None;
        }
        if !args.is_empty() || !f.generics.is_empty() || f.has_self() {
            return Some(self.unsupported(e.span, "the frame of a generic shuttle, or of a method"));
        }
        let shuttle = self.instance(Instance::of(*def));
        self.b.frames.insert(e.id, FrameQuery { shuttle, align });
        Some(self.tcx.usize)
    }

    fn wrong_side(&mut self, span: Span, what: &str, why: &str) {
        self.diags.push(
            Diagnostic::error(codes::T0120, format!("`{what}` doesn't fit this shuttle"))
                .primary(span, why)
                .help("the signature says what a shuttle does: `~> T` yields `T`s, `<~ U` pulls `U`s"),
        );
    }
}
