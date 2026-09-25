//! Warps: their layout, construction, and what a member name means.
//!
//! A warp is a *record* (fields only), a *sum* (cases, and possibly fields
//! shared by every case), or a *tuple warp* (`warp Meters(f64)`). Its
//! [`WarpInfo`] is computed once and shared with lowering, so both agree on
//! each field's [`Key`] and each case's tag.

use loom_syntax::Diagnostic;
use loom_syntax::ast::Vis;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{Checker, Construct, FieldSource};
use crate::hir::{self, DefId, DefKind};
use crate::ty::{Ty, TyKind};

/// Where a field lives in a warp's Lua table.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// `t.name`.
    Name(String),
    /// `t[i]` (1-based): tuple warps and unnamed case payloads.
    Pos(u32),
}

#[derive(Clone, Debug)]
pub struct WarpInfo {
    pub def: DefId,
    /// The type this describes: `Pair[i32, str]`. In a generic warp's
    /// template, the arguments are its parameters (`Pair[A, B]`).
    pub ty: Ty,
    pub name: String,
    /// A record's fields, or the fields every case of a sum shares.
    pub fields: Vec<FieldInfo>,
    pub cases: Vec<CaseInfo>,
    pub tuple: bool,
    /// Copied on assignment rather than moved (every field is `Copy`).
    pub copy: bool,
    /// A `once` warp, or one holding a `once` value: its values must be used
    /// up exactly once.
    pub once: bool,
}

#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub def: DefId,
    pub name: String,
    pub key: Key,
    pub ty: Ty,
    pub has_default: bool,
    pub vis: Vis,
    /// A `through` field, whose methods the warp forwards.
    pub through: bool,
}

#[derive(Clone, Debug)]
pub struct CaseInfo {
    pub def: DefId,
    pub name: String,
    /// 1-based, in declaration order.
    pub tag: u32,
    /// `None` for a unit case.
    pub fields: Option<Vec<CaseFieldInfo>>,
}

#[derive(Clone, Debug)]
pub struct CaseFieldInfo {
    pub name: Option<String>,
    pub key: Key,
    pub ty: Ty,
}

impl WarpInfo {
    /// The types of every field, shared and per case.
    pub fn field_tys(&self) -> Vec<Ty> {
        self.fields
            .iter()
            .map(|f| f.ty)
            .chain(self.cases.iter().flat_map(|c| c.fields.iter().flatten().map(|f| f.ty)))
            .collect()
    }

    pub fn is_sum(&self) -> bool {
        !self.cases.is_empty()
    }

    /// A sum with no payloads and no shared fields: represented as a plain
    /// integer (its tag) rather than a table.
    pub fn unit_only(&self) -> bool {
        self.is_sum() && self.fields.is_empty() && self.cases.iter().all(|c| c.fields.is_none())
    }

    pub fn field(&self, name: &str) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn case(&self, name: &str) -> Option<&CaseInfo> {
        self.cases.iter().find(|c| c.name == name)
    }

    pub fn case_by_def(&self, def: DefId) -> Option<&CaseInfo> {
        self.cases.iter().find(|c| c.def == def)
    }
}

/// What `Type.name` or a leading `.name` found among a type's members.
#[derive(Clone, Copy, Debug)]
pub(super) enum Member {
    Case(DefId),
    /// A function without `self`.
    AssocFn(DefId),
    /// A method, called through the type (`Point.len(p)`).
    Method(DefId),
    Const(DefId),
}

