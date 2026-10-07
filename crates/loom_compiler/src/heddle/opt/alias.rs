//! Which memory accesses may touch the same bytes (alias analysis), for the
//! passes that keep a loaded value instead of loading it again.
//!
//! An address is a *root* plus an offset, when the offset is known: a frame
//! slot, the program's data, the buffer of a list (or slice) whose header is
//! at a known place, or any other value. Two accesses can't overlap when:
//!
//! - they're at known offsets from the same root, and their bytes don't
//!   overlap;
//! - one is in a frame slot whose address never escapes (it's only loaded
//!   from, stored to, copied and zeroed, but by landing pads), and the
//!   other isn't in that slot;
//! - they're in different slots, or one is in a slot and the other in the
//!   program's data;
//! - one is in a list's buffer and the other in that list's header (a
//!   buffer never holds its own list);
//! - one is in memory just allocated (`mem.alloc`, `mem.calloc`) and the
//!   other in a slot, or in another allocation.

use crate::heddle::{Func, InstKind, Value};

/// Where an address points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Root {
    Slot(u32),
    /// The program's data (`Global`), at byte offsets from its start.
    Data,
    /// The buffer of the list whose header is at `header + off`.
    Buffer(Value, i32),
    /// Memory allocated by this native's call: on the heap, not the frame.
    Heap(Value),
    Value(Value),
}

/// An access: its address, and how many bytes.
#[derive(Clone, Copy, Debug)]
pub struct Access {
    pub ptr: Value,
    pub off: i64,
    pub size: u64,
}

pub struct Alias {
    /// What each value is, when an instruction makes it.
    kind: Vec<Option<InstKind>>,
    /// Whether each slot's address escapes.
    pub escapes: Vec<bool>,
    /// Whether it escapes but to the calls it's passed to (and natives
    /// that write nothing): when it doesn't, only those calls can reach
    /// the slot, through their parameters (see `modref`).
    pub leaked: Vec<bool>,
}

