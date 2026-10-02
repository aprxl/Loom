//! Hashing values: map and set keys, and a derived `x.hash()` (design/vm.md
//! D8; it's allowed to differ from Lua's, row P4 of §14).
//!
//! FNV-1a over 64-bit words: each part of a value is mixed in in turn,
//! text a word at a time after hashing its bytes (`str.hash`), a sum's tag
//! before its fields, a sequence's length before its elements. Values in
//! registers are hashed inline, ones in memory through glue (`hash[T]`).

use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, Glue};
use crate::heddle::{BitOp, Cmp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
use crate::ty::{IntTy, Ty, TyKind};

const U64: IntTy = IntTy { signed: false, bits: 64, size: false };
const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0100_0000_01b3;

impl<'a> Builder<'a> {
    /// `h` with `x` mixed in.
    fn mix(&mut self, h: Value, x: Value) -> Value {
        let a = self.emit1(InstKind::Bit { op: BitOp::Xor, a: h, b: x }, VTy::Int);
        let p = self.iconst(PRIME);
        self.emit1(InstKind::Arith { op: IntOp::Mul, policy: Policy::Wrap, ty: U64, a, b: p }, VTy::Int)
    }

    /// The hash of `v`, a value of `ty`.
    pub(super) fn hash_val(&mut self, v: Val, ty: Ty, span: Span) -> Option<Value> {
        let start = self.iconst(OFFSET);
        self.hash_into(start, v, ty, span)
    }

    /// `h` with the hash of `v` mixed in.
    fn hash_into(&mut self, h: Value, v: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        match self.tcx().kind(ty).clone() {
            TyKind::Int(_) | TyKind::Bool | TyKind::Char => {
                let x = self.regs(v, ty, span)?[0];
                Some(self.mix(h, x))
            }
            TyKind::Float(f) => {
                let x = self.regs(v, ty, span)?[0];
                // Its bits, through memory.
                let slot = self.raw_slot(8, 8);
                let at = self.slot_addr(slot);
                let zero = self.iconst(0);
                self.store(Mem::W64, at, 0, zero);
                let mem = if f == crate::ty::FloatTy::F32 { Mem::U32 } else { Mem::W64 };
                self.emit(InstKind::Store { mem, ptr: at, off: 0, val: x }, &[]);
                let bits = self.load(Mem::W64, at, 0, VTy::Int);
                Some(self.mix(h, bits))
            }
            TyKind::Unit => Some(h),
            TyKind::Str => {
                let (p, n) = self.text_of(v, ty, span)?;
                let x = self.emit(InstKind::Native { name: "str.hash", imm: 0, args: vec![p, n] }, &[VTy::Int])[0];
                Some(self.mix(h, x))
            }
            TyKind::Adt(..) if self.is_string(ty) => {
                let (p, n) = self.text_of(v, ty, span)?;
                let x = self.emit(InstKind::Native { name: "str.hash", imm: 0, args: vec![p, n] }, &[VTy::Int])[0];
                Some(self.mix(h, x))
            }
            TyKind::Adt(..) if self.warp_info(ty).is_some_and(|i| i.unit_only()) => {
                let t = self.tag_of(&v, ty, span)?;
                Some(self.mix(h, t))
            }
            _ if self.in_regs(ty) => self.hash_structure(h, v, ty, span),
            _ => {
                let func = self.glue(Glue::Hash, ty);
                let ptr = self.addr_of(v, ty, span)?;
                let x = self.emit(InstKind::Call { func, args: vec![ptr] }, &[VTy::Int])[0];
                Some(self.mix(h, x))
            }
        }
    }

    /// The glue `hash[T]`: the hash of the value at its parameter.
    pub(super) fn hash_glue(&mut self, ty: Ty, span: Span) -> Option<Vec<VTy>> {
        let ptr = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.push(ptr);
        let start = self.iconst(OFFSET);
        let h = self.hash_structure(start, Val::Mem(ptr), ty, span)?;
        self.terminate(Term::Ret(vec![h]));
        Some(vec![VTy::Int])
    }

    /// `h` with each part of `v` mixed in.
    fn hash_structure(&mut self, h: Value, v: Val, ty: Ty, span: Span) -> Option<Value> {
        let ty = self.tcx().degauge(ty);
        let r = self.new_var(VTy::Int);
        self.write(r, h);
        match self.tcx().kind(ty).clone() {
            TyKind::Tuple(_) => {
                for (x, t) in self.tuple_items(&v, ty, span)? {
                    let cur = self.read(r);
                    let next = self.hash_into(cur, x, t, span)?;
                    self.write(r, next);
                }
            }
            TyKind::Optional(inner) => {
                let present = self.is_some(&v, ty, span)?;
                let cur = self.mix(h, present);
                self.write(r, cur);
                let (yes, done) = (self.new_block(), self.new_block());
                self.br(present, yes, done);
                self.enter(yes);
                let x = self.unwrap_some(&v, ty, span)?;
                let next = self.hash_into(cur, x, inner, span)?;
                self.write(r, next);
                self.jump(done);
                self.enter(done);
            }
            TyKind::Result(..) | TyKind::Union(_) => {
                let members = match self.tcx().kind(ty).clone() {
                    TyKind::Result(t, e) => vec![t, e],
                    TyKind::Union(ms) => ms,
                    _ => Vec::new(),
                };
                let tag = self.tag_value(&v, ty, span)?;
                let cur = self.mix(h, tag);
                self.write(r, cur);
                let done = self.new_block();
                for (i, m) in members.into_iter().enumerate() {
                    let (yes, next) = (self.new_block(), self.new_block());
                    let k = self.iconst(i as u64);
                    let hit = self.ucmp(Cmp::Eq, tag, k);
                    self.br(hit, yes, next);
                    self.enter(yes);
                    let x = self.payload(&v, ty, m, span)?;
                    let after = self.hash_into(cur, x, m, span)?;
                    self.write(r, after);
                    self.jump(done);
                    self.enter(next);
                }
                self.jump(done);
                self.enter(done);
            }
            TyKind::Array(..) | TyKind::Adt(..) if self.seq_of(ty).is_some() => {
                let elem = self.seq_of(ty)?.elem();
                let (buf, len) = self.view_of(&v, ty, span)?;
                let cur = self.mix(h, len);
                self.write(r, cur);
                let zero = self.iconst(0);
                self.count_loop(zero, len, |this, i| {
                    let at = this.elem_at(buf, i, elem)?;
                    let x = this.load_val(at, 0, elem, span)?;
                    let cur = this.read(r);
                    let next = this.hash_into(cur, x, elem, span)?;
                    this.write(r, next);
                    Some(())
                })?;
            }
            TyKind::Adt(def, _) if !self.is_builtin(def) => {
                let info = self.warp_info(ty)?;
                let mut parts = Vec::new();
                if info.is_sum() {
                    let tag = self.tag_of(&v, ty, span)?;
                    let cur = self.mix(h, tag);
                    self.write(r, cur);
                    let done = self.new_block();
                    for (ci, case) in info.cases.iter().enumerate() {
                        let (yes, next) = (self.new_block(), self.new_block());
                        let k = self.iconst(ci as u64);
                        let hit = self.ucmp(Cmp::Eq, tag, k);
                        self.br(hit, yes, next);
                        self.enter(yes);
                        for f in case.fields.iter().flatten() {
                            let pos = self.field_pos(ty, Some(case.def), &f.key, span)?;
                            let x = self.field_of(&v, &pos, span)?;
                            let c = self.read(r);
                            let after = self.hash_into(c, x, pos.ty, span)?;
                            self.write(r, after);
                        }
                        self.jump(done);
                        self.enter(next);
                    }
                    self.jump(done);
                    self.enter(done);
                }
                for f in &info.fields {
                    parts.push(f.key.clone());
                }
                for key in parts {
                    let pos = self.field_pos(ty, None, &key, span)?;
                    let x = self.field_of(&v, &pos, span)?;
                    let c = self.read(r);
                    let after = self.hash_into(c, x, pos.ty, span)?;
                    self.write(r, after);
                }
            }
            _ => {
                let shown = self.tcx().display(self.p, ty);
                return self.unsupported(span, &format!("hashing values of type `{shown}`"));
            }
        }
        Some(self.read(r))
    }
}
