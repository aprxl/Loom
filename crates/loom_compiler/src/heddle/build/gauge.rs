//! Gauges (design/vm.md §10.11): a gauge's value is its base's.
//!
//! Each gauge the program checks gets a function (`check[G](v)`) saying
//! whether a value of its base is in it: comparisons against its range, or
//! its predicate, called as the lambda it is. `as G` converts to the base
//! and checks, `as? G` gives `none` when either step fails, `as| G` clamps
//! to the range, and `x is G` (or `g: G =>` in a `match`) checks.

use loom_syntax::ast::CastKind;
use loom_syntax::source::Span;

use super::text::MSG;
use super::value::Val;
use super::{Builder, FnKey, Vals};
use crate::heddle::{BitOp, Cmp, InstKind, Term, VTy, Value};
use crate::hir::{self, DefId, DefKind};
use crate::ty::{FloatTy, IntTy, Ty, TyKind};
use crate::typeck::GaugeRange;

impl<'a> Builder<'a> {
    /// Whether `v`, a value of gauge `def`'s base, is in the gauge.
    pub(super) fn gauge_check(&mut self, def: DefId, v: Value) -> Value {
        let func = self.func_id(FnKey::GaugeCheck(def));
        self.emit(InstKind::Call { func, args: vec![v] }, &[VTy::Int])[0]
    }

    /// The check function of gauge `def`.
    pub(super) fn gauge_check_func(&mut self, def: DefId) -> Option<Vec<VTy>> {
        self.f.span = None;
        let span = self.p.def(def).span;
        let info = self.typed.results.gauges.get(&def)?.clone();
        let base = self.tcx().degauge(info.base);
        let vty = match self.float_of(base) {
            Some(FloatTy::F32) => VTy::F32,
            Some(FloatTy::F64) => VTy::F64,
            None => VTy::Int,
        };
        let v = self.new_value(vty);
        self.f.blocks[0].params.push(v);
        let mut tests: Vec<Value> = Vec::new();
        match info.range {
            Some(GaugeRange::Int { lo, hi }) => {
                let int = self.int_of(base)?;
                if lo > int.min() {
                    let k = self.iconst(lo as u64);
                    tests
                        .push(self.emit1(InstKind::ICmp { cmp: Cmp::Le, unsigned: !int.signed, a: k, b: v }, VTy::Int));
                }
                if hi < int.max() {
                    let k = self.iconst(hi as u64);
                    tests
                        .push(self.emit1(InstKind::ICmp { cmp: Cmp::Le, unsigned: !int.signed, a: v, b: k }, VTy::Int));
                }
            }
            Some(GaugeRange::Float { lo, lo_incl, hi, hi_incl }) => {
                let f = self.float_of(base)?;
                if lo != f64::NEG_INFINITY {
                    let k = self.fconst(lo, f);
                    let cmp = if lo_incl { Cmp::Le } else { Cmp::Lt };
                    tests.push(self.emit1(InstKind::FCmp { cmp, ty: f, a: k, b: v }, VTy::Int));
                }
                if hi != f64::INFINITY {
                    let k = self.fconst(hi, f);
                    let cmp = if hi_incl { Cmp::Le } else { Cmp::Lt };
                    tests.push(self.emit1(InstKind::FCmp { cmp, ty: f, a: v, b: k }, VTy::Int));
                }
            }
            None => {
                let body = info.pred?;
                let DefKind::Type(alias) = &self.p.def(def).kind else { return None };
                let pred: &'a hir::Expr = alias.gauge.as_ref()?;
                self.f.body = body;
                self.f.scan = self.scan(body, pred);
                let fty = self.ty_of(pred);
                let f = self.value(pred)?;
                let ok = self.call_fn(&f, fty, vec![Val::Regs(vec![v])], span)?;
                tests.push(self.regs(ok, self.tcx().bool, span)?[0]);
            }
        }
        let all = match tests.into_iter().reduce(|a, b| self.emit1(InstKind::Bit { op: BitOp::And, a, b }, VTy::Int)) {
            Some(t) => t,
            None => self.iconst(1),
        };
        self.terminate(Term::Ret(vec![all]));
        Some(vec![VTy::Int])
    }

    /// Whether `v`, a value of `source`, is in gauge `def`: a value of its
    /// base first, when `source` is wider.
    pub(super) fn gauge_test(&mut self, def: DefId, v: Value, source: Ty) -> Option<Value> {
        let base = self.tcx().degauge(self.typed.results.gauges.get(&def)?.base);
        let source = self.tcx().degauge(source);
        if let (Some(s), Some(b)) = (self.float_of(source), self.float_of(base))
            && s != b
        {
            let x = self.emit1(InstKind::FloatCast { to: b, a: v }, super::expr::float_vty(b));
            return Some(self.gauge_check(def, x));
        }
        if let (Some(s), Some(b)) = (self.int_of(source), self.int_of(base))
            && !s.widens_to(b)
        {
            let spec = self.iconst(loom_isa::word::cast_spec(super::int_spec(b), super::int_spec(s)));
            let r = self
                .emit(InstKind::Native { name: "int.cast_chk", imm: 0, args: vec![v, spec] }, &[VTy::Int, VTy::Int]);
            let res = self.new_var(VTy::Int);
            self.write(res, r[1]);
            let (yes, done) = (self.new_block(), self.new_block());
            self.br(r[1], yes, done);
            self.enter(yes);
            let ok = self.gauge_check(def, r[0]);
            self.write(res, ok);
            self.jump(done);
            self.enter(done);
            return Some(self.read(res));
        }
        Some(self.gauge_check(def, v))
    }

