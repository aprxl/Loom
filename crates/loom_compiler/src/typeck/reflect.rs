//! Reflection: what compile-time code learns about a type.
//!
//! `T.name`, `T.kind`, `T.fields` and `T.cases` are worked out by the
//! constant folder (see `gauge.rs`), and so is everything read from them:
//! `f.name`, `c.fields`, `.len()`, `.is_empty()`. `const for f in T.fields`
//! goes over them, each item a [`ConstVal::Field`] or [`ConstVal::Case`]
//! standing for its loop variable. Two things use them at run time, with
//! nothing reflective left: `v.(f)`, which is `v`'s field `f`, and
//! `v is .(c)`, which tests `v`'s case. Any other use at run time is K0111
//! (see `lower/expr.rs`).
//!
//! Every expression the folder can work out this way gets its value
//! recorded (`Body::const_values`), so lowering writes it as a literal:
//! `f.put("{fld.name}: ")` puts `"x: "`.

use loom_syntax::Diagnostic;
use loom_syntax::ast::{FieldName, Vis};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::gauge::Val;
use super::{Checker, FieldAccess, Key, Receiver};
use crate::hir::{DefId, Expr, ExprKind, Pat};
use crate::ty::{ConstVal, Ty, TyKind};

/// The reflection queries a type answers: `T.name`.
pub(crate) const QUERIES: &[&str] = &["name", "kind", "fields", "cases", "size", "align", "bits"];

/// What reflection says about one field.
struct FieldFacts {
    name: String,
    key: Key,
    ty: Ty,
    is_pub: bool,
}

