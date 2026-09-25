//! Gauges: numeric types limited to a range or a predicate (see 02-types,
//! "Gauges").
//!
//! `type Cents = i64 where 0..` is a type of its own whose values are the
//! `i64`s from 0 up. It widens to `i64` on its own; getting into it is
//! checked: a literal or constant at compile time (G0201), `x as Cents` at
//! run time (it unravels), `x as? Cents` gives `?Cents`, `x as| Cents`
//! clamps, and `if x is Cents { … }` tests it and treats `x` as a `Cents`
//! inside.
//!
//! Arithmetic on a range gauge keeps track of intervals: `Cents / 100` is
//! still from 0 up, so it's a `Cents`, while `Cents - Cents` could be
//! anything, so it's an `i64`. A literal counts as its own value.
//!
//! A predicate (`type Even = i64 where _ % 2 == 0`) is a hole lambda, checked
//! as a function of the base type. The compiler works out literals against it
//! when the predicate uses only arithmetic, comparisons and constants.

use loom_syntax::Diagnostic;
use loom_syntax::ast::{BinaryOp, FieldName, Lit, UnaryOp};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{BodyId, Checker, Instance};
use crate::hir::{DefId, DefKind, Expr, ExprKind, LocalId, StmtKind};
use crate::ty::{Ty, TyKind};

/// What a gauge allows.
#[derive(Clone, Debug)]
pub struct GaugeInfo {
    pub base: Ty,
    /// A range gauge's bounds; `None` for a predicate gauge.
    pub range: Option<GaugeRange>,
    /// A predicate gauge's checked predicate, once it's checked.
    pub pred: Option<BodyId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GaugeRange {
    /// From `lo` to `hi`, both included.
    Int {
        lo: i128,
        hi: i128,
    },
    Float {
        lo: f64,
        lo_incl: bool,
        hi: f64,
        hi_incl: bool,
    },
}

/// A value worked out at compile time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Val {
    Int(i128),
    Float(f64),
    Bool(bool),
}

/// The values an expression may have, as far as the checker can tell.
#[derive(Clone, Copy, Debug)]
enum Interval {
    Int(i128, i128),
    Float(f64, f64),
}

impl GaugeRange {
    fn contains(&self, v: Val) -> bool {
        match (*self, v) {
            (GaugeRange::Int { lo, hi }, Val::Int(v)) => lo <= v && v <= hi,
            (GaugeRange::Int { lo, hi }, Val::Float(f)) => f.fract() == 0.0 && lo as f64 <= f && f <= hi as f64,
            (GaugeRange::Float { lo, lo_incl, hi, hi_incl }, v) => {
                let f = match v {
                    Val::Int(i) => i as f64,
                    Val::Float(f) => f,
                    Val::Bool(_) => return false,
                };
                (if lo_incl { lo <= f } else { lo < f }) && (if hi_incl { f <= hi } else { f < hi })
            }
            _ => false,
        }
    }

    /// Whether every value of `i` is in the range.
    fn holds(&self, i: Interval) -> bool {
        match (*self, i) {
            (GaugeRange::Int { lo, hi }, Interval::Int(a, b)) => lo <= a && b <= hi,
            (GaugeRange::Float { .. }, Interval::Float(a, b)) => {
                a <= b && self.contains(Val::Float(a)) && self.contains(Val::Float(b))
            }
            _ => false,
        }
    }

    /// How it's written: `0..`, `1..=100`, `0.0..=1.0`.
    pub fn show(&self, base_min: i128, base_max: i128) -> String {
        match *self {
            GaugeRange::Int { lo, hi } => {
                let lo = if lo == base_min { String::new() } else { lo.to_string() };
                let hi = if hi == base_max { String::new() } else { format!("={hi}") };
                format!("{lo}..{hi}")
            }
            GaugeRange::Float { lo, lo_incl: _, hi, hi_incl } => {
                let lo = if lo == f64::NEG_INFINITY { String::new() } else { format!("{lo:?}") };
                let hi = if hi == f64::INFINITY {
                    String::new()
                } else if hi_incl {
                    format!("={hi:?}")
                } else {
                    format!("{hi:?}")
                };
                format!("{lo}..{hi}")
            }
        }
    }
}

