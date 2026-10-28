//! Expressions, statements and control flow.
//!
//! A value that comes out of a branch or a block goes through a *join*:
//! variables for a value in registers, or a variable holding the address
//! of a value in memory. A value in memory that the branch owns (a
//! temporary, or one of its locals) would be dropped with the branch's
//! scope, so it moves into the join's own slot first; that slot is zeroed
//! and registered for cleanup before any branch runs, since cleanups are
//! decided while building and branches while running.

use loom_syntax::ast::{AssignOp, BinaryOp, CastKind, Lit, UnaryOp};
use loom_syntax::source::Span;

use super::value::{Place, Val};
use std::cell::Cell;
use std::rc::Rc;

use super::cleanup::{CleanupKind, Stable};
use super::thread::LoomFor;
use super::{Builder, LoopCtx, LoopResult, Vals, Var};
use crate::heddle::{BitOp, Cmp, FOp, InstKind, IntOp, Policy, Term, VTy, Value};
use crate::hir::{self, Block, DefKind, ExprKind, StmtKind};
use crate::ty::{ConstVal, FloatTy, IntTy, Ty, TyKind};
use crate::typeck::Coercion;

/// Where the branches of an `if`, a `match`, a loop or a block put their
/// value.
#[derive(Clone, Debug)]
pub(super) struct Join {
    pub ty: Ty,
    pub kind: JoinKind,
    /// A block's join: a tail that is one of the block's own locals may
    /// hand its slot over instead of moving into the join's.
    pub adopt: bool,
    /// The slot a local's value stayed in, once one was adopted.
    adopted: Cell<Option<u32>>,
}

#[derive(Clone, Debug)]
pub(super) enum JoinKind {
    Regs(Vec<Var>),
    /// The value's address; `slot`, zeroed, holds the values branches own.
    Mem {
        addr: Var,
        slot: Option<Value>,
        /// `slot`'s cleanup, and whether every value put so far moved into
        /// it: then the join's value is a temporary like any other.
        owner: Option<(u32, Rc<Cell<bool>>)>,
    },
}

impl<'a> Builder<'a> {
    // ------------------------------------------------------------ joins

    /// A join for values of `ty`.
    pub(super) fn join(&mut self, ty: Ty, span: Span) -> Option<Join> {
        let kind = match self.repr_of(ty, span)? {
            super::Repr::Regs(parts) => {
                let tys: Vec<VTy> = parts.iter().map(|p| p.vty).collect();
                JoinKind::Regs(self.new_vars(&tys))
            }
            super::Repr::Mem { .. } => {
                // Zero until a branch moves a value in (see `cleanup.rs`).
                let (slot, owner) = if self.needs_drop(ty) {
                    let ptr = self.temp(ty, span)?;
                    self.own_temp(ptr, ty);
                    // Branches move into it; its cleanup stays until the end
                    // of the statement.
                    let id = self.f.temps.remove(&ptr);
                    (Some(ptr), id.map(|id| (id, Rc::new(Cell::new(true)))))
                } else {
                    (None, None)
                };
                JoinKind::Mem { addr: self.new_var(VTy::Ptr), slot, owner }
            }
        };
        Some(Join { ty, kind, adopt: false, adopted: Cell::new(None) })
    }

    /// Puts `val` in `join`, leaving the scopes from stack index `from` up:
    /// what they own moves into the join's slot.
    pub(super) fn join_put(&mut self, join: &Join, val: Val, from: usize, span: Span) -> Option<()> {
        match &join.kind {
            JoinKind::Regs(vars) => {
                let vals = self.regs(val, join.ty, span)?;
                self.write_all(vars, &vals);
            }
            JoinKind::Mem { addr, slot, owner } => {
                let Val::Mem(ptr) = val else {
                    let shown = self.tcx().display(self.p, join.ty);
                    return self.internal(span, &format!("a value in registers for a join in memory ({shown})"));
                };
                if let Some((s, local)) = self.adoptable(join, ptr, from)
                    && let (Some(_), Some((id, _))) = (slot, owner)
                {
                    self.kill_cleanup(local);
                    self.adopt_slot(*id, s, join.ty);
                    join.adopted.set(Some(s));
                    self.write(*addr, ptr);
                    return Some(());
                }
                let at = match slot {
                    Some(slot) if self.owned_since(ptr, from) => {
                        self.move_to(*slot, 0, Val::Mem(ptr), join.ty, span)?;
                        *slot
                    }
                    // A local of an outer scope, given by its name: it moves
                    // into the join, or its scope would drop it before the
                    // value is used (`if c { "x" } else { body }`).
                    Some(slot) if self.f.slot_of.contains_key(&ptr) && self.owned_since(ptr, 0) => {
                        self.move_to(*slot, 0, Val::Mem(ptr), join.ty, span)?;
                        *slot
                    }
                    _ => {
                        if let Some((_, all)) = owner {
                            all.set(false);
                        }
                        ptr
                    }
                };
                self.write(*addr, at);
            }
        }
        Some(())
    }

    /// The value in `join`, once every branch has put one.
    pub(super) fn join_get(&mut self, join: &Join) -> Val {
        match &join.kind {
            JoinKind::Regs(vars) => Val::Regs(self.read_all(vars)),
            JoinKind::Mem { owner: Some((id, _)), .. } if join.adopted.get().is_some() => {
                let slot = join.adopted.get().unwrap_or_default();
                let ptr = self.slot_addr(slot);
                self.f.temps.insert(ptr, *id);
                Val::Mem(ptr)
            }
            // Every branch moved its value into the slot: the slot is the
            // value, a temporary (which a `let` makes the local's own).
            JoinKind::Mem { slot: Some(slot), owner: Some((id, all)), .. } if all.get() => {
                self.f.temps.insert(*slot, *id);
                Val::Mem(*slot)
            }
            JoinKind::Mem { addr, .. } => Val::Mem(self.read(*addr)),
        }
    }

    /// The slot of the block's own local at `ptr` and its drop's id, when
    /// `join` can take it over rather than copy it: nothing else was put in
    /// the join, and the local's drop is live in the scopes from stack
    /// index `from` up.
    fn adoptable(&self, join: &Join, ptr: Value, from: usize) -> Option<(u32, u32)> {
        let JoinKind::Mem { slot: Some(_), owner: Some((_, all)), .. } = &join.kind else { return None };
        if !join.adopt || join.adopted.get().is_some() || self.f.shuttle.is_some() || !all.get() {
            return None;
        }
        if self.f.temps.contains_key(&ptr) {
            return None;
        }
        let s = self.slot_of(ptr)?;
        let local = self.f.cleanups[from.min(self.f.cleanups.len())..]
            .iter()
            .find(|c| c.live && matches!(c.kind, CleanupKind::Drop { at: Stable::Slot(l), .. } if l == s))?;
        Some((s, local.id))
    }

    /// The join's cleanup `id` now drops slot `s`, which was a local's.
    fn adopt_slot(&mut self, id: u32, s: u32, ty: Ty) {
        if let Some(c) = self.f.cleanups.iter_mut().find(|c| c.id == id) {
            c.kind = CleanupKind::Drop { at: Stable::Slot(s), ty };
        }
        self.f.pads.clear();
    }

    /// Whether the value at `ptr` belongs to the scopes from stack index
    /// `from` up: a temporary made there, or a local declared there.
    fn owned_since(&self, ptr: Value, from: usize) -> bool {
        if let Some(&id) = self.f.temps.get(&ptr) {
            return self.f.cleanups.iter().position(|c| c.id == id).is_some_and(|i| i >= from);
        }
        let Some(&slot) = self.f.slot_of.get(&ptr) else { return false };
        self.f.cleanups[from.min(self.f.cleanups.len())..].iter().any(|c| {
            c.live && matches!(c.kind, super::cleanup::CleanupKind::Drop { at: super::cleanup::Stable::Slot(s), .. } if s == slot)
        })
    }

    // ------------------------------------------------------------ expressions

    /// The value of `e`, with its coercion applied. `None` when control
    /// leaves (`return`, `break`, an unravel) or `e` can't be built yet
    /// (reported).
    pub(super) fn expr(&mut self, e: &'a hir::Expr) -> Option<Val> {
        let v = self.expr_raw(e)?;
        self.coerce(e, v)
    }

