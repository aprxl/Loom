//! Maps and sets (design/vm.md §5.2, §10.13), in insertion order.
//!
//! A `Map[K, V]` (and a `Set[T]`, which is one with no values) is a header
//! of eight words: the entries' buffer (0), how many there are (1) and room
//! for (2), an index's buffer (3) and how many slots it has (4; a power of
//! two, or 0 for none yet), `used` (5), `head` (6) and `tail` (7). An entry
//! is a key's hash, the key and the value. The index is open addressing: a
//! slot is 0, or an entry's number plus one; it's kept at most half full.
//!
//! Removing an entry is cheap: its hash becomes 0 (a live entry's never is),
//! so lookups pass over it, and it stays in the entries (and the index)
//! until there are more removed than live entries, when the live ones move
//! down, in order, and the index is built again. The header has two counts:
//! `len` (word 1), the live entries, and `used` (word 5), the entries made,
//! removed ones included. Whatever walks the entries (a loop, `show`, `==`,
//! `copy`) first makes `used` equal `len` by moving them down early
//! (`compact`); only a drop walks them as they are, and so does a stream
//! (it skips the removed ones). While there are live entries, `head` is the
//! number of the oldest and `tail` the number of the newest plus one, kept
//! by every insert and removal: the oldest or the newest entry is read from
//! them (`Map.first`, `Map.last`, `Set.pop`), and taking either over and over
//! (a queue, a stack) does not look at the removed ones again. With none,
//! both are 0.
//!
//! A new entry goes after the removed ones, even when the newest live entry
//! is before them; then the removed entry just behind it (word 1 of its
//! bytes, which hold nothing of a removed entry) says where they start, so
//! that taking the new entry out again moves `tail` back past all of them
//! at once. Every removed entry has a 0 there unless it says so. (An entry
//! that is only a hash has no room for it: `tail` walks past them.)
//!
//! Keys hash structurally (`hash.rs`) and compare with `==` (a warp's own
//! `eq` when it writes one: row P9 of §14).

use loom_syntax::source::Span;

