//! How values of each type are laid out in memory on Treadle (design/vm.md
//! §5.2): a size, an alignment, field offsets, and the bit patterns no value
//! uses (niches), which let `?T` cost nothing for most `T`.
//!
//! - Integers take the smallest of 1, 2, 4 or 8 bytes that holds their bits,
//!   normalized (sign- or zero-extended), so the patterns past their width
//!   are spare: a `u12` has 61 440.
//! - Records and tuples order their fields by alignment, largest first,
//!   declaration order among equals (D4). Offsets are reported in
//!   declaration order.
//! - A sum is its shared fields, then a tag (the case's position, from 0),
//!   then the cases' payloads overlapping. The tag's unused values are
//!   niches.
//! - `?T` is `T` when `T` has a niche (`none` is the niche's first value),
//!   and `T` followed by a `bool` otherwise.
//! - Views, collections, functions, `any` and the like are fixed-size
//!   headers whose insides the runtime owns ([`Shape::Opaque`]).
//!
//! The Lua target uses the same numbers for `T.size`: Lua code never depends
//! on them.

use std::collections::{HashMap, HashSet};

use super::{FloatTy, IntTy, Ty, TyCtx, TyKind};
use crate::hir::Program;
use crate::typeck::{Results, WarpInfo};

/// Where a value keeps its spare bit patterns: a scalar of `bytes` bytes at
/// `offset`, whose patterns `start..start + count` (read as unsigned) no
/// value uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Niche {
    pub offset: u64,
    pub bytes: u8,
    pub start: u128,
    pub count: u128,
}

impl Niche {
    fn at(self, offset: u64) -> Niche {
        Niche { offset: self.offset + offset, ..self }
    }

    /// The niche left once its first value means something (`none`).
    fn rest(self) -> Option<Niche> {
        (self.count > 1).then_some(Niche { start: self.start + 1, count: self.count - 1, ..self })
    }
}

/// A tag: which case (or member, or `ok`/`err`) a value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    pub offset: u64,
    pub bytes: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A number, `bool`, `char`, `()` or a pointer.
    Scalar,
    /// Tuples, records and ranges: each field's offset, in declaration order.
    Fields(Vec<u64>),
    /// A warp with cases. `shared` and each case's fields are offsets in
    /// declaration order, from the start of the value.
    Sum {
        tag: Tag,
        shared: Vec<u64>,
        payload: u64,
        cases: Vec<Vec<u64>>,
    },
    /// `T ! E` (tag 0 is `ok`) and unions (the tag is the member's position).
    Tagged {
        tag: Tag,
        payload: u64,
    },
    /// `?T`: `flag` is where the `bool` saying `some` is, or `None` when
    /// `T`'s niche says `none`.
    Optional {
        flag: Option<u64>,
    },
    Array {
        stride: u64,
        len: u64,
    },
    /// A header whose insides the runtime owns.
    Opaque,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: u64,
    pub align: u64,
    pub niche: Option<Niche>,
    pub shape: Shape,
}

impl Layout {
    fn scalar(bytes: u64, niche: Option<(u128, u128)>) -> Layout {
        let niche = niche.map(|(start, count)| Niche { offset: 0, bytes: bytes as u8, start, count });
        Layout { size: bytes, align: bytes.max(1), niche, shape: Shape::Scalar }
    }

    /// A pointer-sized header of `words` words whose first word is a pointer
    /// that's never null.
    fn header(words: u64) -> Layout {
        let niche = Some(Niche { offset: 0, bytes: 8, start: 0, count: 1 });
        Layout { size: words * 8, align: 8, niche, shape: Shape::Opaque }
    }
}

/// The bytes an integer of `bits` bits takes.
pub fn int_bytes(bits: u8) -> u64 {
    match bits {
        0..=8 => 1,
        9..=16 => 2,
        17..=32 => 4,
        _ => 8,
    }
}

fn round_up(n: u64, align: u64) -> u64 {
    n.div_ceil(align) * align
}