impl Checker<'_> {
    /// The type named by gauge `def`.
    pub(super) fn gauge_ty(&mut self, def: DefId, span: Span) -> Ty {
        let Some(info) = self.gauge_info(def, span) else { return self.tcx.error };
        // Its predicate is checked (and emitted) once it's used.
        self.queue_def(def);
        self.tcx.intern(TyKind::Gauge(def, info.base))
    }

    /// What gauge `def` allows, worked out on first use. `None` if it's
    /// wrong (reported).
    pub(super) fn gauge_info(&mut self, def: DefId, span: Span) -> Option<GaugeInfo> {
        if let Some(info) = self.r.gauges.get(&def) {
            return Some(info.clone());
        }
        if self.gauge_errors.contains(&def) {
            return None;
        }
        let DefKind::Type(alias) = &self.p.def(def).kind else { return None };
        let (Some(value), Some(gauge)) = (&alias.value, &alias.gauge) else { return None };
        self.gauge_errors.insert(def);
        let base = self.lower_ty(value);
        let base = self.resolve(base);
        if self.tcx.is_error(base) {
            return None;
        }
        if !matches!(self.tcx.kind(base), TyKind::Int(_) | TyKind::Float(_)) {
            let shown = self.show(base);
            self.error(
                codes::T0103,
                value.span,
                format!("a gauge limits a number type, and `{shown}` isn't one"),
                "not a number type",
            );
            return None;
        }
        let range = match &gauge.kind {
            ExprKind::Range { lo, hi, inclusive } => {
                Some(self.gauge_range(base, lo.as_deref(), hi.as_deref(), *inclusive, gauge.span)?)
            }
            ExprKind::Lambda { params, .. } if params.len() == 1 => None,
            _ => {
                self.diags.push(
                    Diagnostic::error(codes::T0103, "a gauge is a range or a predicate")
                        .primary(gauge.span, "neither")
                        .help("write a range (`where 0..100`) or a predicate with `_` (`where _ % 2 == 0`)"),
                );
                return None;
            }
        };
        let _ = span;
        self.gauge_errors.remove(&def);
        let info = GaugeInfo { base, range, pred: None };
        self.r.gauges.insert(def, info.clone());
        Some(info)
    }

    fn gauge_range(
        &mut self,
        base: Ty,
        lo: Option<&Expr>,
        hi: Option<&Expr>,
        inclusive: bool,
        span: Span,
    ) -> Option<GaugeRange> {
        let bound = |this: &mut Self, e: Option<&Expr>| -> Option<Option<Val>> {
            let Some(e) = e else { return Some(None) };
            match this.fold(e, None) {
                Some(v @ (Val::Int(_) | Val::Float(_))) => Some(Some(v)),
                _ => {
                    this.error(
                        codes::T0103,
                        e.span,
                        "a gauge's bounds must be known at compile time",
                        "not a literal or a constant",
                    );
                    None
                }
            }
        };
        let (lo, hi) = (bound(self, lo)?, bound(self, hi)?);
        let range = match *self.tcx.kind(base) {
            TyKind::Int(int) => {
                let as_int = |v: Option<Val>| match v {
                    Some(Val::Int(i)) => Some(Some(i)),
                    Some(_) => None,
                    None => Some(None),
                };
                let (Some(lo), Some(hi)) = (as_int(lo), as_int(hi)) else {
                    self.error(codes::T0101, span, "an integer gauge needs integer bounds", "not integers");
                    return None;
                };
                let lo = lo.unwrap_or(int.min());
                let hi = match hi {
                    Some(h) if inclusive => h,
                    Some(h) => h - 1,
                    None => int.max(),
                };
                if !int.contains(lo) || !int.contains(hi) {
                    let base_name = self.show(base);
                    self.error(
                        codes::T0102,
                        span,
                        format!("this range doesn't fit in `{base_name}`"),
                        format!("`{base_name}` holds {} through {}", int.min(), int.max()),
                    );
                    return None;
                }
                GaugeRange::Int { lo, hi }
            }
            _ => {
                let as_float = |v: Option<Val>| match v {
                    Some(Val::Int(i)) => Some(i as f64),
                    Some(Val::Float(f)) => Some(f),
                    _ => None,
                };
                GaugeRange::Float {
                    lo: as_float(lo).unwrap_or(f64::NEG_INFINITY),
                    lo_incl: true,
                    hi: as_float(hi).unwrap_or(f64::INFINITY),
                    hi_incl: inclusive || hi.is_none(),
                }
            }
        };
        let empty = match range {
            GaugeRange::Int { lo, hi } => lo > hi,
            GaugeRange::Float { lo, hi, hi_incl, .. } => lo > hi || (lo == hi && !hi_incl),
        };
        if empty {
            self.error(codes::G0201, span, "this gauge has no values", "an empty range");
            return None;
        }
        Some(range)
    }

    /// Checks a gauge's declaration: a predicate is a function of the base
    /// type that says yes or no, checked in the body `id`.
    pub(super) fn check_gauge_def(&mut self, id: BodyId, def: DefId) {
        let DefKind::Type(alias) = &self.p.def(def).kind else { return };
        let span = self.p.def(def).span;
        let Some(info) = self.gauge_info(def, span) else { return };
        let Some(pred) = alias.gauge.as_ref().filter(|_| info.range.is_none()) else { return };
        let want = self.tcx.intern(TyKind::Fn(vec![info.base], self.tcx.bool));
        self.in_body(id, |this| {
            let first_var = this.vars.len();
            this.check_coerce(pred, want);
            this.finish_body(first_var);
        });
        self.r.reachable.push(id);
        if let Some(g) = self.r.gauges.get_mut(&def) {
            g.pred = Some(id);
        }
    }

    /// Checks a declaration `type X = …` that isn't a gauge: its type.
    pub(super) fn check_alias(&mut self, def: DefId) {
        let DefKind::Type(alias) = &self.p.def(def).kind else { return };
        if !alias.generics.is_empty() {
            self.unsupported(self.p.def(def).span, "generic type aliases");
            return;
        }
        if alias.gauge.is_some() {
            let id = self.instance(Instance::of(def));
            self.check_gauge_def(id, def);
        } else if let Some(value) = &alias.value {
            self.lower_ty(value);
        }
    }

    /// Whether gauge `def` allows `v`: `None` if that can't be worked out
    /// at compile time.
    fn gauge_allows(&mut self, def: DefId, v: Val) -> Option<bool> {
        let info = self.r.gauges.get(&def)?.clone();
        if let Some(range) = info.range {
            return Some(range.contains(v));
        }
        let DefKind::Type(alias) = &self.p.def(def).kind else { return None };
        let pred = alias.gauge.as_ref()?;
        let ExprKind::Lambda { params, body } = &pred.kind else { return None };
        let param = params.first()?.local;
        match self.fold(body, Some((param, v)))? {
            Val::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Reports a value that can't go into the gauge `ty` (a literal or a
    /// constant, whose value is known).
    pub(super) fn check_into_gauge(&mut self, ty: Ty, v: Val, span: Span) {
        let TyKind::Gauge(def, base) = *self.tcx.kind(ty) else { return };
        let name = self.p.def(def).name.clone();
        let shown = match v {
            Val::Int(i) => i.to_string(),
            Val::Float(f) => format!("{f:?}"),
            Val::Bool(b) => b.to_string(),
        };
        match self.gauge_allows(def, v) {
            Some(true) => {}
            Some(false) => {
                let what = match self.r.gauges.get(&def).and_then(|g| g.range) {
                    Some(range) => {
                        let (min, max) = self.tcx.as_int(base).map_or((0, 0), |i| (i.min(), i.max()));
                        format!("`{name}` is {}", range.show(min, max))
                    }
                    None => format!("`{name}`'s predicate says no"),
                };
                self.diags.push(
                    Diagnostic::error(codes::G0201, format!("`{shown}` is outside the gauge `{name}`"))
                        .primary(span, what),
                );
            }
            None => {
                self.unsupported(span, &format!("checking `{name}`'s predicate at compile time"));
            }
        }
    }

    // ------------------------------------------------------------ casts and tests

    /// `x as G`, `as?`, `as|` into gauge `def`: `x` must convert to the base.
    pub(super) fn check_gauge_cast(
        &mut self,
        kind: loom_syntax::ast::CastKind,
        source: Ty,
        target: Ty,
        def: DefId,
        span: Span,
    ) -> Ty {
        use loom_syntax::ast::CastKind as K;
        let name = self.p.def(def).name.clone();
        let range = self.r.gauges.get(&def).and_then(|g| g.range);
        let refuse = |this: &mut Self, why: &str| {
            this.error(codes::T0109, span, format!("can't convert to `{name}` with `{}`", kind.as_str()), why);
            this.tcx.error
        };
        match kind {
            K::Wrap => return refuse(self, "a gauge can't wrap: use `as`, `as?` or `as|`"),
            K::Sat if range.is_none() => return refuse(self, "only a range gauge can clamp"),
            K::Sat if matches!(range, Some(GaugeRange::Float { hi_incl: false, .. })) => {
                return refuse(self, "a range that leaves out its end has no largest value to clamp to");
            }
            _ => {}
        }
        use TyKind as T;
        let numeric = |t: &TyKind| matches!(t, T::Int(_) | T::IntVar(_) | T::Float(_) | T::FloatVar(_));
        if !numeric(self.tcx.kind(source)) {
            let from = self.show(source);
            return refuse(self, &format!("`{from}` isn't a number"));
        }
        if kind == K::Opt { self.tcx.optional(target) } else { target }
    }

    /// A pattern testing whether a number is in gauge `gauge`.
    pub(super) fn check_gauge_test(&mut self, pat: &crate::hir::Pat, gauge: Ty, scrutinee: Ty) {
        let base = self.tcx.degauge(self.resolve(scrutinee));
        if !matches!(self.tcx.kind(base), TyKind::Int(_) | TyKind::IntVar(_) | TyKind::Float(_) | TyKind::FloatVar(_)) {
            let (shown, name) = (self.show(scrutinee), self.show(gauge));
            self.error(
                codes::T0101,
                pat.span,
                format!("`{name}` is a gauge of numbers, and this is `{shown}`"),
                "can't be tested",
            );
            return;
        }
        self.b.gauge_tests.insert(pat.id, gauge);
    }

    /// The locals `cond` shows to be in a gauge where it's true: `n` in
    /// `n is Cents`, and in each side of an `and`. Only locals that can't
    /// change: a `var` could leave the gauge inside the branch.
    pub(super) fn narrowings(&self, cond: &Expr) -> Vec<(LocalId, Ty)> {
        match &cond.kind {
            // Only a local of the gauge's base type: a value of another
            // type (an `f64` tested for an `f32` gauge) isn't one of its values.
            ExprKind::Is { expr, pat } => match (&expr.kind, self.b.gauge_tests.get(&pat.id)) {
                (ExprKind::Local(l), Some(&g))
                    if !self.p.local(*l).mutable
                        && matches!(pat.kind, crate::hir::PatKind::Case { .. })
                        && self
                            .b
                            .local_ty
                            .get(l)
                            .is_some_and(|&t| self.tcx.degauge(self.resolve(t)) == self.tcx.degauge(g)) =>
                {
                    vec![(*l, g)]
                }
                _ => Vec::new(),
            },
            ExprKind::Binary { op: BinaryOp::And, lhs, rhs } => {
                let mut out = self.narrowings(lhs);
                out.extend(self.narrowings(rhs));
                out
            }
            _ => Vec::new(),
        }
    }

    /// Runs `f` with each local in `narrowed` typed as its gauge.
    pub(super) fn with_narrowed<T>(&mut self, narrowed: &[(LocalId, Ty)], f: impl FnOnce(&mut Self) -> T) -> T {
        let saved: Vec<(LocalId, Option<Ty>)> =
            narrowed.iter().map(|&(l, g)| (l, self.b.local_ty.insert(l, g))).collect();
        let out = f(self);
        for (l, old) in saved.into_iter().rev() {
            match old {
                Some(t) => self.b.local_ty.insert(l, t),
                None => self.b.local_ty.remove(&l),
            };
        }
        out
    }

    // ------------------------------------------------------------ arithmetic

    /// The type of `lhs op rhs`, whose base type is `base`, when a gauge
    /// operand's range still holds every possible result.
    pub(super) fn gauge_arith(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, base: Ty) -> Option<Ty> {
        if !matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem) {
            return None;
        }
        let tys: Vec<Ty> =
            [lhs, rhs].iter().filter_map(|e| self.b.expr_ty.get(&e.id).copied()).map(|t| self.resolve(t)).collect();
        let gauges: Vec<Ty> =
            tys.iter().copied().filter(|&t| matches!(self.tcx.kind(t), TyKind::Gauge(_, b) if *b == base)).collect();
        if gauges.is_empty() {
            return None;
        }
        let a = self.interval(lhs, base);
        let b = self.interval(rhs, base);
        let result = arith(op, a?, b?, self.base_interval(base)?)?;
        gauges.into_iter().find(|&g| match self.tcx.kind(g) {
            TyKind::Gauge(def, _) => self.r.gauges.get(def).and_then(|i| i.range).is_some_and(|r| r.holds(result)),
            _ => false,
        })
    }

    /// Checks that `place op= value` keeps a gauge-typed place in its gauge.
    pub(super) fn check_gauge_assign(&mut self, place_ty: Ty, op: BinaryOp, place: &Expr, value: &Expr, span: Span) {
        let place_ty = self.resolve(place_ty);
        let TyKind::Gauge(def, base) = *self.tcx.kind(place_ty) else { return };
        let fits = match (self.interval(place, base), self.interval(value, base), self.base_interval(base)) {
            (Some(a), Some(b), Some(full)) => {
                let range = self.r.gauges.get(&def).and_then(|g| g.range);
                arith(op, a, b, full).is_some_and(|r| range.is_some_and(|g| g.holds(r)))
            }
            _ => false,
        };
        if !fits {
            let name = self.p.def(def).name.clone();
            let o = op.as_str();
            self.diags.push(
                Diagnostic::error(codes::T0101, format!("`{o}=` could take this out of the gauge `{name}`"))
                    .primary(span, format!("the result isn't always a `{name}`"))
                    .help(format!("say how to get back in: `x = (x {o} y) as {name}` (or `as?`, `as|`)")),
            );
        }
    }

    /// What values `e` (of base type `base` once widened) may have.
    fn interval(&mut self, e: &Expr, base: Ty) -> Option<Interval> {
        if let Some(v) = self.fold(e, None) {
            return match (v, self.tcx.kind(base)) {
                (Val::Int(i), TyKind::Int(_)) => Some(Interval::Int(i, i)),
                (Val::Int(i), TyKind::Float(_)) => Some(Interval::Float(i as f64, i as f64)),
                (Val::Float(f), TyKind::Float(_)) => Some(Interval::Float(f, f)),
                _ => None,
            };
        }
        let ty = self.b.expr_ty.get(&e.id).copied().map(|t| self.resolve(t));
        if let Some(ty) = ty
            && let TyKind::Gauge(def, _) = *self.tcx.kind(ty)
            && let Some(range) = self.r.gauges.get(&def).and_then(|g| g.range)
        {
            return Some(match range {
                GaugeRange::Int { lo, hi } => Interval::Int(lo, hi),
                GaugeRange::Float { lo, hi, .. } => Interval::Float(lo, hi),
            });
        }
        self.base_interval(base)
    }

    fn base_interval(&self, base: Ty) -> Option<Interval> {
        match self.tcx.kind(base) {
            TyKind::Int(int) => Some(Interval::Int(int.min(), int.max())),
            TyKind::Float(_) => Some(Interval::Float(f64::NEG_INFINITY, f64::INFINITY)),
            _ => None,
        }
    }

    // ------------------------------------------------------------ folding

    /// The value of `e`, if it's known at compile time: literals, constants
    /// and arithmetic on them. `param` is a predicate's parameter and its value.
    pub(super) fn fold(&mut self, e: &Expr, param: Option<(LocalId, Val)>) -> Option<Val> {
        self.fold_depth(e, param, 0)
    }

    fn fold_depth(&mut self, e: &Expr, param: Option<(LocalId, Val)>, depth: u32) -> Option<Val> {
        if depth > 64 {
            return None;
        }
        let sub = |this: &mut Self, e: &Expr| this.fold_depth(e, param, depth + 1);
        match &e.kind {
            ExprKind::Lit(Lit::Int(i)) => Some(Val::Int(i128::try_from(i.value).ok()?)),
            ExprKind::Lit(Lit::Float(f)) => Some(Val::Float(f.value)),
            ExprKind::Lit(Lit::Bool(b)) => Some(Val::Bool(*b)),
            ExprKind::Local(l) => param.filter(|(p, _)| p == l).map(|(_, v)| v),
            ExprKind::Block(block) => match block.stmts.as_slice() {
                [only] => match &only.kind {
                    StmtKind::Expr(e) => sub(self, e),
                    _ => None,
                },
                _ => None,
            },
            ExprKind::Def { def, .. } => match &self.p.def(*def).kind {
                DefKind::Const(c) => match (self.r.untyped_consts.get(def), self.r.untyped_floats.get(def)) {
                    (Some(&v), _) => Some(Val::Int(v)),
                    (_, Some(&f)) => Some(Val::Float(f)),
                    _ => sub(self, &c.value),
                },
                _ => None,
            },
            ExprKind::Assoc { base, name } => {
                let ty = self.lower_ty(base);
                let int = self.tcx.as_int(ty)?;
                match name.name.as_str() {
                    "MAX" => Some(Val::Int(int.max())),
                    "MIN" => Some(Val::Int(int.min())),
                    _ => None,
                }
            }
            ExprKind::Unary { op, operand } => match (op, sub(self, operand)?) {
                (UnaryOp::Neg, Val::Int(i)) => Some(Val::Int(i.checked_neg()?)),
                (UnaryOp::Neg, Val::Float(f)) => Some(Val::Float(-f)),
                (UnaryOp::Not, Val::Bool(b)) => Some(Val::Bool(!b)),
                (UnaryOp::BitNot, Val::Int(i)) => Some(Val::Int(!i)),
                _ => None,
            },
            ExprKind::Binary { op: BinaryOp::And, lhs, rhs } => match sub(self, lhs)? {
                Val::Bool(false) => Some(Val::Bool(false)),
                Val::Bool(true) => sub(self, rhs),
                _ => None,
            },
            ExprKind::Binary { op: BinaryOp::Or, lhs, rhs } => match sub(self, lhs)? {
                Val::Bool(true) => Some(Val::Bool(true)),
                Val::Bool(false) => sub(self, rhs),
                _ => None,
            },
            ExprKind::Binary { op, lhs, rhs } => {
                let (a, b) = (sub(self, lhs)?, sub(self, rhs)?);
                binary(*op, a, b)
            }
            // `xs.len()` of an array: its type says.
            ExprKind::Call { callee, args } if args.is_empty() => match &callee.kind {
                ExprKind::Field { base, field: FieldName::Named(name) } if name.name == "len" => {
                    let ty = self.b.expr_ty.get(&base.id).copied()?;
                    let ty = self.deep_resolve(ty);
                    match *self.tcx.kind(ty) {
                        TyKind::Array(_, n) => Some(Val::Int(i128::from(n))),
                        _ => None,
                    }
                }
                _ => None,
            },
            ExprKind::Chain { first, rest } => {
                let mut prev = sub(self, first)?;
                let mut all = true;
                for (op, e) in rest {
                    let next = sub(self, e)?;
                    match binary(*op, prev, next)? {
                        Val::Bool(b) => all &= b,
                        _ => return None,
                    }
                    prev = next;
                }
                Some(Val::Bool(all))
            }
            _ => None,
        }
    }
}

