//! Lowering gauges.
//!
//! A gauge's value is its base's value. Each gauge the program checks at run
//! time gets a function in `F` that says whether a number is in it
//! (`F.Cents_12(v)`): comparisons against its range, or its predicate. Casts
//! and `is` tests call it; `as|` clamps to the range instead.

use loom_syntax::ast::CastKind;
use loom_syntax::source::Span;

use super::Lowerer;
use super::lir::{Expr, Stmt, StmtKind};
use crate::hir::{self, DefId, DefKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::GaugeRange;

impl Lowerer<'_> {
    /// `F.<gauge>`, the function that says whether a number is in gauge
    /// `def`, emitted with the others.
    pub(super) fn gauge_check(&mut self, def: DefId) -> Expr {
        self.gauges_used.insert(def);
        Expr::field(Expr::name("F"), &self.def_name(def))
    }

    /// `inner as G`, `as? G` or `as| G`, where `G` is gauge `def`.
    pub(super) fn gauge_cast(&mut self, inner: &hir::Expr, kind: CastKind, def: DefId, base: Ty, span: Span) -> Expr {
        let source = self.ty_of(inner);
        // Into the base type first; clamping a float into an integer or a
        // narrower integer saturates, anything else is checked.
        let saturating = kind == CastKind::Sat && matches!(self.tcx().kind(base), TyKind::Int(_)) && source != base;
        let to_base = match kind {
            CastKind::Opt => CastKind::Opt,
            _ if saturating => CastKind::Sat,
            _ => CastKind::Checked,
        };
        let v = self.cast_to(inner, to_base, base);
        let check = self.gauge_check(def);
        match kind {
            CastKind::Opt => Expr::rt("gauge_opt", vec![check, v]),
            CastKind::Sat => {
                let range = self.typed.results.gauges.get(&def).and_then(|g| g.range);
                let (lo, hi) = match range {
                    Some(GaugeRange::Int { lo, hi }) => (self.int_value(lo, base), self.int_value(hi, base)),
                    Some(GaugeRange::Float { lo, hi, .. }) => (float_bound(lo), float_bound(hi)),
                    None => (Expr::Nil, Expr::Nil),
                };
                let d = self.desc(base, span);
                Expr::rt("clamp", vec![d, v, lo, hi])
            }
            _ => {
                let name = Expr::Str(self.p.def(def).name.clone().into_bytes());
                Expr::rt("into_gauge", vec![check, v, name])
            }
        }
    }

    /// Emits the checking function of every gauge used (and of any their
    /// predicates use).
    pub(super) fn emit_gauges(&mut self) {
        let mut done = 0;
        while done < self.gauges_used.len() {
            let def = self.gauges_used[done];
            done += 1;
            let Some(info) = self.typed.results.gauges.get(&def).cloned() else { continue };
            let span = self.p.def(def).span;
            let v = Expr::name("v");
            let mut tests = Vec::new();
            match info.range {
                Some(GaugeRange::Int { lo, hi }) => tests.extend(self.int_bounds(&v, lo, hi, info.base)),
                Some(GaugeRange::Float { lo, lo_incl, hi, hi_incl }) => {
                    if lo != f64::NEG_INFINITY {
                        tests.push(Expr::bin(if lo_incl { "<=" } else { "<" }, float_bound(lo), v.clone()));
                    }
                    if hi != f64::INFINITY {
                        tests.push(Expr::bin(if hi_incl { "<=" } else { "<" }, v.clone(), float_bound(hi)));
                    }
                }
                None => {
                    // The value may come from a wider type: it must be a
                    // value of the base first.
                    if let Some(int) = self.tcx().as_int(info.base) {
                        tests.extend(self.int_bounds(&v, int.min(), int.max(), info.base));
                    }
                    let Some(pred_body) = info.pred else { continue };
                    let DefKind::Type(alias) = &self.p.def(def).kind else { continue };
                    let Some(pred) = alias.gauge.as_ref() else { continue };
                    let f = self.in_body(pred_body, |this| this.lower_expr(pred));
                    let pred_name = format!("{}_p", self.def_name(def));
                    let target = Expr::field(Expr::name("F"), &pred_name);
                    self.fns.push(Stmt { kind: StmtKind::Assign(vec![target.clone()], vec![f]), span: Some(span) });
                    tests.push(Expr::call(target, vec![v.clone()]));
                }
            }
            let test = tests.into_iter().reduce(|a, b| Expr::bin("and", a, b)).unwrap_or(Expr::Bool(true));
            let body = vec![Stmt { kind: StmtKind::Return(Some(test)), span: None }];
            let func = Expr::Function(vec!["v".into()], body);
            let target = Expr::field(Expr::name("F"), &self.def_name(def));
            self.fns.push(Stmt { kind: StmtKind::Assign(vec![target], vec![func]), span: Some(span) });
        }
    }

    /// `lo <= v and v <= hi`, leaving out a bound the base can't pass.
    fn int_bounds(&self, v: &Expr, lo: i128, hi: i128, base: Ty) -> Vec<Expr> {
        let Some(int) = self.tcx().as_int(base) else { return Vec::new() };
        let u64_ = !int.signed && int.bits == 64;
        let mut out = Vec::new();
        if lo > int.min() {
            let lo = self.int_value(lo, base);
            out.push(if u64_ {
                Expr::Unary("not", Box::new(Expr::rt("ult", vec![v.clone(), lo])))
            } else {
                Expr::bin("<=", lo, v.clone())
            });
        }
        if hi < int.max() {
            let hi = self.int_value(hi, base);
            out.push(if u64_ {
                Expr::Unary("not", Box::new(Expr::rt("ult", vec![hi, v.clone()])))
            } else {
                Expr::bin("<=", v.clone(), hi)
            });
        }
        // A value from a wider type (`x: i64 is Small`) may also be past
        // the base's own range.
        if int.bits < 64 {
            if lo <= int.min() {
                out.push(Expr::bin("<=", Expr::Int(int.min() as i64), v.clone()));
            }
            if hi >= int.max() {
                out.push(Expr::bin("<=", v.clone(), Expr::Int(int.max() as i64)));
            }
        }
        out
    }
}

fn float_bound(f: f64) -> Expr {
    if f.is_infinite() { Expr::Nil } else { Expr::Float(f) }
}
