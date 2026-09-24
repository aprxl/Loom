//! Checking patterns, and whether a `match` covers every value.

use loom_syntax::Diagnostic;
use loom_syntax::ast::Lit;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::Checker;
use crate::hir::{Arm, Pat, PatKind};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    /// Checks that `pat` can match a value of type `ty`, and types its bindings.
    pub(super) fn check_pat(&mut self, pat: &Pat, ty: Ty) {
        self.r.pat_ty.insert(pat.id, ty);
        let ty = self.resolve(ty);
        let span = pat.span;
        match &pat.kind {
            PatKind::Wild | PatKind::Error => {}
            PatKind::Bind(local) => {
                self.r.local_ty.insert(*local, ty);
            }
            PatKind::Lit { lit, negative } => self.check_lit_pat(lit, *negative, ty, span),
            PatKind::Range { lo, hi, .. } => {
                if !matches!(self.tcx.kind(ty), TyKind::Int(_) | TyKind::IntVar(_) | TyKind::Char | TyKind::Error) {
                    let shown = self.show(ty);
                    self.error(codes::T0101, span, format!("a range pattern can't match a `{shown}`"), "here");
                    return;
                }
                for end in [lo, hi].into_iter().flatten() {
                    self.check_pat(end, ty);
                }
            }
            PatKind::Tuple(pats) => match self.tcx.kind(ty).clone() {
                TyKind::Tuple(tys) if tys.len() == pats.len() => {
                    for (p, t) in pats.iter().zip(tys) {
                        self.check_pat(p, t);
                    }
                }
                TyKind::Error => {
                    for p in pats {
                        self.check_pat(p, ty);
                    }
                }
                _ => {
                    let shown = self.show(ty);
                    self.error(
                        codes::T0101,
                        span,
                        format!("a tuple pattern with {} parts can't match a `{shown}`", pats.len()),
                        "here",
                    );
                    for p in pats {
                        self.check_pat(p, self.tcx.error);
                    }
                }
            },
            PatKind::Or(alts) => {
                for alt in alts {
                    self.check_pat(alt, ty);
                }
            }
            PatKind::Twill { .. } => {
                self.unsupported(span, "twill patterns");
            }
            PatKind::Case { .. } | PatKind::ConstCase(_) => {
                self.unsupported(span, "case patterns");
            }
            PatKind::Some(_) | PatKind::None | PatKind::Ok(_) | PatKind::Err(_) => {
                self.unsupported(span, "optional and result patterns");
            }
            PatKind::Slice(_) | PatKind::Rest(_) => {
                self.unsupported(span, "slice patterns");
            }
            PatKind::TypeTest { .. } => {
                self.unsupported(span, "type-test patterns");
            }
        }
    }

    fn check_lit_pat(&mut self, lit: &Lit, negative: bool, ty: Ty, span: Span) {
        if self.tcx.is_error(ty) {
            return;
        }
        let fits = match (lit, self.tcx.kind(ty)) {
            (Lit::Int(i), TyKind::Int(int)) => {
                let int = *int;
                let value = if negative { -(i.value as i128) } else { i.value as i128 };
                if !int.contains(value) {
                    let shown = self.show(ty);
                    self.error(codes::T0102, span, format!("`{value}` can never be a `{shown}`"), "out of range");
                }
                true
            }
            (Lit::Int(i), TyKind::IntVar(_)) => {
                let value = if negative { -(i.value as i128) } else { i.value as i128 };
                self.pending_lits.push(super::infer::PendingLit { ty, value, span });
                true
            }
            (Lit::Float(_), TyKind::Float(_) | TyKind::FloatVar(_)) => true,
            (Lit::Char(_), TyKind::Char) | (Lit::Bool(_), TyKind::Bool) | (Lit::Str(_), TyKind::Str) => true,
            (Lit::Str(_), TyKind::Adt(def, _)) => Some(*def) == self.std.string,
            _ => false,
        };
        if !fits {
            let shown = self.show(ty);
            self.error(codes::T0101, span, format!("this literal can't match a `{shown}`"), "wrong kind of literal");
        }
    }

    /// Checks a pattern that must always match: in `let`, `for` and parameters.
    pub(super) fn check_pat_irrefutable(&mut self, pat: &Pat, ty: Ty, what: &str) {
        self.check_pat(pat, ty);
        if !irrefutable(pat) {
            self.diags.push(
                Diagnostic::error(codes::P0102, format!("this pattern might not match, so it can't be used in {what}"))
                    .primary(pat.span, "might not match")
                    .help("use `match` or `if … is …` to handle the values it doesn't cover"),
            );
        }
    }

    /// A conservative exhaustiveness check: some arm must match anything, or
    /// a `bool` match must cover `true` and `false`. The full check (cases,
    /// integer ranges, twill bit patterns) comes with warps.
    pub(super) fn check_exhaustive(&mut self, arms: &[Arm], ty: Ty, span: Span) {
        // An arm this checker can't handle yet was already reported, and
        // judging coverage without it would only add noise.
        if self.tcx.is_error(ty) || arms.iter().any(|a| !supported(&a.pat)) {
            return;
        }
        let unguarded = || arms.iter().filter(|a| a.guard.is_none());
        if unguarded().any(|a| irrefutable(&a.pat)) {
            return;
        }
        if matches!(self.tcx.kind(self.resolve(ty)), TyKind::Bool) {
            let (mut t, mut f) = (false, false);
            for arm in unguarded() {
                bool_cover(&arm.pat, &mut t, &mut f);
            }
            if t && f {
                return;
            }
        }
        let shown = self.show(ty);
        self.diags.push(
            Diagnostic::error(codes::P0101, format!("this `match` doesn't cover every `{shown}`"))
                .primary(span, "some values aren't handled")
                .help("add an arm like `_ => …` for everything else"),
        );
    }
}

/// Whether `pat` matches every value of its type.
pub(super) fn irrefutable(pat: &Pat) -> bool {
    match &pat.kind {
        PatKind::Wild | PatKind::Bind(_) | PatKind::Error => true,
        PatKind::Tuple(pats) => pats.iter().all(irrefutable),
        PatKind::Or(alts) => alts.iter().any(irrefutable),
        _ => false,
    }
}

/// Whether this checker understands every part of `pat`.
fn supported(pat: &Pat) -> bool {
    match &pat.kind {
        PatKind::Wild | PatKind::Bind(_) | PatKind::Lit { .. } | PatKind::Range { .. } | PatKind::Error => true,
        PatKind::Tuple(pats) | PatKind::Or(pats) => pats.iter().all(supported),
        _ => false,
    }
}

fn bool_cover(pat: &Pat, t: &mut bool, f: &mut bool) {
    match &pat.kind {
        PatKind::Lit { lit: Lit::Bool(true), .. } => *t = true,
        PatKind::Lit { lit: Lit::Bool(false), .. } => *f = true,
        PatKind::Or(alts) => alts.iter().for_each(|a| bool_cover(a, t, f)),
        _ => {}
    }
}