/// `a op b` at compile time.
fn binary(op: BinaryOp, a: Val, b: Val) -> Option<Val> {
    use BinaryOp as B;
    let (a, b) = match (a, b) {
        (Val::Int(x), Val::Float(y)) => (Val::Float(x as f64), Val::Float(y)),
        (Val::Float(x), Val::Int(y)) => (Val::Float(x), Val::Float(y as f64)),
        pair => pair,
    };
    let cmp = |o: std::cmp::Ordering| -> Option<Val> {
        use std::cmp::Ordering::*;
        Some(Val::Bool(match op {
            B::Eq => o == Equal,
            B::Ne => o != Equal,
            B::Lt => o == Less,
            B::Le => o != Greater,
            B::Gt => o == Greater,
            B::Ge => o != Less,
            _ => return None,
        }))
    };
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => match op {
            B::Add => x.checked_add(y).map(Val::Int),
            B::Sub => x.checked_sub(y).map(Val::Int),
            B::Mul => x.checked_mul(y).map(Val::Int),
            B::Div => x.checked_div(y).map(Val::Int),
            B::Rem => x.checked_rem(y).map(Val::Int),
            B::BitAnd => Some(Val::Int(x & y)),
            B::BitOr => Some(Val::Int(x | y)),
            B::BitXor => Some(Val::Int(x ^ y)),
            B::Shl => u32::try_from(y).ok().filter(|&s| s < 64).and_then(|s| x.checked_shl(s)).map(Val::Int),
            B::Shr => u32::try_from(y).ok().filter(|&s| s < 64).map(|s| Val::Int(x >> s)),
            _ => cmp(x.cmp(&y)),
        },
        (Val::Float(x), Val::Float(y)) => match op {
            B::Add => Some(Val::Float(x + y)),
            B::Sub => Some(Val::Float(x - y)),
            B::Mul => Some(Val::Float(x * y)),
            B::Div => Some(Val::Float(x / y)),
            B::Rem => Some(Val::Float(x % y)),
            _ => cmp(x.partial_cmp(&y)?),
        },
        (Val::Bool(x), Val::Bool(y)) => match op {
            B::Eq => Some(Val::Bool(x == y)),
            B::Ne => Some(Val::Bool(x != y)),
            _ => None,
        },
        _ => None,
    }
}