impl Checker<'_> {
    /// `std.meta`'s warp `name`, as a type.
    fn meta_ty(&mut self, name: &str) -> Ty {
        match self.std_item("std.meta", name) {
            Some(def) => self.tcx.intern(TyKind::Adt(def, Vec::new())),
            None => self.tcx.error,
        }
    }

    /// Which of `std.meta`'s `Field` and `Case` `ty` is.
    fn meta_name(&self, ty: Ty) -> Option<&'static str> {
        let TyKind::Adt(def, _) = *self.tcx.kind(ty) else { return None };
        ["Field", "Case"].into_iter().find(|name| self.std_item("std.meta", name) == Some(def))
    }

    /// `T.name` where `name` is a reflection query: its type, if it is one.
    pub(super) fn check_reflect(&mut self, name: &str, span: Span) -> Option<Ty> {
        Some(match name {
            "name" => self.tcx.str,
            "kind" => self.meta_ty("Kind"),
            "fields" => {
                let field = self.meta_ty("Field");
                self.list_of(field)
            }
            "cases" => {
                let case = self.meta_ty("Case");
                self.list_of(case)
            }
            "size" | "align" | "bits" => self.unsupported(span, &format!("layout reflection (`T.{name}`)")),
            _ => return None,
        })
    }

    /// `f.name` where `f` is a `Field` or a `Case`: its type, if `base_ty`
    /// is one of those (an unknown member is reported).
    pub(super) fn check_meta_member(&mut self, base_ty: Ty, field: &FieldName, span: Span) -> Option<Ty> {
        let what = self.meta_name(base_ty)?;
        let name = match field {
            FieldName::Named(n) => n.name.as_str(),
            FieldName::Index(..) => "",
        };
        Some(match (what, name) {
            (_, "name") => self.tcx.str,
            ("Field", "is_pub") => self.tcx.bool,
            ("Case", "fields") => {
                let field = self.meta_ty("Field");
                self.list_of(field)
            }
            ("Field", "ty") => self.unsupported(span, "`f.ty`"),
            _ => {
                let has = if what == "Field" { "`name` and `is_pub`" } else { "`name` and `fields`" };
                self.diags.push(
                    Diagnostic::error(codes::T0111, format!("a `{what}` has no `{name}`"))
                        .primary(span, "no such member")
                        .note(format!("a `{what}` has {has}")),
                );
                self.tcx.error
            }
        })
    }

    /// Whether `e` reads reflection: its value may be known while
    /// compiling even though nothing in it is a literal.
    pub(super) fn reflective(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Assoc { name, .. } => QUERIES.contains(&name.name.as_str()),
            ExprKind::Field { base, .. } => self.reflective(base),
            ExprKind::Call { callee, args } if args.is_empty() => match &callee.kind {
                ExprKind::Field { base, .. } => self.reflective(base),
                _ => false,
            },
            ExprKind::Binary { lhs, rhs, .. } => self.reflective(lhs) || self.reflective(rhs),
            ExprKind::Local(_) => self.const_param_of(e).is_some_and(|t| {
                matches!(
                    self.tcx.kind(t),
                    TyKind::Value(ConstVal::Str(_) | ConstVal::Field { .. } | ConstVal::Case { .. })
                )
            }),
            _ => false,
        }
    }

    /// Records the value of `e`, which reads reflection, if the folder can
    /// work it out: lowering writes it as a literal.
    pub(super) fn record_reflected(&mut self, e: &Expr) {
        if let Some(v) = self.fold(e, None).and_then(Val::to_const) {
            self.b.const_values.insert(e.id, v);
        }
    }

    /// `T.name`, while compiling.
    pub(super) fn reflect(&mut self, ty: Ty, name: &str) -> Option<Val> {
        let ty = self.deep_resolve(ty);
        if matches!(self.tcx.kind(ty), TyKind::Param(_) | TyKind::Var(_) | TyKind::Error) {
            return None;
        }
        match name {
            "name" => {
                let text = match *self.tcx.kind(ty) {
                    TyKind::Adt(def, _) => self.p.def(def).name.clone(),
                    _ => self.show(ty),
                };
                Some(Val::Str(self.tcx.sym(&text)))
            }
            "kind" => {
                let kind = self.kind_name(ty);
                let def = self.std_item("std.meta", "Kind")?;
                let info = self.warp_info(def)?;
                info.cases.iter().find(|c| c.name == kind).map(|c| Val::Case(c.def))
            }
            "fields" => self.adt_info(ty).map(|_| Val::Fields { owner: ty, case: None }),
            "cases" => self.adt_info(ty).filter(|info| info.is_sum()).map(|_| Val::Cases(ty)),
            _ => None,
        }
    }

    /// The name of `ty`'s case of `std.meta.Kind`.
    fn kind_name(&mut self, ty: Ty) -> &'static str {
        match self.tcx.kind(self.tcx.degauge(ty)).clone() {
            TyKind::Unit => "Unit",
            TyKind::Tuple(_) => "Tuple",
            TyKind::Bool => "Bool",
            TyKind::Int(_) => "Int",
            TyKind::Float(_) => "Float",
            TyKind::Char => "Char",
            TyKind::Str => "Str",
            TyKind::Array(..) => "Array",
            TyKind::Optional(_) => "Optional",
            TyKind::Result(..) => "Result",
            TyKind::Fn(..) => "Fn",
            TyKind::Any(_) => "Any",
            TyKind::Union(_) => "Union",
            TyKind::Adt(def, _) => match self.p.def_path(def).as_str() {
                "std.string.String" => "Str",
                "std.list.List" => "List",
                "std.slice.Slice" => "Slice",
                "std.map.Map" => "Map",
                "std.set.Set" => "Set",
                _ => match self.adt_info(ty) {
                    Some(info) if info.is_sum() => "Sum",
                    Some(_) => "Record",
                    None => "Other",
                },
            },
            _ => "Other",
        }
    }

    /// Field `index` of `owner` (of case `case`'s payload).
    fn field_facts(&mut self, owner: Ty, case: Option<DefId>, index: u32) -> Option<FieldFacts> {
        let info = self.adt_info(owner)?;
        let i = index as usize;
        match case {
            None => info.fields.get(i).map(|f| FieldFacts {
                name: f.name.clone(),
                key: f.key.clone(),
                ty: f.ty,
                is_pub: f.vis != Vis::Private,
            }),
            Some(c) => info.case_by_def(c)?.fields.as_ref()?.get(i).map(|f| FieldFacts {
                name: f.name.clone().unwrap_or_else(|| i.to_string()),
                key: f.key.clone(),
                ty: f.ty,
                is_pub: true,
            }),
        }
    }

    /// The items of `T.fields`, `c.fields` or `T.cases`.
    pub(super) fn reflected_items(&mut self, list: Val) -> Option<Vec<Val>> {
        match list {
            Val::Fields { owner, case } => {
                let info = self.adt_info(owner)?;
                let n = match case {
                    None => info.fields.len(),
                    Some(c) => info.case_by_def(c)?.fields.as_ref().map_or(0, Vec::len),
                };
                Some((0..n as u32).map(|index| Val::Field { owner, case, index }).collect())
            }
            Val::Cases(owner) => {
                let info = self.adt_info(owner)?;
                Some(info.cases.iter().map(|c| Val::CaseInfo { owner, case: c.def }).collect())
            }
            _ => None,
        }
    }

    /// `v.name` of a field, a case or a list of them, while compiling.
    pub(super) fn reflected_member(&mut self, v: Val, name: &str) -> Option<Val> {
        match (v, name) {
            (Val::Field { owner, case, index }, "name") => {
                let facts = self.field_facts(owner, case, index)?;
                Some(Val::Str(self.tcx.sym(&facts.name)))
            }
            (Val::Field { owner, case, index }, "is_pub") => {
                Some(Val::Bool(self.field_facts(owner, case, index)?.is_pub))
            }
            (Val::CaseInfo { case, .. }, "name") => Some(Val::Str(self.tcx.sym(&self.p.def(case).name.clone()))),
            (Val::CaseInfo { owner, case }, "fields") => Some(Val::Fields { owner, case: Some(case) }),
            (Val::Fields { .. } | Val::Cases(_), "len") => Some(Val::Int(self.reflected_items(v)?.len() as i128)),
            (Val::Fields { .. } | Val::Cases(_), "is_empty") => Some(Val::Bool(self.reflected_items(v)?.is_empty())),
            _ => None,
        }
    }

    /// `base.(field)`: `base`'s field `field` (a compile-time `Field`), or
    /// `base` as its case `field` (a compile-time `Case`), whose payload's
    /// fields `.(f)` then reads.
    pub(super) fn check_const_field(&mut self, e: &Expr, base: &Expr, field: &Expr) -> Ty {
        let base_ty = self.check_expr(base, None);
        let base_ty = self.deep_resolve(base_ty);
        self.check_expr(field, None);
        if self.tcx.is_error(base_ty) {
            return self.tcx.error;
        }
        let Some(v) = self.fold(field, None) else {
            self.diags.push(
                Diagnostic::error(codes::K0110, "the field in `v.(f)` must be known at compile time")
                    .primary(field.span, "the constant folder can't work this out")
                    .help("use a `Field` from `T.fields` or `c.fields`, or a `Case` from `T.cases`"),
            );
            return self.tcx.error;
        };
        let (owner, case) = match v {
            Val::Field { owner, case, .. } => (owner, case),
            Val::CaseInfo { owner, case } => (owner, Some(case)),
            _ => {
                self.error(codes::T0101, field.span, "`v.(…)` takes a `Field` or a `Case`", "not one");
                return self.tcx.error;
            }
        };
        if self.deep_resolve(owner) != base_ty {
            let (want, got) = (self.show(owner), self.show(base_ty));
            let what = if matches!(v, Val::Field { .. }) { "field" } else { "case" };
            self.diags.push(
                Diagnostic::error(codes::T0101, format!("a {what} of `{want}`, on a `{got}`"))
                    .primary(field.span, format!("from `{want}`"))
                    .secondary(base.span, format!("this is `{got}`")),
            );
            return self.tcx.error;
        }
        match v {
            // `v.(c)`: `v`, known to be case `c` (checked when it runs).
            Val::CaseInfo { case, .. } => {
                self.b.case_views.insert(e.id, case);
                base_ty
            }
            Val::Field { owner, index, .. } => {
                // A payload's field is read through its case: `v.(c).(f)`.
                if let Some(c) = case
                    && self.b.case_views.get(&base.id) != Some(&c)
                {
                    let name = self.p.def(c).name.clone();
                    self.diags.push(
                        Diagnostic::error(
                            codes::T0111,
                            format!("a field of case `.{name}`, read from the whole value"),
                        )
                        .primary(e.span, format!("`.{name}`'s payload"))
                        .help("name the case first: `v.(c).(f)`, inside `if v is .(c)`"),
                    );
                    return self.tcx.error;
                }
                let Some(facts) = self.field_facts(owner, case, index) else { return self.tcx.error };
                self.b.fields.insert(e.id, FieldAccess { key: facts.key, base: Receiver::Expr });
                facts.ty
            }
            _ => self.tcx.error,
        }
    }

    /// `v is .(c)`: a case pattern whose case is known while compiling.
    pub(super) fn check_const_case_pat(&mut self, pat: &Pat, case: &Expr, ty: Ty) {
        self.check_expr(case, None);
        let ty = self.deep_resolve(ty);
        match self.fold(case, None) {
            Some(Val::CaseInfo { owner, case: c }) if self.deep_resolve(owner) == ty => {
                let Some(info) = self.adt_info(ty) else { return };
                self.b.case_pats.insert(pat.id, super::CasePat { warp: info.def, case: Some(c), keys: Vec::new() });
            }
            Some(Val::CaseInfo { owner, .. }) => {
                let (want, got) = (self.show(owner), self.show(ty));
                self.error(codes::T0101, pat.span, format!("a case of `{want}` can't match a `{got}`"), "wrong type");
            }
            Some(_) => self.error(codes::T0101, case.span, "`.(c)` takes a `Case`", "not a `Case`"),
            None => self.diags.push(
                Diagnostic::error(codes::K0110, "the case in `.(c)` must be known at compile time")
                    .primary(case.span, "the constant folder can't work this out"),
            ),
        }
    }
}