/// The builtin warps of std whose layout Treadle's runtime decides.
fn builtin(path: &str) -> Option<Builtin> {
    Some(match path {
        "std.slice.Slice" => Builtin::Header(2),
        // A `File` is its path.
        "std.string.String" | "std.list.List" | "std.fs.File" => Builtin::Header(3),
        "std.map.Map" | "std.set.Set" => Builtin::Header(8),
        "std.iter.Stream" | "std.iter.Shuttle" | "std.iter.Burst" => Builtin::Header(2),
        "std.sync.Atomic" => Builtin::Atomic,
        "std.sync.Mutex" => Builtin::Mutex,
        "std.sync.Pipe" => Builtin::Header(1),
        "std.fmt.Fmt" | "std.sync.Spun" | "std.mem.Arena" | "std.mem.SharedArena" | "std.mem.Pool" => Builtin::Handle,
        "std.mem.Heap" => Builtin::Empty,
        _ => return None,
    })
}

/// Whether `path` names a warp of std that the runtime lays out.
pub fn is_builtin(path: &str) -> bool {
    builtin(path).is_some()
}

enum Builtin {
    Header(u64),
    Atomic,
    /// A lock word, then the value.
    Mutex,
    /// A number the runtime gives out: any 8 bytes, 0 included.
    Handle,
    /// Nothing to hold: the global spool.
    Empty,
}

/// Works out layouts, remembering each.
#[derive(Debug)]
pub struct Layouts<'a> {
    tcx: &'a TyCtx,
    p: &'a Program,
    results: &'a Results,
    cache: HashMap<Ty, Option<Layout>>,
    /// Types being laid out: one that contains itself has no layout.
    busy: HashSet<Ty>,
}

impl<'a> Layouts<'a> {
    pub fn new(tcx: &'a TyCtx, p: &'a Program, results: &'a Results) -> Self {
        Layouts { tcx, p, results, cache: HashMap::new(), busy: HashSet::new() }
    }

    /// The layout of `ty`, or `None` for types values don't have at run time
    /// (`type`, a generic parameter, an error) or that aren't laid out yet.
    pub fn of(&mut self, ty: Ty) -> Option<&Layout> {
        if !self.cache.contains_key(&ty) {
            if !self.busy.insert(ty) {
                return None;
            }
            let layout = self.compute(ty);
            debug_assert!(layout.as_ref().is_none_or(|l| l.align <= 16), "no type is aligned past 16 bytes");
            self.busy.remove(&ty);
            self.cache.insert(ty, layout);
        }
        self.cache.get(&ty).and_then(Option::as_ref)
    }