    /// The value of `e`, which lives in registers, as its parts.
    pub(super) fn value(&mut self, e: &'a hir::Expr) -> Option<Vals> {
        let v = self.expr(e)?;
        let ty = self.coerced_ty(e);
        self.regs(v, ty, e.span)
    }

    /// The type of `e`'s value once it's converted where it's used.
    pub(super) fn coerced_ty(&self, e: &hir::Expr) -> Ty {
        match self.tables().coercions.get(&e.id) {
            None | Some(Coercion::Widen) => self.ty_of(e),
            Some(Coercion::Discard) => self.tcx().unit,
            Some(_) => {
                let ty = self.tables().coerced.get(&e.id).copied().unwrap_or(self.tcx().error);
                self.tcx().degauge(ty)
            }
        }
    }

    fn coerce(&mut self, e: &'a hir::Expr, v: Val) -> Option<Val> {
        let Some(coercion) = self.tables().coercions.get(&e.id) else { return Some(v) };
        match coercion {
            Coercion::Widen => Some(v),
            Coercion::Discard => Some(Val::Regs(Vec::new())),
            Coercion::ToOptional => {
                let target = self.coerced_ty(e);
                let inner = self.ty_of(e);
                self.some(target, inner, v, e.span)
            }
            // A literal's text, copied into a `String` of its own.
            Coercion::StrToString => {
                let target = self.coerced_ty(e);
                let vals = self.regs(v, self.tcx().str, e.span)?;
                let dst = self.temp(target, e.span)?;
                self.emit(InstKind::Native { name: "str.new", imm: 0, args: vec![vals[0], vals[1], dst] }, &[]);
                self.own_temp(dst, target);
                Some(Val::Mem(dst))
            }
            // A view of a `String`'s text.
            Coercion::StringToStr => {
                let ty = self.ty_of(e);
                let (p, n) = self.text_of(v, ty, e.span)?;
                Some(Val::Regs(vec![p, n]))
            }
            Coercion::ToResult => {
                let target = self.coerced_ty(e);
                self.result(target, true, v, e.span)
            }
            Coercion::ToAny(wefts) => {
                let wefts = wefts.clone();
                let (from, to) = (self.ty_of(e), self.coerced_ty(e));
                self.any_of(v, from, to, &wefts, e.span)
            }
            Coercion::ToSlice => {
                let from = self.ty_of(e);
                self.slice_of(v, from, e.span)
            }
            Coercion::ToStream(source) => {
                let source = source.clone();
                let (from, to) = (self.ty_of(e), self.coerced_ty(e));
                self.stream_of(&source, v, from, to, e.span)
            }
            Coercion::ToUnion | Coercion::WidenUnion => {
                let target = self.coerced_ty(e);
                let from = self.ty_of(e);
                self.union_of(target, from, v, e.span)
            }
            Coercion::Deref => {
                let ptr = self.ty_of(e);
                self.pointee(&v, ptr, e.span).map(|(x, _)| x)
            }
            // The same bytes, as a value that owns nothing: in registers if
            // that is how the target lives.
            Coercion::Region => {
                let target = self.coerced_ty(e);
                match v {
                    Val::Mem(at) if self.in_regs(target) => self.load_val(at, 0, target, e.span),
                    v => Some(v),
                }
            }
        }
    }

