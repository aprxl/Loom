//! How a value of each type is held while it's worked with: in registers,
//! one Heddle value per scalar part, or in memory, at an address.
//!
//! Small values made only of scalars live in registers: numbers, `bool`,
//! `char`, text views, tuples and records of those, ranges, function
//! values, a sum with no payloads (its tag), and `?T` of any of them (with
//! `none` in a niche of `T`, or a flag after it). Everything else lives in
//! memory: sums with payloads, results, unions, `any`, arrays, anything
//! that owns memory (`String`, `List`, maps, streams) and anything too big.
//! A value in registers can be in memory too, as a part of something that
//! is: its parts are loaded from, and stored at, their offsets in the
//! layout (`ty::layout`), so both agree on where each part is.

use std::rc::Rc;

use super::Builder;
use crate::heddle::{Mem, VTy};
use crate::ty::layout::{Layout, Shape};
use crate::ty::{FloatTy, IntTy, Ty, TyKind};

/// The most parts a value in registers may have.
const MAX_PARTS: usize = 8;

/// One scalar of a value in registers: its machine type, how it's loaded
/// and stored, and its offset in the value's layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Part {
    pub vty: VTy,
    pub mem: Mem,
    pub off: u64,
}

#[derive(Clone, Debug)]
pub(super) enum Repr {
    /// In registers, one value per part (none for `()`).
    Regs(Rc<[Part]>),
    /// In memory: `size` bytes.
    Mem { size: u64 },
}

impl Repr {
    pub fn parts(&self) -> &[Part] {
        match self {
            Repr::Regs(parts) => parts,
            Repr::Mem { .. } => &[],
        }
    }

    pub fn is_regs(&self) -> bool {
        matches!(self, Repr::Regs(_))
    }
}

/// How the load of an integer of `int` extends it: signed types
/// sign-extend, so registers hold every integer normalized.
pub(super) fn int_mem(int: IntTy) -> Mem {
    match (crate::ty::layout::int_bytes(int.bits), int.signed) {
        (1, true) => Mem::I8,
        (1, false) => Mem::U8,
        (2, true) => Mem::I16,
        (2, false) => Mem::U16,
        (4, true) => Mem::I32,
        (4, false) => Mem::U32,
        _ => Mem::W64,
    }
}

/// The register value of a bit pattern of `mem`'s width, loaded as `mem`
/// loads it: the value a niche's pattern has in a register.
pub(super) fn loaded(mem: Mem, pattern: u128) -> u64 {
    let p = pattern as u64;
    match mem {
        Mem::I8 => p as u8 as i8 as i64 as u64,
        Mem::I16 => p as u16 as i16 as i64 as u64,
        Mem::I32 => p as u32 as i32 as i64 as u64,
        Mem::U8 => p as u8 as u64,
        Mem::U16 => p as u16 as u64,
        Mem::U32 => p as u32 as u64,
        Mem::W64 => p,
    }
}

impl<'a> Builder<'a> {
    /// The layout of `ty` on Treadle, or `None` for a type that has none.
    pub(super) fn layout(&mut self, ty: Ty) -> Option<Layout> {
        let ty = self.tcx().degauge(ty);
        self.layouts.of(ty).cloned()
    }

    /// How values of `ty` are held, or `None` (not reported) for a type with
    /// no layout.
    pub(super) fn repr(&mut self, ty: Ty) -> Option<Repr> {
        let ty = self.tcx().degauge(ty);
        if let Some(r) = self.reprs.get(&ty) {
            return r.clone();
        }
        let layout = self.layout(ty);
        let repr = layout.map(|l| match self.parts_of(ty, &l) {
            Some(parts) if parts.len() <= MAX_PARTS && !self.needs_drop(ty) => Repr::Regs(parts.into()),
            _ => Repr::Mem { size: l.size },
        });
        self.reprs.insert(ty, repr.clone());
        repr
    }

    /// Whether values of `ty` live in registers.
    pub(super) fn in_regs(&mut self, ty: Ty) -> bool {
        self.repr(ty).is_some_and(|r| r.is_regs())
    }