    fn compute(&mut self, ty: Ty) -> Option<Layout> {
        let tcx = self.tcx;
        Some(match tcx.kind(ty) {
            TyKind::Int(int) => int_layout(*int),
            TyKind::Bool => Layout::scalar(1, Some((2, 254))),
            TyKind::Char => Layout::scalar(4, Some((0x11_0000, (1u128 << 32) - 0x11_0000))),
            TyKind::Float(FloatTy::F32) => Layout::scalar(4, None),
            TyKind::Float(FloatTy::F64) => Layout::scalar(8, None),
            TyKind::Unit | TyKind::Never => Layout::scalar(0, None),
            TyKind::Str => Layout::header(2),
            TyKind::Gauge(_, base) => self.of(*base)?.clone(),
            TyKind::Tuple(items) => {
                let items = items.clone();
                self.record(&items)?
            }
            // Both ends, then a byte saying which are open or inclusive.
            TyKind::Range(elem) => {
                let elem = self.of(*elem)?.clone();
                let flags = int_layout(IntTy { signed: false, bits: 8, size: false });
                let (offsets, size, align, niche) = place(&[elem.clone(), elem, flags])?;
                Layout { size, align, niche, shape: Shape::Fields(offsets) }
            }
            TyKind::Optional(inner) => {
                let inner = self.of(*inner)?.clone();
                match inner.niche {
                    Some(niche) => Layout { niche: niche.rest(), shape: Shape::Optional { flag: None }, ..inner },
                    None => {
                        let flag = inner.size;
                        let align = inner.align;
                        let niche = Some(Niche { offset: flag, bytes: 1, start: 2, count: 254 });
                        let size = round_up(flag + 1, align);
                        Layout { size, align, niche, shape: Shape::Optional { flag: Some(flag) } }
                    }
                }
            }
            TyKind::Result(ok, err) => {
                let (ok, err) = (*ok, *err);
                self.tagged(&[ok, err])?
            }
            TyKind::Union(members) => {
                let members = members.clone();
                self.tagged(&members)?
            }
            TyKind::Array(elem, len) => {
                let len = *len;
                let elem = self.of(*elem)?.clone();
                let niche = if len > 0 { elem.niche } else { None };
                let size = elem.size.checked_mul(len)?;
                Layout { size, align: elem.align, niche, shape: Shape::Array { stride: elem.size, len } }
            }
            // A function's number (4 bytes, padded) and its environment.
            TyKind::Fn(..) => Layout { size: 16, align: 8, niche: None, shape: Shape::Opaque },
            // A 16-byte payload, then the vtable, which is never null.
            TyKind::Any(_) => Layout {
                size: 24,
                align: 8,
                niche: Some(Niche { offset: 16, bytes: 8, start: 0, count: 1 }),
                shape: Shape::Opaque,
            },
            // A heap pointer is an address, never null.
            TyKind::Ptr(..) => Layout {
                size: 8,
                align: 8,
                niche: Some(Niche { offset: 0, bytes: 8, start: 0, count: 1 }),
                shape: Shape::Scalar,
            },
            TyKind::Adt(def, args) => {
                let (def, args) = (*def, args.clone());
                match builtin(&self.p.def_path(def)) {
                    Some(Builtin::Header(words)) => Layout::header(words),
                    Some(Builtin::Atomic) => self.of(*args.first()?)?.clone(),
                    Some(Builtin::Mutex) => {
                        let value = self.of(*args.first()?)?.clone();
                        let align = value.align.max(8);
                        let size = (8 + value.size).div_ceil(align) * align;
                        Layout { size, align, niche: None, shape: Shape::Opaque }
                    }
                    Some(Builtin::Handle) => Layout::scalar(8, None),
                    Some(Builtin::Empty) => Layout::scalar(0, None),
                    None => {
                        let info = self.info(ty, def)?;
                        self.warp(&info)?
                    }
                }
            }
            TyKind::ArrayOf(..)
            | TyKind::Value(_)
            | TyKind::Region(_)
            | TyKind::IntVar(_)
            | TyKind::FloatVar(_)
            | TyKind::Var(_)
            | TyKind::Param(_)
            | TyKind::Error => return None,
        })
    }

    fn info(&self, ty: Ty, def: crate::hir::DefId) -> Option<WarpInfo> {
        self.results.adts.get(&ty).or_else(|| self.results.warps.get(&def)).cloned()
    }

    /// Fields of types `tys`, laid out by [`place`].
    fn fields(&mut self, tys: &[Ty]) -> Option<(Vec<u64>, u64, u64, Option<Niche>)> {
        let layouts: Vec<Layout> = tys.iter().map(|&t| self.of(t).cloned()).collect::<Option<Vec<Layout>>>()?;
        place(&layouts)
    }

    fn record(&mut self, tys: &[Ty]) -> Option<Layout> {
        let (offsets, size, align, niche) = self.fields(tys)?;
        Some(Layout { size, align, niche, shape: Shape::Fields(offsets) })
    }

    /// A tag, then values of one of `tys` overlapping.
    fn tagged(&mut self, tys: &[Ty]) -> Option<Layout> {
        let tag = tag_bytes(tys.len());
        let mut payload_align = 1u64;
        let mut payload_size = 0u64;
        for &t in tys {
            let l = self.of(t)?;
            payload_align = payload_align.max(l.align);
            payload_size = payload_size.max(l.size);
        }
        let payload = round_up(tag, payload_align);
        let align = payload_align.max(tag);
        let size = round_up(payload + payload_size, align);
        let niche = tag_niche(0, tag, tys.len());
        Some(Layout { size, align, niche, shape: Shape::Tagged { tag: Tag { offset: 0, bytes: tag as u8 }, payload } })
    }

