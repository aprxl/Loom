//! Patterns: tests as branches, and bindings.
//!
//! A pattern tests a value in registers or in memory. What it binds is a
//! value of its own when it's plain data (`Copy`), and a view of the
//! matched value otherwise: the binding's local is the address of that part
//! of the value, and moving it moves out of the matched value (see
//! `typeck/moves.rs`). A `let` owns what it binds instead.

use loom_syntax::ast::BinaryOp;
use loom_syntax::source::Span;

use super::value::Val;
use super::{BlockId, Builder};
use crate::heddle::{BitOp, InstKind, VTy, Value};
use crate::hir::{self, PatKind};
use crate::ty::{IntTy, Ty, TyKind};

/// `u64`, for picking bits out of a value: its shifts never trap for an
/// amount below 64, and never wrap.
const BITS: IntTy = IntTy { signed: false, bits: 64, size: false };

/// Whether a pattern's bindings own what they bind (a `let`), or view the
/// matched value (a `match` arm, `is`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Binding {
    Owned,
    View,
}

impl<'a> Builder<'a> {
    /// The items of a tuple value: each item's value and type.
    pub(super) fn tuple_items(&mut self, v: &Val, ty: Ty, span: Span) -> Option<Vec<(Val, Ty)>> {
        let items = match self.tcx().kind(ty).clone() {
            TyKind::Tuple(items) => items,
            TyKind::Unit => return Some(Vec::new()),
            _ => return self.internal(span, "the items of something that isn't a tuple"),
        };
        let items: Vec<Ty> = items.iter().map(|&t| self.tcx().degauge(t)).collect();
        match v {
            Val::Regs(vals) => {
                let mut out = Vec::with_capacity(items.len());
                let mut at = 0;
                for t in items {
                    let n = self.repr_of(t, span)?.parts().len();
                    out.push((Val::Regs(vals[at..at + n].to_vec()), t));
                    at += n;
                }
                Some(out)
            }
            &Val::Mem(ptr) => {
                let offsets = self.field_offsets(ty, span)?;
                let mut out = Vec::with_capacity(items.len());
                for (t, off) in items.into_iter().zip(offsets) {
                    out.push((self.load_val(ptr, off as i64, t, span)?, t));
                }
                Some(out)
            }
        }
    }

