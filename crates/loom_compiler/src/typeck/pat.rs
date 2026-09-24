//! Checking patterns, and whether a `match` covers every value.

use loom_syntax::Diagnostic;
use loom_syntax::ast::Lit;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::Checker;
use super::warp::Key;
use crate::hir::{Arm, CasePath, FieldPat, Pat, PatKind};
use crate::ty::{Ty, TyKind};

impl Checker<'_> {
    /// Checks that `pat` can match a value of type `ty`, and types its bindings.
    pub(super) fn check_pat(&mut self, pat: &Pat, ty: Ty) {
        self.b.pat_ty.insert(pat.id, ty);
        let ty = self.resolve(ty);
        let span = pat.span;
        match &pat.kind {
            PatKind::Wild | PatKind::Error => {}
            PatKind::Bind(local) => {
                self.b.local_ty.insert(*local, ty);
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
                TyKind::Unit if pats.is_empty() => {}
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
            PatKind::Twill { pattern, captures } => {
                let width = pattern.width;
                match self.tcx.kind(ty) {
                    TyKind::Int(int) if int.bits as u32 == width => {}
                    TyKind::Error => {}
                    _ => {
                        let shown = self.show(ty);
                        self.diags.push(
                            Diagnostic::error(
                                codes::T0101,
                                format!("this twill pattern is {width} bits wide, but it matches a `{shown}`"),
                            )
                            .primary(span, format!("{width} bits"))
                            .help("a twill pattern needs as many digits as its value has bits"),
                        );
                    }
                }
                for (capture, &local) in pattern.captures.iter().zip(captures) {
                    let bits = capture.width() as u8;
                    let cty = self.tcx.int(crate::ty::IntTy { signed: false, bits, size: false });
                    self.b.local_ty.insert(local, cty);
                }
            }
            PatKind::Case { path, fields, rest } => self.check_case_pat(pat, path, fields.as_deref(), *rest, ty),
            PatKind::ConstCase(_) => {
                self.unsupported(span, "case patterns chosen at compile time");
            }
            PatKind::Some(inner) => match self.tcx.kind(self.resolve(ty)).clone() {
                TyKind::Optional(t) => self.check_pat(inner, t),
                _ => self.wrong_wrapper(pat, inner, ty, "`some(…)` matches an optional"),
            },
            PatKind::None => {
                if !matches!(self.tcx.kind(self.resolve(ty)), TyKind::Optional(_) | TyKind::Error) {
                    let shown = self.show(ty);
                    self.error(
                        codes::T0101,
                        span,
                        format!("`none` matches an optional, and this is `{shown}`"),
                        "not an optional",
                    );
                }
            }
            PatKind::Ok(inner) => match self.tcx.kind(self.resolve(ty)).clone() {
                TyKind::Result(t, _) => self.check_pat(inner, t),
                _ => self.wrong_wrapper(pat, inner, ty, "`ok(…)` matches a result"),
            },
            PatKind::Err(inner) => match self.tcx.kind(self.resolve(ty)).clone() {
                TyKind::Result(_, e) => self.check_pat(inner, e),
                _ => self.wrong_wrapper(pat, inner, ty, "`err(…)` matches a result"),
            },
            PatKind::Slice(_) | PatKind::Rest(_) => {
                self.unsupported(span, "slice patterns");
            }
            PatKind::TypeTest { local, ty: tested } => self.check_type_test(pat, *local, tested, ty),
        }
    }

    fn wrong_wrapper(&mut self, pat: &Pat, inner: &Pat, ty: Ty, what: &str) {
        if !self.tcx.is_error(ty) {
            let shown = self.show(ty);
            self.error(codes::T0101, pat.span, format!("{what}, and this is `{shown}`"), "wrong kind of value");
        }
        self.check_pat(inner, self.tcx.error);
    }

    fn check_case_pat(&mut self, pat: &Pat, path: &CasePath, fields: Option<&[FieldPat]>, rest: bool, ty: Ty) {
        let span = pat.span;
        if self.tcx.is_error(ty) {
            for f in fields.into_iter().flatten() {
                self.check_pat(&f.pat, ty);
            }
            return;
        }
        let shown = self.show(ty);
        let scrutinee = self.adt_info(ty);
        let (info, case) = match path {
            CasePath::Const(def) => {
                let cty = self.const_ty(*def);
                if !self.unify(cty, ty) && !self.tcx.is_error(cty) {
                    self.mismatch(span, ty, cty);
                }
                self.b.const_pats.insert(pat.id, *def);
                return;
            }
            CasePath::Dot(name) => match &scrutinee {
                Some(info) => match info.case(&name.name) {
                    Some(c) => (info.clone(), Some(c.def)),
                    None => {
                        self.error(
                            codes::T0111,
                            name.span,
                            format!("`{shown}` has no case `{}`", name.name),
                            "no such case",
                        );
                        return self.check_sub_pats_alone(fields);
                    }
                },
                None => {
                    self.error(
                        codes::T0101,
                        span,
                        format!("a case pattern can't match a `{shown}`"),
                        "not a warp with cases",
                    );
                    return self.check_sub_pats_alone(fields);
                }
            },
            CasePath::Type(t) => {
                let pty = self.lower_ty(t);
                match self.adt_info(pty) {
                    Some(info) if info.is_sum() => {
                        self.diags.push(
                            Diagnostic::error(codes::T0101, format!("`{}` has cases: match one of them", info.name))
                                .primary(span, "not a record")
                                .help(format!("for example `.{}`", info.cases[0].name)),
                        );
                        return self.check_sub_pats_alone(fields);
                    }
                    Some(info) => (info, None),
                    None => {
                        if !self.tcx.is_error(pty) {
                            self.error(codes::T0101, span, "this pattern names a type that isn't a warp", "here");
                        }
                        return self.check_sub_pats_alone(fields);
                    }
                }
            }
            CasePath::Assoc { base, name } => {
                let bty = self.lower_ty(base);
                match self.adt_info(bty) {
                    Some(info) => match info.case(&name.name) {
                        Some(c) => {
                            let c = c.def;
                            (info, Some(c))
                        }
                        None => {
                            self.error(
                                codes::T0111,
                                name.span,
                                format!("`{}` has no case `{}`", info.name, name.name),
                                "no such case",
                            );
                            return self.check_sub_pats_alone(fields);
                        }
                    },
                    None => {
                        if !self.tcx.is_error(bty) {
                            self.error(codes::T0101, span, "this pattern names something that isn't a case", "here");
                        }
                        return self.check_sub_pats_alone(fields);
                    }
                }
            }
        };
        if scrutinee.as_ref().map(|s| s.def) != Some(info.def) {
            let pty = self.tcx.intern(TyKind::Adt(info.def, Vec::new()));
            self.mismatch(span, ty, pty);
            return self.check_sub_pats_alone(fields);
        }
        // The fields a pattern can name: the case's own (by position or
        // name), then the shared ones (by name). A record's are all its fields.
        let mut slots: Vec<(Option<String>, Key, Ty, bool)> = Vec::new();
        let case_info = case.and_then(|c| info.case_by_def(c)).cloned();
        match &case_info {
            Some(c) => {
                for f in c.fields.iter().flatten() {
                    slots.push((f.name.clone(), f.key.clone(), f.ty, true));
                }
                for f in &info.fields {
                    slots.push((Some(f.name.clone()), f.key.clone(), f.ty, false));
                }
            }
            None => {
                for f in &info.fields {
                    slots.push((Some(f.name.clone()), f.key.clone(), f.ty, true));
                }
            }
        }
        let what = match &case_info {
            Some(c) => format!(".{}", c.name),
            None => info.name.clone(),
        };
        let Some(fields) = fields else {
            let has_payload = case_info.as_ref().is_some_and(|c| c.fields.is_some());
            if has_payload {
                self.diags.push(
                    Diagnostic::error(codes::T0104, format!("`{what}` has fields"))
                        .primary(span, "match them, or ignore them with `..`")
                        .help(format!("`{what}(..)`")),
                );
            }
            self.b.case_pats.insert(pat.id, crate::typeck::CasePat { warp: info.def, case, keys: Vec::new() });
            return;
        };
        let mut keys = Vec::with_capacity(fields.len());
        let mut used = vec![false; slots.len()];
        let mut next = 0;
        for f in fields {
            let slot = match &f.label {
                None => {
                    let slot = next;
                    next += 1;
                    if slot >= slots.len() || !slots[slot].3 {
                        self.error(
                            codes::T0104,
                            f.span,
                            format!("`{what}` doesn't have this many fields"),
                            "one too many",
                        );
                        self.check_pat(&f.pat, self.tcx.error);
                        keys.push(Key::Pos(0));
                        continue;
                    }
                    slot
                }
                Some(label) => match slots.iter().position(|s| s.0.as_deref() == Some(label.name.as_str())) {
                    Some(slot) => slot,
                    None => {
                        self.error(
                            codes::T0105,
                            label.span,
                            format!("`{what}` has no field `{}`", label.name),
                            "unknown field",
                        );
                        self.check_pat(&f.pat, self.tcx.error);
                        keys.push(Key::Pos(0));
                        continue;
                    }
                },
            };
            used[slot] = true;
            keys.push(slots[slot].1.clone());
            let fty = slots[slot].2;
            self.check_pat(&f.pat, fty);
        }
        let missing = slots.iter().zip(&used).filter(|(s, used)| s.3 && !**used).count();
        if missing > 0 && !rest {
            self.diags.push(
                Diagnostic::error(codes::T0104, format!("this pattern leaves out {missing} of `{what}`'s fields"))
                    .primary(span, "not every field is matched")
                    .help("match them all, or end with `..` to ignore the rest"),
            );
        }
        self.b.case_pats.insert(pat.id, crate::typeck::CasePat { warp: info.def, case, keys });
    }

    fn check_sub_pats_alone(&mut self, fields: Option<&[FieldPat]>) {
        for f in fields.into_iter().flatten() {
            self.check_pat(&f.pat, self.tcx.error);
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
        if !self.irrefutable(pat) {
            self.diags.push(
                Diagnostic::error(codes::P0102, format!("this pattern might not match, so it can't be used in {what}"))
                    .primary(pat.span, "might not match")
                    .help("use `match` or `if … is …` to handle the values it doesn't cover"),
            );
        }
    }

    /// Whether a `match` covers every value. An arm that always matches
    /// covers everything; otherwise a `bool` needs `true` and `false`, and a
    /// warp with cases needs each case (with patterns that match anything
    /// inside it). Integer ranges and twill bit patterns aren't counted yet,
    /// so those need a catch-all arm.
    pub(super) fn check_exhaustive(&mut self, arms: &[Arm], ty: Ty, span: Span) {
        // An arm this checker can't handle yet was already reported, and
        // judging coverage without it would only add noise.
        if self.tcx.is_error(ty) || arms.iter().any(|a| !supported(&a.pat)) {
            return;
        }
        let unguarded: Vec<&Arm> = arms.iter().filter(|a| a.guard.is_none()).collect();
        if unguarded.iter().any(|a| self.irrefutable(&a.pat)) {
            return;
        }
        let ty = self.resolve(ty);
        if matches!(self.tcx.kind(ty), TyKind::Bool) {
            let (mut t, mut f) = (false, false);
            for arm in &unguarded {
                bool_cover(&arm.pat, &mut t, &mut f);
            }
            if t && f {
                return;
            }
        }
        // `some(x)` and `none`; `ok(x)` and `err(e)`.
        let (mut first, mut second) = (false, false);
        for arm in &unguarded {
            self.wrapper_cover(&arm.pat, &mut first, &mut second);
        }
        if first && second {
            return;
        }
        let shown = self.show(ty);
        // A union is covered by a type test for each of its types.
        if let TyKind::Union(members) = self.tcx.kind(ty).clone() {
            let mut tested = Vec::new();
            for arm in &unguarded {
                self.types_tested(&arm.pat, &mut tested);
            }
            let missing: Vec<String> =
                members.iter().filter(|m| !tested.contains(m)).map(|&m| format!("`{}`", self.show(m))).collect();
            if !missing.is_empty() {
                self.diags.push(
                    Diagnostic::error(codes::P0101, format!("this `match` doesn't cover every `{shown}`"))
                        .primary(span, format!("not handled: {}", missing.join(", ")))
                        .help("add an arm like `x: Type => …` for each, or `_ => …` for everything else"),
                );
            }
            return;
        }
        match self.tcx.kind(ty) {
            TyKind::Optional(_) | TyKind::Result(..) => {
                let (a, b) = if matches!(self.tcx.kind(ty), TyKind::Optional(_)) {
                    ("`some(_)`", "`none`")
                } else {
                    ("`ok(_)`", "`err(_)`")
                };
                let missing: Vec<&str> =
                    [(first, a), (second, b)].iter().filter(|(c, _)| !c).map(|(_, n)| *n).collect();
                self.diags.push(
                    Diagnostic::error(codes::P0101, format!("this `match` doesn't cover every `{shown}`"))
                        .primary(span, format!("not handled: {}", missing.join(", ")))
                        .help("add an arm for each, or `_ => …` for everything else"),
                );
                return;
            }
            _ => {}
        }
        if let Some(info) = self.adt_info(ty).filter(|i| i.is_sum()) {
            let mut covered = Vec::new();
            for arm in &unguarded {
                self.cases_covered(&arm.pat, &mut covered);
            }
            let missing: Vec<String> =
                info.cases.iter().filter(|c| !covered.contains(&c.def)).map(|c| format!("`.{}`", c.name)).collect();
            if missing.is_empty() {
                return;
            }
            self.diags.push(
                Diagnostic::error(codes::P0101, format!("this `match` doesn't cover every `{shown}`"))
                    .primary(span, format!("not handled: {}", missing.join(", ")))
                    .help("add an arm for each, or `_ => …` for everything else"),
            );
            return;
        }
        self.diags.push(
            Diagnostic::error(codes::P0101, format!("this `match` doesn't cover every `{shown}`"))
                .primary(span, "some values aren't handled")
                .help("add an arm like `_ => …` for everything else"),
        );
    }

    /// The types a pattern's type tests cover.
    fn types_tested(&self, pat: &Pat, out: &mut Vec<crate::ty::Ty>) {
        match &pat.kind {
            PatKind::TypeTest { .. } => out.extend(self.b.type_tests.get(&pat.id)),
            PatKind::Or(alts) => alts.iter().for_each(|a| self.types_tested(a, out)),
            _ => {}
        }
    }

    /// Whether a pattern covers every `some`/`ok` (`first`) or the
    /// `none`/every `err` (`second`).
    fn wrapper_cover(&self, pat: &Pat, first: &mut bool, second: &mut bool) {
        match &pat.kind {
            PatKind::Some(p) | PatKind::Ok(p) if self.irrefutable(p) => *first = true,
            PatKind::Err(p) if self.irrefutable(p) => *second = true,
            PatKind::None => *second = true,
            PatKind::Or(alts) => alts.iter().for_each(|a| self.wrapper_cover(a, first, second)),
            _ => {}
        }
    }

    /// The cases a pattern matches completely.
    fn cases_covered(&self, pat: &Pat, out: &mut Vec<crate::hir::DefId>) {
        match &pat.kind {
            PatKind::Case { fields, .. } => {
                let whole = fields.iter().flatten().all(|f| self.irrefutable(&f.pat));
                if let (Some(case_pat), true) = (self.b.case_pats.get(&pat.id), whole)
                    && let Some(case) = case_pat.case
                {
                    out.push(case);
                }
            }
            PatKind::Or(alts) => alts.iter().for_each(|a| self.cases_covered(a, out)),
            _ => {}
        }
    }

    /// Whether `pat` matches every value of its type.
    pub(super) fn irrefutable(&self, pat: &Pat) -> bool {
        match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Error => true,
            PatKind::Tuple(pats) => pats.iter().all(|p| self.irrefutable(p)),
            PatKind::Or(alts) => alts.iter().any(|p| self.irrefutable(p)),
            // A record destructure, or a sum with a single case.
            PatKind::Case { fields, .. } => {
                let Some(case_pat) = self.b.case_pats.get(&pat.id) else { return false };
                let only_case = match case_pat.case {
                    None => true,
                    Some(_) => self.r.warps.get(&case_pat.warp).is_some_and(|w| w.cases.len() == 1),
                };
                only_case && fields.iter().flatten().all(|f| self.irrefutable(&f.pat))
            }
            // `let 0bc_ssss_ssss = …`: no fixed digits.
            PatKind::Twill { pattern, .. } => pattern.mask == 0,
            _ => false,
        }
    }
}

/// Whether this checker understands every part of `pat`.
fn supported(pat: &Pat) -> bool {
    match &pat.kind {
        PatKind::Wild
        | PatKind::Bind(_)
        | PatKind::Lit { .. }
        | PatKind::Range { .. }
        | PatKind::Twill { .. }
        | PatKind::TypeTest { .. }
        | PatKind::Error => true,
        PatKind::Tuple(pats) | PatKind::Or(pats) => pats.iter().all(supported),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => supported(p),
        PatKind::None => true,
        PatKind::Case { fields, .. } => fields.iter().flatten().all(|f| supported(&f.pat)),
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
