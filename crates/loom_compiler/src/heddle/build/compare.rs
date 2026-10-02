//! Comparing values: `==`, `<` and the rest, for every type Treadle holds
//! (`rt.eq` and `rt.cmp` in the Lua runtime).
//!
//! Scalars compare with one instruction, and text byte by byte. A warp
//! that writes its own `eq` (or `lt`) is compared by it. Anything else
//! compares structurally: field by field, the first that differs deciding
//! the order, and for a sum, its case first. A value in registers is
//! compared inline; one in memory through glue (`eq[T]`, `cmp[T]`), made
//! once per type.
//!
//! `<` on a structure goes through a three-way comparison, as on Lua, so a
//! NaN inside one compares as equal to anything; on a float itself, `<`
//! is the float comparison.

use loom_syntax::ast::BinaryOp;
use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, Glue};
use crate::heddle::{Cmp, InstKind, IntOp, Mem, Policy, VTy, Value};
use crate::ty::{IntTy, Ty, TyKind};
use crate::typeck::BodyId;

const I64: IntTy = IntTy { signed: true, bits: 64, size: false };
const U64: IntTy = IntTy { signed: false, bits: 64, size: false };

/// What the parts of a structure are compared as: the pairs of values, and
/// their type.
type Pairs = Vec<(Val, Val, Ty)>;

impl<'a> Builder<'a> {
    /// `a op b`, for a comparison operator, as a `bool`.
    pub(super) fn compare_vals(&mut self, op: BinaryOp, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        let cmp = match op {
            BinaryOp::Eq => Cmp::Eq,
            BinaryOp::Ne => Cmp::Ne,
            BinaryOp::Lt => Cmp::Lt,
            BinaryOp::Le => Cmp::Le,
            BinaryOp::Gt => Cmp::Gt,
            _ => Cmp::Ge,
        };
        if let Some(unsigned) = self.scalar(ty) {
            let (a, b) = (self.regs(a, ty, span)?, self.regs(b, ty, span)?);
            return Some(self.scalar_cmp(cmp, a[0], b[0], ty, unsigned));
        }
        if matches!(cmp, Cmp::Eq | Cmp::Ne) {
            let eq = self.eq_vals(a, b, ty, span)?;
            return Some(if cmp == Cmp::Ne { self.emit1(InstKind::Not(eq), VTy::Int) } else { eq });
        }
        let c = self.cmp_vals(a, b, ty, span)?;
        let zero = self.iconst(0);
        Some(self.emit1(InstKind::ICmp { cmp, unsigned: false, a: c, b: zero }, VTy::Int))
    }

    /// Whether `ty` compares with one instruction, and if so, whether
    /// unsigned (for integers).
    fn scalar(&mut self, ty: Ty) -> Option<bool> {
        match self.tcx().kind(ty) {
            TyKind::Int(i) => Some(!i.signed),
            TyKind::Bool | TyKind::Char | TyKind::Float(_) => Some(true),
            TyKind::Adt(..) if self.has_op_fns(ty) => None,
            TyKind::Adt(..) => self.warp_info(ty).filter(|i| i.unit_only()).map(|_| true),
            _ => None,
        }
    }

    /// Whether the warp `ty` writes its own `eq` or `lt`.
    fn has_op_fns(&self, ty: Ty) -> bool {
        self.typed.results.op_fns.get(&ty).is_some_and(|f| f.eq.is_some() || f.lt.is_some())
    }

    fn scalar_cmp(&mut self, cmp: Cmp, a: Value, b: Value, ty: Ty, unsigned: bool) -> Value {
        match *self.tcx().kind(ty) {
            TyKind::Float(f) => self.emit1(InstKind::FCmp { cmp, ty: f, a, b }, VTy::Int),
            _ => self.emit1(InstKind::ICmp { cmp, unsigned, a, b }, VTy::Int),
        }
    }

    // ------------------------------------------------------------ equality