use super::Builder;
use super::value::{Place, Val};
use crate::heddle::{BitOp, Cmp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
use crate::hir::{self, ExprKind};
use crate::ty::{IntTy, Ty, TyKind};

const U64: IntTy = IntTy { signed: false, bits: 64, size: false };
/// "None": no entry.
const NONE: u64 = u64::MAX;

/// Where things are in a map's (or a set's) entries.
#[derive(Clone, Copy, Debug)]
pub(super) struct Coll {
    pub key: Ty,
    pub val: Option<Ty>,
    pub koff: u64,
    pub voff: u64,
    pub esize: u64,
    pub ealign: u64,
}

fn round_up(n: u64, a: u64) -> u64 {
    n.div_ceil(a) * a
}

impl<'a> Builder<'a> {
    /// The layout of `ty`'s entries, if it's a map or a set.
    pub(super) fn coll_of(&mut self, ty: Ty) -> Option<Coll> {
        let ty = self.tcx().degauge(ty);
        let TyKind::Adt(def, args) = self.tcx().kind(ty).clone() else { return None };
        let (key, val) = match (self.p.def_path(def).as_str(), args.as_slice()) {
            ("std.map.Map", [k, v]) => (*k, Some(*v)),
            ("std.set.Set", [t]) => (*t, None),
            _ => return None,
        };
        let key = self.tcx().degauge(key);
        let val = val.map(|v| self.tcx().degauge(v));
        let (ks, ka) = self.size_align(key)?;
        let koff = round_up(8, ka);
        let (voff, end, va) = match val {
            Some(v) => {
                let (vs, va) = self.size_align(v)?;
                let voff = round_up(koff + ks, va);
                (voff, voff + vs, va)
            }
            None => (koff + ks, koff + ks, 1),
        };
        let ealign = 8u64.max(ka).max(va);
        Some(Coll { key, val, koff, voff, esize: round_up(end, ealign), ealign })
    }

    /// Whether `ty` is a map or a set.
    pub(super) fn coll_of_ty(&self, ty: Ty) -> bool {
        let ty = self.tcx().degauge(ty);
        matches!(self.tcx().kind(ty), TyKind::Adt(def, _)
            if matches!(self.p.def_path(*def).as_str(), "std.map.Map" | "std.set.Set"))
    }

    fn entry(&mut self, c: &Coll, buf: Value, i: Value) -> Value {
        self.emit1(InstKind::Index { base: buf, index: i, scale: c.esize }, VTy::Ptr)
    }

    fn hdr(&mut self, h: Value, off: i64) -> Value {
        self.load(Mem::W64, h, off, VTy::Int)
    }

    fn hdr_ptr(&mut self, h: Value, off: i64) -> Value {
        self.load(Mem::W64, h, off, VTy::Ptr)
    }

    /// A key's hash, as kept in its entry.
    fn key_hash(&mut self, c: &Coll, key: &Val, span: Span) -> Option<Value> {
        let h = self.hash_val(key.clone(), c.key, span)?;
        let z = self.finalize_hash(h);
        Some(self.nonzero(z))
    }

    /// `h`, or 1 for 0: a removed entry's hash is 0, so a live one's isn't.
    /// (The natives' `key_hash` does the same.)
    fn nonzero(&mut self, h: Value) -> Value {
        let r = self.new_var(VTy::Int);
        self.write(r, h);
        let zero = self.iconst(0);
        let is_zero = self.ucmp(Cmp::Eq, h, zero);
        let (fix, done) = (self.new_block(), self.new_block());
        self.br(is_zero, fix, done);
        self.enter(fix);
        let one = self.iconst(1);
        self.write(r, one);
        self.jump(done);
        self.enter(done);
        self.read(r)
    }

    /// The finalizer of a key's hash, which the native lookups
    /// (`methods.rs`' `finalize`) do the same: splitmix64's mixer. The
    /// index is the hash's low bits, and a product's low bits only depend
    /// on its operands' low bits, so keys that differ in their high bits
    /// (or are all multiples of a power of two) would start at the same few
    /// slots without it.
    fn finalize_hash(&mut self, h: Value) -> Value {
        let mut z = h;
        for (shift, mul) in [(30, 0xbf58_476d_1ce4_e5b9u64), (27, 0x94d0_49bb_1331_11eb)] {
            let k = self.iconst(shift);
            let hi = self.emit1(InstKind::Shr { ty: U64, a: z, b: k }, VTy::Int);
            let x = self.emit1(InstKind::Bit { op: BitOp::Xor, a: z, b: hi }, VTy::Int);
            let m = self.iconst(mul);
            z = self.emit1(InstKind::Arith { op: IntOp::Mul, policy: Policy::Wrap, ty: U64, a: x, b: m }, VTy::Int);
        }
        let k = self.iconst(31);
        let hi = self.emit1(InstKind::Shr { ty: U64, a: z, b: k }, VTy::Int);
        self.emit1(InstKind::Bit { op: BitOp::Xor, a: z, b: hi }, VTy::Int)
    }

    /// The number of the entry with `key` in the map at `header` (or
    /// `NONE`), and the key's hash. Text and integer keys (and others
    /// compared as one word) are looked up by a native; it hashes them as
    /// `key_hash` does.
    fn lookup(&mut self, c: &Coll, header: Value, key: &Val, span: Span) -> Option<(Value, Value)> {
        let key_ty = self.tcx().degauge(c.key);
        let text = matches!(self.tcx().kind(key_ty), TyKind::Str) || self.is_string(key_ty);
        let word = matches!(self.tcx().kind(key_ty), TyKind::Int(_) | TyKind::Char | TyKind::Bool)
            || self.warp_info(key_ty).is_some_and(|i| i.unit_only());
        if text || word {
            let (key_bytes, _) = self.size_align(key_ty)?;
            let layout = self.iconst(c.koff | (key_bytes << 16) | (c.esize << 32));
            let (name, mut args): (&'static str, Vec<Value>) = if text {
                let (p, n) = self.text_of(key.clone(), key_ty, span)?;
                ("map.find_str", vec![header, p, n])
            } else {
                ("map.find_int", vec![header, self.regs(key.clone(), key_ty, span)?[0]])
            };
            args.push(layout);
            let r = self.emit(InstKind::Native { name, imm: 0, args }, &[VTy::Int, VTy::Int]);
            return Some((r[0], r[1]));
        }
        let h = self.key_hash(c, key, span)?;
        let e = self.find_key(c, header, key, h, span)?;
        Some((e, h))
    }

    /// The number of the entry with `key` (hash `h`) in the map at
    /// `header`, or `NONE`.
    fn find_key(&mut self, c: &Coll, header: Value, key: &Val, h: Value, span: Span) -> Option<Value> {
        let r = self.new_var(VTy::Int);
        let none = self.iconst(NONE);
        self.write(r, none);
        let done = self.new_block();
        let slots = self.hdr(header, 32);
        let zero = self.iconst(0);
        let empty = self.ucmp(Cmp::Eq, slots, zero);
        let search = self.new_block();
        self.br(empty, done, search);
        self.enter(search);
        let ibuf = self.hdr_ptr(header, 24);
        let ebuf = self.hdr_ptr(header, 0);
        let one = self.iconst(1);
        let mask = self.uarith(IntOp::Sub, slots, one);
        let i = self.new_var(VTy::Int);
        let start = self.emit1(InstKind::Bit { op: BitOp::And, a: h, b: mask }, VTy::Int);
        self.write(i, start);
        let (head, occupied, same_hash, next) =
            (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let at = self.emit1(InstKind::Index { base: ibuf, index: cur, scale: 8 }, VTy::Ptr);
        let s = self.hdr(at, 0);
        let zero = self.iconst(0);
        let free = self.ucmp(Cmp::Eq, s, zero);
        self.br(free, done, occupied);
        self.enter(occupied);
        let one = self.iconst(1);
        let e = self.uarith(IntOp::Sub, s, one);
        let ent = self.entry(c, ebuf, e);
        let eh = self.hdr(ent, 0);
        let same = self.ucmp(Cmp::Eq, eh, h);
        self.br(same, same_hash, next);
        self.enter(same_hash);
        let k = self.load_val(ent, c.koff as i64, c.key, span)?;
        let eq = self.eq_vals(k, key.clone(), c.key, span)?;
        let found = self.new_block();
        self.br(eq, found, next);
        self.enter(found);
        self.write(r, e);
        self.jump(done);
        self.enter(next);
        let cur = self.read(i);
        let one = self.iconst(1);
        let n = self.uarith(IntOp::Add, cur, one);
        let n = self.emit1(InstKind::Bit { op: BitOp::And, a: n, b: mask }, VTy::Int);
        self.write(i, n);
        self.jump(head);
        self.seal(head);
        self.enter(done);
        Some(self.read(r))
    }

    /// Puts entry `e` (hash `h`) in the index, which has room.
    fn index_put(&mut self, header: Value, h: Value, e: Value) {
        let slots = self.hdr(header, 32);
        let ibuf = self.hdr_ptr(header, 24);
        let one = self.iconst(1);
        let mask = self.uarith(IntOp::Sub, slots, one);
        let i = self.new_var(VTy::Int);
        let start = self.emit1(InstKind::Bit { op: BitOp::And, a: h, b: mask }, VTy::Int);
        self.write(i, start);
        let (head, taken, free) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let at = self.emit1(InstKind::Index { base: ibuf, index: cur, scale: 8 }, VTy::Ptr);
        let s = self.hdr(at, 0);
        let zero = self.iconst(0);
        let empty = self.ucmp(Cmp::Eq, s, zero);
        self.br(empty, free, taken);
        self.enter(taken);
        let cur = self.read(i);
        let one = self.iconst(1);
        let n = self.uarith(IntOp::Add, cur, one);
        let n = self.emit1(InstKind::Bit { op: BitOp::And, a: n, b: mask }, VTy::Int);
        self.write(i, n);
        self.jump(head);
        self.seal(head);
        self.enter(free);
        let one = self.iconst(1);
        let v = self.uarith(IntOp::Add, e, one);
        self.store(Mem::W64, at, 0, v);
    }

    /// Builds the index again, with `slots` slots, from the entries.
    fn reindex(&mut self, c: &Coll, header: Value, slots: Value) {
        let esize = self.iconst(c.esize);
        self.emit(InstKind::Native { name: "map.reindex", imm: 0, args: vec![header, slots, esize] }, &[]);
    }

    /// Adds an entry (`key`, hash `h`, and `val` for a map), which isn't
    /// there yet: both are moved in.
    fn push_entry(&mut self, c: &Coll, header: Value, h: Value, key: Val, val: Option<Val>, span: Span) -> Option<()> {
        let used = self.hdr(header, 40);
        let one = self.iconst(1);
        let want = self.uarith(IntOp::Add, used, one);
        let (size, align) = (self.iconst(c.esize), self.iconst(c.ealign));
        self.emit(InstKind::Native { name: "mem.grow", imm: 0, args: vec![header, size, align, want] }, &[]);
        let ebuf = self.hdr_ptr(header, 0);
        let ent = self.entry(c, ebuf, used);
        self.store(Mem::W64, ent, 0, h);
        self.move_to(ent, c.koff as i64, key, c.key, span)?;
        if let (Some(v), Some(vt)) = (val, c.val) {
            self.move_to(ent, c.voff as i64, v, vt, span)?;
        }
        let len = self.hdr(header, 8);
        let one = self.iconst(1);
        let more = self.uarith(IntOp::Add, len, one);
        self.store(Mem::W64, header, 8, more);
        self.store(Mem::W64, header, 40, want);
        if c.esize >= 16 {
            let old = self.hdr(header, 56);
            let behind = self.ucmp(Cmp::Lt, old, used);
            let (hint, after) = (self.new_block(), self.new_block());
            self.br(behind, hint, after);
            self.enter(hint);
            let one = self.iconst(1);
            let last = self.uarith(IntOp::Sub, used, one);
            let ebuf = self.hdr_ptr(header, 0);
            let ent = self.entry(c, ebuf, last);
            self.store(Mem::W64, ent, 8, old);
            self.jump(after);
            self.enter(after);
        }
        self.store(Mem::W64, header, 56, want);
        let zero = self.iconst(0);
        let was_empty = self.ucmp(Cmp::Eq, len, zero);
        let (first, rest) = (self.new_block(), self.new_block());
        self.br(was_empty, first, rest);
        self.enter(first);
        self.store(Mem::W64, header, 48, used);
        self.jump(rest);
        self.enter(rest);
        // At most half full: else twice the slots (at least 8).
        let slots = self.hdr(header, 32);
        let two = self.iconst(2);
        let need = self.uarith(IntOp::Mul, want, two);
        let full = self.ucmp(Cmp::Gt, need, slots);
        let (grow, put, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(full, grow, put);
        self.enter(grow);
        let doubled = self.uarith(IntOp::Mul, slots, two);
        let eight = self.iconst(8);
        let new = self.umin_max(doubled, eight);
        self.reindex(c, header, new);
        self.jump(done);
        self.enter(put);
        self.index_put(header, h, used);
        self.jump(done);
        self.enter(done);
        Some(())
    }

    /// `max(a, b)` of two `u64`s.
    fn umin_max(&mut self, a: Value, b: Value) -> Value {
        let r = self.new_var(VTy::Int);
        self.write(r, a);
        let less = self.ucmp(Cmp::Lt, a, b);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(less, yes, done);
        self.enter(yes);
        self.write(r, b);
        self.jump(done);
        self.enter(done);
        self.read(r)
    }

    /// Takes entry `e` out, its key and value already taken (or dropped): its
    /// hash becomes 0, `head` and `tail` move past it if it was the oldest or
    /// the newest, and when more than half the entries made are removed ones,
    /// the live ones move down.
    fn remove_entry(&mut self, c: &Coll, header: Value, e: Value) {
        let ebuf = self.hdr_ptr(header, 0);
        let ent = self.entry(c, ebuf, e);
        let zero = self.iconst(0);
        self.store(Mem::W64, ent, 0, zero);
        if c.esize >= 16 {
            self.store(Mem::W64, ent, 8, zero);
        }
        let len = self.hdr(header, 8);
        let one = self.iconst(1);
        let left = self.uarith(IntOp::Sub, len, one);
        self.store(Mem::W64, header, 8, left);
        let none_left = self.ucmp(Cmp::Eq, left, zero);
        let (empty, some_left, ends) = (self.new_block(), self.new_block(), self.new_block());
        self.br(none_left, empty, some_left);
        self.enter(empty);
        self.store(Mem::W64, header, 48, zero);
        self.store(Mem::W64, header, 56, zero);
        self.jump(ends);
        self.enter(some_left);
        let head = self.hdr(header, 48);
        let was_head = self.ucmp(Cmp::Eq, e, head);
        let (forward, after_head) = (self.new_block(), self.new_block());
        self.br(was_head, forward, after_head);
        self.enter(forward);
        let next = self.live_from(c, ebuf, e, true);
        self.store(Mem::W64, header, 48, next);
        self.jump(after_head);
        self.enter(after_head);
        let tail = self.hdr(header, 56);
        let last = self.uarith(IntOp::Sub, tail, one);
        let was_tail = self.ucmp(Cmp::Eq, e, last);
        let (back, after_tail) = (self.new_block(), self.new_block());
        self.br(was_tail, back, after_tail);
        self.enter(back);
        let newest = self.live_from(c, ebuf, e, false);
        let past = self.uarith(IntOp::Add, newest, one);
        self.store(Mem::W64, header, 56, past);
        self.jump(after_tail);
        self.enter(after_tail);
        self.jump(ends);
        self.enter(ends);
        let used = self.hdr(header, 40);
        let dead = self.uarith(IntOp::Sub, used, left);
        let many = self.ucmp(Cmp::Gt, dead, left);
        let eight = self.iconst(8);
        let past = self.ucmp(Cmp::Gt, used, eight);
        let (check, squeeze, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(many, check, done);
        self.enter(check);
        self.br(past, squeeze, done);
        self.enter(squeeze);
        self.compact(c, header);
        self.jump(done);
        self.enter(done);
    }

    /// The number of the first live entry after `e` (`up`) or before it,
    /// which there must be one of. Going down, a removed entry that says
    /// where a run of them starts is jumped over.
    fn live_from(&mut self, c: &Coll, ebuf: Value, e: Value, up: bool) -> Value {
        let at = self.new_var(VTy::Int);
        self.write(at, e);
        let (look, skip, found) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(look);
        self.switch_to(look);
        let cur = self.read(at);
        let one = self.iconst(1);
        let step = if up { IntOp::Add } else { IntOp::Sub };
        let moved = self.uarith(step, cur, one);
        self.write(at, moved);
        let ent = self.entry(c, ebuf, moved);
        let h = self.hdr(ent, 0);
        let zero = self.iconst(0);
        let live = self.ucmp(Cmp::Ne, h, zero);
        self.br(live, found, skip);
        self.enter(skip);
        if !up && c.esize >= 16 {
            let start = self.hdr(ent, 8);
            let says = self.ucmp(Cmp::Ne, start, zero);
            let (over, on) = (self.new_block(), self.new_block());
            self.br(says, over, on);
            self.enter(over);
            self.write(at, start);
            self.jump(look);
            self.enter(on);
        }
        self.jump(look);
        self.seal(look);
        self.enter(found);
        self.read(at)
    }

    /// Moves the live entries down over the removed ones, in order, and
    /// builds the index again: afterwards `used` is `len`. Nothing when
    /// none were removed.
    pub(super) fn compact(&mut self, c: &Coll, header: Value) {
        let (len, used) = (self.hdr(header, 8), self.hdr(header, 40));
        let same = self.ucmp(Cmp::Eq, len, used);
        let (work, done) = (self.new_block(), self.new_block());
        self.br(same, done, work);
        self.enter(work);
        let esize = self.iconst(c.esize);
        self.emit(InstKind::Native { name: "map.compact", imm: 0, args: vec![header, esize] }, &[]);
        self.jump(done);
        self.enter(done);
    }

    /// Drops every entry's key and value.
    fn drop_entries(&mut self, c: &Coll, header: Value) {
        let (kd, vd) = (self.needs_drop(c.key), c.val.is_some_and(|v| self.needs_drop(v)));
        if !kd && !vd {
            return;
        }
        // As they are: the removed ones (hash 0) were dropped when removed.
        let used = self.hdr(header, 40);
        let zero = self.iconst(0);
        let _ = self.count_loop(zero, used, |this, e| {
            let ebuf = this.hdr_ptr(header, 0);
            let ent = this.entry(c, ebuf, e);
            let h = this.hdr(ent, 0);
            let zero = this.iconst(0);
            let live = this.ucmp(Cmp::Ne, h, zero);
            let (go, next) = (this.new_block(), this.new_block());
            this.br(live, go, next);
            this.enter(go);
            let k = this.offset(ent, c.koff as i64);
            this.drop_at(k, c.key);
            if let Some(vt) = c.val {
                let v = this.offset(ent, c.voff as i64);
                this.drop_at(v, vt);
            }
            this.jump(next);
            this.enter(next);
            Some(())
        });
    }

    /// Drop glue for a map or a set.
    pub(super) fn drop_coll(&mut self, c: &Coll, header: Value) {
        self.drop_entries(c, header);
        self.free_buffer(header, c.esize, c.ealign);
        let slots = self.hdr(header, 32);
        let zero = self.iconst(0);
        let had = self.ucmp(Cmp::Ne, slots, zero);
        let (free, done) = (self.new_block(), self.new_block());
        self.br(had, free, done);
        self.enter(free);
        let ibuf = self.hdr_ptr(header, 24);
        let eight = self.iconst(8);
        let bytes = self.uarith(IntOp::Mul, slots, eight);
        self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![ibuf, bytes, eight] }, &[]);
        self.jump(done);
        self.enter(done);
    }

    /// Copy glue for a map or a set: its bytes are at `dst` already.
    pub(super) fn copy_coll(&mut self, c: &Coll, dst: Value, src: Value, span: Span) -> Option<()> {
        self.compact(c, src);
        let len = self.hdr(src, 8);
        let scale = self.iconst(c.esize);
        let bytes = self.uarith(IntOp::Mul, len, scale);
        let align = self.iconst(c.ealign);
        let ebuf = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![bytes, align] }, VTy::Ptr);
        let old = self.hdr_ptr(src, 0);
        self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![ebuf, old, bytes] }, &[]);
        let (kd, vd) = (self.needs_drop(c.key), c.val.is_some_and(|v| self.needs_drop(v)));
        if kd || vd {
            let zero = self.iconst(0);
            self.count_loop(zero, len, |this, e| {
                let (d, s) = (this.entry(c, ebuf, e), this.entry(c, old, e));
                if kd {
                    let k = this.load_val(s, c.koff as i64, c.key, span)?;
                    let at = this.offset(d, c.koff as i64);
                    this.copy_into(at, &k, c.key, span)?;
                }
                if let (true, Some(vt)) = (vd, c.val) {
                    let v = this.load_val(s, c.voff as i64, vt, span)?;
                    let at = this.offset(d, c.voff as i64);
                    this.copy_into(at, &v, vt, span)?;
                }
                Some(())
            })?;
        }
        self.store(Mem::W64, dst, 0, ebuf);
        // (Compacting may have changed the source's `used` since its bytes
        // were copied.)
        self.store(Mem::W64, dst, 40, len);
        let zero = self.iconst(0);
        self.store(Mem::W64, dst, 48, zero);
        self.store(Mem::W64, dst, 56, len);
        self.store(Mem::W64, dst, 16, len);
        // The index: the same slots, for the same hashes.
        let slots = self.hdr(src, 32);
        let eight = self.iconst(8);
        let ibytes = self.uarith(IntOp::Mul, slots, eight);
        let ibuf = self.emit1(InstKind::Native { name: "mem.calloc", imm: 0, args: vec![ibytes, eight] }, VTy::Ptr);
        let oldi = self.hdr_ptr(src, 24);
        self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![ibuf, oldi, ibytes] }, &[]);
        self.store(Mem::W64, dst, 24, ibuf);
        Some(())
    }

    /// Writes a map (`[k: v, …]`, `[:]` for none) or a set (`{x, …}`).
    pub(super) fn show_coll(
        &mut self,
        sink: super::text::Sink,
        c: &Coll,
        header: Value,
        debug: bool,
        span: Span,
    ) -> Option<()> {
        self.compact(c, header);
        let len = self.hdr(header, 8);
        let (open, close) = if c.val.is_some() { ("[", "]") } else { ("{", "}") };
        if c.val.is_some() {
            let zero = self.iconst(0);
            let empty = self.ucmp(Cmp::Eq, len, zero);
            let (none, some, done) = (self.new_block(), self.new_block(), self.new_block());
            self.br(empty, none, some);
            self.enter(none);
            self.write_text(sink, "[:]");
            self.jump(done);
            self.enter(some);
            self.show_entries(sink, c, header, len, open, close, debug, span)?;
            self.jump(done);
            self.enter(done);
            return Some(());
        }
        self.show_entries(sink, c, header, len, open, close, debug, span)
    }

    #[allow(clippy::too_many_arguments)]
    fn show_entries(
        &mut self,
        sink: super::text::Sink,
        c: &Coll,
        header: Value,
        len: Value,
        open: &str,
        close: &str,
        debug: bool,
        span: Span,
    ) -> Option<()> {
        self.write_text(sink, open);
        let zero = self.iconst(0);
        self.count_loop(zero, len, |this, e| {
            let zero = this.iconst(0);
            let first = this.ucmp(Cmp::Eq, e, zero);
            let (sep, item) = (this.new_block(), this.new_block());
            this.br(first, item, sep);
            this.enter(sep);
            this.write_text(sink, ", ");
            this.jump(item);
            this.enter(item);
            let ebuf = this.hdr_ptr(header, 0);
            let ent = this.entry(c, ebuf, e);
            let k = this.load_val(ent, c.koff as i64, c.key, span)?;
            this.write_value(sink, k, c.key, None, debug, span)?;
            if let Some(vt) = c.val {
                this.write_text(sink, ": ");
                let v = this.load_val(ent, c.voff as i64, vt, span)?;
                this.write_value(sink, v, vt, None, debug, span)?;
            }
            Some(())
        })?;
        self.write_text(sink, close);
        Some(())
    }

    /// `a == b` for maps or sets: the same keys, and values for them, in
    /// any order.
    pub(super) fn eq_colls(&mut self, c: &Coll, a: Value, b: Value, span: Span) -> Option<Value> {
        self.compact(c, a);
        self.compact(c, b);
        let r = self.new_var(VTy::Int);
        let done = self.new_block();
        let (la, lb) = (self.hdr(a, 8), self.hdr(b, 8));
        let same = self.ucmp(Cmp::Eq, la, lb);
        self.write(r, same);
        let go = self.new_block();
        self.br(same, go, done);
        self.enter(go);
        let zero = self.iconst(0);
        self.count_loop(zero, la, |this, e| {
            let ebuf = this.hdr_ptr(a, 0);
            let ent = this.entry(c, ebuf, e);
            let h = this.hdr(ent, 0);
            let k = this.load_val(ent, c.koff as i64, c.key, span)?;
            let j = this.find_key(c, b, &k, h, span)?;
            let none = this.iconst(NONE);
            let missing = this.ucmp(Cmp::Eq, j, none);
            let no = this.iconst(0);
            this.write(r, no);
            let check = this.new_block();
            this.br(missing, done, check);
            this.enter(check);
            if let Some(vt) = c.val {
                let bbuf = this.hdr_ptr(b, 0);
                let other = this.entry(c, bbuf, j);
                let (x, y) =
                    (this.load_val(ent, c.voff as i64, vt, span)?, this.load_val(other, c.voff as i64, vt, span)?);
                let eq = this.eq_vals(x, y, vt, span)?;
                this.write(r, eq);
                let next = this.new_block();
                this.br(eq, next, done);
                this.enter(next);
            }
            Some(())
        })?;
        let yes = self.iconst(1);
        self.write(r, yes);
        self.jump(done);
        self.enter(done);
        Some(self.read(r))
    }

    // ------------------------------------------------------------ in expressions

    /// `[k: v, …]`: a new map; every key and value is worked out first.
    pub(super) fn map_lit(&mut self, e: &'a hir::Expr, pairs: &'a [(hir::Expr, hir::Expr)]) -> Option<Val> {
        let span = e.span;
        let ty = self.ty_of(e);
        let Some(c) = self.coll_of(ty) else { return self.unsupported(span, "this kind of `[k: v]`") };
        let mut vals = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let kv = self.expr(k)?;
            self.moved_away(k);
            let vv = self.expr(v)?;
            self.moved_away(v);
            vals.push((kv, vv));
        }
        let header = self.temp(ty, span)?;
        self.empty_header(header, ty)?;
        self.own_temp(header, ty);
        for (k, v) in vals {
            self.map_put(&c, header, k, v, span)?;
        }
        Some(Val::Mem(header))
    }

    /// `m[k] = v`: replaces the value for `k`, or adds it. Both are moved
    /// in (a key that's there already is dropped).
    fn map_put(&mut self, c: &Coll, header: Value, key: Val, val: Val, span: Span) -> Option<()> {
        let Some(vt) = c.val else { return self.internal(span, "a value for a set") };
        let (e, h) = self.lookup(c, header, &key, span)?;
        let none = self.iconst(NONE);
        let missing = self.ucmp(Cmp::Eq, e, none);
        let (add, replace, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(missing, add, replace);
        self.enter(replace);
        let ebuf = self.hdr_ptr(header, 0);
        let ent = self.entry(c, ebuf, e);
        let at = self.offset(ent, c.voff as i64);
        self.drop_at(at, vt);
        self.store_val(at, 0, vt, &val, span)?;
        // The key given isn't kept: a temporary is dropped here (adding
        // takes it, so it has no cleanup of its own), a place keeps it.
        if let Val::Mem(k) = key
            && self.f.temps.contains_key(&k)
        {
            self.drop_at(k, c.key);
        }
        self.jump(done);
        self.enter(add);
        let key_copy = key.clone();
        self.push_entry(c, header, h, key_copy, Some(val.clone()), span)?;
        self.jump(done);
        self.enter(done);
        // Moved in either way: the value always, the key when it's kept.
        self.moved_from(&val, vt);
        let _ = key;
        Some(())
    }

    /// The value for `key` in the map at `header`, or an unravel.
    fn map_at(&mut self, c: &Coll, header: Value, key: Val, span: Span) -> Option<Value> {
        let vt = c.val?;
        let (e, _) = self.lookup(c, header, &key, span)?;
        let none = self.iconst(NONE);
        let missing = self.ucmp(Cmp::Eq, e, none);
        let (fail, ok) = (self.new_block(), self.new_block());
        self.br(missing, fail, ok);
        self.enter(fail);
        let msg = self.msg_sink();
        self.write_text(msg, "key not found: ");
        self.write_value(msg, key, c.key, None, true, span)?;
        self.terminate(Term::UnravelMsg);
        self.enter(ok);
        let ebuf = self.hdr_ptr(header, 0);
        let ent = self.entry(c, ebuf, e);
        let _ = vt;
        Some(self.offset(ent, c.voff as i64))
    }

    /// `m[k]` as a place: the value's address.
    pub(super) fn map_index_place(
        &mut self,
        base: &'a hir::Expr,
        index: &'a hir::Expr,
        span: Span,
    ) -> Option<(Place, Ty)> {
        let (place, ty) = self.place_or_temp(base)?;
        let c = self.coll_of(ty)?;
        let key = self.expr(index)?;
        let header = self.place_addr(&place, span)?;
        let at = self.map_at(&c, header, key, span)?;
        Some((Place::At { ptr: at, off: 0 }, c.val?))
    }

    /// `m.entry(k, default: d)`: the value's place, made first (from `d`,
    /// which `k` is kept with) when `k` isn't there. What isn't kept is
    /// dropped here.
    pub(super) fn map_entry(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        let span = e.span;
        let hir::ExprKind::Call { callee, args } = &e.kind else { return None };
        let info = self.tables().calls.get(&e.id)?.clone();
        let sig = self.typed.results.sigs.get(&info.callee)?.clone();
        let modes: Vec<loom_syntax::ast::ParamMode> = sig.params.iter().map(|p| p.mode).collect();
        let recv_ty = self.plain_ty(sig.params.first()?.ty);
        let c = self.coll_of(recv_ty)?;
        let vt = c.val?;
        let worked = self.call_args(callee, args, &info, &modes, Some(recv_ty))?;
        let mut worked = worked.into_iter();
        let header = match worked.next()? {
            super::call::Arg::Place(place) => self.place_addr(&place, span)?,
            super::call::Arg::Val(v, ty) => self.addr_of(v, ty, span)?,
        };
        let key = match worked.next()? {
            super::call::Arg::Val(v, _) => v,
            super::call::Arg::Place(place) => Val::Mem(self.place_addr(&place, span)?),
        };
        let default = match worked.next()? {
            super::call::Arg::Val(v, _) => v,
            super::call::Arg::Place(place) => Val::Mem(self.place_addr(&place, span)?),
        };
        let (found, h) = self.lookup(&c, header, &key, span)?;
        let none = self.iconst(NONE);
        let missing = self.ucmp(Cmp::Eq, found, none);
        let at = self.new_var(VTy::Int);
        let (add, have, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(missing, add, have);
        // There: neither is kept.
        self.enter(have);
        self.write(at, found);
        if let Val::Mem(k) = key {
            self.drop_at(k, c.key);
        }
        if let Val::Mem(d) = default {
            self.drop_at(d, vt);
        }
        self.jump(done);
        // Not: both are, and then it's there.
        self.enter(add);
        // The new entry goes after the ones used so far.
        let used = self.hdr(header, 40);
        self.push_entry(&c, header, h, key.clone(), Some(default.clone()), span)?;
        self.write(at, used);
        self.jump(done);
        self.enter(done);
        // Both are used up, kept or dropped: no cleanup of theirs is left.
        self.moved_from(&default, vt);
        self.moved_from(&key, c.key);
        let slot = self.read(at);
        let ebuf = self.hdr_ptr(header, 0);
        let ent = self.entry(&c, ebuf, slot);
        let value = self.offset(ent, c.voff as i64);
        Some((Place::At { ptr: value, off: 0 }, vt))
    }

    /// `m[k] = v`, for `target` `m[k]`: `None` when it isn't one.
    pub(super) fn map_assign(&mut self, target: &'a hir::Expr, v: Val) -> Option<Option<()>> {
        let ExprKind::Index { base, args } = &target.kind else { return Some(None) };
        let [hir::GenericArg::Expr(index)] = args.as_slice() else { return Some(None) };
        let bty = self.ty_of(base);
        let Some(c) = self.coll_of(bty) else { return Some(None) };
        let span = target.span;
        let (place, _) = self.place_or_temp(base)?;
        let key = self.expr(index)?;
        let header = self.place_addr(&place, span)?;
        self.map_put(&c, header, key, v, span)?;
        Some(Some(()))
    }
    /// The body of a method of `Map` or `Set`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn coll_method(&mut self, name: &str, args: &[Val], tys: &[Ty], ret: Ty, span: Span) -> Option<()> {
        if name == "of" {
            // `Set.of(xs)`: each element, once.
            let c = self.coll_of(ret)?;
            let dst = self.f.sret?;
            self.empty_header(dst, ret)?;
            let (buf, len) = self.view_of(&args[0], tys[0], span)?;
            let zero = self.iconst(0);
            self.count_loop(zero, len, |this, i| {
                let at = this.elem_at(buf, i, c.key)?;
                let x = this.load_val(at, 0, c.key, span)?;
                this.set_add(&c, dst, x, true, span)?;
                Some(())
            })?;
            return self.ret_written();
        }
        let header = match args[0] {
            Val::Mem(h) => h,
            Val::Regs(_) => return self.internal(span, "a map in registers"),
        };
        let c = self.coll_of(tys[0])?;
        let len = self.hdr(header, 8);
        match name {
            "len" => self.ret_val(Val::Regs(vec![len]), span),
            "is_empty" => {
                let zero = self.iconst(0);
                let e = self.ucmp(Cmp::Eq, len, zero);
                self.ret_val(Val::Regs(vec![e]), span)
            }
            "has" => {
                let (e, _) = self.lookup(&c, header, &args[1], span)?;
                let none = self.iconst(NONE);
                let has = self.ucmp(Cmp::Ne, e, none);
                self.ret_val(Val::Regs(vec![has]), span)
            }
            "get" => {
                let vt = c.val?;
                let (e, _) = self.lookup(&c, header, &args[1], span)?;
                let none = self.iconst(NONE);
                let missing = self.ucmp(Cmp::Eq, e, none);
                let (no, yes) = (self.new_block(), self.new_block());
                self.br(missing, no, yes);
                self.enter(no);
                let n = self.none(ret, span)?;
                self.ret_val(n, span)?;
                self.enter(yes);
                let ebuf = self.hdr_ptr(header, 0);
                let ent = self.entry(&c, ebuf, e);
                let v = self.load_val(ent, c.voff as i64, vt, span)?;
                let v = self.copy_val(&v, vt, span)?;
                let v = self.some(ret, vt, v, span)?;
                self.ret_val(v, span)
            }
            "insert" if c.val.is_some() => {
                // The old value, if the key was there.
                let vt = c.val?;
                let (key, val) = (args[1].clone(), args[2].clone());
                let (e, h) = self.lookup(&c, header, &key, span)?;
                let none = self.iconst(NONE);
                let missing = self.ucmp(Cmp::Eq, e, none);
                let (add, replace) = (self.new_block(), self.new_block());
                self.br(missing, add, replace);
                self.enter(replace);
                let ebuf = self.hdr_ptr(header, 0);
                let ent = self.entry(&c, ebuf, e);
                let at = self.offset(ent, c.voff as i64);
                let old = self.temp(vt, span)?;
                let (size, _) = self.size_align(vt)?;
                if size > 0 {
                    self.emit(InstKind::Copy { dst: old, src: at, size }, &[]);
                }
                self.store_val(at, 0, vt, &val, span)?;
                if let Val::Mem(k) = key {
                    self.drop_at(k, c.key);
                }
                let old = self.load_val(old, 0, vt, span)?;
                let old = self.owned_copy(old, vt, span)?;
                let v = self.some(ret, vt, old, span)?;
                self.ret_val(v, span)?;
                self.enter(add);
                self.push_entry(&c, header, h, key, Some(val), span)?;
                let n = self.none(ret, span)?;
                self.ret_val(n, span)
            }
            "insert" => {
                // A set: whether it's new.
                let added = self.set_add(&c, header, args[1].clone(), false, span)?;
                if let Val::Mem(k) = args[1] {
                    // Not added: it's this function's to drop.
                    let (drop_b, done) = (self.new_block(), self.new_block());
                    self.br(added, done, drop_b);
                    self.enter(drop_b);
                    self.drop_at(k, c.key);
                    self.jump(done);
                    self.enter(done);
                }
                self.ret_val(Val::Regs(vec![added]), span)
            }
            "remove" => {
                let (e, _) = self.lookup(&c, header, &args[1], span)?;
                let none = self.iconst(NONE);
                let missing = self.ucmp(Cmp::Eq, e, none);
                let (no, yes) = (self.new_block(), self.new_block());
                self.br(missing, no, yes);
                self.enter(no);
                match c.val {
                    Some(_) => {
                        let n = self.none(ret, span)?;
                        self.ret_val(n, span)?;
                    }
                    None => {
                        let f = self.iconst(0);
                        self.ret_val(Val::Regs(vec![f]), span)?;
                    }
                }
                self.enter(yes);
                let ebuf = self.hdr_ptr(header, 0);
                let ent = self.entry(&c, ebuf, e);
                let k = self.offset(ent, c.koff as i64);
                self.drop_at(k, c.key);
                let taken = match c.val {
                    Some(vt) => {
                        let at = self.offset(ent, c.voff as i64);
                        let dst = self.temp(vt, span)?;
                        let (size, _) = self.size_align(vt)?;
                        if size > 0 {
                            self.emit(InstKind::Copy { dst, src: at, size }, &[]);
                        }
                        Some((dst, vt))
                    }
                    None => None,
                };
                self.remove_entry(&c, header, e);
                match taken {
                    Some((dst, vt)) => {
                        let v = self.load_val(dst, 0, vt, span)?;
                        let v = self.owned_copy(v, vt, span)?;
                        let v = self.some(ret, vt, v, span)?;
                        self.ret_val(v, span)
                    }
                    None => {
                        let t = self.iconst(1);
                        self.ret_val(Val::Regs(vec![t]), span)
                    }
                }
            }
            "first" | "last" => {
                let vt = c.val?;
                let TyKind::Optional(pair) = *self.tcx().kind(ret) else {
                    return self.internal(span, "`first` or `last` of a map without an optional");
                };
                let zero = self.iconst(0);
                let empty = self.ucmp(Cmp::Eq, len, zero);
                let (no, yes) = (self.new_block(), self.new_block());
                self.br(empty, no, yes);
                self.enter(no);
                let n = self.none(ret, span)?;
                self.ret_val(n, span)?;
                self.enter(yes);
                let e = if name == "first" {
                    self.hdr(header, 48)
                } else {
                    let tail = self.hdr(header, 56);
                    let one = self.iconst(1);
                    self.uarith(IntOp::Sub, tail, one)
                };
                let ebuf = self.hdr_ptr(header, 0);
                let ent = self.entry(&c, ebuf, e);
                let offs = self.field_offsets(pair, span)?;
                let dst = self.temp(pair, span)?;
                let k = self.load_val(ent, c.koff as i64, c.key, span)?;
                let at = self.offset(dst, offs[0] as i64);
                self.copy_into(at, &k, c.key, span)?;
                let v = self.load_val(ent, c.voff as i64, vt, span)?;
                let at = self.offset(dst, offs[1] as i64);
                self.copy_into(at, &v, vt, span)?;
                let p = self.load_val(dst, 0, pair, span)?;
                let p = self.some(ret, pair, p, span)?;
                self.ret_val(p, span)
            }
            "pop" => {
                // The oldest item of a set, moved out.
                let zero = self.iconst(0);
                let empty = self.ucmp(Cmp::Eq, len, zero);
                let (no, yes) = (self.new_block(), self.new_block());
                self.br(empty, no, yes);
                self.enter(no);
                let n = self.none(ret, span)?;
                self.ret_val(n, span)?;
                self.enter(yes);
                let e = self.hdr(header, 48);
                let ebuf = self.hdr_ptr(header, 0);
                let ent = self.entry(&c, ebuf, e);
                let at = self.offset(ent, c.koff as i64);
                let dst = self.temp(c.key, span)?;
                let (size, _) = self.size_align(c.key)?;
                if size > 0 {
                    self.emit(InstKind::Copy { dst, src: at, size }, &[]);
                }
                self.remove_entry(&c, header, e);
                let x = self.load_val(dst, 0, c.key, span)?;
                let x = self.owned_copy(x, c.key, span)?;
                let x = self.some(ret, c.key, x, span)?;
                self.ret_val(x, span)
            }
            "clear" => {
                self.drop_entries(&c, header);
                let zero = self.iconst(0);
                self.store(Mem::W64, header, 8, zero);
                self.store(Mem::W64, header, 40, zero);
                self.store(Mem::W64, header, 48, zero);
                self.store(Mem::W64, header, 56, zero);
                let slots = self.hdr(header, 32);
                let has = self.ucmp(Cmp::Ne, slots, zero);
                let (re, done) = (self.new_block(), self.new_block());
                self.br(has, re, done);
                self.enter(re);
                self.reindex(&c, header, slots);
                self.jump(done);
                self.enter(done);
                self.ret_written()
            }
            "sorted" => {
                // Copies of the entries (pairs for a map), sorted by key.
                self.compact(&c, header);
                let dst = self.f.sret?;
                let Some(crate::typeck::Seq::List(item)) = self.seq_of(ret) else { return None };
                let item = self.tcx().degauge(item);
                let buf = self.alloc_elems(len, item)?;
                let offs = if c.val.is_some() { Some(self.field_offsets(item, span)?) } else { None };
                let zero = self.iconst(0);
                self.count_loop(zero, len, |this, e| {
                    let ebuf = this.hdr_ptr(header, 0);
                    let ent = this.entry(&c, ebuf, e);
                    let to = this.elem_at(buf, e, item)?;
                    let k = this.load_val(ent, c.koff as i64, c.key, span)?;
                    match (&offs, c.val) {
                        (Some(offs), Some(vt)) => {
                            let at = this.offset(to, offs[0] as i64);
                            this.copy_into(at, &k, c.key, span)?;
                            let v = this.load_val(ent, c.voff as i64, vt, span)?;
                            let at = this.offset(to, offs[1] as i64);
                            this.copy_into(at, &v, vt, span)?;
                        }
                        _ => this.copy_into(to, &k, c.key, span)?,
                    }
                    Some(())
                })?;
                let cap = self.cap_for(len, item)?;
                self.set_header(dst, buf, len, cap);
                self.sort_list(buf, len, item, span)?;
                self.ret_written()
            }
            "copy" => {
                let dst = self.f.sret?;
                let v = Val::Mem(header);
                self.copy_into(dst, &v, tys[0], span)?;
                self.ret_written()
            }
            "keys" | "values" | "pairs" => {
                let what = match name {
                    "keys" => CollItems::Keys,
                    "values" => CollItems::Values,
                    _ => CollItems::Pairs,
                };
                let sty = ret;
                self.coll_stream_at(header, tys[0], what, sty, span)?;
                self.ret_written()
            }
            _ => self.internal(span, &format!("no body for `{name}` of a map or a set")),
        }
    }

    /// Adds `x` (a copy of it, with `copy`) to the set at `header` unless
    /// it's there: whether it was added (then it's moved in).
    fn set_add(&mut self, c: &Coll, header: Value, x: Val, copy: bool, span: Span) -> Option<Value> {
        let (e, h) = self.lookup(c, header, &x, span)?;
        let none = self.iconst(NONE);
        let missing = self.ucmp(Cmp::Eq, e, none);
        let (add, done) = (self.new_block(), self.new_block());
        self.br(missing, add, done);
        self.enter(add);
        let x = if copy { self.copy_val(&x, c.key, span)? } else { x };
        self.push_entry(c, header, h, x, None, span)?;
        self.jump(done);
        self.enter(done);
        Some(missing)
    }

    /// `v` (bytes a function took out of somewhere) as a temporary of its
    /// own, dropped unless something takes it.
    fn owned_copy(&mut self, v: Val, ty: Ty, span: Span) -> Option<Val> {
        match v {
            Val::Mem(p) => {
                self.own_temp(p, ty);
                let _ = span;
                Some(Val::Mem(p))
            }
            v => Some(v),
        }
    }
}

/// Which of a map's parts a stream of it gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum CollItems {
    Keys,
    Values,
    Pairs,
}
