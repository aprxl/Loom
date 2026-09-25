//! Exhaustiveness: whether a `match`'s arms cover every value, by pattern
//! usefulness (Maranget, "Warnings for pattern matching", 2007).
//!
//! Each arm's pattern is taken apart into a constructor (`true`, `some`, a
//! case, a tuple, an integer range…) and the patterns of its fields. The
//! match is exhaustive when a wildcard after every unguarded arm matches
//! nothing new; when it would, the values it matches are the example the
//! error shows (`not handled: (false, _)`).
//!
//! Integers and chars are split into ranges at every bound the patterns
//! mention, so `0..=127` and `128..=255` cover a `u8`. A twill pattern is the
//! ranges it stands for when there are few enough of them (`0x1nnn` is one
//! range, `0x8xy4` 256 single values); a wider one, a string or float
//! literal, or a constant covers nothing for certain, so a match that relies
//! on those needs a `_` arm.

use loom_syntax::Diagnostic;
use loom_syntax::ast::Lit;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::Checker;
use super::warp::{Key, WarpInfo};
use crate::hir::{Arm, CasePath, Pat, PatKind};
use crate::ty::{Ty, TyKind};

/// More single ranges than this in one twill pattern and it's opaque.
const MAX_TWILL_RANGES: u32 = 256;

#[derive(Clone, Debug, PartialEq)]
enum Ctor {
    Bool(bool),
    Some,
    None,
    Ok,
    Err,
    /// A warp's case, by its index in the warp's cases; `None` for a record.
    Case(Option<usize>),
    Tuple(usize),
    /// A type test on a union's member.
    Type(Ty),
    /// The integers (or chars) from `lo` to `hi`.
    Range(i128, i128),
}

#[derive(Clone, Debug)]
enum DPat {
    Wild,
    Ctor(Ctor, Vec<DPat>),
    Or(Vec<DPat>),
    /// Matches values this checker can't enumerate: it covers nothing for
    /// certain.
    Opaque,
}