    /// Whether `a` and `b`, values of `ty`, are equal.
    pub(super) fn eq_vals(&mut self, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        if let Some(eq) = self.typed.results.op_fns.get(&ty).and_then(|f| f.eq) {
            return self.call_op(eq, a, b, ty, span);
        }
        if let Some(unsigned) = self.scalar(ty) {
            let (a, b) = (self.regs(a, ty, span)?, self.regs(b, ty, span)?);
            return Some(self.scalar_cmp(Cmp::Eq, a[0], b[0], ty, unsigned));
        }
        if self.is_text(ty) {
            return self.text_cmp("str.eq", a, b, ty, span);
        }
        if self.in_regs(ty) {
            return self.eq_structure(a, b, ty, span);
        }
        self.glue_cmp(Glue::Eq, a, b, ty, span)
    }

    /// Whether `a` and `b` are equal, part by part.
    pub(super) fn eq_structure(&mut self, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        match self.tcx().kind(ty).clone() {
            TyKind::Unit => Some(self.iconst(1)),
            TyKind::Tuple(_) => {
                let pairs = self.tuple_pairs(&a, &b, ty, span)?;
                self.all_eq(pairs, span)
            }
            TyKind::Optional(inner) => {
                // Equal when both are `none`, or both `some` of equal values.
                let (pa, pb) = (self.is_some(&a, ty, span)?, self.is_some(&b, ty, span)?);
                let both = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a: pa, b: pb }, VTy::Int);
                let r = self.new_var(VTy::Int);
                let (yes, no, done) = (self.new_block(), self.new_block(), self.new_block());
                self.br(both, yes, no);
                self.enter(yes);
                let (x, y) = (self.unwrap_some(&a, ty, span)?, self.unwrap_some(&b, ty, span)?);
                let eq = self.eq_vals(x, y, inner, span)?;
                self.write(r, eq);
                self.jump(done);
                self.enter(no);
                let same = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: pa, b: pb }, VTy::Int);
                self.write(r, same);
                self.jump(done);
                self.enter(done);
                Some(self.read(r))
            }
            // The same member, and equal as that member.
            TyKind::Result(..) | TyKind::Union(_) => {
                let members = match self.tcx().kind(ty).clone() {
                    TyKind::Result(t, e) => vec![t, e],
                    TyKind::Union(ms) => ms,
                    _ => Vec::new(),
                };
                let (ta, tb) = (self.tag_value(&a, ty, span)?, self.tag_value(&b, ty, span)?);
                self.by_tag(ta, tb, members.len(), span, |this, i| {
                    let m = members[i];
                    let (x, y) = (this.payload(&a, ty, m, span)?, this.payload(&b, ty, m, span)?);
                    Some(vec![(x, y, m)])
                })
            }
            TyKind::Array(elem, n) => {
                let (Val::Mem(pa), Val::Mem(pb)) = (&a, &b) else {
                    return self.internal(span, "an array in registers");
                };
                let len = self.iconst(n);
                self.elems_eq((*pa, len), (*pb, len), elem, span)
            }
            TyKind::Adt(def, args) => match self.p.def_path(def).as_str() {
                "std.list.List" => {
                    let (Val::Mem(pa), Val::Mem(pb)) = (&a, &b) else {
                        return self.internal(span, "a list in registers");
                    };
                    let (pa, pb) = (*pa, *pb);
                    let (ba, la) = (self.load(Mem::W64, pa, 0, VTy::Ptr), self.load(Mem::W64, pa, 8, VTy::Int));
                    let (bb, lb) = (self.load(Mem::W64, pb, 0, VTy::Ptr), self.load(Mem::W64, pb, 8, VTy::Int));
                    self.elems_eq((ba, la), (bb, lb), args[0], span)
                }
                "std.slice.Slice" => {
                    let (x, y) = (self.regs(a, ty, span)?, self.regs(b, ty, span)?);
                    self.elems_eq((x[0], x[1]), (y[0], y[1]), args[0], span)
                }
                "std.map.Map" | "std.set.Set" => {
                    let (Val::Mem(pa), Val::Mem(pb)) = (&a, &b) else {
                        return self.internal(span, "a map in registers");
                    };
                    let (pa, pb) = (*pa, *pb);
                    let c = self.coll_of(ty)?;
                    self.eq_colls(&c, pa, pb, span)
                }
                path if self.is_builtin(def) => self.unsupported(span, &format!("comparing values of `{path}`")),
                _ => {
                    let (shared, cases) = self.warp_pairs(&a, &b, ty, span)?;
                    match cases {
                        None => self.all_eq(shared, span),
                        Some(cases) => {
                            // The same case, with equal payloads and shared fields.
                            let (ta, tb) = (self.tag_of(&a, ty, span)?, self.tag_of(&b, ty, span)?);
                            self.by_tag(ta, tb, cases.len(), span, |_, i| {
                                let mut pairs = cases[i].clone();
                                pairs.extend(shared.iter().cloned());
                                Some(pairs)
                            })
                        }
                    }
                }
            },
            _ => {
                let shown = self.tcx().display(self.p, ty);
                self.unsupported(span, &format!("comparing values of type `{shown}`"))
            }
        }
    }

    /// Equal when every pair is, stopping at the first that isn't.
    fn all_eq(&mut self, pairs: Pairs, span: Span) -> Option<Value> {
        let r = self.new_var(VTy::Int);
        let done = self.new_block();
        for (x, y, t) in pairs {
            let eq = self.eq_vals(x, y, t, span)?;
            self.write(r, eq);
            let next = self.new_block();
            self.br(eq, next, done);
            self.enter(next);
        }
        let yes = self.iconst(1);
        self.write(r, yes);
        self.jump(done);
        self.enter(done);
        Some(self.read(r))
    }

    /// Equal when the tags are, and then the pairs `pairs(tag)` gives.
    fn by_tag(
        &mut self,
        ta: Value,
        tb: Value,
        count: usize,
        span: Span,
        mut pairs: impl FnMut(&mut Self, usize) -> Option<Pairs>,
    ) -> Option<Value> {
        let r = self.new_var(VTy::Int);
        let done = self.new_block();
        let same = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: ta, b: tb }, VTy::Int);
        self.write(r, same);
        let first = self.new_block();
        self.br(same, first, done);
        self.enter(first);
        for i in 0..count {
            let (yes, next) = (self.new_block(), self.new_block());
            let k = self.iconst(i as u64);
            let hit = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: ta, b: k }, VTy::Int);
            self.br(hit, yes, next);
            self.enter(yes);
            let ps = pairs(self, i)?;
            let eq = self.all_eq(ps, span)?;
            self.write(r, eq);
            self.jump(done);
            self.enter(next);
        }
        self.terminate(crate::heddle::Term::Crash(0));
        self.enter(done);
        Some(self.read(r))
    }

    /// Whether `a` and `b`, runs of `elem` (address, length), are equal.
    fn elems_eq(&mut self, a: (Value, Value), b: (Value, Value), elem: Ty, span: Span) -> Option<Value> {
        let (size, _) = self.size_align(elem)?;
        let r = self.new_var(VTy::Int);
        let done = self.new_block();
        let same = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: a.1, b: b.1 }, VTy::Int);
        self.write(r, same);
        let start = self.new_block();
        self.br(same, start, done);
        self.enter(start);
        let i = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(i, zero);
        let (head, body, step) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let more = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: cur, b: a.1 }, VTy::Int);
        let yes = self.iconst(1);
        self.write(r, yes);
        self.br(more, body, done);
        self.enter(body);
        let cur = self.read(i);
        let x = self.emit1(InstKind::Index { base: a.0, index: cur, scale: size }, VTy::Ptr);
        let y = self.emit1(InstKind::Index { base: b.0, index: cur, scale: size }, VTy::Ptr);
        let (x, y) = (self.load_val(x, 0, elem, span)?, self.load_val(y, 0, elem, span)?);
        let eq = self.eq_vals(x, y, elem, span)?;
        self.write(r, eq);
        self.br(eq, step, done);
        self.enter(step);
        let cur = self.read(i);
        let one = self.iconst(1);
        let next =
            self.emit1(InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: U64, a: cur, b: one }, VTy::Int);
        self.write(i, next);
        self.jump(head);
        self.seal(head);
        self.enter(done);
        Some(self.read(r))
    }

    // ------------------------------------------------------------ order

    /// -1, 0 or 1 as `a` comes before, with or after `b`, values of `ty`.
    pub(super) fn cmp_vals(&mut self, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        if let Some(lt) = self.typed.results.op_fns.get(&ty).and_then(|f| f.lt) {
            // -1 if `a < b`, else 1 if `b < a`, else 0.
            let r = self.new_var(VTy::Int);
            let (before, not_before, done) = (self.new_block(), self.new_block(), self.new_block());
            let first = self.call_op(lt, a.clone(), b.clone(), ty, span)?;
            self.br(first, before, not_before);
            self.enter(before);
            let minus = self.iconst(u64::MAX);
            self.write(r, minus);
            self.jump(done);
            self.enter(not_before);
            let after = self.call_op(lt, b, a, ty, span)?;
            self.write(r, after);
            self.jump(done);
            self.enter(done);
            return Some(self.read(r));
        }
        if let Some(unsigned) = self.scalar(ty) {
            let (a, b) = (self.regs(a, ty, span)?, self.regs(b, ty, span)?);
            let lt = self.scalar_cmp(Cmp::Lt, a[0], b[0], ty, unsigned);
            let gt = self.scalar_cmp(Cmp::Lt, b[0], a[0], ty, unsigned);
            return Some(
                self.emit1(InstKind::Arith { op: IntOp::Sub, policy: Policy::Wrap, ty: I64, a: gt, b: lt }, VTy::Int),
            );
        }
        if self.is_text(ty) {
            return self.text_cmp("str.cmp", a, b, ty, span);
        }
        if self.in_regs(ty) {
            return self.cmp_structure(a, b, ty, span);
        }
        self.glue_cmp(Glue::Cmp, a, b, ty, span)
    }

    /// How `a` and `b` order, part by part.
    pub(super) fn cmp_structure(&mut self, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        match self.tcx().kind(ty).clone() {
            TyKind::Unit => Some(self.iconst(0)),
            TyKind::Tuple(_) => {
                let pairs = self.tuple_pairs(&a, &b, ty, span)?;
                self.first_order(pairs, span)
            }
            TyKind::Optional(inner) => {
                // `none` comes first.
                let (pa, pb) = (self.is_some(&a, ty, span)?, self.is_some(&b, ty, span)?);
                let both = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a: pa, b: pb }, VTy::Int);
                let r = self.new_var(VTy::Int);
                let (yes, no, done) = (self.new_block(), self.new_block(), self.new_block());
                self.br(both, yes, no);
                self.enter(yes);
                let (x, y) = (self.unwrap_some(&a, ty, span)?, self.unwrap_some(&b, ty, span)?);
                let c = self.cmp_vals(x, y, inner, span)?;
                self.write(r, c);
                self.jump(done);
                self.enter(no);
                let c = self
                    .emit1(InstKind::Arith { op: IntOp::Sub, policy: Policy::Wrap, ty: I64, a: pa, b: pb }, VTy::Int);
                self.write(r, c);
                self.jump(done);
                self.enter(done);
                Some(self.read(r))
            }
            TyKind::Adt(def, _) if !self.is_builtin(def) => {
                let (shared, cases) = self.warp_pairs(&a, &b, ty, span)?;
                let Some(cases) = cases else { return self.first_order(shared, span) };
                // Earlier cases first; then the payload, then the shared fields.
                let (ta, tb) = (self.tag_of(&a, ty, span)?, self.tag_of(&b, ty, span)?);
                let r = self.new_var(VTy::Int);
                let done = self.new_block();
                let lt = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: ta, b: tb }, VTy::Int);
                let gt = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: tb, b: ta }, VTy::Int);
                let c = self
                    .emit1(InstKind::Arith { op: IntOp::Sub, policy: Policy::Wrap, ty: I64, a: gt, b: lt }, VTy::Int);
                self.write(r, c);
                let zero = self.iconst(0);
                let differ = self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: c, b: zero }, VTy::Int);
                let first = self.new_block();
                self.br(differ, done, first);
                self.enter(first);
                for (i, case) in cases.into_iter().enumerate() {
                    let (yes, next) = (self.new_block(), self.new_block());
                    let k = self.iconst(i as u64);
                    let hit = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: ta, b: k }, VTy::Int);
                    self.br(hit, yes, next);
                    self.enter(yes);
                    let mut pairs = case;
                    pairs.extend(shared.iter().cloned());
                    let c = self.first_order(pairs, span)?;
                    self.write(r, c);
                    self.jump(done);
                    self.enter(next);
                }
                self.terminate(crate::heddle::Term::Crash(0));
                self.enter(done);
                Some(self.read(r))
            }
            _ => {
                let shown = self.tcx().display(self.p, ty);
                self.unsupported(span, &format!("ordering values of type `{shown}`"))
            }
        }
    }

    /// The order of the first pair that differs, or 0.
    fn first_order(&mut self, pairs: Pairs, span: Span) -> Option<Value> {
        let r = self.new_var(VTy::Int);
        let done = self.new_block();
        for (x, y, t) in pairs {
            let c = self.cmp_vals(x, y, t, span)?;
            self.write(r, c);
            let zero = self.iconst(0);
            let differ = self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: c, b: zero }, VTy::Int);
            let next = self.new_block();
            self.br(differ, done, next);
            self.enter(next);
        }
        let zero = self.iconst(0);
        self.write(r, zero);
        self.jump(done);
        self.enter(done);
        Some(self.read(r))
    }

    // ------------------------------------------------------------ the parts compared

    fn tuple_pairs(&mut self, a: &Val, b: &Val, ty: Ty, span: Span) -> Option<Pairs> {
        let xs = self.tuple_items(a, ty, span)?;
        let ys = self.tuple_items(b, ty, span)?;
        Some(xs.into_iter().zip(ys).map(|((x, t), (y, _))| (x, y, t)).collect())
    }

    /// A warp's shared fields (all of a record's), and for a sum, each
    /// case's payload fields.
    #[allow(clippy::type_complexity)]
    fn warp_pairs(&mut self, a: &Val, b: &Val, ty: Ty, span: Span) -> Option<(Pairs, Option<Vec<Pairs>>)> {
        let info = self.warp_info(ty)?;
        let field_pairs = |this: &mut Self, case: Option<crate::hir::DefId>, keys: Vec<crate::typeck::Key>| {
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                let pos = this.field_pos(ty, case, &key, span)?;
                let (x, y) = (this.field_of(a, &pos, span)?, this.field_of(b, &pos, span)?);
                out.push((x, y, pos.ty));
            }
            Some(out)
        };
        let shared = field_pairs(self, None, info.fields.iter().map(|f| f.key.clone()).collect())?;
        if !info.is_sum() {
            return Some((shared, None));
        }
        let mut cases = Vec::with_capacity(info.cases.len());
        for case in &info.cases {
            let keys = case.fields.iter().flatten().map(|f| f.key.clone()).collect();
            cases.push(field_pairs(self, Some(case.def), keys)?);
        }
        Some((shared, Some(cases)))
    }

    // ------------------------------------------------------------ calls

    /// Whether `ty` is text: `str` or `String`.
    fn is_text(&self, ty: Ty) -> bool {
        matches!(self.tcx().kind(ty), TyKind::Str) || self.is_string(ty)
    }

    /// `str.eq` or `str.cmp` on the text of `a` and `b`.
    fn text_cmp(&mut self, native: &'static str, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let (pa, la) = self.text_of(a, ty, span)?;
        let (pb, lb) = self.text_of(b, ty, span)?;
        Some(self.emit(InstKind::Native { name: native, imm: 0, args: vec![pa, la, pb, lb] }, &[VTy::Int])[0])
    }

    /// Calls glue `g` (`Eq` or `Cmp`) for `ty` on `a` and `b`.
    fn glue_cmp(&mut self, g: Glue, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let func = self.glue(g, ty);
        let mut args = self.read_args(a, ty, span)?;
        args.extend(self.read_args(b, ty, span)?);
        Some(self.emit(InstKind::Call { func, args }, &[VTy::Int])[0])
    }

    /// Calls a hand-written `eq` or `lt` (instance `id`) on `a` and `b`.
    fn call_op(&mut self, id: BodyId, a: Val, b: Val, ty: Ty, span: Span) -> Option<Value> {
        let func = self.func_id(super::FnKey::Body(id));
        let mut args = self.read_args(a, ty, span)?;
        args.extend(self.read_args(b, ty, span)?);
        Some(self.emit(InstKind::Call { func, args }, &[VTy::Int])[0])
    }

    /// `v` as a read parameter of `ty` is passed: its parts, or its address.
    fn read_args(&mut self, v: Val, ty: Ty, span: Span) -> Option<Vec<Value>> {
        match self.repr_of(ty, span)? {
            super::Repr::Regs(_) => self.regs(v, ty, span),
            super::Repr::Mem { .. } => Some(vec![self.addr_of(v, ty, span)?]),
        }
    }
}
