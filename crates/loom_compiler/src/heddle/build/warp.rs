//! Warps: building values, reading and writing fields, dot-blocks, and
//! case and record patterns (design/vm.md §10.4).
//!
//! A record's fields are at their offsets in its layout; one that lives in
//! registers has its fields' parts one after another, in declaration order.
//! A sum is its shared fields, a tag (the case's position, from 0) and the
//! case's payload, in memory; one whose cases have no payloads is its tag.

use loom_syntax::source::Span;

use super::value::{Place, Val};
use super::{BlockId, Builder};
use crate::heddle::{InstKind, Mem, VTy, Value};
use crate::hir::{self, DefId, ExprKind, PatKind};
use crate::ty::layout::Shape;
use crate::ty::{Ty, TyKind};
use crate::typeck::{Construct, FieldSource, Key, Receiver, WarpInfo};

/// Where a field is in a value: which of its parts (in registers), and at
/// what offset (in memory).
#[derive(Clone, Debug)]
pub(super) struct FieldPos {
    pub ty: Ty,
    /// The field's first part among the value's, when it's in registers.
    pub part: usize,
    pub off: u64,
}

impl<'a> Builder<'a> {
    // ------------------------------------------------------------ where fields are

    /// Where the field `key` of a value of `ty` is (for a sum: a shared
    /// field, or with `case`, a field of that case's payload).
    pub(super) fn field_pos(&mut self, ty: Ty, case: Option<DefId>, key: &Key, span: Span) -> Option<FieldPos> {
        let ty = self.tcx().degauge(ty);
        let (tys, offsets, index): (Vec<Ty>, Vec<u64>, usize) = match self.tcx().kind(ty).clone() {
            TyKind::Tuple(items) => {
                let Key::Pos(i) = key else { return self.internal(span, "a named field of a tuple") };
                (items, self.field_offsets(ty, span)?, *i as usize - 1)
            }
            TyKind::Adt(..) => {
                let Some(info) = self.warp_info(ty) else {
                    return self.internal(span, "a field of a warp with no layout");
                };
                let layout = self.layout(ty)?;
                match (case, &layout.shape) {
                    (None, Shape::Fields(offsets)) => {
                        let i = info.fields.iter().position(|f| &f.key == key)?;
                        (info.fields.iter().map(|f| f.ty).collect(), offsets.clone(), i)
                    }
                    (None, Shape::Sum { shared, .. }) => {
                        let i = info.fields.iter().position(|f| &f.key == key)?;
                        (info.fields.iter().map(|f| f.ty).collect(), shared.clone(), i)
                    }
                    (Some(c), Shape::Sum { cases, .. }) => {
                        let ci = info.cases.iter().position(|x| x.def == c)?;
                        let fields = info.cases[ci].fields.clone().unwrap_or_default();
                        let i = fields.iter().position(|f| &f.key == key)?;
                        (fields.iter().map(|f| f.ty).collect(), cases[ci].clone(), i)
                    }
                    _ => return self.internal(span, "a field of something without fields"),
                }
            }
            _ => return self.internal(span, "a field of something without fields"),
        };
        // The field's first part: the parts of the fields before it (only
        // meaningful for a value in registers).
        let mut part = 0;
        if self.in_regs(ty) {
            for &t in &tys[..index] {
                part += self.repr_of(t, span)?.parts().len();
            }
        }
        Some(FieldPos { ty: self.tcx().degauge(tys[index]), part, off: offsets[index] })
    }

    /// The field at `pos` of `v`.
    pub(super) fn field_of(&mut self, v: &Val, pos: &FieldPos, span: Span) -> Option<Val> {
        match v {
            Val::Regs(vals) => {
                let n = self.repr_of(pos.ty, span)?.parts().len();
                Some(Val::Regs(vals[pos.part..pos.part + n].to_vec()))
            }
            &Val::Mem(ptr) => self.load_val(ptr, pos.off as i64, pos.ty, span),
        }
    }