    /// `inner as G` (and `as?`, `as|`), `G` being gauge `def` of `base`;
    /// `target` is `G`, `ty` the cast's type.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn gauge_cast(
        &mut self,
        inner: &'a hir::Expr,
        kind: CastKind,
        def: DefId,
        base: Ty,
        target: Ty,
        ty: Ty,
        span: Span,
    ) -> Option<Vals> {
        let source = self.coerced_ty(inner);
        let base = self.tcx().degauge(base);
        let name = self.p.def(def).name.clone();
        match kind {
            CastKind::Opt => {
                // `none` when it isn't a value of the base, or isn't in it.
                let opt_base = self.tcx().find(&TyKind::Optional(base)).unwrap_or(ty);
                let r = self.cast_opt(inner, opt_base, base, span)?;
                let present = self.is_some(&Val::Regs(r.clone()), opt_base, span)?;
                let x = self.unwrap_some(&Val::Regs(r), opt_base, span)?;
                let x = self.regs(x, base, span)?[0];
                let res = self.new_var(VTy::Int);
                self.write(res, present);
                let (check, done) = (self.new_block(), self.new_block());
                self.br(present, check, done);
                self.enter(check);
                let ok = self.gauge_check(def, x);
                self.write(res, ok);
                self.jump(done);
                self.enter(done);
                let ok = self.read(res);
                self.opt_from(x, ok, ty, target, span)
            }
            CastKind::Sat => {
                // Into the base (saturating, from a float or a wider
                // integer), then clamped to the range.
                let v = self.value(inner)?[0];
                let saturating = self.int_of(base).is_some() && source != base;
                let k = if saturating { CastKind::Sat } else { CastKind::Checked };
                let v = self.convert(v, source, base, k, span)?[0];
                let range = self.typed.results.gauges.get(&def).and_then(|g| g.range);
                Some(vec![self.clamp(v, base, range)?])
            }
            _ => {
                let v = self.value(inner)?[0];
                let v = self.convert(v, source, base, kind, span)?[0];
                let ok = self.gauge_check(def, v);
                let (fail, good) = (self.new_block(), self.new_block());
                self.br(ok, good, fail);
                self.enter(fail);
                let msg = self.sink(MSG);
                self.write_text(msg, &format!("value out of range for `{name}`"));
                self.terminate(Term::UnravelMsg);
                self.enter(good);
                Some(vec![v])
            }
        }
    }

    /// `v` (of `base`) clamped to `range`: a NaN goes to the low end (or
    /// the high one, or 0), as on Lua.
    fn clamp(&mut self, v: Value, base: Ty, range: Option<GaugeRange>) -> Option<Value> {
        let r = self.new_var(self.f.types[v.0 as usize]);
        self.write(r, v);
        match range {
            Some(GaugeRange::Int { lo, hi }) => {
                let int: IntTy = self.int_of(base)?;
                for (bound, cmp) in [(lo, Cmp::Lt), (hi, Cmp::Gt)] {
                    if (cmp == Cmp::Lt && bound <= int.min()) || (cmp == Cmp::Gt && bound >= int.max()) {
                        continue;
                    }
                    let cur = self.read(r);
                    let k = self.iconst(bound as u64);
                    let past = self.emit1(InstKind::ICmp { cmp, unsigned: !int.signed, a: cur, b: k }, VTy::Int);
                    let (yes, done) = (self.new_block(), self.new_block());
                    self.br(past, yes, done);
                    self.enter(yes);
                    self.write(r, k);
                    self.jump(done);
                    self.enter(done);
                }
            }
            Some(GaugeRange::Float { lo, hi, .. }) => {
                let f = self.float_of(base)?;
                let vty = if f == FloatTy::F32 { VTy::F32 } else { VTy::F64 };
                let nan = self.emit1(InstKind::FCmp { cmp: Cmp::Ne, ty: f, a: v, b: v }, VTy::Int);
                let (is_nan, num, done) = (self.new_block(), self.new_block(), self.new_block());
                self.br(nan, is_nan, num);
                self.enter(is_nan);
                let fallback = if lo.is_finite() {
                    lo
                } else if hi.is_finite() {
                    hi
                } else {
                    0.0
                };
                let k = self.fconst(fallback, f);
                self.write(r, k);
                self.jump(done);
                self.enter(num);
                for (bound, low) in [(lo, true), (hi, false)] {
                    if !bound.is_finite() {
                        continue;
                    }
                    let cur = self.read(r);
                    let k = self.fconst(bound, f);
                    let (a, b) = if low { (cur, k) } else { (k, cur) };
                    let past = self.emit1(InstKind::FCmp { cmp: Cmp::Lt, ty: f, a, b }, VTy::Int);
                    let (yes, next) = (self.new_block(), self.new_block());
                    self.br(past, yes, next);
                    self.enter(yes);
                    self.write(r, k);
                    self.jump(next);
                    self.enter(next);
                }
                let _ = vty;
                self.jump(done);
                self.enter(done);
            }
            None => {}
        }
        Some(self.read(r))
    }
}