    fn warp(&mut self, info: &WarpInfo) -> Option<Layout> {
        let shared: Vec<Ty> = info.fields.iter().map(|f| f.ty).collect();
        if !info.is_sum() {
            return self.record(&shared);
        }
        let (shared_offsets, shared_size, shared_align, shared_niche) = self.fields(&shared)?;
        let tag_size = tag_bytes(info.cases.len());
        let tag_at = round_up(shared_size, tag_size);
        let mut cases = Vec::with_capacity(info.cases.len());
        let mut payload_align = 1u64;
        let mut payload_size = 0u64;
        for case in &info.cases {
            let tys: Vec<Ty> = case.fields.iter().flatten().map(|f| f.ty).collect();
            let (offsets, size, align, _) = self.fields(&tys)?;
            payload_align = payload_align.max(align);
            payload_size = payload_size.max(size);
            cases.push(offsets);
        }
        let payload = round_up(tag_at + tag_size, payload_align);
        for offsets in &mut cases {
            offsets.iter_mut().for_each(|o| *o += payload);
        }
        let align = shared_align.max(tag_size).max(payload_align);
        let size = round_up(payload + payload_size, align);
        let tag_niche = tag_niche(tag_at, tag_size, info.cases.len());
        let niche = match (tag_niche, shared_niche) {
            (Some(t), Some(s)) if s.count > t.count => Some(s),
            (t, s) => t.or(s),
        };
        let tag = Tag { offset: tag_at, bytes: tag_size as u8 };
        Some(Layout { size, align, niche, shape: Shape::Sum { tag, shared: shared_offsets, payload, cases } })
    }
}

/// Lays fields out by alignment, largest first (declaration order among
/// equals): their offsets in the order given, the size and alignment of the
/// whole, and its roomiest niche (the first among equals).
fn place(layouts: &[Layout]) -> Option<(Vec<u64>, u64, u64, Option<Niche>)> {
    let mut order: Vec<usize> = (0..layouts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(layouts[i].align));
    let mut offsets = vec![0; layouts.len()];
    let mut end = 0u64;
    let mut align = 1u64;
    for &i in &order {
        let l = &layouts[i];
        let at = round_up(end, l.align);
        offsets[i] = at;
        end = at.checked_add(l.size)?;
        align = align.max(l.align);
    }
    let niche = layouts.iter().zip(&offsets).filter_map(|(l, &at)| l.niche.map(|n| n.at(at))).fold(
        None,
        |best: Option<Niche>, n| match best {
            Some(b) if b.count >= n.count => Some(b),
            _ => Some(n),
        },
    );
    Some((offsets, round_up(end, align), align, niche))
}

fn int_layout(int: IntTy) -> Layout {
    let bytes = int_bytes(int.bits);
    let spare = (1u128 << (bytes * 8)) - (1u128 << int.bits);
    // Unused patterns start just past the largest value (read unsigned):
    // `2^bits` for unsigned types, `2^(bits-1)` for sign-extended ones.
    let start = if int.signed { 1u128 << (int.bits - 1) } else { 1u128 << int.bits };
    Layout::scalar(bytes, (spare > 0).then_some((start, spare)))
}

fn tag_bytes(cases: usize) -> u64 {
    if cases <= 256 { 1 } else { 2 }
}

fn tag_niche(offset: u64, bytes: u64, used: usize) -> Option<Niche> {
    let total = 1u128 << (bytes * 8);
    let used = used as u128;
    (used < total).then_some(Niche { offset, bytes: bytes as u8, start: used, count: total - used })
}

#[cfg(test)]
mod tests {
    use loom_syntax::SourceDb;

    use super::*;
    use crate::hir::resolve;
    use crate::modules::MemoryLoader;
    use crate::target::Target;

    /// Checks `src` for Treadle and gives the layout of the type of each
    /// named local of `main`, by name.
    fn layouts(src: &str, names: &[&str]) -> Vec<Layout> {
        let mut db = SourceDb::new();
        let root = db.add("test.loom", src);
        let resolved = resolve(&mut db, root, &MemoryLoader::default());
        assert!(resolved.diagnostics.is_empty(), "{:?}", resolved.diagnostics);
        let typed = crate::typeck::check_for(&resolved.program, Target::VM);
        assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
        let p = &resolved.program;
        let mut l = Layouts::new(&typed.tcx, p, &typed.results);
        names
            .iter()
            .map(|name| {
                let (id, _) = p.locals.iter().enumerate().find(|(_, l)| l.name == *name).expect("local exists");
                let local = crate::hir::LocalId(id as u32);
                let ty = typed.results.bodies.iter().find_map(|b| b.local_ty.get(&local)).copied().expect("typed");
                l.of(ty).cloned().unwrap_or_else(|| panic!("no layout for `{name}`"))
            })
            .collect()
    }

    fn size_align(l: &Layout) -> (u64, u64) {
        (l.size, l.align)
    }