/// The interval of `a op b`, kept within `full` (a result past the base
/// type's range unravels, so it's never a value).
fn arith(op: BinaryOp, a: Interval, b: Interval, full: Interval) -> Option<Interval> {
    use BinaryOp as B;
    match (a, b, full) {
        (Interval::Int(al, ah), Interval::Int(bl, bh), Interval::Int(min, max)) => {
            let (lo, hi) = match op {
                B::Add => (al.saturating_add(bl), ah.saturating_add(bh)),
                B::Sub => (al.saturating_sub(bh), ah.saturating_sub(bl)),
                B::Mul => corners([al, ah], [bl, bh], |x, y| x.saturating_mul(y)),
                // Division by an interval that holds 0 can be anything (or
                // unravel); otherwise the corners bound it.
                B::Div if bl <= 0 && 0 <= bh => return Some(full),
                B::Div => corners([al, ah], [bl, bh], |x, y| x / y),
                B::Rem => {
                    let m = bl.unsigned_abs().max(bh.unsigned_abs()).min(i128::MAX as u128) as i128 - 1;
                    if al >= 0 {
                        (0, ah.min(m))
                    } else if ah <= 0 {
                        (al.max(-m), 0)
                    } else {
                        (-m, m)
                    }
                }
                _ => return None,
            };
            Some(Interval::Int(lo.max(min), hi.min(max)))
        }
        (Interval::Float(al, ah), Interval::Float(bl, bh), _) => {
            let (lo, hi) = match op {
                B::Add => (al + bl, ah + bh),
                B::Sub => (al - bh, ah - bl),
                B::Mul => fcorners([al, ah], [bl, bh], |x, y| x * y),
                B::Div if bl <= 0.0 && 0.0 <= bh => return Some(full),
                B::Div => fcorners([al, ah], [bl, bh], |x, y| x / y),
                _ => return Some(full),
            };
            if lo.is_nan() || hi.is_nan() { Some(full) } else { Some(Interval::Float(lo, hi)) }
        }
        _ => None,
    }
}

fn corners(a: [i128; 2], b: [i128; 2], f: impl Fn(i128, i128) -> i128) -> (i128, i128) {
    let vals = [f(a[0], b[0]), f(a[0], b[1]), f(a[1], b[0]), f(a[1], b[1])];
    (*vals.iter().min().expect("four"), *vals.iter().max().expect("four"))
}

fn fcorners(a: [f64; 2], b: [f64; 2], f: impl Fn(f64, f64) -> f64) -> (f64, f64) {
    let vals = [f(a[0], b[0]), f(a[0], b[1]), f(a[1], b[0]), f(a[1], b[1])];
    (vals.iter().copied().fold(f64::INFINITY, f64::min), vals.iter().copied().fold(f64::NEG_INFINITY, f64::max))
}