    /// The field at `pos` of the value in `place`.
    pub(super) fn field_place_of(&mut self, place: &Place, pos: &FieldPos, span: Span) -> Option<Place> {
        Some(match place {
            Place::Vars(vars) => {
                let n = self.repr_of(pos.ty, span)?.parts().len();
                Place::Vars(vars[pos.part..pos.part + n].to_vec())
            }
            Place::At { ptr, off } => Place::At { ptr: *ptr, off: off + pos.off as i64 },
        })
    }

    // ------------------------------------------------------------ fields

    /// What a field access reads from: the receiver's place (or value).
    fn receiver_place(&mut self, base: Receiver, expr: Option<&'a hir::Expr>, span: Span) -> Option<(Place, Ty)> {
        match base {
            Receiver::Expr => {
                let e = expr?;
                self.place_or_temp(e)
            }
            Receiver::Local(l) => {
                let place = self.local_place(l, span)?;
                Some((place, self.local_ty(l)))
            }
            Receiver::Subject(id) => self.f.subjects.get(&id).cloned(),
        }
    }

    /// The place `e` is, or for a value that isn't one, a place holding it.
    pub(super) fn place_or_temp(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        if self.is_place(e) {
            return self.place(e);
        }
        let v = self.expr(e)?;
        let ty = self.coerced_ty(e);
        match v {
            Val::Regs(vals) => {
                // Not a place: variables of its own hold it.
                let tys: Vec<VTy> = self.repr_of(ty, e.span)?.parts().iter().map(|p| p.vty).collect();
                let vars = self.new_vars(&tys);
                self.write_all(&vars, &vals);
                Some((Place::Vars(vars), ty))
            }
            Val::Mem(ptr) => Some((Place::At { ptr, off: 0 }, ty)),
        }
    }

    /// Whether `e` names a place: a local, or a field or element of one.
    pub(super) fn is_place(&self, e: &hir::Expr) -> bool {
        match &e.kind {
            ExprKind::Local(_) => true,
            ExprKind::Field { base, .. } | ExprKind::ConstField { base, .. } => {
                self.tables().fields.contains_key(&e.id) && self.is_place(base)
            }
            ExprKind::LeadingDot(_) => self.tables().fields.contains_key(&e.id),
            ExprKind::Index { .. } => self.is_index_place(e),
            _ => false,
        }
    }

    /// `p.x`, `t.0`, a leading `.x`, or `v.(f)`.
    pub(super) fn field(&mut self, e: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        if let ExprKind::LeadingDot(_) = &e.kind
            && let Some(def) = self.tables().leading_consts.get(&e.id).copied()
        {
            return self.leading_const(def, self.ty_of(e), span);
        }
        // `v.(c)`: `v`, which must be case `c`.
        if let ExprKind::ConstField { base, .. } = &e.kind
            && let Some(case) = self.tables().case_views.get(&e.id).copied()
        {
            let v = self.expr(base)?;
            let ty = self.coerced_ty(base);
            self.check_case(&v, ty, case, span)?;
            return Some(v);
        }
        if !self.tables().fields.contains_key(&e.id) {
            return self.unsupported(span, "methods of warps used as values");
        }
        let (place, pos) = self.field_target(e)?;
        match place {
            FieldBase::Place(p) => {
                let fp = self.field_place_of(&p, &pos, span)?;
                self.read_place(&fp, pos.ty, span)
            }
            FieldBase::Value(v) => self.field_of(&v, &pos, span),
        }
    }

    /// The place `e` (a field access) is.
    pub(super) fn field_place(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        let span = e.span;
        let (base, pos) = self.field_target(e)?;
        match base {
            FieldBase::Place(p) => {
                let fp = self.field_place_of(&p, &pos, span)?;
                Some((fp, pos.ty))
            }
            FieldBase::Value(_) => self.unsupported(span, "assigning to a field of a value"),
        }
    }