impl Checker<'_> {
    /// Reports a `match` over `ty` that some value gets through.
    pub(super) fn check_exhaustive(&mut self, arms: &[Arm], ty: Ty, span: Span) {
        let ty = self.deep_resolve(ty);
        // An arm this checker can't handle yet was already reported, and
        // judging coverage without it would only add noise.
        if self.tcx.has_error(ty) || arms.iter().any(|a| !supported(&a.pat)) {
            return;
        }
        let rows: Vec<Vec<DPat>> =
            arms.iter().filter(|a| a.guard.is_none()).map(|a| vec![self.dpat(&a.pat, ty)]).collect();
        let Some(witness) = self.useful(rows, &[ty]) else { return };
        let shown = self.show(ty);
        let example = witness.into_iter().next().unwrap_or_else(|| "_".into());
        let help = if example == "_" {
            "add an arm like `_ => …` for everything else".to_string()
        } else {
            format!("add an arm for `{example}`, or `_ => …` for everything else")
        };
        self.diags.push(
            Diagnostic::error(codes::P0101, format!("this `match` doesn't cover every `{shown}`"))
                .primary(span, format!("not handled: `{example}`"))
                .help(help),
        );
    }

    /// If a row of wildcards is useful after `rows` (some values of `tys`
    /// get past every row), an example of those values, one per column.
    fn useful(&mut self, rows: Vec<Vec<DPat>>, tys: &[Ty]) -> Option<Vec<String>> {
        let Some((&ty, rest_tys)) = tys.split_first() else {
            return rows.is_empty().then(Vec::new);
        };
        let rows = expand_or(rows);
        // The constructors of the first column's type, if it has finitely
        // many; for integers, the ranges its patterns split it into.
        let ctors = match self.domain(ty) {
            Some((lo, hi)) => Some(split(lo, hi, rows.iter().filter_map(|r| head_range(&r[0])))),
            None => self.all_ctors(ty),
        };
        if let Some(ctors) = ctors {
            let missing = ctors.iter().find(|c| !rows.iter().any(|r| covers(&r[0], c))).cloned();
            if missing.is_none() {
                // Every constructor appears: some values get through iff they
                // do under one of them.
                for c in ctors {
                    let fields = self.ctor_fields(&c, ty);
                    let specialized: Vec<Vec<DPat>> =
                        rows.iter().filter_map(|r| specialize(r, &c, fields.len())).collect();
                    let mut sub_tys = fields.clone();
                    sub_tys.extend_from_slice(rest_tys);
                    if let Some(w) = self.useful(specialized, &sub_tys) {
                        let (inner, rest) = w.split_at(fields.len());
                        let mut out = vec![self.show_ctor(&c, ty, inner)];
                        out.extend_from_slice(rest);
                        return Some(out);
                    }
                }
                return None;
            }
            // A constructor no row names: only the rows that match anything
            // there can cover it.
            let defaults: Vec<Vec<DPat>> =
                rows.iter().filter(|r| matches!(r[0], DPat::Wild)).map(|r| r[1..].to_vec()).collect();
            let w = self.useful(defaults, rest_tys)?;
            let c = missing.expect("checked above");
            let blanks = vec!["_".to_string(); self.ctor_fields(&c, ty).len()];
            let mut out = vec![self.show_ctor(&c, ty, &blanks)];
            out.extend(w);
            return Some(out);
        }
        // Too many values to list: only wildcards cover them.
        let defaults: Vec<Vec<DPat>> =
            rows.iter().filter(|r| matches!(r[0], DPat::Wild)).map(|r| r[1..].to_vec()).collect();
        let w = self.useful(defaults, rest_tys)?;
        let mut out = vec!["_".to_string()];
        out.extend(w);
        Some(out)
    }

    /// Takes a pattern apart for checking against a value of `ty`.
    fn dpat(&mut self, pat: &Pat, ty: Ty) -> DPat {
        let ty = self.deep_resolve(ty);
        match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Error => DPat::Wild,
            PatKind::Lit { lit, negative } => match lit {
                Lit::Bool(b) => DPat::Ctor(Ctor::Bool(*b), Vec::new()),
                Lit::None => DPat::Ctor(Ctor::None, Vec::new()),
                _ => match lit_value(lit, *negative) {
                    Some(v) => DPat::Ctor(Ctor::Range(v, v), Vec::new()),
                    None => DPat::Opaque,
                },
            },
            PatKind::Range { lo, hi, inclusive } => {
                let Some((min, max)) = self.domain(ty) else { return DPat::Opaque };
                let bound = |p: &Option<Box<Pat>>| match p.as_deref().map(|p| &p.kind) {
                    None => Some(None),
                    Some(PatKind::Lit { lit, negative }) => lit_value(lit, *negative).map(Some),
                    Some(_) => None,
                };
                let (Some(lo), Some(hi)) = (bound(lo), bound(hi)) else { return DPat::Opaque };
                let lo = lo.unwrap_or(min);
                let hi = match hi {
                    Some(h) if *inclusive => h,
                    Some(h) => h - 1,
                    None => max,
                };
                let (lo, hi) = (lo.max(min), hi.min(max));
                if lo > hi {
                    return DPat::Opaque;
                }
                DPat::Ctor(Ctor::Range(lo, hi), Vec::new())
            }
            PatKind::Tuple(pats) => {
                let items = match self.tcx.kind(ty).clone() {
                    TyKind::Tuple(items) if items.len() == pats.len() => items,
                    TyKind::Unit if pats.is_empty() => Vec::new(),
                    _ => return DPat::Wild,
                };
                let subs = pats.iter().zip(items).map(|(p, t)| self.dpat(p, t)).collect();
                DPat::Ctor(Ctor::Tuple(pats.len()), subs)
            }
            PatKind::Some(p) => match *self.tcx.kind(ty) {
                TyKind::Optional(inner) => DPat::Ctor(Ctor::Some, vec![self.dpat(p, inner)]),
                _ => DPat::Wild,
            },
            PatKind::None => DPat::Ctor(Ctor::None, Vec::new()),
            PatKind::Ok(p) => match *self.tcx.kind(ty) {
                TyKind::Result(ok, _) => DPat::Ctor(Ctor::Ok, vec![self.dpat(p, ok)]),
                _ => DPat::Wild,
            },
            PatKind::Err(p) => match *self.tcx.kind(ty) {
                TyKind::Result(_, err) => DPat::Ctor(Ctor::Err, vec![self.dpat(p, err)]),
                _ => DPat::Wild,
            },
            PatKind::TypeTest { .. } => match self.b.type_tests.get(&pat.id) {
                Some(&t) => DPat::Ctor(Ctor::Type(self.deep_resolve(t)), Vec::new()),
                None => DPat::Opaque,
            },
            PatKind::Or(alts) => DPat::Or(alts.iter().map(|a| self.dpat(a, ty)).collect()),
            PatKind::Case { path: CasePath::Const(_), .. } => DPat::Opaque,
            PatKind::Case { fields, .. } => {
                let Some(case_pat) = self.b.case_pats.get(&pat.id).cloned() else { return DPat::Opaque };
                let Some(info) = self.adt_info(ty) else { return DPat::Opaque };
                let index = match case_pat.case {
                    Some(case) => match info.cases.iter().position(|c| c.def == case) {
                        Some(i) => Some(i),
                        None => return DPat::Opaque,
                    },
                    None => None,
                };
                let columns = columns(&info, index);
                let field_pats: Vec<&Pat> = fields.iter().flatten().map(|f| &f.pat).collect();
                let subs = columns
                    .iter()
                    .map(|(key, t)| match case_pat.keys.iter().position(|k| k == key) {
                        Some(i) => field_pats.get(i).map_or(DPat::Wild, |p| self.dpat(p, *t)),
                        None => DPat::Wild,
                    })
                    .collect();
                DPat::Ctor(Ctor::Case(index), subs)
            }
            PatKind::Twill { pattern, .. } => {
                if pattern.mask == 0 {
                    return DPat::Wild;
                }
                // Twill digits are unsigned bits.
                if self.domain(ty).is_none_or(|(lo, _)| lo < 0) {
                    return DPat::Opaque;
                }
                match twill_ranges(pattern.mask, pattern.value, pattern.width) {
                    Some(ranges) => DPat::Or(
                        ranges.into_iter().map(|(lo, hi)| DPat::Ctor(Ctor::Range(lo, hi), Vec::new())).collect(),
                    ),
                    None => DPat::Opaque,
                }
            }
            PatKind::Slice(_) | PatKind::Rest(_) | PatKind::ConstCase(_) => DPat::Opaque,
        }
    }

    /// The values of an integer or char type, as a range.
    fn domain(&self, ty: Ty) -> Option<(i128, i128)> {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Int(int) => Some((int.min(), int.max())),
            // A range gauge's values are its range.
            TyKind::Gauge(def, base) => match self.r.gauges.get(def).and_then(|g| g.range) {
                Some(super::gauge::GaugeRange::Int { lo, hi }) => Some((lo, hi)),
                _ => self.domain(*base),
            },
            TyKind::Char => Some((0, 0x10_FFFF)),
            _ => None,
        }
    }

    /// Every constructor of `ty`, when there are finitely many.
    fn all_ctors(&mut self, ty: Ty) -> Option<Vec<Ctor>> {
        match self.tcx.kind(ty).clone() {
            TyKind::Bool => Some(vec![Ctor::Bool(true), Ctor::Bool(false)]),
            TyKind::Optional(_) => Some(vec![Ctor::Some, Ctor::None]),
            TyKind::Result(..) => Some(vec![Ctor::Ok, Ctor::Err]),
            TyKind::Tuple(items) => Some(vec![Ctor::Tuple(items.len())]),
            TyKind::Unit => Some(vec![Ctor::Tuple(0)]),
            TyKind::Never => Some(Vec::new()),
            TyKind::Union(members) => Some(members.into_iter().map(Ctor::Type).collect()),
            TyKind::Adt(..) => {
                let info = self.adt_info(ty)?;
                if info.is_sum() {
                    Some((0..info.cases.len()).map(|i| Ctor::Case(Some(i))).collect())
                } else {
                    Some(vec![Ctor::Case(None)])
                }
            }
            _ => None,
        }
    }

    /// The types of constructor `c`'s fields, for a value of `ty`.
    fn ctor_fields(&mut self, c: &Ctor, ty: Ty) -> Vec<Ty> {
        match (c, self.tcx.kind(ty).clone()) {
            (Ctor::Some, TyKind::Optional(inner)) => vec![inner],
            (Ctor::Ok, TyKind::Result(ok, _)) => vec![ok],
            (Ctor::Err, TyKind::Result(_, err)) => vec![err],
            (Ctor::Tuple(_), TyKind::Tuple(items)) => items,
            (Ctor::Case(index), _) => match self.adt_info(ty) {
                Some(info) => columns(&info, *index).into_iter().map(|(_, t)| t).collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// A constructor as pattern text, with its fields' texts.
    fn show_ctor(&mut self, c: &Ctor, ty: Ty, fields: &[String]) -> String {
        let all_blank = fields.iter().all(|f| f == "_");
        match c {
            Ctor::Bool(b) => b.to_string(),
            Ctor::Some => format!("some({})", fields.first().map_or("_", |s| s.as_str())),
            Ctor::None => "none".into(),
            Ctor::Ok => format!("ok({})", fields.first().map_or("_", |s| s.as_str())),
            Ctor::Err => format!("err({})", fields.first().map_or("_", |s| s.as_str())),
            Ctor::Tuple(_) => format!("({})", fields.join(", ")),
            Ctor::Type(t) => format!("x: {}", self.show(*t)),
            // A column no pattern narrowed: any value.
            Ctor::Range(lo, hi) if self.domain(ty) == Some((*lo, *hi)) => "_".into(),
            Ctor::Range(lo, _) => match self.tcx.kind(ty) {
                TyKind::Char => char::from_u32(*lo as u32).map_or(lo.to_string(), |ch| format!("{ch:?}")),
                _ => lo.to_string(),
            },
            Ctor::Case(index) => {
                let Some(info) = self.adt_info(ty) else { return "_".into() };
                let name = match index {
                    Some(i) => format!(".{}", info.cases[*i].name),
                    None => info.name.clone(),
                };
                if all_blank {
                    if index.is_none() && !fields.is_empty() { format!("{name}(..)") } else { name }
                } else {
                    format!("{name}({})", fields.join(", "))
                }
            }
        }
    }
}

/// The fields a case pattern (or a record's, for `None`) lists, in order:
/// the case's own, then the fields every case shares.
fn columns(info: &WarpInfo, index: Option<usize>) -> Vec<(Key, Ty)> {
    let mut out: Vec<(Key, Ty)> = match index {
        Some(i) => info.cases[i].fields.iter().flatten().map(|f| (f.key.clone(), f.ty)).collect(),
        None => Vec::new(),
    };
    out.extend(info.fields.iter().map(|f| (f.key.clone(), f.ty)));
    out
}

/// A literal's value as an integer or a char's code point.
fn lit_value(lit: &Lit, negative: bool) -> Option<i128> {
    match lit {
        Lit::Int(int) => {
            let v = i128::try_from(int.value).ok()?;
            Some(if negative { -v } else { v })
        }
        Lit::Char(c) => Some(*c as i128),
        _ => None,
    }
}

/// The ranges of values a twill pattern matches, if there are few enough.
fn twill_ranges(mask: u128, value: u128, width: u32) -> Option<Vec<(i128, i128)>> {
    if width == 0 || width > 64 {
        return None;
    }
    // The free bits below the lowest fixed bit make each run; the free bits
    // above it pick the run.
    let low = mask.trailing_zeros().min(width);
    let free_above = (0..width).filter(|&b| b >= low && mask & (1 << b) == 0).count() as u32;
    if free_above >= 32 || (1u32 << free_above) > MAX_TWILL_RANGES {
        return None;
    }
    let above: Vec<u32> = (low..width).filter(|&b| mask & (1 << b) == 0).collect();
    let run = 1u128 << low;
    let mut out = Vec::with_capacity(1 << free_above);
    for pick in 0..(1u128 << free_above) {
        let mut start = value & !(run - 1);
        for (i, &b) in above.iter().enumerate() {
            if pick & (1 << i) != 0 {
                start |= 1 << b;
            }
        }
        out.push((start as i128, (start + run - 1) as i128));
    }
    out.sort();
    Some(out)
}

/// Rows with an or-pattern first become one row per alternative.
fn expand_or(rows: Vec<Vec<DPat>>) -> Vec<Vec<DPat>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row.first() {
            Some(DPat::Or(alts)) => {
                let alts = alts.clone();
                let expanded: Vec<Vec<DPat>> = alts
                    .into_iter()
                    .map(|a| {
                        let mut r = row.clone();
                        r[0] = a;
                        r
                    })
                    .collect();
                out.extend(expand_or(expanded));
            }
            _ => out.push(row),
        }
    }
    out
}