    pub(super) fn expr_raw(&mut self, e: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        let ty = self.ty_of(e);
        // A function used as a value: `String.of`, `is_prime`.
        if let Some(&id) = self.tables().fn_values.get(&e.id) {
            return self.fn_value(id);
        }
        if let Some(&v) = self.tables().const_values.get(&e.id) {
            return self.const_val(v, ty, span).map(Val::Regs);
        }
        // A place a projection lends, read.
        if self.tables().lends.contains_key(&e.id) {
            let (place, pty) = self.lend_place(e)?;
            return self.read_place(&place, pty, span);
        }
        if let Some(&q) = self.tables().frames.get(&e.id) {
            let layout = self.frame_layout(q.shuttle)?;
            let n = if q.align { layout.align } else { layout.size };
            return Some(Val::Regs(vec![self.iconst(n)]));
        }
        if let Some(c) = self.tables().constructs.get(&e.id).cloned() {
            let args: &'a [hir::Arg] = match &e.kind {
                ExprKind::Call { args, .. } => args,
                _ => &[],
            };
            return self.construct(e, &c, args);
        }
        match &e.kind {
            ExprKind::Lit(Lit::None) => self.none(ty, span),
            ExprKind::Lit(Lit::Bytes(bytes)) => self.bytes_value(bytes, ty, span),
            ExprKind::Lit(lit) => self.lit(lit, false, ty, span).map(Val::Regs),
            ExprKind::Local(l) => {
                let place = self.local_place(*l, span)?;
                let lty = self.local_ty(*l);
                let v = self.read_place(&place, lty, span)?;
                // Narrowed (`x is Percent`) to a gauge of a narrower float.
                match (self.float_of(lty), self.float_of(ty)) {
                    (Some(from), Some(to)) if from != to => {
                        let x = self.regs(v, lty, span)?[0];
                        Some(Val::Regs(vec![self.emit1(InstKind::FloatCast { to, a: x }, float_vty(to))]))
                    }
                    _ => Some(v),
                }
            }
            ExprKind::Def { def, .. } => match &self.p.def(*def).kind {
                DefKind::Const(_) => self.const_def(*def, ty, span),
                _ => self.unsupported(span, "this kind of value"),
            },
            ExprKind::Tuple(items) => self.tuple(e, items, ty),
            ExprKind::Block(block) => {
                let mut join = self.join(ty, span)?;
                join.adopt = true;
                self.block_into(block, &join)?;
                Some(self.join_get(&join))
            }
            ExprKind::If { cond, then, else_, deferred } => {
                self.if_expr(e, cond, then, else_.as_deref(), deferred.is_some(), ty)
            }
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, ty, span),
            ExprKind::Loop { body } => self.loop_expr(e, body, ty),
            ExprKind::While { cond, body } => {
                self.while_expr(e, cond, body);
                self.live().then(|| Val::Regs(Vec::new()))
            }
            ExprKind::For { is_const: true, body, .. } => {
                for id in self.tables().unrolled.get(&e.id).cloned().unwrap_or_default() {
                    let saved = std::mem::replace(&mut self.f.body, id);
                    let join = self.join(self.tcx().unit, span);
                    let done = join.and_then(|j| self.block_into(body, &j));
                    self.f.body = saved;
                    done?;
                }
                Some(Val::Regs(Vec::new()))
            }
            ExprKind::For { pat, iter, body, is_loom: false, .. } => {
                self.for_expr(e, pat, iter, body)?;
                self.live().then(|| Val::Regs(Vec::new()))
            }
            ExprKind::Break { target, value } => {
                self.f.span = Some(span);
                self.break_to(*target, value.as_deref())
            }
            ExprKind::Continue { target } => {
                self.f.span = Some(span);
                let i = self.f.loops.iter().rposition(|l| l.id == *target)?;
                let (to, scopes) = (self.f.loops[i].cont, self.f.loops[i].scopes);
                self.leave_scopes(scopes);
                if self.live() {
                    self.jump(to);
                }
                None
            }
            ExprKind::Return(value) => {
                self.f.span = Some(span);
                let val = match value {
                    Some(v) => self.expr(v)?,
                    None => Val::Regs(Vec::new()),
                };
                self.ret_val(val, span)?;
                None
            }
            ExprKind::Assign { target, op, value } => {
                self.f.span = Some(span);
                self.assign(target, *op, value)?;
                Some(Val::Regs(Vec::new()))
            }
            ExprKind::Call { callee, args } => self.call(e, callee, args, ty),
            ExprKind::Unary { op: UnaryOp::Alloc, operand } => self.alloc_expr(e.id, operand, ty, span),
            ExprKind::Unary { op, operand } => self.unary(*op, operand, ty, span).map(Val::Regs),
            ExprKind::Deref(inner) => {
                let v = self.expr(inner)?;
                let ptr = self.coerced_ty(inner);
                self.pointee(&v, ptr, span).map(|(x, _)| x)
            }
            ExprKind::Binary { op: BinaryOp::And | BinaryOp::Or, .. } => self.bool_value(e).map(Val::Regs),
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, ty, span).map(Val::Regs),
            ExprKind::Chain { .. } | ExprKind::Is { .. } => self.bool_value(e).map(Val::Regs),
            ExprKind::Cast { expr, kind, .. } => self.cast(e, expr, *kind, ty).map(Val::Regs),
            ExprKind::Field { .. } | ExprKind::LeadingDot(_) | ExprKind::ConstField { .. } => self.field(e),
            ExprKind::Assoc { .. } => match self.tables().assoc_values.get(&e.id).copied() {
                Some(crate::typeck::AssocValue::Int(v)) => self.int_value(v, ty, span).map(Val::Regs),
                Some(crate::typeck::AssocValue::Const(def)) => self.const_def(def, ty, span),
                None => self.unsupported(span, "this kind of value"),
            },
            ExprKind::DotBlock { subject, body } => self.dot_block(e, subject, body),
            ExprKind::Try(inner) => self.try_expr(e, inner),
            ExprKind::Fail(value) => {
                self.f.span = Some(span);
                self.fail_expr(e, value)
            }
            ExprKind::Mend { expr, handler, .. } => self.mend_expr(e, expr, handler, ty),
            // Checked while compiling: nothing is left to run.
            ExprKind::Const(inner) if crate::typeck::assert_args(self.p, inner).is_some() => {
                Some(Val::Regs(Vec::new()))
            }
            ExprKind::Const(inner) => self.const_expr(e, inner),
            ExprKind::Template(parts) => self.template_string(e, parts),
            ExprKind::Array(items) => self.array_lit(e, items),
            ExprKind::ArrayRepeat { value, count } => self.array_repeat(e, value, count),
            ExprKind::Index { base, args } => self.index(e, base, args),
            ExprKind::Map(pairs) => self.map_lit(e, pairs),
            ExprKind::Lambda { .. } => self.lambda(e),
            ExprKind::Range { lo, hi, inclusive } => self.range_value(lo.as_deref(), hi.as_deref(), *inclusive),
            ExprKind::Intrinsic(hir::Intrinsic::Target) => self.target_value(ty, span),
            ExprKind::Intrinsic(hir::Intrinsic::Heap) => Some(Val::Regs(Vec::new())),
            ExprKind::Yield { place: None, value } => self.yield_expr(value, span),
            ExprKind::Yield { value, .. } => self.yield_place(value, span),
            ExprKind::Pull => self.pull_expr(e, span),
            ExprKind::Loom { args, body, group } => self.loom_expr(e, args, body, *group, ty),
            ExprKind::Thread { group, .. } => self.thread_expr(e, *group),
            _ => self.unsupported(span, "this expression"),
        }
    }

    /// `target`: the facts about what the program is built for.
    fn target_value(&mut self, ty: Ty, span: Span) -> Option<Val> {
        use crate::target::Target;
        let info = self.warp_info(ty)?;
        let mut vals = Vec::new();
        let mut tys = Vec::new();
        for f in &info.fields {
            let v = match f.name.as_str() {
                "pointer_bits" => self.iconst(u64::from(self.tcx().target.pointer_bits)),
                name => {
                    // A unit case: its tag, the case's place in the warp.
                    let case = match name {
                        "os" => Target::OS,
                        "arch" => Target::ARCH,
                        _ => Target::ENDIAN,
                    };
                    let tag = self.warp_info(f.ty).and_then(|w| w.cases.iter().position(|c| c.name == case));
                    self.iconst(tag.unwrap_or(0) as u64)
                }
            };
            vals.push(Val::Regs(vec![v]));
            tys.push(f.ty);
        }
        self.aggregate(ty, &tys, vals, span)
    }

    /// `b"GET"`: an array of the bytes.
    fn bytes_value(&mut self, bytes: &[u8], ty: Ty, span: Span) -> Option<Val> {
        let dst = self.temp(ty, span)?;
        if !bytes.is_empty() {
            let text = self.str_lit(bytes);
            self.emit(InstKind::Copy { dst, src: text[0], size: bytes.len() as u64 }, &[]);
        }
        Some(Val::Mem(dst))
    }

    /// `(a, b, …)`.
    fn tuple(&mut self, e: &'a hir::Expr, items: &'a [hir::Expr], ty: Ty) -> Option<Val> {
        let span = e.span;
        let mut vals = Vec::with_capacity(items.len());
        for item in items {
            vals.push(self.expr(item)?);
        }
        let item_tys: Vec<Ty> = items.iter().map(|i| self.coerced_ty(i)).collect();
        self.aggregate(ty, &item_tys, vals, span)
    }

    /// A value of `ty` (a tuple or a record) made of `vals`, one per field
    /// of types `tys` in declaration order, each owned from here.
    pub(super) fn aggregate(&mut self, ty: Ty, tys: &[Ty], vals: Vec<Val>, span: Span) -> Option<Val> {
        match self.repr_of(ty, span)? {
            super::Repr::Regs(_) => {
                let mut out = Vec::new();
                for (v, &t) in vals.into_iter().zip(tys) {
                    out.extend(self.regs(v, t, span)?);
                }
                Some(Val::Regs(out))
            }
            super::Repr::Mem { .. } => {
                let offsets = self.field_offsets(ty, span)?;
                let ptr = self.temp(ty, span)?;
                for ((v, &t), off) in vals.into_iter().zip(tys).zip(offsets) {
                    self.move_to(ptr, off as i64, v, t, span)?;
                }
                self.own_temp(ptr, ty);
                Some(Val::Mem(ptr))
            }
        }
    }

    /// The offset of each field of a tuple or a record, in declaration order.
    pub(super) fn field_offsets(&mut self, ty: Ty, span: Span) -> Option<Vec<u64>> {
        match self.layout(ty).map(|l| l.shape) {
            Some(crate::ty::layout::Shape::Fields(offsets)) => Some(offsets),
            _ => self.internal(span, "the fields of something without fields"),
        }
    }

    // ------------------------------------------------------------ constants

    /// An integer as a value of `ty`: an integer, or a float when the
    /// literal became one (`let x: f64 = 0`).
    pub(super) fn int_value(&mut self, v: i128, ty: Ty, span: Span) -> Option<Vals> {
        if let Some(f) = self.float_of(ty) {
            return Some(vec![self.fconst(v as f64, f)]);
        }
        match self.tcx().kind(self.tcx().degauge(ty)) {
            TyKind::Int(_) | TyKind::Bool | TyKind::Char => Some(vec![self.iconst(v as u64)]),
            TyKind::Optional(inner) => {
                let (inner, opt) = (*inner, ty);
                let vals = self.int_value(v, inner, span)?;
                let v = self.some(opt, inner, Val::Regs(vals), span)?;
                self.regs(v, opt, span)
            }
            _ => {
                let shown = self.tcx().display(self.p, ty);
                self.unsupported(span, &format!("values of type `{shown}`"))
            }
        }
    }

    pub(super) fn fconst(&mut self, v: f64, ty: FloatTy) -> Value {
        match ty {
            FloatTy::F32 => self.emit1(InstKind::Fconst(u64::from((v as f32).to_bits())), VTy::F32),
            FloatTy::F64 => self.emit1(InstKind::Fconst(v.to_bits()), VTy::F64),
        }
    }

    pub(super) fn lit(&mut self, lit: &Lit, negative: bool, ty: Ty, span: Span) -> Option<Vals> {
        match lit {
            Lit::Int(i) => {
                let v = if negative { -(i.value as i128) } else { i.value as i128 };
                self.int_value(v, ty, span)
            }
            Lit::Float(f) => {
                let v = if negative { -f.value } else { f.value };
                let Some(ft) = self.float_of(ty) else { return self.unsupported(span, "this literal") };
                Some(vec![self.fconst(v, ft)])
            }
            Lit::Char(c) => Some(vec![self.iconst(*c as u64)]),
            Lit::Bool(b) => Some(vec![self.iconst(u64::from(*b))]),
            Lit::Str(s) if matches!(self.tcx().kind(ty), TyKind::Str) => Some(self.str_lit(s.as_bytes())),
            Lit::Str(_) => self.unsupported(span, "text literals of this type"),
            Lit::Bytes(_) => self.unsupported(span, "byte strings"),
            Lit::None => {
                let v = self.none(ty, span)?;
                self.regs(v, ty, span)
            }
        }
    }

    fn const_val(&mut self, v: ConstVal, ty: Ty, span: Span) -> Option<Vals> {
        match v {
            ConstVal::Int(v) => self.int_value(v, ty, span),
            ConstVal::Bool(b) => Some(vec![self.iconst(u64::from(b))]),
            ConstVal::Str(s) => {
                let text = self.tcx().sym_str(s).as_bytes().to_vec();
                Some(self.str_lit(&text))
            }
            ConstVal::Unit(case) => {
                // A unit case of a sum with no payloads: its tag.
                let info = self.warp_info(ty);
                match info.and_then(|i| i.cases.iter().position(|c| c.def == case)) {
                    Some(tag) if self.in_regs(ty) => Some(vec![self.iconst(tag as u64)]),
                    _ => self.unsupported(span, "cases"),
                }
            }
            ConstVal::Field { .. } | ConstVal::Case { .. } => {
                let code = loom_syntax::diag::codes::K0111;
                if !self.diags.iter().any(|d| d.code == code && d.labels.first().is_some_and(|l| l.span == span)) {
                    self.diags.push(
                        loom_syntax::Diagnostic::error(code, "reflection only exists while compiling")
                            .primary(span, "needed when the program runs")
                            .help("read what you need from it at compile time: `f.name`, `self.(f)`, `const for f in T.fields`"),
                    );
                }
                None
            }
        }
    }

    /// A constant's value: untyped ones are literals; typed ones are
    /// computed while compiling, which Treadle doesn't read yet.
    pub(super) fn const_def(&mut self, def: crate::hir::DefId, ty: Ty, span: Span) -> Option<Val> {
        let results = &self.typed.results;
        if let Some(&v) = results.untyped_consts.get(&def) {
            return self.int_value(v, ty, span).map(Val::Regs);
        }
        if let Some(&v) = results.untyped_floats.get(&def) {
            let Some(ft) = self.float_of(ty) else { return self.unsupported(span, "this constant") };
            return Some(Val::Regs(vec![self.fconst(v, ft)]));
        }
        self.read_const(super::consts::ConstKey::Def(def), span)
    }

    // ------------------------------------------------------------ statements and blocks

    /// A block's statements, its value into `join`.
    pub(super) fn block_into(&mut self, block: &'a Block, join: &Join) -> Option<()> {
        let depth = self.open_scope();
        let start = self.scope_start(depth);
        let done = self.block_stmts(block, join, start);
        self.close_scope(depth);
        done
    }

    fn block_stmts(&mut self, block: &'a Block, join: &Join, start: usize) -> Option<()> {
        // What comes after the block is reported where it was before it.
        let span = self.f.span;
        let done = self.block_stmts_in(block, join, start);
        self.f.span = span;
        done
    }

    fn block_stmts_in(&mut self, block: &'a Block, join: &Join, start: usize) -> Option<()> {
        let Some((last, rest)) = block.stmts.split_last() else {
            return self.join_put(join, Val::Regs(Vec::new()), start, self.here());
        };
        for stmt in rest {
            self.stmt(stmt)?;
            if !self.live() {
                return None;
            }
        }
        let val = match &last.kind {
            StmtKind::Expr(tail) if !matches!(tail.kind, ExprKind::Assign { .. }) => {
                self.f.span = Some(last.span);
                self.expr(tail)?
            }
            _ => {
                self.stmt(last)?;
                if !self.live() {
                    return None;
                }
                // An assignment's `()`, converted as the block's value is
                // (into `() ! E`, say).
                match &last.kind {
                    StmtKind::Expr(tail) => self.coerce(tail, Val::Regs(Vec::new()))?,
                    _ => Val::Regs(Vec::new()),
                }
            }
        };
        if !self.live() {
            return None;
        }
        self.join_put(join, val, start, last.span)
    }

    /// A statement, with its temporaries dropped at its end.
    fn stmt(&mut self, stmt: &'a hir::Stmt) -> Option<()> {
        self.f.span = Some(stmt.span);
        let depth = self.open_scope();
        let saved = self.f.stmt_start.replace(depth);
        let done = self.stmt_inner(stmt);
        self.f.stmt_start = saved;
        self.close_scope(depth);
        done
    }

    fn stmt_inner(&mut self, stmt: &'a hir::Stmt) -> Option<()> {
        match &stmt.kind {
            StmtKind::Let { pat, init, .. } => {
                let val = self.expr(init)?;
                let ty = self.tables().pat_ty.get(&pat.id).copied().unwrap_or_else(|| self.coerced_ty(init));
                self.bind_irrefutable(pat, val, self.tcx().degauge(ty))
            }
            StmtKind::Expr(e) => self.expr(e).map(drop),
            StmtKind::Item(_) => Some(()),
            StmtKind::Defer { binding, body } => self.defer(*binding, body, stmt.span),
            StmtKind::Spool { local, init } => {
                let val = self.expr(init)?;
                self.bind_local(*local, val, super::pat::Binding::Owned, stmt.span)
            }
            // The spool under another name: a view of where it is.
            StmtKind::Given { local, value } => {
                let Some(local) = local else { return Some(()) };
                // A spool binding is lent: a view of where it is. Anything
                // else is the given's own, like a `let`.
                if let hir::ExprKind::Local(l) = &value.kind
                    && self.p.local(*l).kind == crate::hir::LocalKind::Spool
                {
                    let (place, _) = self.place(value)?;
                    let at = self.place_addr(&place, stmt.span)?;
                    self.bind_ref(*local, at);
                    return Some(());
                }
                let val = self.expr(value)?;
                self.bind_local(*local, val, super::pat::Binding::Owned, stmt.span)
            }
        }
    }

    /// `defer body`, or `defer x => body`: a cleanup of the enclosing
    /// block, from here on.
    fn defer(&mut self, binding: Option<crate::hir::LocalId>, body: &'a hir::Expr, span: Span) -> Option<()> {
        let flag = match binding {
            Some(l) => {
                let slot = self.raw_slot(1, 1);
                let ptr = self.slot_addr(slot);
                let one = self.iconst(1);
                self.store(crate::heddle::Mem::U8, ptr, 0, one);
                self.f.defer_flags.entry(l).or_default().push(slot);
                Some(slot)
            }
            None => None,
        };
        let tables = self.f.body;
        self.add_scope_cleanup(super::cleanup::CleanupKind::Defer { body, tables, flag, span });
        Some(())
    }

    // ------------------------------------------------------------ if and match

    fn if_expr(
        &mut self,
        e: &'a hir::Expr,
        cond: &'a hir::Expr,
        then: &'a Block,
        else_: Option<&'a hir::Expr>,
        is_const: bool,
        ty: Ty,
    ) -> Option<Val> {
        let join = self.join(ty, e.span)?;
        if is_const {
            // `const if`: only the branch taken.
            match (self.tables().const_ifs.get(&e.id).copied(), else_) {
                (Some(true), _) => self.block_into(then, &join)?,
                (Some(false), Some(else_)) => self.expr_into(else_, &join)?,
                _ => {
                    let from = self.f.cleanups.len();
                    self.join_put(&join, Val::Regs(Vec::new()), from, e.span)?;
                }
            }
            return self.live().then(|| self.join_get(&join));
        }
        let (t, f, done) = (self.new_block(), self.new_block(), self.new_block());
        self.branch(cond, t, f)?;
        self.enter(t);
        if self.live() && self.block_into(then, &join).is_some() && self.live() {
            self.jump(done);
        }
        self.enter(f);
        if self.live() {
            let put = match else_ {
                Some(else_) => self.expr_into(else_, &join),
                None => {
                    let from = self.f.cleanups.len();
                    self.join_put(&join, Val::Regs(Vec::new()), from, e.span)
                }
            };
            if put.is_some() && self.live() {
                self.jump(done);
            }
        }
        self.enter(done);
        self.live().then(|| self.join_get(&join))
    }

    /// `e`'s value into `join`.
    pub(super) fn expr_into(&mut self, e: &'a hir::Expr, join: &Join) -> Option<()> {
        if let ExprKind::Block(b) = &e.kind
            && !self.tables().coercions.contains_key(&e.id)
        {
            return self.block_into(b, join);
        }
        let from = self.f.cleanups.len();
        let v = self.expr(e)?;
        if !self.live() {
            return None;
        }
        self.join_put(join, v, from, e.span)
    }

    fn match_expr(&mut self, scrutinee: &'a hir::Expr, arms: &'a [hir::Arm], ty: Ty, span: Span) -> Option<Val> {
        // What comes after the arms is reported where it was before them.
        let saved = self.f.span;
        let v = self.match_arms(scrutinee, arms, ty, span);
        self.f.span = saved;
        v
    }

    fn match_arms(&mut self, scrutinee: &'a hir::Expr, arms: &'a [hir::Arm], ty: Ty, span: Span) -> Option<Val> {
        let s = self.expr(scrutinee)?;
        let sty = self.coerced_ty(scrutinee);
        let join = self.join(ty, span)?;
        let done = self.new_block();
        for arm in arms {
            // An or-pattern with bindings is one arm per alternative, since
            // each alternative finds its bindings in different places.
            let alts: Vec<&'a hir::Pat> = match &arm.pat.kind {
                hir::PatKind::Or(alts) if super::pat::binds_anything(&arm.pat) => alts.iter().collect(),
                _ => vec![&arm.pat],
            };
            for pat in alts {
                if !self.live() {
                    break;
                }
                self.f.span = Some(arm.span);
                let (matched, next) = (self.new_block(), self.new_block());
                self.test_pat(pat, &s, sty, matched, next)?;
                self.enter(matched);
                if self.live() {
                    let depth = self.open_scope();
                    let start = self.scope_start(depth);
                    let mut ok = self.bind_pat(pat, &s, sty).is_some();
                    if ok && let Some(guard) = &arm.guard {
                        let body = self.new_block();
                        // The guard failing leaves the arm's scope too.
                        let fail = self.new_block();
                        ok = self.branch(guard, body, fail).is_some();
                        self.enter(fail);
                        if self.live() {
                            self.run_cleanups(start);
                            if self.live() {
                                self.jump(next);
                            }
                        }
                        self.enter(body);
                    }
                    if ok && self.live() {
                        let from = self.f.cleanups.len();
                        if let Some(v) = self.expr(&arm.body)
                            && self.live()
                            && self.join_put(&join, v, from.min(start), arm.body.span).is_some()
                        {
                            self.run_cleanups(start);
                            if self.live() {
                                self.jump(done);
                            }
                        }
                    }
                    // Control has left the arm by now.
                    self.f.cur = None;
                    self.close_scope(depth);
                }
                self.enter(next);
            }
        }
        if self.live() {
            // Exhaustiveness is checked: this can't happen.
            self.terminate(Term::Crash(0));
        }
        self.enter(done);
        self.live().then(|| self.join_get(&join))
    }

    // ------------------------------------------------------------ conditions

    /// Jumps to `t` if `cond` holds and to `f` if not. `and`, `or`, `not`,
    /// comparisons and `is` become branches rather than `bool` values.
    pub(super) fn branch(&mut self, cond: &'a hir::Expr, t: super::BlockId, f: super::BlockId) -> Option<()> {
        match &cond.kind {
            ExprKind::Binary { op: BinaryOp::And, lhs, rhs } => {
                let mid = self.new_block();
                self.branch(lhs, mid, f)?;
                self.enter(mid);
                if self.live() {
                    self.branch(rhs, t, f)?;
                }
                Some(())
            }
            ExprKind::Binary { op: BinaryOp::Or, lhs, rhs } => {
                let mid = self.new_block();
                self.branch(lhs, t, mid)?;
                self.enter(mid);
                if self.live() {
                    self.branch(rhs, t, f)?;
                }
                Some(())
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => self.branch(operand, f, t),
            ExprKind::Is { expr, pat } => {
                let v = self.expr(expr)?;
                let ty = self.coerced_ty(expr);
                let matched = self.new_block();
                self.test_pat(pat, &v, ty, matched, f)?;
                self.enter(matched);
                if self.live() {
                    self.bind_pat(pat, &v, ty)?;
                    self.jump(t);
                }
                Some(())
            }
            ExprKind::Chain { first, rest } => {
                let operands: Vec<&hir::Expr> =
                    std::iter::once(first.as_ref()).chain(rest.iter().map(|(_, e)| e)).collect();
                let ty = self.common_ty(&operands);
                let mut prev = self.value(first)?;
                for (op, e) in rest {
                    let next = self.value(e)?;
                    let c = self.compare(*op, &prev, &next, ty, e.span)?;
                    let cont = self.new_block();
                    self.br(c, cont, f);
                    self.enter(cont);
                    prev = next;
                }
                self.jump(t);
                Some(())
            }
            _ if !self.tables().coercions.contains_key(&cond.id)
                && matches!(cond.kind, ExprKind::Lit(Lit::Bool(_))) =>
            {
                let ExprKind::Lit(Lit::Bool(b)) = cond.kind else { unreachable!() };
                self.jump(if b { t } else { f });
                Some(())
            }
            _ => {
                let v = self.value(cond)?;
                self.br(v[0], t, f);
                Some(())
            }
        }
    }

    /// A condition (`and`, `or`, `is`, a chain) as a `bool` value.
    fn bool_value(&mut self, e: &'a hir::Expr) -> Option<Vals> {
        let var = self.new_var(VTy::Int);
        let (t, f, join) = (self.new_block(), self.new_block(), self.new_block());
        self.branch(e, t, f)?;
        for (b, v) in [(t, 1), (f, 0)] {
            self.enter(b);
            if self.live() {
                let c = self.iconst(v);
                self.write(var, c);
                self.jump(join);
            }
        }
        self.enter(join);
        if !self.live() {
            return None;
        }
        Some(vec![self.read(var)])
    }

    // ------------------------------------------------------------ loops

    /// Leaves the scopes opened after the first `scopes`, running their
    /// cleanups (control goes on elsewhere).
    pub(super) fn leave_scopes(&mut self, scopes: usize) {
        let from = self.f.scopes.get(scopes).copied().unwrap_or(self.f.cleanups.len());
        self.run_cleanups(from);
    }

    fn break_to(&mut self, target: hir::HirId, value: Option<&'a hir::Expr>) -> Option<Val> {
        let i = self.f.loops.iter().rposition(|l| l.id == target)?;
        let (brk, scopes) = (self.f.loops[i].brk, self.f.loops[i].brk_scopes);
        if let Some(value) = value {
            let from = self.f.scopes.get(scopes).copied().unwrap_or(self.f.cleanups.len());
            let v = self.expr(value)?;
            let ty = self.coerced_ty(value);
            match self.f.loops[i].result.clone() {
                LoopResult::Vars(vars) => {
                    let vals = self.regs(v, ty, value.span)?;
                    self.write_all(&vars, &vals);
                }
                LoopResult::Slot(slot) => {
                    let join = super::expr::Join {
                        ty,
                        kind: JoinKind::Mem { addr: self.new_var(VTy::Ptr), slot: Some(slot), owner: None },
                        adopt: false,
                        adopted: Cell::new(None),
                    };
                    // Everything a loop's value goes through is owned: it moves.
                    let _ = from;
                    self.move_to(slot, 0, v, join.ty, value.span)?;
                }
                LoopResult::None => {}
            }
        }
        self.leave_scopes(scopes);
        if self.live() {
            self.jump(brk);
        }
        None
    }

    fn loop_expr(&mut self, e: &'a hir::Expr, body: &'a Block, ty: Ty) -> Option<Val> {
        let result = match self.repr_of(ty, e.span)? {
            super::Repr::Regs(parts) => {
                let tys: Vec<VTy> = parts.iter().map(|p| p.vty).collect();
                LoopResult::Vars(self.new_vars(&tys))
            }
            super::Repr::Mem { .. } => {
                let ptr = self.temp(ty, e.span)?;
                self.own_temp(ptr, ty);
                self.f.temps.remove(&ptr);
                LoopResult::Slot(ptr)
            }
        };
        let (head, exit) = (self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let scopes = self.f.scopes.len();
        self.f.loops.push(LoopCtx {
            id: e.id,
            brk: exit,
            cont: head,
            result: result.clone(),
            scopes,
            brk_scopes: scopes,
        });
        let join = self.join(self.tcx().unit, e.span)?;
        let done = self.block_into(body, &join);
        if self.live() && done.is_some() {
            self.jump(head);
        }
        self.f.loops.pop();
        self.seal(head);
        self.enter(exit);
        if !self.live() {
            return None;
        }
        Some(match result {
            LoopResult::Vars(vars) => Val::Regs(self.read_all(&vars)),
            LoopResult::Slot(ptr) => Val::Mem(ptr),
            LoopResult::None => Val::Regs(Vec::new()),
        })
    }

    fn while_expr(&mut self, e: &'a hir::Expr, cond: &'a hir::Expr, body: &'a Block) {
        let (head, body_b, exit) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        // What the condition made (its temporaries, and what `is` bound,
        // which the body uses) is dropped at the end of each iteration, and
        // where the loop is left. A `break` or `continue` leaves its scope
        // too, so it's outside the scopes the loop's own.
        let scopes = self.f.scopes.len();
        let depth = self.open_scope();
        let start = self.scope_start(depth);
        let ok = self.branch(cond, body_b, exit).is_some();
        self.f.cur = None;
        if ok {
            self.enter(body_b);
            self.f.loops.push(LoopCtx {
                id: e.id,
                brk: exit,
                cont: head,
                result: LoopResult::None,
                scopes,
                brk_scopes: scopes,
            });
            if self.live()
                && let Some(join) = self.join(self.tcx().unit, e.span)
                && self.block_into(body, &join).is_some()
                && self.live()
            {
                self.run_cleanups(start);
                self.jump(head);
            }
            self.f.loops.pop();
        }
        self.seal(head);
        self.enter(exit);
        // On the way out through the false edge they're dropped here (a
        // `break` dropped them already: a slot that's been dropped holds
        // nothing, so this does nothing for it).
        self.close_scope(depth);
    }

    /// `a..b` as a value: its ends, and flags saying it has a high end
    /// (1) and includes it (2).
    fn range_value(&mut self, lo: Option<&'a hir::Expr>, hi: Option<&'a hir::Expr>, inclusive: bool) -> Option<Val> {
        let lo = match lo {
            Some(lo) => self.value(lo)?[0],
            None => self.iconst(0),
        };
        let hi_v = match hi {
            Some(hi) => self.value(hi)?[0],
            None => self.iconst(0),
        };
        let flags = u64::from(hi.is_some()) | (u64::from(inclusive && hi.is_some()) << 1);
        let flags = self.iconst(flags);
        Some(Val::Regs(vec![lo, hi_v, flags]))
    }

    fn for_expr(&mut self, e: &'a hir::Expr, pat: &'a hir::Pat, iter: &'a hir::Expr, body: &'a Block) -> Option<()> {
        if let Some(g) = self.inlinable_generator(e, iter) {
            return self.for_generator(e, pat, iter, body, g);
        }
        if let Some(p) = self.pipeline(iter) {
            return self.for_pipeline(e, pat, body, p);
        }
        let ity = self.ty_of(iter);
        if let Some(seq) = self.seq_of(ity) {
            return self.for_seq(e, pat, iter, body, seq);
        }
        if let Some(source) = self.tables().streams.get(&e.id).cloned() {
            return self.for_stream(e, pat, iter, body, Some(source));
        }
        if let TyKind::Range(elem) = *self.tcx().kind(ity)
            && !matches!(iter.kind, ExprKind::Range { lo: Some(_), .. })
        {
            return self.for_stream(e, pat, iter, body, Some(crate::typeck::IterSource::Range(elem)));
        }
        if !matches!(self.tcx().kind(ity), TyKind::Range(_)) {
            return self.for_stream(e, pat, iter, body, None);
        }
        let ExprKind::Range { lo: Some(lo), hi, inclusive } = &iter.kind else {
            return self.unsupported(iter.span, "`for` loops over anything but a range");
        };
        let TyKind::Range(elem) = *self.tcx().kind(self.ty_of(iter)) else {
            return self.unsupported(iter.span, "this kind of `for` loop");
        };
        let Some(int) = self.int_of(elem) else {
            return self.unsupported(iter.span, "`for` loops over ranges that aren't integers");
        };
        if let (Some(hi), false, Some((thread, group))) = (hi, *inclusive, loom_for_thread(body)) {
            let parts = LoomFor { pat, body, thread, group, int, elem };
            return self.loom_for_range(e, parts, lo, hi);
        }
        // The bounds, evaluated once, before the loop.
        let lo = self.value(lo)?[0];
        let hi = match hi {
            Some(hi) => Some(self.value(hi)?[0]),
            None => None,
        };
        self.range_loop(e, pat, body, lo, hi, *inclusive, int, elem)
    }

    /// `for pat in lo..hi { body }` (or `lo..=hi`, or `lo..`) over integers
    /// of type `int`, with the bounds worked out.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn range_loop(
        &mut self,
        e: &'a hir::Expr,
        pat: &'a hir::Pat,
        body: &'a Block,
        lo: Value,
        hi: Option<Value>,
        inclusive: bool,
        int: IntTy,
        elem: Ty,
    ) -> Option<()> {
        let i = self.new_var(VTy::Int);
        self.write(i, lo);
        let (head, body_b, step, exit) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        match hi {
            Some(hi) => {
                let cmp = if inclusive { Cmp::Le } else { Cmp::Lt };
                let c = self.emit1(InstKind::ICmp { cmp, unsigned: !int.signed, a: cur, b: hi }, VTy::Int);
                self.br(c, body_b, exit);
            }
            None => self.jump(body_b),
        }
        self.enter(body_b);
        let scopes = self.f.scopes.len();
        self.f.loops.push(LoopCtx {
            id: e.id,
            brk: exit,
            cont: step,
            result: LoopResult::None,
            scopes,
            brk_scopes: scopes,
        });
        let item = self.read(i);
        let depth = self.open_scope();
        let mut done = self.bind_irrefutable(pat, Val::Regs(vec![item]), elem);
        if done.is_some() {
            let join = self.join(self.tcx().unit, e.span)?;
            done = self.block_into(body, &join);
        }
        self.close_scope(depth);
        if self.live() && done.is_some() {
            self.jump(step);
        }
        self.f.loops.pop();
        self.enter(step);
        if self.live() {
            let cur = self.read(i);
            let one = self.iconst(1);
            match hi {
                // Stepping past the last value of an inclusive range at the
                // type's maximum mustn't overflow: stop before stepping.
                Some(hi) if inclusive => {
                    let at_end = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: cur, b: hi }, VTy::Int);
                    let inc = self.new_block();
                    self.br(at_end, exit, inc);
                    self.enter(inc);
                    let next = self.emit1(
                        InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: int, a: cur, b: one },
                        VTy::Int,
                    );
                    self.write(i, next);
                }
                Some(_) => {
                    let next = self.emit1(
                        InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: int, a: cur, b: one },
                        VTy::Int,
                    );
                    self.write(i, next);
                }
                // An open range traps when it runs past the type's maximum.
                None => {
                    let next = self.emit1(
                        InstKind::Arith { op: IntOp::Add, policy: Policy::Trap, ty: int, a: cur, b: one },
                        VTy::Int,
                    );
                    self.write(i, next);
                }
            }
            self.jump(head);
        }
        self.seal(head);
        self.enter(exit);
        Some(())
    }

    // ------------------------------------------------------------ assignment

    /// The place `e` names, and its type.
    pub(super) fn place(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        let (place, ty) = self.place_of_kind(e)?;
        // A pointer used as its target: the place it points at.
        if self.tables().coercions.get(&e.id) == Some(&Coercion::Deref) {
            let v = self.read_place(&place, ty, e.span)?;
            return self.pointee_place(&v, ty, e.span);
        }
        Some((place, ty))
    }

    fn place_of_kind(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        let span = e.span;
        match &e.kind {
            ExprKind::Deref(inner) => {
                let v = self.expr(inner)?;
                let ptr = self.coerced_ty(inner);
                self.pointee_place(&v, ptr, span)
            }
            ExprKind::Local(l) => {
                let place = self.local_place(*l, span)?;
                Some((place, self.local_ty(*l)))
            }
            ExprKind::Field { .. } | ExprKind::LeadingDot(_) | ExprKind::ConstField { .. } => self.field_place(e),
            ExprKind::Index { .. } => self.index_place(e),
            ExprKind::Call { .. } if self.tables().lends.contains_key(&e.id) => self.lend_place(e),
            _ => self.unsupported(span, "assigning to this"),
        }
    }

    fn assign(&mut self, target: &'a hir::Expr, op: AssignOp, value: &'a hir::Expr) -> Option<()> {
        match op {
            AssignOp::Assign => {
                // Every value is worked out before anything is assigned, so
                // `(a, b) = (b, a)` swaps.
                let v = self.expr(value)?;
                let ty = self.coerced_ty(value);
                self.assign_parts(target, v, ty)
            }
            AssignOp::Compound(bin) => {
                let ty = self.ty_of(target);
                let rhs = self.value(value)?;
                let (place, pty) = self.place(target)?;
                let cur = self.read_place(&place, pty, target.span)?;
                let cur = self.regs(cur, pty, target.span)?;
                let result = self.apply(bin, &cur, &rhs, ty, target.span)?;
                self.write_place(&place, pty, Val::Regs(result), target.span)
            }
        }
    }

    fn assign_parts(&mut self, target: &'a hir::Expr, v: Val, ty: Ty) -> Option<()> {
        match &target.kind {
            ExprKind::Tuple(items) => {
                // Each part, from the whole value, which was worked out first.
                let span = target.span;
                let whole = self.repr_of(ty, span)?;
                let item_tys: Vec<Ty> = match self.tcx().kind(ty) {
                    TyKind::Tuple(items) => items.clone(),
                    _ => return self.internal(span, "a tuple assigned from something else"),
                };
                let parts: Vec<Val> = match (&whole, v) {
                    (super::Repr::Regs(_), v) => {
                        let vals = self.regs(v, ty, span)?;
                        let counts = self.tuple_parts(ty, span)?;
                        let mut at = 0;
                        counts
                            .iter()
                            .map(|(_, n)| {
                                let part = Val::Regs(vals[at..at + n].to_vec());
                                at += n;
                                part
                            })
                            .collect()
                    }
                    (super::Repr::Mem { .. }, v) => {
                        let ptr = self.addr_of(v, ty, span)?;
                        let offsets = self.field_offsets(ty, span)?;
                        let mut out = Vec::new();
                        for (&t, off) in item_tys.iter().zip(offsets) {
                            out.push(self.load_val(ptr, off as i64, t, span)?);
                        }
                        out
                    }
                };
                for ((item, part), &t) in items.iter().zip(parts).zip(&item_tys) {
                    self.assign_parts(item, part, t)?;
                }
                Some(())
            }
            _ => {
                // `m[k] = v` adds `k` when it's missing.
                if self.map_assign(target, v.clone())?.is_some() {
                    return Some(());
                }
                let (place, pty) = self.place(target)?;
                // The old value goes first.
                if self.needs_drop(pty)
                    && let Place::At { ptr, off } = &place
                {
                    let at = self.offset(*ptr, *off);
                    self.drop_at(at, pty);
                }
                self.write_place(&place, pty, v, target.span)?;
                self.rearm(target);
                Some(())
            }
        }
    }

    /// A local given a new value has a new value for its fallback `defer`s
    /// to clean up.
    fn rearm(&mut self, target: &hir::Expr) {
        if let ExprKind::Local(l) = &target.kind {
            for slot in self.f.defer_flags.get(l).cloned().unwrap_or_default() {
                let ptr = self.slot_addr(slot);
                let one = self.iconst(1);
                self.store(crate::heddle::Mem::U8, ptr, 0, one);
            }
        }
    }

    /// If `e` moves a local's value away, its fallback `defer`s have nothing
    /// to clean up any more.
    pub(super) fn moved_away(&mut self, e: &hir::Expr) {
        let Some(&l) = self.tables().moves.get(&e.id) else { return };
        for slot in self.f.defer_flags.get(&l).cloned().unwrap_or_default() {
            let ptr = self.slot_addr(slot);
            let zero = self.iconst(0);
            self.store(crate::heddle::Mem::U8, ptr, 0, zero);
        }
    }

    // ------------------------------------------------------------ operators

    fn unary(&mut self, op: UnaryOp, operand: &'a hir::Expr, ty: Ty, span: Span) -> Option<Vals> {
        if op == UnaryOp::Neg
            && let ExprKind::Lit(lit @ (Lit::Int(_) | Lit::Float(_))) = &operand.kind
            && !self.tables().coercions.contains_key(&operand.id)
        {
            // `-128i8` is one constant.
            return self.lit(lit, true, ty, span);
        }
        let a = self.value(operand)?[0];
        let kind = match op {
            UnaryOp::Not => InstKind::Not(a),
            UnaryOp::Neg => match (self.float_of(ty), self.int_of(ty)) {
                (Some(f), _) => InstKind::FNeg { ty: f, a },
                (_, Some(i)) => InstKind::Neg { ty: i, a },
                _ => return self.unsupported(span, "this negation"),
            },
            UnaryOp::BitNot => match self.int_of(ty) {
                Some(i) => InstKind::BitNot { ty: i, a },
                None => return self.unsupported(span, "this `~`"),
            },
            UnaryOp::Alloc => return self.unsupported(span, "`^` allocation"),
        };
        let vty = if self.float_of(ty) == Some(FloatTy::F32) {
            VTy::F32
        } else if self.float_of(ty).is_some() {
            VTy::F64
        } else {
            VTy::Int
        };
        Some(vec![self.emit1(kind, vty)])
    }

    fn binary(&mut self, op: BinaryOp, lhs: &'a hir::Expr, rhs: &'a hir::Expr, ty: Ty, span: Span) -> Option<Vals> {
        if op.is_comparison() {
            let operand_ty = self.common_ty(&[lhs, rhs]);
            let a = self.expr(lhs)?;
            let b = self.expr(rhs)?;
            return Some(vec![self.compare_vals(op, a, b, operand_ty, span)?]);
        }
        let a = self.value(lhs)?;
        let b = self.value(rhs)?;
        self.apply(op, &a, &b, ty, span)
    }

    /// `a op b`, both already worked out, of type `ty` (for `op=` too).
    pub(super) fn apply(&mut self, op: BinaryOp, a: &[Value], b: &[Value], ty: Ty, span: Span) -> Option<Vals> {
        use BinaryOp as B;
        let (a, b) = (a[0], b[0]);
        if let Some(f) = self.float_of(ty) {
            let fop = match op {
                B::Add => FOp::Add,
                B::Sub => FOp::Sub,
                B::Mul => FOp::Mul,
                B::Div => FOp::Div,
                B::Rem => FOp::Rem,
                _ => return self.unsupported(span, "this operator on floats"),
            };
            let vty = if f == FloatTy::F32 { VTy::F32 } else { VTy::F64 };
            return Some(vec![self.emit1(InstKind::FArith { op: fop, ty: f, a, b }, vty)]);
        }
        let int_ty = match (op, self.tcx().kind(ty)) {
            (B::AddChecked | B::SubChecked | B::MulChecked, TyKind::Optional(inner)) => *inner,
            _ => ty,
        };
        let Some(t) = self.int_of(int_ty) else { return self.unsupported(span, "this operator") };
        let arith = |op, policy| InstKind::Arith { op, policy, ty: t, a, b };
        let kind = match op {
            B::Add => arith(IntOp::Add, Policy::Trap),
            B::Sub => arith(IntOp::Sub, Policy::Trap),
            B::Mul => arith(IntOp::Mul, Policy::Trap),
            B::AddWrap => arith(IntOp::Add, Policy::Wrap),
            B::SubWrap => arith(IntOp::Sub, Policy::Wrap),
            B::MulWrap => arith(IntOp::Mul, Policy::Wrap),
            B::AddSat => arith(IntOp::Add, Policy::Sat),
            B::SubSat => arith(IntOp::Sub, Policy::Sat),
            B::MulSat => arith(IntOp::Mul, Policy::Sat),
            B::Div => InstKind::Div { ty: t, a, b },
            B::Rem => InstKind::Rem { ty: t, a, b },
            B::Shl => InstKind::Shl { ty: t, a, b },
            B::Shr => InstKind::Shr { ty: t, a, b },
            B::BitAnd => InstKind::Bit { op: BitOp::And, a, b },
            B::BitOr => InstKind::Bit { op: BitOp::Or, a, b },
            B::BitXor => InstKind::Bit { op: BitOp::Xor, a, b },
            B::AddChecked | B::SubChecked | B::MulChecked => {
                let TyKind::Optional(inner) = *self.tcx().kind(ty) else { return None };
                let name = match op {
                    B::AddChecked => "int.add_chk",
                    B::SubChecked => "int.sub_chk",
                    _ => "int.mul_chk",
                };
                return self.checked_arith(name, a, b, ty, inner, span);
            }
            _ => return self.unsupported(span, "this operator"),
        };
        Some(vec![self.emit1(kind, VTy::Int)])
    }

    /// The type several operands are compared at: the widest of their
    /// types, which the narrower ones were widened to.
    fn common_ty(&self, es: &[&hir::Expr]) -> Ty {
        let mut best = self.coerced_ty(es[0]);
        for e in &es[1..] {
            let ty = self.coerced_ty(e);
            if let (Some(a), Some(b)) = (self.int_of(best), self.int_of(ty))
                && a.widens_to(b)
            {
                best = ty;
            }
        }
        best
    }

    /// `a op b` for a comparison, as a `bool`, of values in registers.
    pub(super) fn compare(&mut self, op: BinaryOp, a: &[Value], b: &[Value], ty: Ty, span: Span) -> Option<Value> {
        self.compare_vals(op, Val::Regs(a.to_vec()), Val::Regs(b.to_vec()), ty, span)
    }

    fn cast(&mut self, e: &'a hir::Expr, inner: &'a hir::Expr, kind: CastKind, ty: Ty) -> Option<Vals> {
        let span = e.span;
        let raw = self.tables().expr_ty.get(&e.id).copied().unwrap_or(ty);
        let target = match (kind, self.tcx().kind(raw)) {
            (CastKind::Opt, TyKind::Optional(t)) => *t,
            _ => raw,
        };
        if let TyKind::Gauge(def, base) = *self.tcx().kind(target) {
            return self.gauge_cast(inner, kind, def, base, target, ty, span);
        }
        if kind == CastKind::Opt {
            return self.cast_opt(inner, ty, target, span);
        }
        let source = self.coerced_ty(inner);
        let v = self.value(inner)?[0];
        self.convert(v, source, ty, kind, span)
    }

    /// `v`, a value of `source`, converted to `ty` by `as` (or `as%`,
    /// `as|`).
    pub(super) fn convert(&mut self, v: Value, source: Ty, ty: Ty, kind: CastKind, span: Span) -> Option<Vals> {
        let policy = match kind {
            CastKind::Wrap => Policy::Wrap,
            CastKind::Sat => Policy::Sat,
            _ => Policy::Trap,
        };
        let tcx = self.tcx();
        let (k, vty) = match (tcx.kind(source).clone(), tcx.kind(ty).clone()) {
            (TyKind::Int(s), TyKind::Int(t)) => {
                if s.widens_to(t) {
                    return Some(vec![v]);
                }
                (InstKind::IntCast { policy, to: t, from: s, a: v }, VTy::Int)
            }
            (TyKind::Int(s), TyKind::Float(f)) => (InstKind::IntToFloat { from: s, to: f, a: v }, float_vty(f)),
            (TyKind::Float(s), TyKind::Int(t)) => {
                (InstKind::FloatToInt { sat: kind == CastKind::Sat, from: s, to: t, a: v }, VTy::Int)
            }
            (TyKind::Float(s), TyKind::Float(t)) if s == t => return Some(vec![v]),
            (TyKind::Float(_), TyKind::Float(t)) => (InstKind::FloatCast { to: t, a: v }, float_vty(t)),
            (TyKind::Char, TyKind::Int(t)) => {
                // A char is a code point: a value of u21.
                let u21 = IntTy { signed: false, bits: 21, size: false };
                if u21.widens_to(t) {
                    return Some(vec![v]);
                }
                (InstKind::IntCast { policy: Policy::Trap, to: t, from: u21, a: v }, VTy::Int)
            }
            (TyKind::Bool, TyKind::Int(_)) => return Some(vec![v]),
            (TyKind::Int(s), TyKind::Char) => (InstKind::IntToChar { from: s, a: v }, VTy::Int),
            (TyKind::Char, TyKind::Char) | (TyKind::Bool, TyKind::Bool) => return Some(vec![v]),
            _ => return self.unsupported(span, "this conversion"),
        };
        Some(vec![self.emit1(k, vty)])
    }
}