    /// The base of a field access, and where the field is in it.
    fn field_target(&mut self, e: &'a hir::Expr) -> Option<(FieldBase, FieldPos)> {
        let span = e.span;
        let access = self.tables().fields.get(&e.id).cloned()?;
        let base_expr = match &e.kind {
            ExprKind::Field { base, .. } | ExprKind::ConstField { base, .. } => Some(base.as_ref()),
            _ => None,
        };
        // A payload's field, read through its case: `v.(c).(f)`.
        let case = base_expr.and_then(|b| self.tables().case_views.get(&b.id).copied());
        let (base, base_ty) = match (access.base, base_expr) {
            (Receiver::Expr, Some(b)) if case.is_some() => {
                let ExprKind::ConstField { base: inner, .. } = &b.kind else { return None };
                let v = self.expr(inner)?;
                let ty = self.coerced_ty(inner);
                self.check_case(&v, ty, case?, span)?;
                (FieldBase::Value(v), ty)
            }
            (Receiver::Expr, Some(b)) if !self.is_place(b) => {
                let v = self.expr(b)?;
                (FieldBase::Value(v), self.coerced_ty(b))
            }
            (base, b) => {
                let (p, ty) = self.receiver_place(base, b, span)?;
                (FieldBase::Place(p), ty)
            }
        };
        let pos = self.field_pos(base_ty, case, &access.key, span)?;
        Some((base, pos))
    }

    /// Unravels unless `v`, a value of sum `ty`, is case `case`.
    fn check_case(&mut self, v: &Val, ty: Ty, case: DefId, span: Span) -> Option<()> {
        let info = self.warp_info(ty)?;
        let (ok, fail) = (self.new_block(), self.new_block());
        self.test_case(v, ty, &info, case, ok, fail, span)?;
        self.enter(fail);
        if self.live() {
            let name = info.case_by_def(case).map(|c| c.name.clone()).unwrap_or_default();
            let (ptr, len) = {
                let text = self.str_lit(format!("the value isn't .{name}").as_bytes());
                (text[0], text[1])
            };
            self.terminate(crate::heddle::Term::Unravel { ptr, len });
        }
        self.enter(ok);
        Some(())
    }

    /// A leading `.MAX`: a constant of the expected type.
    fn leading_const(&mut self, def: DefId, ty: Ty, span: Span) -> Option<Val> {
        let results = &self.typed.results;
        if let Some(&v) = results.untyped_consts.get(&def) {
            return self.int_value(v, ty, span).map(Val::Regs);
        }
        self.unsupported(span, "constants with a type")
    }

    // ------------------------------------------------------------ building values

    /// The tag of `case` of the sum `info` describes: its position.
    pub(super) fn case_tag(info: &WarpInfo, case: DefId) -> u64 {
        info.cases.iter().position(|c| c.def == case).unwrap_or(0) as u64
    }