    #[test]
    fn integers_take_the_smallest_power_of_two() {
        let [a, b, c, d, e]: [Layout; 5] = layouts(
            "fn main() { let a: u1 = 1\n let b: u12 = 1\n let c: i24 = 1\n let d: u64 = 1\n let e: usize = 1 }",
            &["a", "b", "c", "d", "e"],
        )
        .try_into()
        .expect("five");
        assert_eq!(size_align(&a), (1, 1));
        assert_eq!(a.niche.map(|n| (n.start, n.count)), Some((2, 254)));
        assert_eq!(size_align(&b), (2, 2));
        assert_eq!(b.niche.map(|n| n.count), Some(61_440));
        assert_eq!(size_align(&c), (4, 4));
        assert_eq!(c.niche.map(|n| (n.start, n.count)), Some((1 << 23, (1 << 32) - (1 << 24))));
        assert_eq!((size_align(&d), d.niche), ((8, 8), None));
        assert_eq!((size_align(&e), e.niche), ((8, 8), None), "usize is 64 bits on Treadle");
    }

    #[test]
    fn fields_go_largest_alignment_first() {
        let [t]: [Layout; 1] = layouts("fn main() { let t = (1u8, 2u64, 3u16) }", &["t"]).try_into().expect("one");
        assert_eq!(size_align(&t), (16, 8));
        assert_eq!(t.shape, Shape::Fields(vec![10, 0, 8]));
    }

    #[test]
    fn records_and_sums_match_the_design() {
        let src = "warp Point { x: f64, y: f64 }\n\
                   warp Span(u32, u32)\n\
                   warp Token { span: Span\n case Ident(String)\n case Int(u64)\n case Eof }\n\
                   warp Color { case Red, Green, Blue }\n\
                   fn main() {\n\
                       let p = Point(1, 2)\n\
                       let t = Token.Eof(span: Span(0, 0))\n\
                       let ot: ?Token = none\n\
                       let op: ?Point = none\n\
                       let c = Color.Red\n\
                       let oc: ?char = none\n\
                       let os: ?String = none\n\
                       let oo: ??bool = none\n\
                       let ou: ?u8 = none\n\
                   }";
        let [p, t, ot, op, c, oc, os, oo, ou]: [Layout; 9] =
            layouts(src, &["p", "t", "ot", "op", "c", "oc", "os", "oo", "ou"]).try_into().expect("nine");
        assert_eq!(size_align(&p), (16, 8));
        assert_eq!(size_align(&t), (40, 8));
        let Shape::Sum { tag, shared, payload, cases } = &t.shape else { panic!("{t:?}") };
        assert_eq!((tag.offset, tag.bytes, shared.as_slice(), *payload), (8, 1, [0].as_slice(), 16));
        assert_eq!(cases, &[vec![16], vec![16], vec![]]);
        assert_eq!(size_align(&ot), (40, 8), "`none` is a spare tag value");
        assert_eq!(ot.shape, Shape::Optional { flag: None });
        assert_eq!(size_align(&op), (24, 8));
        assert_eq!(op.shape, Shape::Optional { flag: Some(16) });
        assert_eq!(size_align(&c), (1, 1));
        assert_eq!(size_align(&oc), (4, 4));
        assert_eq!(size_align(&os), (24, 8));
        assert_eq!(size_align(&oo), (1, 1), "bool has room for two nones");
        assert_eq!(size_align(&ou), (2, 1));
    }

    #[test]
    fn results_unions_and_arrays() {
        let src = "fn main() {\n\
                       let r: u32 ! u8 = 1\n\
                       let u: u8 | f64 = 1.5\n\
                       let a = [1u16, 2, 3]\n\
                       let e: () = ()\n\
                   }";
        let [r, u, a, e]: [Layout; 4] = layouts(src, &["r", "u", "a", "e"]).try_into().expect("four");
        assert_eq!(size_align(&r), (8, 4));
        assert_eq!(r.shape, Shape::Tagged { tag: Tag { offset: 0, bytes: 1 }, payload: 4 });
        assert_eq!(size_align(&u), (16, 8));
        assert_eq!(size_align(&a), (6, 2));
        assert_eq!(a.shape, Shape::Array { stride: 2, len: 3 });
        assert_eq!(size_align(&e), (0, 1));
    }
}