impl<'a> Builder<'a> {
    /// `x as? T`: `some` of the value when it fits `T`, `none` otherwise.
    pub(super) fn cast_opt(&mut self, inner: &'a hir::Expr, opt: Ty, target: Ty, span: Span) -> Option<Vals> {
        let source = self.coerced_ty(inner);
        let v = self.value(inner)?[0];
        let tcx = self.tcx();
        let (name, v, spec) = match (tcx.kind(source).clone(), tcx.kind(target).clone()) {
            (TyKind::Int(s), TyKind::Int(t)) => {
                ("int.cast_chk", v, loom_isa::word::cast_spec(super::int_spec(t), super::int_spec(s)))
            }
            (TyKind::Char, TyKind::Int(t)) => {
                let u21 = IntTy { signed: false, bits: 21, size: false };
                ("int.cast_chk", v, loom_isa::word::cast_spec(super::int_spec(t), super::int_spec(u21)))
            }
            (TyKind::Float(f), TyKind::Int(t)) => {
                let v = if f == FloatTy::F32 {
                    self.emit1(InstKind::FloatCast { to: FloatTy::F64, a: v }, VTy::F64)
                } else {
                    v
                };
                ("float.to_int_chk", v, super::int_spec(t).pack())
            }
            (TyKind::Int(s), TyKind::Char) => ("int.to_char_chk", v, super::int_spec(s).pack()),
            // Every float converts (to infinity, past `f32`'s range).
            (TyKind::Float(_), TyKind::Float(_)) => {
                let v = self.convert(v, source, target, CastKind::Checked, span)?[0];
                let one = self.iconst(1);
                return self.opt_from(v, one, opt, target, span);
            }
            _ => return self.unsupported(span, "this `as?`"),
        };
        let spec = self.iconst(spec);
        let out = self.emit(InstKind::Native { name, imm: 0, args: vec![v, spec] }, &[VTy::Int, VTy::Int]);
        self.opt_from(out[0], out[1], opt, target, span)
    }
}