    /// A new value of a warp: `Point(3, 4)`, `Shape.Circle(r: 2.0)`, `.Dot`.
    pub(super) fn construct(&mut self, e: &'a hir::Expr, c: &Construct, args: &'a [hir::Arg]) -> Option<Val> {
        let span = e.span;
        let ty = self.ty_of(e);
        let info = self.warp_info(ty)?;
        // Arguments run in source order, whatever order the fields take them in.
        let mut vals: Vec<Option<Val>> = Vec::with_capacity(args.len());
        for arg in args {
            vals.push(Some(self.expr(&arg.value)?));
        }
        let mut fields: Vec<(Key, Val, Ty)> = Vec::with_capacity(c.fields.len());
        for (key, source) in &c.fields {
            let (val, fty) = match source {
                FieldSource::Arg(i) => {
                    let fty = self.coerced_ty(&args[*i].value);
                    (vals[*i].take()?, fty)
                }
                FieldSource::Default(field) => {
                    let crate::hir::DefKind::Field(f) = &self.p.def(*field).kind else { return None };
                    let default = f.default.as_ref()?;
                    let saved = std::mem::replace(&mut self.f.body, c.defaults);
                    let v = self.expr(default);
                    let fty = self.coerced_ty(default);
                    self.f.body = saved;
                    (v?, fty)
                }
            };
            fields.push((key.clone(), val, fty));
        }
        match c.case {
            None => {
                // A record: its fields in declaration order.
                let mut ordered: Vec<(Val, Ty)> = Vec::with_capacity(info.fields.len());
                for f in &info.fields {
                    let i = fields.iter().position(|(k, _, _)| k == &f.key)?;
                    let (_, v, _) = fields[i].clone();
                    ordered.push((v, f.ty));
                }
                let (vs, ts): (Vec<Val>, Vec<Ty>) = ordered.into_iter().unzip();
                self.aggregate(ty, &ts, vs, span)
            }
            Some(case) => {
                let tag = Self::case_tag(&info, case);
                let layout = self.layout(ty)?;
                let Shape::Sum { tag: tag_at, .. } = layout.shape else {
                    return self.internal(span, "a case of something that isn't a sum");
                };
                if self.in_regs(ty) {
                    return Some(Val::Regs(vec![self.iconst(tag)]));
                }
                let ptr = self.temp(ty, span)?;
                let t = self.iconst(tag);
                self.store(tag_mem(tag_at.bytes), ptr, tag_at.offset as i64, t);
                for (key, v, _) in fields {
                    let in_case = info.case_by_def(case)?.fields.iter().flatten().any(|f| f.key == key);
                    let pos = self.field_pos(ty, in_case.then_some(case), &key, span)?;
                    self.move_to(ptr, pos.off as i64, v, pos.ty, span)?;
                }
                self.own_temp(ptr, ty);
                Some(Val::Mem(ptr))
            }
        }
    }

    // ------------------------------------------------------------ dot-blocks

    /// `subject.{ … }`: edits the subject in place (and is `()`), or edits a
    /// copy and is that copy.
    pub(super) fn dot_block(&mut self, e: &'a hir::Expr, subject: &'a hir::Expr, body: &'a hir::Block) -> Option<Val> {
        let span = e.span;
        let in_place = self.tables().dot_blocks.get(&e.id).copied().unwrap_or(false);
        let ty = self.coerced_ty(subject);
        let place = if in_place {
            self.place(subject)?.0
        } else {
            let v = self.expr(subject)?;
            match v {
                Val::Regs(vals) => {
                    let tys: Vec<VTy> = self.repr_of(ty, span)?.parts().iter().map(|p| p.vty).collect();
                    let vars = self.new_vars(&tys);
                    self.write_all(&vars, &vals);
                    Place::Vars(vars)
                }
                v @ Val::Mem(_) => {
                    let v = self.owned(v, ty, span)?;
                    let Val::Mem(ptr) = v else { return None };
                    self.own_temp(ptr, ty);
                    Place::At { ptr, off: 0 }
                }
            }
        };
        self.f.subjects.insert(e.id, (place.clone(), ty));
        let join = self.join(self.tcx().unit, span)?;
        self.block_into(body, &join)?;
        if in_place {
            return Some(Val::Regs(Vec::new()));
        }
        self.read_place(&place, ty, span)
    }

    // ------------------------------------------------------------ patterns

