//! Baked constants (design/vm.md §17, V4): what compile-time code computed,
//! written into the program's data as bytes laid out as the program's own
//! code would have made them.
//!
//! A constant's slot is made as before (a flag word, then its value), but
//! with its flag already saying it's computed and its value in it. What it
//! points to (a string's bytes, a list's buffer) follows in the data, and
//! each pointer is a byte offset listed in `Module::relocs`, which the
//! loader turns into an address. Nothing owns that memory: a read of an
//! owning constant is a copy as ever, and `const_exit` leaves it alone.
//!
//! A value that doesn't fit its type's layout as expected (which it always
//! does: the tree was read by the same layout) is not baked: the constant
//! is computed when read, as if the compiler hadn't computed it.

use loom_syntax::source::Span;

use super::Builder;
use super::consts::{ConstKey, DONE};
use super::value::Val;
use crate::baked::BakedVal;
use crate::ty::layout::{Shape, int_bytes};
use crate::ty::{FloatTy, Ty, TyKind};

impl<'a> Builder<'a> {
    /// Writes the value compile-time code computed for constant `key` into
    /// its slot (its flag word at byte `off`), if there is one that can be:
    /// whether it was.
    pub(super) fn bake_const(&mut self, key: ConstKey, off: u32, ty: Ty) -> bool {
        let Some(baked) = self.baked else { return false };
        let Some(c) = baked.get(&key) else { return false };
        if c.ty != ty {
            return false;
        }
        let slot = off as usize;
        let (data_len, relocs_len) = (self.data.len(), self.relocs.len());
        let slot_words = (data_len - slot / 8).min(1 + self.size_align(ty).map_or(0, |(s, _)| s.div_ceil(8) as usize));
        if self.put_val(ty, &c.val, slot + 8).is_some() {
            self.write_le(slot, DONE, 8);
            self.plain.push(((slot / 8) as u32, self.data.len() as u32));
            self.baked_keys.insert(key);
            return true;
        }
        // Not baked after all: the slot as it was, and nothing it made.
        self.data.truncate(data_len);
        self.relocs.truncate(relocs_len);
        for w in &mut self.data[slot / 8..slot / 8 + slot_words] {
            *w = 0;
        }
        false
    }

    /// Reads baked constant `key`, whose slot is at byte `off`: a number is
    /// an immediate, and the rest is read from the data, copied when it owns
    /// memory (the data's is never freed).
    pub(super) fn read_baked(&mut self, key: ConstKey, off: u32, ty: Ty, span: Span) -> Option<Val> {
        let val = self.baked.and_then(|b| b.get(&key)).map(|c| c.val.clone());
        let kind = self.tcx().kind(ty).clone();
        match (&kind, &val) {
            (TyKind::Int(_), Some(BakedVal::Int(n))) => return self.int_value(*n, ty, span).map(Val::Regs),
            (TyKind::Bool, Some(BakedVal::Bool(b))) => return Some(Val::Regs(vec![self.iconst(u64::from(*b))])),
            (TyKind::Char, Some(BakedVal::Char(c))) => return Some(Val::Regs(vec![self.iconst(u64::from(*c))])),
            (TyKind::Float(ft), Some(BakedVal::Float(bits))) => {
                return Some(Val::Regs(vec![self.fconst(f64::from_bits(*bits), *ft)]));
            }
            (TyKind::Unit, _) => return Some(Val::Regs(Vec::new())),
            _ => {}
        }
        let at = self.emit1(crate::heddle::InstKind::Global(off), crate::heddle::VTy::Ptr);
        let v = self.load_val(at, 8, ty, span)?;
        if self.needs_drop(ty) { self.copy_val(&v, ty, span) } else { Some(v) }
    }

    /// Whether constant `key` was baked.
    pub(super) fn is_baked(&self, key: &ConstKey) -> bool {
        self.baked_keys.contains(key)
    }

    // ---------------------------------------------------------- writing

    /// Room for `bytes` more bytes of data, zeroed, 8-aligned: where it is.
    fn alloc_data(&mut self, bytes: usize) -> usize {
        let at = self.data.len() * 8;
        self.data.resize(self.data.len() + bytes.div_ceil(8), 0);
        at
    }

    /// Writes the low `n` bytes of `v`, little-endian, at byte `at`.
    fn write_le(&mut self, at: usize, v: u64, n: usize) {
        for i in 0..n {
            let (word, shift) = ((at + i) / 8, 8 * ((at + i) % 8));
            let byte = u64::from((v >> (8 * i)) as u8);
            self.data[word] = (self.data[word] & !(0xff << shift)) | (byte << shift);
        }
    }

    /// A pointer to data at byte `to`, to be made an address when loaded,
    /// written at byte `at` (a word of its own).
    fn write_pointer(&mut self, at: usize, to: usize) {
        debug_assert_eq!(at % 8, 0);
        self.data[at / 8] = to as u64;
        self.relocs.push((at / 8) as u32);
    }

