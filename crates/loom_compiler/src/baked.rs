//! Constants read back from the VM that computed them, to be baked into the
//! program (design/vm.md §17, V4).
//!
//! Compile-time code ran on Treadle (see `consteval.rs`). What it computed
//! is in that VM's memory, with pointers into its heap, which are different
//! on every run. So a value is read back *by its type*, through its layout,
//! into a tree of what it means: the numbers, the text, the elements. The
//! tree has nothing of the VM's addresses in it, so what's built from it is
//! the same every time (V4's passing item 4), and a backend writes it its
//! own way: as bytes in the data of an image, or as a Lua literal.
//!
//! Only some types are read: numbers, `bool`, `char`, `()`, `str`,
//! `String`, tuples, arrays, `List`, records, tuple warps and `?T`.
//! Anything else (maps, sums, slices, `any`, functions, a value too big)
//! is `None`, and that constant is computed when the program reads it, as it
//! was before: never wrong, only slower.

use indexmap::IndexMap;
use loom_vm::Vm;

use crate::heddle::ConstKey;
use crate::hir::Program;
use crate::ty::layout::{Layouts, Shape, int_bytes};
use crate::ty::{FloatTy, Ty, TyKind};
use crate::typeck::Typed;

/// What a constant is: its type's values, as a tree.
#[derive(Clone, Debug, PartialEq)]
pub enum BakedVal {
    /// `()`.
    Unit,
    Bool(bool),
    /// An integer of any width, as the number it is.
    Int(i128),
    /// A float's bits as an `f64`'s (an `f32` widened: exactly).
    Float(u64),
    /// A `char`'s code point.
    Char(u32),
    /// A `str` or a `String`.
    Str(Vec<u8>),
    /// A tuple, or the fields of a record, in declaration order.
    Tuple(Vec<BakedVal>),
    /// An array or a `List`.
    Seq(Vec<BakedVal>),
    /// `none`.
    None,
    /// `some(v)`.
    Some(Box<BakedVal>),
}

/// A constant's type and value.
#[derive(Clone, Debug)]
pub struct BakedConst {
    pub ty: Ty,
    pub val: BakedVal,
}

/// The constants of a program that compile-time code computed and that can
/// be baked, by key and in the order the jobs found them.
#[derive(Clone, Debug, Default)]
pub struct Baked {
    pub consts: IndexMap<ConstKey, BakedConst>,
}

impl Baked {
    pub fn get(&self, key: &ConstKey) -> Option<&BakedConst> {
        self.consts.get(key)
    }
}

/// The deepest a value may nest, how many values it may have, and how many
/// bytes of text and elements: a bigger constant is computed when the
/// program runs (Lua's parser, too, has limits a deep literal would meet).
const MAX_DEPTH: u32 = 32;
const MAX_NODES: usize = 1 << 16;
const MAX_BYTES: usize = 16 << 20;

/// Reads values of a program's types out of a VM.
#[derive(Debug)]
pub struct Reader<'a> {
    p: &'a Program,
    typed: &'a Typed,
    layouts: Layouts<'a>,
    nodes: usize,
    bytes: usize,
}

impl<'a> Reader<'a> {
    pub fn new(p: &'a Program, typed: &'a Typed) -> Self {
        Reader { p, typed, layouts: Layouts::new(&typed.tcx, p, &typed.results), nodes: 0, bytes: 0 }
    }

    /// The value of type `ty` at address `addr` in `vm`, or `None` if it's
    /// of a type that isn't read, or too big.
    ///
    /// `addr` must be a value of `ty` that `vm` computed: a constant's slot
    /// once its flag says it's done.
    pub fn read(&mut self, vm: &Vm, ty: Ty, addr: u64) -> Option<BakedVal> {
        self.nodes = 0;
        self.bytes = 0;
        self.val(vm, ty, addr, 0)
    }

    fn word(&self, vm: &Vm, addr: u64, bytes: usize) -> u64 {
        let mut le = [0u8; 8];
        le[..bytes].copy_from_slice(self.slice(vm, addr, bytes));
        u64::from_le_bytes(le)
    }