pub(super) fn float_vty(f: FloatTy) -> VTy {
    match f {
        FloatTy::F32 => VTy::F32,
        FloatTy::F64 => VTy::F64,
    }
}

/// Whether an expression reads any local (so it can't be moved to a call site).
pub(super) fn mentions_locals(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Local(_) => true,
        ExprKind::Lit(_) | ExprKind::Def { .. } | ExprKind::Const(_) => false,
        ExprKind::Unary { operand, .. } => mentions_locals(operand),
        ExprKind::Binary { lhs, rhs, .. } => mentions_locals(lhs) || mentions_locals(rhs),
        ExprKind::Tuple(items) => items.iter().any(mentions_locals),
        ExprKind::Cast { expr, .. } => mentions_locals(expr),
        ExprKind::Template(parts) => parts.iter().any(|p| match p {
            hir::TemplatePart::Interp { expr, .. } => mentions_locals(expr),
            hir::TemplatePart::Text(_) => false,
        }),
        _ => true,
    }
}

/// The thread and group of a `loom for`'s desugared body (`{ thread { … } }`,
/// design/vm.md §11.3), if `body` is one.
fn loom_for_thread(body: &Block) -> Option<(&hir::Expr, hir::LocalId)> {
    let [stmt] = body.stmts.as_slice() else { return None };
    let StmtKind::Expr(thread) = &stmt.kind else { return None };
    match &thread.kind {
        ExprKind::Thread { group: Some(group), item: true, .. } => Some((thread, *group)),
        _ => None,
    }
}