    /// The header of a buffer of `len` items of `size` bytes: its address,
    /// its length, and for an owned one its capacity. Empty ones point at
    /// nothing (an address of the right alignment, as the runtime makes).
    fn write_header(&mut self, at: usize, bytes: &[u8], len: usize, align: usize, words: usize) {
        if bytes.is_empty() {
            self.data[at / 8] = align.max(1) as u64;
        } else {
            let buf = self.alloc_data(bytes.len());
            for (i, b) in bytes.iter().enumerate() {
                self.write_le(buf + i, u64::from(*b), 1);
            }
            self.write_pointer(at, buf);
        }
        self.write_le(at + 8, len as u64, 8);
        if words == 3 {
            self.write_le(at + 16, len as u64, 8);
        }
    }

    /// Writes `v`, a value of type `ty`, at byte `at`.
    fn put_val(&mut self, ty: Ty, v: &BakedVal, at: usize) -> Option<()> {
        let ty = self.tcx().degauge(ty);
        let layout = self.layout(ty)?;
        match (self.tcx().kind(ty).clone(), v) {
            (TyKind::Int(int), BakedVal::Int(n)) => self.write_le(at, *n as u64, int_bytes(int.bits) as usize),
            (TyKind::Bool, BakedVal::Bool(b)) => self.write_le(at, u64::from(*b), 1),
            (TyKind::Char, BakedVal::Char(c)) => self.write_le(at, u64::from(*c), 4),
            (TyKind::Float(FloatTy::F32), BakedVal::Float(bits)) => {
                self.write_le(at, u64::from((f64::from_bits(*bits) as f32).to_bits()), 4);
            }
            (TyKind::Float(FloatTy::F64), BakedVal::Float(bits)) => self.write_le(at, *bits, 8),
            (TyKind::Unit, BakedVal::Unit) => {}
            (TyKind::Str, BakedVal::Str(bytes)) => self.write_header(at, bytes, bytes.len(), 1, 2),
            (TyKind::Tuple(items), BakedVal::Tuple(vals)) => {
                let Shape::Fields(offsets) = &layout.shape else { return None };
                if items.len() != vals.len() || items.len() != offsets.len() {
                    return None;
                }
                for ((item, val), off) in items.iter().zip(vals).zip(offsets) {
                    self.put_val(*item, val, at + *off as usize)?;
                }
            }
            (TyKind::Array(elem, len), BakedVal::Seq(vals)) => {
                let Shape::Array { stride, .. } = layout.shape else { return None };
                if len as usize != vals.len() {
                    return None;
                }
                for (i, val) in vals.iter().enumerate() {
                    self.put_val(elem, val, at + i * stride as usize)?;
                }
            }
            (TyKind::Optional(inner), BakedVal::None) => match layout.shape {
                Shape::Optional { flag: Some(_) } => {}
                Shape::Optional { flag: None } => {
                    let niche = self.layout(inner)?.niche?;
                    self.write_le(at + niche.offset as usize, niche.start as u64, niche.bytes as usize);
                }
                _ => return None,
            },
            (TyKind::Optional(inner), BakedVal::Some(val)) => {
                if let Shape::Optional { flag: Some(off) } = layout.shape {
                    self.write_le(at + off as usize, 1, 1);
                }
                self.put_val(inner, val, at)?;
            }
            (TyKind::Adt(def, args), val) => match (self.p.def_path(def).as_str(), val) {
                ("std.string.String", BakedVal::Str(bytes)) => self.write_header(at, bytes, bytes.len(), 1, 3),
                ("std.list.List", BakedVal::Seq(vals)) => {
                    let elem = *args.first()?;
                    let el = self.layout(elem)?;
                    if el.align > 8 {
                        return None;
                    }
                    let size = el.size as usize;
                    let n = vals.len();
                    if n == 0 {
                        self.data[at / 8] = el.align.max(1);
                        // Length and capacity are the zeros already there.
                    } else {
                        let buf = self.alloc_data(n.checked_mul(size)?.max(1));
                        for (i, val) in vals.iter().enumerate() {
                            self.put_val(elem, val, buf + i * size)?;
                        }
                        self.write_pointer(at, buf);
                        self.write_le(at + 8, n as u64, 8);
                        self.write_le(at + 16, n as u64, 8);
                    }
                }
                (_, BakedVal::Tuple(vals)) => {
                    let info = self.warp_info(ty)?;
                    let Shape::Fields(offsets) = &layout.shape else { return None };
                    if !info.cases.is_empty() || info.fields.len() != vals.len() || vals.len() != offsets.len() {
                        return None;
                    }
                    let fields: Vec<Ty> = info.fields.iter().map(|f| f.ty).collect();
                    for ((field, val), off) in fields.iter().zip(vals).zip(offsets) {
                        self.put_val(*field, val, at + *off as usize)?;
                    }
                }
                _ => return None,
            },
            _ => return None,
        }
        Some(())
    }
}