    /// Jumps to `yes` if `pat` matches `v` (a value of type `ty`), and to
    /// `no` if it doesn't.
    pub(super) fn test_pat(&mut self, pat: &'a hir::Pat, v: &Val, ty: Ty, yes: BlockId, no: BlockId) -> Option<()> {
        let span = pat.span;
        // `x is Cents`, `c: Cents =>`: the gauge's check.
        if let Some(&gauge) = self.tables().gauge_tests.get(&pat.id)
            && let TyKind::Gauge(def, _) = *self.tcx().kind(gauge)
        {
            let x = self.regs(v.clone(), ty, span)?[0];
            let ok = self.gauge_test(def, x, ty)?;
            self.br(ok, yes, no);
            return Some(());
        }
        if let Some(done) = self.test_warp_pat(pat, v, ty, yes, no) {
            return done;
        }
        match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Error => self.jump(yes),
            PatKind::Lit { lit, negative } => {
                let lit_v = self.lit(lit, *negative, ty, span)?;
                let vals = self.regs(v.clone(), ty, span)?;
                let c = self.compare(BinaryOp::Eq, &vals, &lit_v, ty, span)?;
                self.br(c, yes, no);
            }
            PatKind::Range { lo, hi, inclusive } => {
                let vals = self.regs(v.clone(), ty, span)?;
                let bound = |this: &mut Self, p: &Option<Box<hir::Pat>>| -> Option<Option<Vec<Value>>> {
                    match p.as_ref().map(|p| &p.kind) {
                        None => Some(None),
                        Some(PatKind::Lit { lit, negative }) => Some(Some(this.lit(lit, *negative, ty, span)?)),
                        Some(_) => this.unsupported(span, "this range pattern"),
                    }
                };
                let (lo, hi) = (bound(self, lo)?, bound(self, hi)?);
                if let Some(lo) = lo {
                    let c = self.compare(BinaryOp::Le, &lo, &vals, ty, span)?;
                    let next = self.new_block();
                    self.br(c, next, no);
                    self.enter(next);
                }
                match hi {
                    Some(hi) => {
                        let op = if *inclusive { BinaryOp::Le } else { BinaryOp::Lt };
                        let c = self.compare(op, &vals, &hi, ty, span)?;
                        self.br(c, yes, no);
                    }
                    None => self.jump(yes),
                }
            }
            PatKind::Tuple(pats) => {
                let items = self.tuple_items(v, ty, span)?;
                for (p, (item, item_ty)) in pats.iter().zip(items) {
                    let next = self.new_block();
                    self.test_pat(p, &item, item_ty, next, no)?;
                    self.enter(next);
                }
                self.jump(yes);
            }
            PatKind::Or(alts) => {
                let (last, rest) = alts.split_last()?;
                for alt in rest {
                    let next = self.new_block();
                    self.test_pat(alt, v, ty, yes, next)?;
                    self.enter(next);
                }
                self.test_pat(last, v, ty, yes, no)?;
            }
            PatKind::Some(_) | PatKind::None => {
                let present = self.is_some(v, ty, span)?;
                let Some(inner) = inner_pat(&pat.kind) else {
                    self.br(present, no, yes);
                    return Some(());
                };
                let next = self.new_block();
                self.br(present, next, no);
                self.enter(next);
                let TyKind::Optional(t) = *self.tcx().kind(ty) else { return None };
                let x = self.unwrap_some(v, ty, span)?;
                self.test_pat(inner, &x, t, yes, no)?;
            }
            PatKind::Ok(inner) | PatKind::Err(inner) => {
                let TyKind::Result(t, e) = *self.tcx().kind(ty) else { return None };
                let is_ok = matches!(pat.kind, PatKind::Ok(_));
                let tag = self.tag_value(v, ty, span)?;
                let want = self.iconst(u64::from(!is_ok));
                let c = self
                    .emit1(InstKind::ICmp { cmp: crate::heddle::Cmp::Eq, unsigned: true, a: tag, b: want }, VTy::Int);
                let next = self.new_block();
                self.br(c, next, no);
                self.enter(next);
                let inner_ty = if is_ok { t } else { e };
                let x = self.payload(v, ty, inner_ty, span)?;
                self.test_pat(inner, &x, inner_ty, yes, no)?;
            }
            PatKind::TypeTest { .. } => {
                let Some(tested) = self.tables().type_tests.get(&pat.id).copied() else {
                    return self.unsupported(span, "this type test");
                };
                if let TyKind::Any(_) = self.tcx().kind(ty) {
                    let Val::Mem(at) = *v else { return self.internal(span, "an `any` in registers") };
                    let c = self.any_is(at, tested);
                    self.br(c, yes, no);
                    return Some(());
                }
                let tag = self.tag_value(v, ty, span)?;
                let k = self.member_index(ty, tested, span)?;
                let want = self.iconst(k);
                let c = self
                    .emit1(InstKind::ICmp { cmp: crate::heddle::Cmp::Eq, unsigned: true, a: tag, b: want }, VTy::Int);
                self.br(c, yes, no);
            }
            PatKind::Twill { pattern, .. } => {
                if pattern.mask == 0 {
                    self.jump(yes);
                } else {
                    let vals = self.regs(v.clone(), ty, span)?;
                    let mask = self.iconst(pattern.mask as u64);
                    let masked = self.emit1(InstKind::Bit { op: BitOp::And, a: vals[0], b: mask }, VTy::Int);
                    let want = self.iconst(pattern.value as u64);
                    let c = self.emit1(
                        InstKind::ICmp { cmp: crate::heddle::Cmp::Eq, unsigned: true, a: masked, b: want },
                        VTy::Int,
                    );
                    self.br(c, yes, no);
                }
            }
            _ => return self.unsupported(span, "this pattern"),
        }
        Some(())
    }

    /// Binds the names of a pattern that matched `v`, as views of it.
    pub(super) fn bind_pat(&mut self, pat: &'a hir::Pat, v: &Val, ty: Ty) -> Option<()> {
        self.bind_with(pat, v, ty, Binding::View)
    }

    /// Binds the names of `pat`, which matched `v`, a value of `ty`.
    pub(super) fn bind_with(&mut self, pat: &'a hir::Pat, v: &Val, ty: Ty, how: Binding) -> Option<()> {
        let span = pat.span;
        if self.tables().gauge_tests.contains_key(&pat.id) {
            if let PatKind::TypeTest { local, .. } = &pat.kind {
                self.bind_local(*local, v.clone(), how, span)?;
            }
            return Some(());
        }
        if let Some(done) = self.bind_warp_pat(pat, v, ty, how) {
            return done;
        }
        match &pat.kind {
            PatKind::Bind(l) => self.bind_local(*l, v.clone(), how, span)?,
            PatKind::Tuple(pats) => {
                let items = self.tuple_items(v, ty, span)?;
                for (p, (item, item_ty)) in pats.iter().zip(items) {
                    self.bind_with(p, &item, item_ty, how)?;
                }
            }
            PatKind::Twill { pattern, captures } => {
                let vals = self.regs(v.clone(), ty, span)?;
                for (capture, &l) in pattern.captures.iter().zip(captures) {
                    let value = self.twill_capture(vals[0], capture);
                    self.bind_local(l, Val::Regs(vec![value]), Binding::Owned, span)?;
                }
            }
            PatKind::Or(alts) => {
                // Checked alone when it binds anything (see `match_expr`).
                if let Some(first) = alts.first() {
                    self.bind_with(first, v, ty, how)?;
                }
            }
            PatKind::Some(inner) => {
                let TyKind::Optional(t) = *self.tcx().kind(ty) else { return None };
                let x = self.unwrap_some(v, ty, span)?;
                self.bind_with(inner, &x, t, how)?;
            }
            PatKind::Ok(inner) | PatKind::Err(inner) => {
                let TyKind::Result(t, e) = *self.tcx().kind(ty) else { return None };
                let inner_ty = if matches!(pat.kind, PatKind::Ok(_)) { t } else { e };
                let x = self.payload(v, ty, inner_ty, span)?;
                self.bind_with(inner, &x, inner_ty, how)?;
            }
            PatKind::TypeTest { local, .. } => {
                let tested = self.tables().type_tests.get(&pat.id).copied()?;
                let x = match (self.tcx().kind(ty), v) {
                    (TyKind::Any(_), &Val::Mem(at)) => self.any_value(at, tested, span)?,
                    _ => self.payload(v, ty, tested, span)?,
                };
                self.bind_local(*local, x, how, span)?;
            }
            _ => {}
        }
        Some(())
    }

    /// The tag of member `member` of union `ty`.
    pub(super) fn member_index(&mut self, ty: Ty, member: Ty, span: Span) -> Option<u64> {
        match self.tcx().kind(ty) {
            TyKind::Union(members) => match members.iter().position(|&m| m == member) {
                Some(i) => Some(i as u64),
                None => self.internal(span, "a type test for a type the union doesn't have"),
            },
            _ => self.internal(span, "a type test on something that isn't a union"),
        }
    }

    /// Binds local `l` to `v`: a value of its own for plain data, or a view
    /// of `v` (for `how == View`) when it owns memory.
    pub(super) fn bind_local(&mut self, l: crate::hir::LocalId, v: Val, how: Binding, span: Span) -> Option<()> {
        let ty = self.local_ty(l);
        match (how, &v) {
            (Binding::View, &Val::Mem(ptr)) if self.needs_drop(ty) => {
                self.f.viewed.insert(ptr);
                self.bind_ref(l, ptr);
                Some(())
            }
            (Binding::View, _) => {
                // A copy: what it was matched against may change while it's
                // in scope.
                let place = self.local_place(l, span)?;
                let v = match v {
                    Val::Mem(_) if self.in_regs(ty) => Val::Regs(self.regs(v, ty, span)?),
                    v => v,
                };
                match &place {
                    super::value::Place::At { ptr, off } => self.store_val(*ptr, *off, ty, &v, span),
                    super::value::Place::Vars(_) => self.write_place(&place, ty, v, span),
                }
            }
            (Binding::Owned, _) => self.bind_owned(l, v, span),
        }
    }

    /// The bits of `v` a twill capture names, most significant first.
    fn twill_capture(&mut self, v: Value, capture: &loom_syntax::twill::TwillCapture) -> Value {
        let bits = |this: &mut Self, shift: u32, width: u32| {
            let shifted = if shift == 0 {
                v
            } else {
                let n = this.iconst(u64::from(shift));
                this.emit1(InstKind::Shr { ty: BITS, a: v, b: n }, VTy::Int)
            };
            let mask = this.iconst(((1u128 << width) - 1) as u64);
            this.emit1(InstKind::Bit { op: BitOp::And, a: shifted, b: mask }, VTy::Int)
        };
        if let Some((shift, width)) = capture.contiguous() {
            return bits(self, shift, width);
        }
        // Scattered bits, gathered from the most significant.
        let n = capture.bits.len();
        let mut acc: Option<Value> = None;
        for (i, &bit) in capture.bits.iter().enumerate() {
            let one = bits(self, bit, 1);
            let place = (n - 1 - i) as u64;
            let placed = if place == 0 {
                one
            } else {
                let k = self.iconst(place);
                self.emit1(InstKind::Shl { ty: BITS, a: one, b: k }, VTy::Int)
            };
            acc = Some(match acc {
                None => placed,
                Some(prev) => self.emit1(InstKind::Bit { op: BitOp::Or, a: prev, b: placed }, VTy::Int),
            });
        }
        acc.unwrap_or_else(|| self.iconst(0))
    }

    /// Binds a pattern that can't fail (`let`, `for`), owning what it binds.
    pub(super) fn bind_irrefutable(&mut self, pat: &'a hir::Pat, v: Val, ty: Ty) -> Option<()> {
        match &pat.kind {
            PatKind::Wild => {
                // `let _ = x` throws the value away: a temporary is dropped at
                // the end of the statement, and a place keeps it.
                Some(())
            }
            PatKind::Bind(_)
            | PatKind::Tuple(_)
            | PatKind::Twill { .. }
            | PatKind::Error
            | PatKind::Case { .. }
            | PatKind::Some(_)
            | PatKind::Ok(_)
            | PatKind::Err(_) => self.bind_with(pat, &v, ty, Binding::Owned),
            _ => self.unsupported(pat.span, "this pattern in a `let`"),
        }
    }
}

/// The pattern inside `some(p)`; `None` for `none`.
fn inner_pat(kind: &PatKind) -> Option<&hir::Pat> {
    match kind {
        PatKind::Some(p) => Some(p),
        _ => None,
    }
}

/// Whether a pattern binds any names.
pub(super) fn binds_anything(pat: &hir::Pat) -> bool {
    let mut locals = Vec::new();
    super::scan::pat_locals(pat, &mut locals);
    !locals.is_empty()
}