fn head_range(p: &DPat) -> Option<(i128, i128)> {
    match p {
        DPat::Ctor(Ctor::Range(lo, hi), _) => Some((*lo, *hi)),
        _ => None,
    }
}

/// `lo..=hi` cut at every bound of `ranges`, so each piece is either inside
/// a range or outside it.
fn split(lo: i128, hi: i128, ranges: impl Iterator<Item = (i128, i128)>) -> Vec<Ctor> {
    let mut points = vec![lo];
    for (a, b) in ranges {
        if a > lo && a <= hi {
            points.push(a);
        }
        if b < hi && b >= lo {
            points.push(b + 1);
        }
    }
    points.sort();
    points.dedup();
    let mut out = Vec::with_capacity(points.len());
    for (i, &start) in points.iter().enumerate() {
        let end = points.get(i + 1).map_or(hi, |&next| next - 1);
        out.push(Ctor::Range(start, end));
    }
    out
}

/// Whether a row's first pattern matches every value `c` makes.
fn covers(p: &DPat, c: &Ctor) -> bool {
    match (p, c) {
        (DPat::Wild, _) => true,
        (DPat::Ctor(Ctor::Range(a, b), _), Ctor::Range(x, y)) => a <= x && y <= b,
        (DPat::Ctor(have, _), want) => have == want,
        _ => false,
    }
}

/// The row with its first pattern replaced by that pattern's fields, if it
/// matches constructor `c` (with `arity` fields).
fn specialize(row: &[DPat], c: &Ctor, arity: usize) -> Option<Vec<DPat>> {
    let head: Vec<DPat> = match &row[0] {
        DPat::Wild => vec![DPat::Wild; arity],
        p @ DPat::Ctor(_, subs) if covers(p, c) => {
            if subs.len() == arity {
                subs.clone()
            } else {
                vec![DPat::Wild; arity]
            }
        }
        _ => return None,
    };
    let mut out = head;
    out.extend_from_slice(&row[1..]);
    Some(out)
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
        | PatKind::None
        | PatKind::Error => true,
        PatKind::Tuple(pats) | PatKind::Or(pats) => pats.iter().all(supported),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => supported(p),
        PatKind::Case { fields, .. } => fields.iter().flatten().all(|f| supported(&f.pat)),
        PatKind::Slice(_) | PatKind::Rest(_) | PatKind::ConstCase(_) => false,
    }
}