impl Alias {
    pub fn new(f: &Func) -> Alias {
        let mut kind: Vec<Option<InstKind>> = vec![None; f.types.len()];
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let [r] = inst.results.as_slice() {
                kind[r.0 as usize] = Some(inst.kind.clone());
            }
        }
        let mut alias = Alias { kind, escapes: vec![false; f.slots.len()], leaked: vec![false; f.slots.len()] };
        let escape = |alias: &mut Alias, v: Value| {
            if let (Root::Slot(n), _) = alias.root(v) {
                alias.escapes[n as usize] = true;
            }
        };
        // Only what the entry reaches: once a landing pad runs, the code
        // that runs normally never does again, so what a pad does with a
        // slot's address (pass it to a drop) can't change what it reads.
        let mut reached = vec![false; f.blocks.len()];
        let mut stack = vec![0usize];
        while let Some(b) = stack.pop() {
            if !std::mem::replace(&mut reached[b], true) {
                stack.extend(f.blocks[b].term.iter().flat_map(|t| t.edges()).map(|e| e.block.0 as usize));
            }
        }
        for b in f.blocks.iter().zip(&reached).filter(|(_, r)| **r).map(|(b, _)| b) {
            for inst in &b.insts {
                use InstKind as K;
                let escaping: Vec<Value> = match &inst.kind {
                    // Addresses read or written through, and addresses
                    // derived from (followed by `root`).
                    K::Load { .. }
                    | K::Copy { .. }
                    | K::Zero { .. }
                    | K::FreeBuf { .. }
                    | K::Elem { .. }
                    | K::Offset { .. }
                    | K::Index { .. }
                    | K::Bounds { .. } => Vec::new(),
                    K::Store { val, .. } => vec![*val],
                    other => crate::heddle::inst_uses(other),
                };
                let passed = match &inst.kind {
                    K::Call { args, .. } => args.clone(),
                    K::Native { name, args, .. }
                        if super::gvn::QUIET_NATIVES.contains(name) || super::gvn::PURE_NATIVES.contains(name) =>
                    {
                        args.clone()
                    }
                    _ => Vec::new(),
                };
                for v in escaping {
                    escape(&mut alias, v);
                    if !passed.contains(&v)
                        && let (Root::Slot(n), _) = alias.root(v)
                    {
                        alias.leaked[n as usize] = true;
                    }
                }
            }
            for v in b.term.iter().flat_map(super::term_uses) {
                escape(&mut alias, v);
                if let (Root::Slot(n), _) = alias.root(v) {
                    alias.leaked[n as usize] = true;
                }
            }
        }
        alias
    }

    /// Where `v` points: its root, and its offset from it, when known.
    pub fn root(&self, mut v: Value) -> (Root, Option<i64>) {
        let mut off = 0i64;
        loop {
            match self.kind.get(v.0 as usize).and_then(Option::as_ref) {
                Some(InstKind::Offset { ptr, off: o }) => {
                    off = off.wrapping_add(*o);
                    v = *ptr;
                }
                Some(InstKind::Slot(n)) => return (Root::Slot(*n), Some(off)),
                Some(InstKind::Global(n)) => return (Root::Data, Some(off.wrapping_add(i64::from(*n)))),
                Some(InstKind::Elem { header, off: ho, .. }) => return (Root::Buffer(*header, *ho), None),
                Some(InstKind::Index { base, .. }) => {
                    return match self.kind.get(base.0 as usize).and_then(Option::as_ref) {
                        // An element of the buffer a header points at.
                        Some(InstKind::Load { ptr, off: ho, .. }) => (Root::Buffer(*ptr, *ho), None),
                        Some(k) if allocates(k) => (Root::Heap(*base), None),
                        _ => (Root::Value(*base), None),
                    };
                }
                Some(k) if allocates(k) => return (Root::Heap(v), Some(off)),
                _ => return (Root::Value(v), Some(off)),
            }
        }
    }

    /// Whether two accesses may touch the same bytes.
    pub fn may_alias(&self, a: Access, b: Access) -> bool {
        let (ra, oa) = self.root(a.ptr);
        let (rb, ob) = self.root(b.ptr);
        let (oa, ob) = (oa.map(|o| o.wrapping_add(a.off)), ob.map(|o| o.wrapping_add(b.off)));
        if ra == rb {
            return match (oa, ob) {
                (Some(x), Some(y)) => overlap(x, a.size, y, b.size),
                _ => true,
            };
        }
        match (ra, rb) {
            (Root::Slot(_), Root::Slot(_)) => false,
            (Root::Heap(_), Root::Slot(_) | Root::Heap(_)) | (Root::Slot(_), Root::Heap(_)) => false,
            (Root::Slot(n), _) | (_, Root::Slot(n)) if !self.escapes[n as usize] => false,
            (Root::Slot(_), Root::Data) | (Root::Data, Root::Slot(_)) => false,
            (Root::Buffer(h, ho), _) => !self.in_header(h, ho, rb, ob, b.size),
            (_, Root::Buffer(h, ho)) => !self.in_header(h, ho, ra, oa, a.size),
            _ => true,
        }
    }

    /// Whether an access (root, offset, size) is inside the header (buffer,
    /// length, capacity) at `h + ho`.
    fn in_header(&self, h: Value, ho: i32, root: Root, off: Option<i64>, size: u64) -> bool {
        let (hr, hoff) = self.root(h);
        match (hoff, off) {
            (Some(start), Some(o)) if hr == root => {
                let start = start.wrapping_add(i64::from(ho));
                o >= start && o.wrapping_add(size as i64) <= start.wrapping_add(24)
            }
            _ => false,
        }
    }
}

/// Whether `k` allocates memory: what it gives is in no slot, and in no
/// other allocation.
fn allocates(k: &InstKind) -> bool {
    matches!(k, InstKind::Native { name: "mem.alloc" | "mem.calloc", .. })
}

/// Whether `[x, x + n)` and `[y, y + m)` overlap.
fn overlap(x: i64, n: u64, y: i64, m: u64) -> bool {
    x < y.wrapping_add(m as i64) && y < x.wrapping_add(n as i64)
}