    #[allow(unsafe_code)]
    fn slice<'v>(&self, vm: &'v Vm, addr: u64, len: usize) -> &'v [u8] {
        // SAFETY: `addr` and `len` come from walking a value of its type
        // through its layout, in a VM that's computed it and isn't running:
        // a slot of its data, or a buffer its header names, which it owns.
        unsafe { vm.read(addr, len) }
    }

    fn val(&mut self, vm: &Vm, ty: Ty, addr: u64, depth: u32) -> Option<BakedVal> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return None;
        }
        let tcx = &self.typed.tcx;
        let ty = tcx.degauge(ty);
        let layout = self.layouts.of(ty)?.clone();
        Some(match tcx.kind(ty).clone() {
            TyKind::Int(int) => {
                let bytes = int_bytes(int.bits) as usize;
                let raw = self.word(vm, addr, bytes);
                let n = if int.signed {
                    // Stored sign-extended to its bytes.
                    let shift = 64 - 8 * bytes as u32;
                    i128::from(((raw << shift) as i64) >> shift)
                } else {
                    i128::from(raw)
                };
                BakedVal::Int(n)
            }
            TyKind::Bool => BakedVal::Bool(self.word(vm, addr, 1) != 0),
            TyKind::Char => BakedVal::Char(self.word(vm, addr, 4) as u32),
            TyKind::Float(FloatTy::F32) => {
                let bits = self.word(vm, addr, 4) as u32;
                BakedVal::Float(f64::from(f32::from_bits(bits)).to_bits())
            }
            TyKind::Float(FloatTy::F64) => BakedVal::Float(self.word(vm, addr, 8)),
            TyKind::Unit => BakedVal::Unit,
            TyKind::Str => self.text(vm, addr)?,
            TyKind::Tuple(items) => {
                let Shape::Fields(offsets) = &layout.shape else { return None };
                let mut out = Vec::with_capacity(items.len());
                for (item, off) in items.iter().zip(offsets) {
                    out.push(self.val(vm, *item, addr + off, depth + 1)?);
                }
                BakedVal::Tuple(out)
            }
            TyKind::Array(elem, len) => {
                let Shape::Array { stride, .. } = layout.shape else { return None };
                self.elements(vm, elem, addr, stride, len, depth)?
            }
            TyKind::Optional(inner) => match &layout.shape {
                Shape::Optional { flag: Some(off) } => {
                    if self.word(vm, addr + off, 1) == 1 {
                        BakedVal::Some(Box::new(self.val(vm, inner, addr, depth + 1)?))
                    } else {
                        BakedVal::None
                    }
                }
                Shape::Optional { flag: None } => {
                    // `none` is the first pattern of what the inside has
                    // spare.
                    let niche = self.layouts.of(inner)?.niche?;
                    let found = u128::from(self.word(vm, addr + niche.offset, niche.bytes as usize));
                    if found == niche.start {
                        BakedVal::None
                    } else {
                        BakedVal::Some(Box::new(self.val(vm, inner, addr, depth + 1)?))
                    }
                }
                _ => return None,
            },
            TyKind::Adt(def, args) => match self.p.def_path(def).as_str() {
                "std.string.String" => self.text(vm, addr)?,
                "std.list.List" => {
                    let elem = *args.first()?;
                    let size = self.layouts.of(elem)?.size;
                    let (ptr, len) = (self.word(vm, addr, 8), self.word(vm, addr + 8, 8));
                    self.elements(vm, elem, ptr, size, len, depth)?
                }
                // The runtime's own: maps, sets, slices, streams…
                path if crate::ty::layout::is_builtin(path) => return None,
                _ => {
                    let info = self.typed.results.adts.get(&ty).or_else(|| self.typed.results.warps.get(&def))?;
                    // Records and tuple warps. A sum's values are represented
                    // differently on each backend, and a warp with no fields
                    // is a unit or a tag.
                    if !info.cases.is_empty() || info.fields.is_empty() {
                        return None;
                    }
                    let Shape::Fields(offsets) = &layout.shape else { return None };
                    let fields: Vec<Ty> = info.fields.iter().map(|f| f.ty).collect();
                    let mut out = Vec::with_capacity(fields.len());
                    for (field, off) in fields.iter().zip(offsets) {
                        out.push(self.val(vm, *field, addr + off, depth + 1)?);
                    }
                    BakedVal::Tuple(out)
                }
            },
            _ => return None,
        })
    }

    /// The text of the header (`str`: pointer, length; `String`: pointer,
    /// length, capacity) at `addr`.
    fn text(&mut self, vm: &Vm, addr: u64) -> Option<BakedVal> {
        let (ptr, len) = (self.word(vm, addr, 8), self.word(vm, addr + 8, 8) as usize);
        self.bytes = self.bytes.checked_add(len)?;
        if self.bytes > MAX_BYTES {
            return None;
        }
        Some(BakedVal::Str(self.slice(vm, ptr, len).to_vec()))
    }

    /// `len` elements of type `elem`, `stride` bytes apart from `addr`.
    fn elements(&mut self, vm: &Vm, elem: Ty, addr: u64, stride: u64, len: u64, depth: u32) -> Option<BakedVal> {
        if len as usize > MAX_NODES {
            return None;
        }
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            out.push(self.val(vm, elem, addr + i * stride, depth + 1)?);
        }
        Some(BakedVal::Seq(out))
    }
}