impl Checker<'_> {
    /// The layout of warp `def`, computed on first use. A generic warp's
    /// field types mention its parameters; [`Checker::adt_info`] gives the
    /// layout for particular arguments.
    pub(super) fn warp_info(&mut self, def: DefId) -> Option<WarpInfo> {
        if let Some(info) = self.r.warps.get(&def) {
            return Some(info.clone());
        }
        let p = self.p;
        let d = p.def(def);
        let DefKind::Warp(w) = &d.kind else { return None };
        if w.modifier == loom_syntax::ast::WarpModifier::Ref {
            self.unsupported(d.span, "`ref` warps");
            return None;
        }
        let once = w.modifier == loom_syntax::ast::WarpModifier::Once;
        if let Some(&param) = w.generics.iter().find(|&&g| !self.is_type_param(g)) {
            self.unsupported(p.def(param).span, "`const` and region parameters");
            return None;
        }
        let params: Vec<(DefId, Ty)> = w.generics.iter().map(|&g| (g, self.tcx.intern(TyKind::Param(g)))).collect();
        let this = self.tcx.intern(TyKind::Adt(def, params.iter().map(|(_, t)| *t).collect()));
        let saved_self = self.self_ty.replace(this);
        let saved_subst = std::mem::replace(&mut self.subst, params);
        let saved_current = self.current.replace(def);
        let mut fields = Vec::new();
        for (i, &field) in w.fields.iter().enumerate() {
            let fd = p.def(field);
            let DefKind::Field(f) = &fd.kind else { continue };
            let key = if w.tuple { Key::Pos(i as u32 + 1) } else { Key::Name(fd.name.clone()) };
            let ty = self.lower_ty(&f.ty);
            fields.push(FieldInfo {
                def: field,
                name: fd.name.clone(),
                key,
                ty,
                has_default: f.default.is_some(),
                vis: fd.vis,
                through: f.through,
            });
        }
        let mut cases = Vec::new();
        for (i, &case) in w.cases.iter().enumerate() {
            let cd = p.def(case);
            let DefKind::Case(c) = &cd.kind else { continue };
            let payload = c.fields.as_ref().map(|fs| {
                fs.iter()
                    .enumerate()
                    .map(|(j, f)| {
                        let name = f.name.as_ref().map(|n| n.name.clone());
                        let key = match &name {
                            Some(n) => Key::Name(n.clone()),
                            None => Key::Pos(j as u32 + 1),
                        };
                        CaseFieldInfo { name, key, ty: self.lower_ty(&f.ty) }
                    })
                    .collect()
            });
            cases.push(CaseInfo { def: case, name: cd.name.clone(), tag: i as u32 + 1, fields: payload });
        }
        self.self_ty = saved_self;
        self.subst = saved_subst;
        self.current = saved_current;
        let mut info =
            WarpInfo { def, ty: this, name: d.name.clone(), fields, cases, tuple: w.tuple, copy: !once, once };
        // Insert first, so a warp that contains itself doesn't recurse forever.
        self.r.warps.insert(def, info.clone());
        info.once = once || info.field_tys().iter().any(|&t| self.is_once(t));
        info.copy = !info.once && info.field_tys().iter().all(|&t| self.is_copy(t));
        self.r.warps.insert(def, info.clone());
        Some(info)
    }

    /// Whether generic parameter `def` stands for a type (rather than a
    /// `const` value or a region).
    pub(super) fn is_type_param(&self, def: DefId) -> bool {
        matches!(&self.p.def(def).kind, DefKind::GenericParam(g) if matches!(g.kind, hir::GenericParamKind::Type { .. }))
    }

    /// The layout of warp type `ty`, with a generic warp's parameters
    /// replaced by `ty`'s arguments.
    fn instantiate_warp(&mut self, ty: Ty) -> Option<WarpInfo> {
        let TyKind::Adt(def, args) = self.tcx.kind(ty).clone() else { return None };
        if let Some(info) = self.r.adts.get(&ty) {
            return Some(info.clone());
        }
        let template = self.warp_info(def)?;
        if args.is_empty() {
            self.r.adts.insert(ty, template.clone());
            return Some(template);
        }
        let DefKind::Warp(w) = &self.p.def(def).kind else { return None };
        if args.len() != w.generics.len() {
            return None;
        }
        let map: Vec<(DefId, Ty)> = w.generics.iter().copied().zip(args.iter().copied()).collect();
        let mut info = template;
        info.ty = ty;
        for f in &mut info.fields {
            f.ty = self.tcx.subst(f.ty, &map);
        }
        for c in &mut info.cases {
            for f in c.fields.iter_mut().flatten() {
                f.ty = self.tcx.subst(f.ty, &map);
            }
        }
        let concrete = !self.has_vars(ty) && !self.tcx.has_params(ty);
        if concrete {
            // Insert first, so a warp that contains itself doesn't recurse forever.
            self.r.adts.insert(ty, info.clone());
        }
        info.once = info.once || info.field_tys().iter().any(|&t| self.is_once(t));
        info.copy = !info.once && info.field_tys().iter().all(|&t| self.is_copy(t));
        if concrete {
            self.r.adts.insert(ty, info.clone());
        }
        Some(info)
    }

    /// Whether values of `ty` must be used up exactly once: a `once` warp,
    /// or anything holding one (`List[Tx]` must be drained).
    pub(super) fn is_once(&mut self, ty: Ty) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty).clone() {
            TyKind::Tuple(items) => items.iter().any(|&t| self.is_once(t)),
            TyKind::Optional(t) | TyKind::Array(t, _) => self.is_once(t),
            TyKind::Result(t, e) => self.is_once(t) || self.is_once(e),
            TyKind::Adt(_, args) if self.is_builtin_adt(ty) => args.iter().any(|&t| self.is_once(t)),
            TyKind::Adt(..) => self.adt_info(ty).is_some_and(|i| i.once),
            _ => false,
        }
    }

    fn is_builtin_adt(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), TyKind::Adt(def, _) if self.p.def(*def).attrs.iter().any(|a| a.name == "builtin"))
    }

    /// Whether values of `ty` are copied (rather than moved) on assignment.
    pub(super) fn is_copy(&mut self, ty: Ty) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty).clone() {
            TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Char
            | TyKind::Str
            | TyKind::Unit
            | TyKind::Never
            | TyKind::IntVar(_)
            | TyKind::FloatVar(_)
            | TyKind::Range(_)
            | TyKind::Var(_)
            | TyKind::Param(_)
            | TyKind::Gauge(..)
            | TyKind::Error => true,
            TyKind::Tuple(items) => items.iter().all(|&t| self.is_copy(t)),
            TyKind::Optional(t) => self.is_copy(t),
            TyKind::Result(t, e) => self.is_copy(t) && self.is_copy(e),
            TyKind::Adt(def, _) if Some(def) == self.std.string => false,
            // A slice is a view: copying it copies the view.
            TyKind::Adt(def, _) if Some(def) == self.std.slice => true,
            TyKind::Adt(..) => match self.r.adts.get(&ty) {
                Some(info) => info.copy,
                None => self.adt_info(ty).is_some_and(|i| i.copy),
            },
            TyKind::Fn(..) | TyKind::Any(_) | TyKind::Union(_) => false,
            TyKind::Array(t, _) => self.is_copy(t),
        }
    }

    /// The warp a type names, if it's one this checker handles, with its
    /// field types for this type's generic arguments.
    pub(super) fn adt_info(&mut self, ty: Ty) -> Option<WarpInfo> {
        let ty = self.deep_resolve(ty);
        match *self.tcx.kind(ty) {
            TyKind::Adt(def, _) if Some(def) != self.std.string => {
                if self.p.def(def).attrs.iter().any(|a| a.name == "builtin") {
                    return None;
                }
                self.instantiate_warp(ty)
            }
            _ => None,
        }
    }

    /// A member of warp `def` by name: a case, constant, or function.
    pub(super) fn member(&self, def: DefId, name: &str) -> Option<Member> {
        let p = self.p;
        let DefKind::Warp(w) = &p.def(def).kind else { return None };
        if let Some(&case) = w.cases.iter().find(|&&c| p.def(c).name == name) {
            return Some(Member::Case(case));
        }
        let &m = w.members.iter().find(|&&m| p.def(m).name == name)?;
        match &p.def(m).kind {
            DefKind::Fn(f) if f.has_self() => Some(Member::Method(m)),
            DefKind::Fn(_) => Some(Member::AssocFn(m)),
            DefKind::Const(_) => Some(Member::Const(m)),
            _ => None,
        }
    }

    /// Whether code in the definition being checked may see private members
    /// of warp `def`: only from inside its own module.
    pub(super) fn can_see_private(&self, def: DefId) -> bool {
        let here = self.current.map(|c| self.p.def(c).module);
        here == Some(self.p.def(def).module)
    }

    // ------------------------------------------------------------ construction

    /// Checks the arguments of a constructor: `Point(3, y: 4)` (a record or
    /// tuple warp, when `case` is `None`) or `Shape.Circle(2.0, pos: p)`.
    /// `args` is `None` for a bare unit case (`Shape.Dot`).
    pub(super) fn construct(
        &mut self,
        id: hir::HirId,
        info: &WarpInfo,
        case: Option<DefId>,
        args: Option<&[hir::Arg]>,
        span: Span,
    ) -> Ty {
        let ty = info.ty;
        let args = args.unwrap_or(&[]);
        if case.is_none() && info.is_sum() {
            self.diags.push(
                Diagnostic::error(codes::T0106, format!("`{}` has cases: build one of them", info.name))
                    .primary(span, "not a record")
                    .help(format!("for example `{}.{}(…)`", info.name, info.cases[0].name)),
            );
            self.check_stray_args(args);
            return self.tcx.error;
        }
        if case.is_none() && !self.can_see_private(info.def) && info.fields.iter().any(|f| f.vis == Vis::Private) {
            self.diags.push(
                Diagnostic::error(
                    codes::N0103,
                    format!("`{}` has private fields, so only its module can build it", info.name),
                )
                .primary(span, "can't call the memberwise constructor here")
                .help("use one of its associated functions instead"),
            );
        }
        // The slots: the case's own fields first (positional or named), then
        // the shared fields (named only, since positions would be ambiguous).
        struct Slot {
            name: Option<String>,
            key: Key,
            ty: Ty,
            default: Option<DefId>,
            positional: bool,
        }
        let mut slots: Vec<Slot> = Vec::new();
        let case_info = case.and_then(|c| info.case_by_def(c)).cloned();
        if let Some(c) = &case_info {
            for f in c.fields.iter().flatten() {
                slots.push(Slot {
                    name: f.name.clone(),
                    key: f.key.clone(),
                    ty: f.ty,
                    default: None,
                    positional: true,
                });
            }
        }
        for f in &info.fields {
            let default = f.has_default.then_some(f.def);
            slots.push(Slot {
                name: Some(f.name.clone()),
                key: f.key.clone(),
                ty: f.ty,
                default,
                positional: case.is_none(),
            });
        }
        let what = match &case_info {
            Some(c) => format!("{}.{}", info.name, c.name),
            None => info.name.clone(),
        };
        let mut filled: Vec<Option<usize>> = vec![None; slots.len()];
        let mut next = 0;
        for (i, arg) in args.iter().enumerate() {
            if arg.is_given || arg.mutable {
                self.error(codes::T0105, arg.span, "a constructor takes plain values", "not here");
                continue;
            }
            let slot = match &arg.label {
                None => {
                    let slot = next;
                    next += 1;
                    if slot >= slots.len() || !slots[slot].positional {
                        let hint = if slot < slots.len() {
                            format!("shared fields are named: `{}: …`", slots[slot].name.clone().unwrap_or_default())
                        } else {
                            "one too many".to_string()
                        };
                        self.error(codes::T0104, arg.span, format!("`{what}` doesn't take this many values"), hint);
                        self.check_expr(&arg.value, None);
                        continue;
                    }
                    slot
                }
                Some(label) => match slots.iter().position(|s| s.name.as_deref() == Some(label.name.as_str())) {
                    Some(slot) => slot,
                    None => {
                        self.error(
                            codes::T0105,
                            label.span,
                            format!("`{what}` has no field `{}`", label.name),
                            "unknown field",
                        );
                        self.check_expr(&arg.value, None);
                        continue;
                    }
                },
            };
            if filled[slot].is_some() {
                self.error(codes::T0105, arg.span, "this field is given twice", "already given");
                continue;
            }
            filled[slot] = Some(i);
            self.check_coerce(&arg.value, slots[slot].ty);
        }
        let mut fields = Vec::with_capacity(slots.len());
        let mut ok = true;
        for (slot, filled) in slots.iter().zip(&filled) {
            match (filled, slot.default) {
                (Some(i), _) => fields.push((slot.key.clone(), FieldSource::Arg(*i))),
                (None, Some(def)) => fields.push((slot.key.clone(), FieldSource::Default(def))),
                (None, None) => {
                    let name = slot.name.clone().unwrap_or_else(|| format!("{:?}", slot.key));
                    self.error(
                        codes::T0104,
                        span,
                        format!("`{what}` needs a value for `{name}`"),
                        format!("`{name}` is missing"),
                    );
                    ok = false;
                }
            }
        }
        if ok {
            // A generic warp's defaults are checked for its arguments, once
            // they're known.
            let args = match self.tcx.kind(info.ty) {
                TyKind::Adt(_, args) => args.clone(),
                _ => Vec::new(),
            };
            let inst = super::Instance { args, ..super::Instance::of(info.def) };
            let defaults = if inst.args.iter().any(|&t| self.has_vars(t)) {
                self.pending.push(super::generic::Pending {
                    inst,
                    patch: super::generic::Patch::Construct(id),
                    span,
                    bounds: Vec::new(),
                });
                super::generic::UNKNOWN
            } else {
                self.instance(inst)
            };
            self.b.constructs.insert(id, Construct { warp: info.def, defaults, case, fields });
        }
        ty
    }

    fn check_stray_args(&mut self, args: &[hir::Arg]) {
        for arg in args {
            self.check_expr(&arg.value, None);
        }
    }
}