    /// Tests `v`, a value of sum `ty` (`info`), for `case`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn test_case(
        &mut self,
        v: &Val,
        ty: Ty,
        info: &WarpInfo,
        case: DefId,
        yes: BlockId,
        no: BlockId,
        span: Span,
    ) -> Option<()> {
        let tag = self.tag_of(v, ty, span)?;
        let want = self.iconst(Self::case_tag(info, case));
        let c = self.emit1(InstKind::ICmp { cmp: crate::heddle::Cmp::Eq, unsigned: true, a: tag, b: want }, VTy::Int);
        self.br(c, yes, no);
        Some(())
    }

    /// The tag of `v`, a value of sum `ty`.
    pub(super) fn tag_of(&mut self, v: &Val, ty: Ty, span: Span) -> Option<Value> {
        match v {
            Val::Regs(vals) => Some(vals[0]),
            &Val::Mem(ptr) => {
                let layout = self.layout(ty)?;
                let Shape::Sum { tag, .. } = layout.shape else { return self.internal(span, "the tag of a record") };
                Some(self.load(tag_mem(tag.bytes), ptr, tag.offset as i64, VTy::Int))
            }
        }
    }

    /// A case or record pattern's test, if `pat` is one.
    pub(super) fn test_warp_pat(
        &mut self,
        pat: &'a hir::Pat,
        v: &Val,
        ty: Ty,
        yes: BlockId,
        no: BlockId,
    ) -> Option<Option<()>> {
        let span = pat.span;
        match &pat.kind {
            PatKind::Case { fields, .. } => {
                if self.tables().const_pats.contains_key(&pat.id) {
                    return Some(self.unsupported(span, "constants with a type in patterns"));
                }
                let case_pat = self.tables().case_pats.get(&pat.id)?.clone();
                let info = self.warp_info(ty)?;
                if let Some(case) = case_pat.case {
                    let next = self.new_block();
                    if self.test_case(v, ty, &info, case, next, no, span).is_none() {
                        return Some(None);
                    }
                    self.enter(next);
                }
                for (f, key) in fields.iter().flatten().zip(&case_pat.keys) {
                    let in_case = case_pat.case.is_some_and(|c| {
                        info.case_by_def(c).is_some_and(|x| x.fields.iter().flatten().any(|cf| &cf.key == key))
                    });
                    let Some(pos) = self.field_pos(ty, case_pat.case.filter(|_| in_case), key, span) else {
                        return Some(None);
                    };
                    let Some(fv) = self.field_of(v, &pos, span) else { return Some(None) };
                    let next = self.new_block();
                    if self.test_pat(&f.pat, &fv, pos.ty, next, no).is_none() {
                        return Some(None);
                    }
                    self.enter(next);
                }
                self.jump(yes);
                Some(Some(()))
            }
            PatKind::ConstCase(_) => {
                let case_pat = self.tables().case_pats.get(&pat.id)?.clone();
                let info = self.warp_info(ty)?;
                let case = case_pat.case?;
                Some(self.test_case(v, ty, &info, case, yes, no, span))
            }
            _ => None,
        }
    }

    /// A case or record pattern's bindings, if `pat` is one.
    pub(super) fn bind_warp_pat(
        &mut self,
        pat: &'a hir::Pat,
        v: &Val,
        ty: Ty,
        how: super::pat::Binding,
    ) -> Option<Option<()>> {
        let span = pat.span;
        match &pat.kind {
            PatKind::Case { fields, .. } => {
                let case_pat = self.tables().case_pats.get(&pat.id)?.clone();
                let info = self.warp_info(ty)?;
                for (f, key) in fields.iter().flatten().zip(&case_pat.keys) {
                    let in_case = case_pat.case.is_some_and(|c| {
                        info.case_by_def(c).is_some_and(|x| x.fields.iter().flatten().any(|cf| &cf.key == key))
                    });
                    let Some(pos) = self.field_pos(ty, case_pat.case.filter(|_| in_case), key, span) else {
                        return Some(None);
                    };
                    let Some(fv) = self.field_of(v, &pos, span) else { return Some(None) };
                    if self.bind_with(&f.pat, &fv, pos.ty, how).is_none() {
                        return Some(None);
                    }
                }
                Some(Some(()))
            }
            PatKind::ConstCase(_) => Some(Some(())),
            _ => None,
        }
    }
}

/// What a field access reads from.
enum FieldBase {
    Place(Place),
    /// A value that isn't a place.
    Value(Val),
}

/// How a tag of `bytes` bytes is loaded and stored.
pub(super) fn tag_mem(bytes: u8) -> Mem {
    if bytes == 1 { Mem::U8 } else { Mem::U16 }
}