    /// The parts of `ty` if it can live in registers: made only of scalars,
    /// with no two parts sharing bytes.
    fn parts_of(&mut self, ty: Ty, layout: &Layout) -> Option<Vec<Part>> {
        let tcx = self.tcx();
        // A gauge is its base (inside an optional, a tuple, …).
        let ty = tcx.degauge(ty);
        let scalar = |vty, mem| Some(vec![Part { vty, mem, off: 0 }]);
        match tcx.kind(ty).clone() {
            TyKind::Int(int) => scalar(VTy::Int, int_mem(int)),
            TyKind::Bool => scalar(VTy::Int, Mem::U8),
            TyKind::Char => scalar(VTy::Int, Mem::U32),
            TyKind::Float(FloatTy::F32) => scalar(VTy::F32, Mem::U32),
            TyKind::Float(FloatTy::F64) => scalar(VTy::F64, Mem::W64),
            TyKind::Unit | TyKind::Never => Some(Vec::new()),
            TyKind::Str => Some(view_parts()),
            TyKind::Fn(..) => {
                Some(vec![Part { vty: VTy::Int, mem: Mem::U32, off: 0 }, Part { vty: VTy::Ptr, mem: Mem::W64, off: 8 }])
            }
            TyKind::Tuple(items) => {
                let Shape::Fields(offsets) = &layout.shape else { return None };
                self.fields_parts(&items, offsets)
            }
            TyKind::Range(elem) => {
                let Shape::Fields(offsets) = &layout.shape else { return None };
                let mut parts = self.fields_parts(&[elem, elem], &offsets[..2])?;
                parts.push(Part { vty: VTy::Int, mem: Mem::U8, off: offsets[2] });
                Some(parts)
            }
            TyKind::Optional(inner) => {
                let inner_layout = self.layout(inner)?;
                let mut parts = self.parts_of(inner, &inner_layout)?;
                if let Shape::Optional { flag: Some(flag) } = layout.shape {
                    parts.push(Part { vty: VTy::Int, mem: Mem::U8, off: flag });
                }
                Some(parts)
            }
            TyKind::Adt(def, _) => match self.p.def_path(def).as_str() {
                "std.slice.Slice" => Some(view_parts()),
                "std.fmt.Fmt" | "std.sync.Spun" => scalar(VTy::Int, Mem::W64),
                _ if self.is_builtin(def) => None,
                _ => {
                    let info = self.warp_info(ty)?;
                    match &layout.shape {
                        Shape::Fields(offsets) if !info.is_sum() => {
                            let tys: Vec<Ty> = info.fields.iter().map(|f| f.ty).collect();
                            self.fields_parts(&tys, offsets)
                        }
                        // Only its tag: every case is a unit case.
                        Shape::Sum { tag, .. } if info.unit_only() => {
                            let mem = if tag.bytes == 1 { Mem::U8 } else { Mem::U16 };
                            Some(vec![Part { vty: VTy::Int, mem, off: tag.offset }])
                        }
                        _ => None,
                    }
                }
            },
            _ => None,
        }
    }

    /// The parts of fields of types `tys` at `offsets`, one after another.
    fn fields_parts(&mut self, tys: &[Ty], offsets: &[u64]) -> Option<Vec<Part>> {
        let mut out = Vec::new();
        for (&t, &at) in tys.iter().zip(offsets) {
            let t = self.tcx().degauge(t);
            let layout = self.layout(t)?;
            let parts = self.parts_of(t, &layout)?;
            out.extend(parts.into_iter().map(|p| Part { off: p.off + at, ..p }));
        }
        Some(out)
    }

    /// Whether dropping a value of `ty` has anything to do: it owns memory,
    /// or holds something that does.
    pub(super) fn needs_drop(&mut self, ty: Ty) -> bool {
        let ty = self.tcx().degauge(ty);
        if let Some(&d) = self.drops.get(&ty) {
            return d;
        }
        // A type that holds itself (through a list) is answered as `true`
        // while it's being worked out.
        self.drops.insert(ty, true);
        let d = match self.tcx().kind(ty).clone() {
            TyKind::Tuple(items) | TyKind::Union(items) => items.iter().any(|&t| self.needs_drop(t)),
            TyKind::Optional(t) | TyKind::Array(t, _) => self.needs_drop(t),
            TyKind::Result(t, e) => self.needs_drop(t) || self.needs_drop(e),
            TyKind::Any(_) => true,
            TyKind::Adt(def, _) => match self.p.def_path(def).as_str() {
                "std.slice.Slice" | "std.fmt.Fmt" => false,
                // A mutex owns its value.
                "std.sync.Mutex" => match self.tcx().kind(ty).clone() {
                    TyKind::Adt(_, args) => args.first().is_some_and(|&t| self.needs_drop(t)),
                    _ => false,
                },
                "std.string.String" | "std.list.List" | "std.map.Map" | "std.set.Set" | "std.iter.Stream" => true,
                "std.iter.Shuttle" | "std.iter.Burst" | "std.fs.File" | "std.sync.Pipe" => true,
                _ if self.is_builtin(def) => false,
                _ => match self.warp_info(ty) {
                    Some(info) => info.field_tys().into_iter().any(|t| self.needs_drop(t)),
                    None => false,
                },
            },
            _ => false,
        };
        self.drops.insert(ty, d);
        d
    }

    /// The size and alignment of `ty`'s values in memory.
    pub(super) fn size_align(&mut self, ty: Ty) -> Option<(u64, u64)> {
        self.layout(ty).map(|l| (l.size, l.align))
    }

    /// The fields and cases of warp type `ty`.
    pub(super) fn warp_info(&self, ty: Ty) -> Option<crate::typeck::WarpInfo> {
        let TyKind::Adt(def, _) = self.tcx().kind(ty) else { return None };
        self.typed.results.adts.get(&ty).or_else(|| self.typed.results.warps.get(def)).cloned()
    }

    /// Whether `def` is a warp std declares `#builtin`.
    pub(super) fn is_builtin(&self, def: crate::hir::DefId) -> bool {
        self.p.def(def).attrs.iter().any(|a| a.name == "builtin")
    }
}

/// A view of text or of elements: an address and a length.
fn view_parts() -> Vec<Part> {
    vec![Part { vty: VTy::Ptr, mem: Mem::W64, off: 0 }, Part { vty: VTy::Int, mem: Mem::W64, off: 8 }]
}
