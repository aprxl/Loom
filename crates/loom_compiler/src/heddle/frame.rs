//! The frame pass: a shuttle's body, built as one function with
//! [`Term::Suspend`]s in it, becomes its *step function* (design/vm.md
//! §10.14), `step(op, frame, out, input) -> status`.
//!
//! What's live across a suspension point is kept in the shuttle's frame,
//! memory the step is given the address of, whose first 4 bytes are the
//! *state*: the suspension point it's at (0 before it starts, [`DONE`] once
//! it's finished) and the [`CLOSED`] bit. A value in registers is stored
//! there as the step suspends and loaded back when it resumes; a slot that
//! holds something across a suspension lives there for good. Each goes at
//! the lowest offset where it doesn't overlap one that's live at the same
//! time, largest alignment first, so `Frame.size` is about what the source
//! keeps, worked out on Heddle before any optimizing.
//!
//! A step:
//!
//! 1. reads the state, and marks the shuttle finished (an unravel, or a
//!    return, leaves it so);
//! 2. resuming (`op` 0), goes on after the suspension point the state
//!    names; closing (`op` 1), runs that point's cleanups (the pending
//!    `defer`s and drops) and gives `DONE`;
//! 3. at its next suspension point, stores what's live there and the new
//!    state, and returns [`YIELDED`] or [`PULLED`]; returning, it gives
//!    `DONE`.

use std::collections::{HashMap, HashSet};

use super::{BitOp, Block, BlockId, Cmp, Edge, Func, Inst, InstKind, Mem, Term, VTy, Value};

/// The state of a shuttle that has finished.
pub const DONE: u64 = 0x7fff_ffff;
/// The state's bit for a closed shuttle: a `pull` gives `none` without
/// suspending.
pub const CLOSED: u64 = 0x8000_0000;
/// What a step gives: it finished, it yielded (the item is at `out`), or
/// it's waiting for its input.
pub const FINISHED: u64 = 0;
pub const YIELDED: u64 = 1;
pub const PULLED: u64 = 2;
/// A step's `op`s.
pub const RESUME: u64 = 0;
pub const CLOSE: u64 = 1;
/// The status of the suspension at the start of the body (nothing is
/// returned there: it's where a shuttle that hasn't started is).
pub const START: u8 = u8::MAX;

/// What the frame pass needs from the builder.
#[derive(Clone, Debug)]
pub struct FrameInput {
    /// The step's parameters: `op`, the frame's address, where an item
    /// goes, and the input.
    pub entry: [Value; 4],
    /// How many bytes a value in registers takes in memory, where the
    /// builder knows (a local's parts): 8 for the rest.
    pub widths: HashMap<Value, Mem>,
    /// The slots a drop is registered for: zeroed where their values
    /// start.
    pub owned: HashSet<u32>,
    /// The slots of the shuttle's parameters, in order.
    pub params: Vec<u32>,
}

/// Where a shuttle's frame keeps things.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameLayout {
    pub size: u64,
    pub align: u64,
    /// Where each parameter goes, if the body ever reads it.
    pub params: Vec<Option<u64>>,
}

/// A set of small numbers.
#[derive(Clone, Default, PartialEq, Eq)]
struct Bits(Vec<u64>);

impl Bits {
    fn insert(&mut self, i: usize) -> bool {
        let (w, b) = (i / 64, i % 64);
        if self.0.len() <= w {
            self.0.resize(w + 1, 0);
        }
        let new = self.0[w] & (1 << b) == 0;
        self.0[w] |= 1 << b;
        new
    }

    fn remove(&mut self, i: usize) {
        if let Some(w) = self.0.get_mut(i / 64) {
            *w &= !(1 << (i % 64));
        }
    }

    fn contains(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    fn union(&mut self, o: &Bits) -> bool {
        if self.0.len() < o.0.len() {
            self.0.resize(o.0.len(), 0);
        }
        let mut changed = false;
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            let n = *a | b;
            changed |= n != *a;
            *a = n;
        }
        changed
    }

    fn intersects(&self, o: &Bits) -> bool {
        self.0.iter().zip(&o.0).any(|(a, b)| a & b != 0)
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0
            .iter()
            .enumerate()
            .flat_map(|(w, &bits)| (0..64).filter(move |b| bits & (1 << b) != 0).map(move |b| w * 64 + b))
    }
}

/// A suspension point: the block that suspends, its status, and the
/// blocks resuming and closing it go to.
struct Point {
    block: usize,
    status: u8,
    resume: usize,
    close: usize,
}

/// Turns `f`, a shuttle's body, into its step function: its frame's
/// layout.
pub fn lay_out(f: &mut Func, inp: &FrameInput) -> Result<FrameLayout, String> {
    let [op, frame, _, _] = inp.entry;
    let points = landings(f)?;
    let n = f.blocks.len();

    // ---------------------------------------------------------------- values
    let live = value_liveness(f);
    let mut def_at: Vec<Option<(usize, usize)>> = vec![None; f.types.len()];
    for (b, block) in f.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            for &r in &inst.results {
                def_at[r.0 as usize] = Some((b, i));
            }
        }
    }
    let entry: HashSet<Value> = inp.entry.iter().copied().collect();
    let remat = |v: Value| -> bool {
        let mut v = v;
        loop {
            if entry.contains(&v) {
                return true;
            }
            let Some((b, i)) = def_at[v.0 as usize] else { return false };
            match &f.blocks[b].insts[i].kind {
                InstKind::Iconst(_)
                | InstKind::Fconst(_)
                | InstKind::Str(_)
                | InstKind::Global(_)
                | InstKind::FuncRef(_)
                | InstKind::Slot(_)
                | InstKind::Undef => return true,
                InstKind::Offset { ptr, .. } => v = *ptr,
                _ => return false,
            }
        }
    };
    // What's live across each point, besides the step's parameters.
    let across: Vec<Vec<Value>> = points
        .iter()
        .map(|p| {
            let mut s = live[p.resume].clone();
            s.union(&live[p.close]);
            s.iter().map(|v| Value(v as u32)).filter(|v| !entry.contains(v)).collect()
        })
        .collect();
    let spilled: Vec<Value> = {
        let mut all: Vec<Value> = across.iter().flatten().copied().filter(|&v| !remat(v)).collect();
        all.sort();
        all.dedup();
        all
    };

    // ---------------------------------------------------------------- slots
    let slots = slot_liveness(f);
    let frame_slots: Vec<u32> = {
        let mut s = Bits::default();
        for p in &points {
            s.union(&slots.live_in[p.resume]);
            s.union(&slots.live_in[p.close]);
        }
        s.iter().map(|i| i as u32).collect()
    };

    // ---------------------------------------------------------------- layout
    // The things the frame holds: spilled values, then slots.
    #[derive(Clone, Copy)]
    enum Item {
        Value(Value),
        Slot(u32),
    }
    let mut items: Vec<(Item, u64, u64)> = Vec::new();
    let width = |v: Value| inp.widths.get(&v).copied().unwrap_or(Mem::W64);
    for &v in &spilled {
        let w = width(v).bytes();
        items.push((Item::Value(v), w, w));
    }
    for &s in &frame_slots {
        let slot = f.slots[s as usize];
        items.push((Item::Slot(s), slot.size, slot.align.max(1)));
    }
    let index_of = |it: Item| -> usize {
        items
            .iter()
            .position(|(i, ..)| match (i, it) {
                (Item::Value(a), Item::Value(b)) => *a == b,
                (Item::Slot(a), Item::Slot(b)) => *a == b,
                _ => false,
            })
            .expect("an item")
    };
    let m = items.len();
    let mut clash = vec![vec![false; m]; m];
    for (k, p) in points.iter().enumerate() {
        let mut here: Vec<usize> =
            across[k].iter().filter(|v| !remat(**v)).map(|&v| index_of(Item::Value(v))).collect();
        let mut s = slots.live_in[p.resume].clone();
        s.union(&slots.live_in[p.close]);
        here.extend(s.iter().map(|i| index_of(Item::Slot(i as u32))));
        for &a in &here {
            for &b in &here {
                clash[a][b] = true;
            }
        }
    }
    for (a, &sa) in frame_slots.iter().enumerate() {
        for &sb in frame_slots.iter().skip(a + 1) {
            if slots.present[sa as usize].intersects(&slots.present[sb as usize]) {
                let (ia, ib) = (index_of(Item::Slot(sa)), index_of(Item::Slot(sb)));
                clash[ia][ib] = true;
                clash[ib][ia] = true;
            }
        }
    }
    // Largest alignment first; then in the order they first appear.
    let first = first_seen(f);
    let order_key = |it: &Item| match it {
        Item::Value(v) => first.values.get(v).copied().unwrap_or(usize::MAX),
        Item::Slot(s) => first.slots.get(s).copied().unwrap_or(usize::MAX),
    };
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(items[i].2), order_key(&items[i].0), i));
    let mut offset: Vec<Option<u64>> = vec![None; m];
    let mut end = 4u64;
    let mut align = 4u64;
    for &i in &order {
        let (_, size, al) = items[i];
        align = align.max(al);
        let taken: Vec<(u64, u64)> = std::iter::once((0, 4))
            .chain((0..m).filter(|&j| clash[i][j] && j != i).filter_map(|j| offset[j].map(|o| (o, o + items[j].1))))
            .collect();
        let mut at = 0u64;
        loop {
            at = at.div_ceil(al) * al;
            match taken.iter().find(|&&(lo, hi)| at < hi && lo < at + size.max(1)) {
                Some(&(_, hi)) => at = hi,
                None => break,
            }
        }
        offset[i] = Some(at);
        end = end.max(at + size);
    }
    let size = end.div_ceil(align) * align;
    let value_off: HashMap<Value, u64> = items
        .iter()
        .zip(&offset)
        .filter_map(|((it, ..), o)| match it {
            Item::Value(v) => Some((*v, o.expect("placed"))),
            Item::Slot(_) => None,
        })
        .collect();
    let slot_off: HashMap<u32, u64> = items
        .iter()
        .zip(&offset)
        .filter_map(|((it, ..), o)| match it {
            Item::Slot(s) => Some((*s, o.expect("placed"))),
            Item::Value(_) => None,
        })
        .collect();
    let params = inp.params.iter().map(|s| slot_off.get(s).copied()).collect();

    // ---------------------------------------------------------------- rewriting
    let new_value = |f: &mut Func, ty: VTy| {
        f.types.push(ty);
        Value(f.types.len() as u32 - 1)
    };
    let inst = |kind: InstKind, results: Vec<Value>| Inst { kind, results, span: None, unwind: None, site: None };
    // Reloads (and recomputations) where each point resumes or closes.
    let mut reloads: HashMap<Value, Vec<(usize, Value)>> = HashMap::new();
    for (k, p) in points.iter().enumerate() {
        for target in [p.resume, p.close] {
            let mut at_start = Vec::new();
            for &v in &across[k] {
                if !live[target].contains(v.0 as usize) {
                    continue;
                }
                let ty = f.ty(v);
                let nv = if let Some(&off) = value_off.get(&v) {
                    let nv = new_value(f, ty);
                    at_start.push(inst(InstKind::Load { mem: width(v), ptr: frame, off: off as i32 }, vec![nv]));
                    nv
                } else {
                    // Recomputed: a constant, or an address in the frame.
                    let mut chain = Vec::new();
                    let mut w = v;
                    while !entry.contains(&w) {
                        let (b, i) = def_at[w.0 as usize].expect("defined");
                        let kind = f.blocks[b].insts[i].kind.clone();
                        let next = match &kind {
                            InstKind::Offset { ptr, .. } => Some(*ptr),
                            _ => None,
                        };
                        chain.push((w, kind));
                        match next {
                            Some(p) => w = p,
                            None => break,
                        }
                    }
                    let mut prev: Option<Value> = if entry.contains(&w) { Some(w) } else { None };
                    let mut result = v;
                    for (w, mut kind) in chain.into_iter().rev() {
                        if let (InstKind::Offset { ptr, .. }, Some(p)) = (&mut kind, prev) {
                            *ptr = p;
                        }
                        let nv = new_value(f, f.ty(w));
                        at_start.push(inst(kind, vec![nv]));
                        prev = Some(nv);
                        result = nv;
                    }
                    if result == v {
                        continue; // a parameter of the step: there anyway
                    }
                    result
                };
                reloads.entry(v).or_default().push((target, nv));
            }
            let block = &mut f.blocks[target];
            block.insts.splice(0..0, at_start);
        }
    }
    // Uses of each value reloaded: the definition that reaches them.
    let mut defs: HashMap<Value, HashMap<usize, Value>> = HashMap::new();
    for (&v, rs) in &reloads {
        let d = defs.entry(v).or_default();
        if let Some((b, _)) = def_at[v.0 as usize] {
            d.insert(b, v);
        } else if let Some(b) = f.blocks.iter().position(|bl| bl.params.contains(&v)) {
            d.insert(b, v);
        }
        for &(b, nv) in rs {
            d.insert(b, nv);
        }
    }
    // Suspensions become stores and returns; slots in the frame become
    // addresses in it; lifetimes go.
    let cl = new_value(f, VTy::Int);
    for b in 0..n {
        let insts = std::mem::take(&mut f.blocks[b].insts);
        let mut out = Vec::with_capacity(insts.len());
        for i in insts {
            match i.kind {
                InstKind::Slot(s) if slot_off.contains_key(&s) => {
                    out.push(Inst { kind: InstKind::Offset { ptr: frame, off: slot_off[&s] as i64 }, ..i });
                }
                InstKind::Life { slot, end: false } if b != 0 && inp.owned.contains(&slot) => {
                    let ptr = new_value(f, VTy::Ptr);
                    let kind = match slot_off.get(&slot) {
                        Some(&off) => InstKind::Offset { ptr: frame, off: off as i64 },
                        None => InstKind::Slot(slot),
                    };
                    out.push(inst(kind, vec![ptr]));
                    let size = f.slots[slot as usize].size;
                    out.push(inst(InstKind::Zero { dst: ptr, size }, Vec::new()));
                }
                InstKind::Life { .. } => {}
                _ => out.push(i),
            }
        }
        f.blocks[b].insts = out;
    }
    for (k, p) in points.iter().enumerate().skip(1) {
        let mut tail = Vec::new();
        for &v in &across[k] {
            if let Some(&off) = value_off.get(&v) {
                tail.push(inst(InstKind::Store { mem: width(v), ptr: frame, off: off as i32, val: v }, Vec::new()));
            }
        }
        let kv = new_value(f, VTy::Int);
        tail.push(inst(InstKind::Iconst(k as u64), vec![kv]));
        let sv = new_value(f, VTy::Int);
        tail.push(inst(InstKind::Bit { op: BitOp::Or, a: cl, b: kv }, vec![sv]));
        tail.push(inst(InstKind::Store { mem: Mem::U32, ptr: frame, off: 0, val: sv }, Vec::new()));
        let status = new_value(f, VTy::Int);
        tail.push(inst(InstKind::Iconst(u64::from(p.status)), vec![status]));
        let block = &mut f.blocks[p.block];
        block.insts.extend(tail);
        block.term = Some(Term::Ret(vec![status]));
        block.term_unwind = None;
    }
    update_ssa(f, &defs)?;

    // ---------------------------------------------------------------- dispatch
    let resumes: Vec<usize> = points.iter().map(|p| p.resume).collect();
    let closes: Vec<usize> = points.iter().map(|p| p.close).collect();
    let done = push_block(f);
    let z = new_value(f, VTy::Int);
    f.blocks[done].insts.push(inst(InstKind::Iconst(FINISHED), vec![z]));
    f.blocks[done].term = Some(Term::Ret(vec![z]));
    let mut entry_insts = Vec::new();
    let k = |kind: InstKind, f: &mut Func, insts: &mut Vec<Inst>| {
        let v = new_value(f, VTy::Int);
        insts.push(inst(kind, vec![v]));
        v
    };
    let e = &mut entry_insts;
    let st = k(InstKind::Load { mem: Mem::U32, ptr: frame, off: 0 }, f, e);
    let mask = k(InstKind::Iconst(DONE), f, e);
    let pt = k(InstKind::Bit { op: BitOp::And, a: st, b: mask }, f, e);
    let cbit = k(InstKind::Iconst(CLOSED), f, e);
    e.push(inst(InstKind::Bit { op: BitOp::And, a: st, b: cbit }, vec![cl]));
    let w = k(InstKind::Bit { op: BitOp::Or, a: cl, b: mask }, f, e);
    e.push(inst(InstKind::Store { mem: Mem::U32, ptr: frame, off: 0, val: w }, Vec::new()));
    let one = k(InstKind::Iconst(CLOSE), f, e);
    let closing = k(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: op, b: one }, f, e);
    let to_close = switch(f, pt, &closes, done);
    let to_resume = switch(f, pt, &resumes, done);
    let entry_block = &mut f.blocks[0];
    entry_block.insts.extend(entry_insts);
    entry_block.term = Some(Term::Br {
        cond: closing,
        then: Edge { block: BlockId(to_close as u32), args: Vec::new() },
        else_: Edge { block: BlockId(to_resume as u32), args: Vec::new() },
    });
    entry_block.term_unwind = None;
    Ok(FrameLayout { size, align, params })
}

fn push_block(f: &mut Func) -> usize {
    f.blocks.push(Block::default());
    f.blocks.len() - 1
}

/// A block that goes to `targets[x]`, or to `default` when `x` is past
/// them: compares, halving the range each time.
fn switch(f: &mut Func, x: Value, targets: &[usize], default: usize) -> usize {
    let new_value = |f: &mut Func| {
        f.types.push(VTy::Int);
        Value(f.types.len() as u32 - 1)
    };
    let jump = |b: usize| Edge { block: BlockId(b as u32), args: Vec::new() };
    let head = push_block(f);
    let (n, c) = (new_value(f), new_value(f));
    let inst = |kind, results| Inst { kind, results, span: None, unwind: None, site: None };
    f.blocks[head].insts.push(inst(InstKind::Iconst(targets.len() as u64), vec![n]));
    f.blocks[head].insts.push(inst(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: x, b: n }, vec![c]));
    fn tree(f: &mut Func, x: Value, lo: usize, targets: &[usize]) -> usize {
        if targets.len() == 1 {
            return targets[0];
        }
        let mid = targets.len() / 2;
        let left = tree(f, x, lo, &targets[..mid]);
        let right = tree(f, x, lo + mid, &targets[mid..]);
        f.blocks.push(Block::default());
        let b = f.blocks.len() - 1;
        f.types.push(VTy::Int);
        let k = Value(f.types.len() as u32 - 1);
        f.types.push(VTy::Int);
        let c = Value(f.types.len() as u32 - 1);
        let inst = |kind, results| Inst { kind, results, span: None, unwind: None, site: None };
        f.blocks[b].insts.push(inst(InstKind::Iconst((lo + mid) as u64), vec![k]));
        f.blocks[b].insts.push(inst(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: x, b: k }, vec![c]));
        let jump = |b: usize| Edge { block: BlockId(b as u32), args: Vec::new() };
        f.blocks[b].term = Some(Term::Br { cond: c, then: jump(left), else_: jump(right) });
        b
    }
    let inside = tree(f, x, 0, targets);
    f.blocks[head].term = Some(Term::Br { cond: c, then: jump(inside), else_: jump(default) });
    head
}

/// Gives every suspension edge a block of its own, which jumps on to where
/// it went: where its point's values are loaded back. The points, the start
/// first.
fn landings(f: &mut Func) -> Result<Vec<Point>, String> {
    let mut points = Vec::new();
    for b in 0..f.blocks.len() {
        let Some(Term::Suspend { status, resume, close }) = f.blocks[b].term.clone() else { continue };
        let mut land = |e: Edge| {
            f.blocks.push(Block { term: Some(Term::Jump(e)), ..Block::default() });
            f.blocks.len() - 1
        };
        let (r, c) = (land(resume), land(close));
        f.blocks[b].term = Some(Term::Suspend {
            status,
            resume: Edge { block: BlockId(r as u32), args: Vec::new() },
            close: Edge { block: BlockId(c as u32), args: Vec::new() },
        });
        points.push(Point { block: b, status, resume: r, close: c });
    }
    match points.first() {
        Some(p) if p.block == 0 && p.status == START => Ok(points),
        _ => Err("a shuttle's body that doesn't start suspended".into()),
    }
}

/// The values in registers live where each block starts.
fn value_liveness(f: &Func) -> Vec<Bits> {
    let n = f.blocks.len();
    let mut ue = vec![Bits::default(); n];
    let mut defs = vec![Bits::default(); n];
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, block) in f.blocks.iter().enumerate() {
        for &p in &block.params {
            defs[b].insert(p.0 as usize);
        }
        let uses = |v: Value, defs: &Bits, ue: &mut Bits| {
            if !defs.contains(v.0 as usize) {
                ue.insert(v.0 as usize);
            }
        };
        for inst in &block.insts {
            for u in super::inst_uses(&inst.kind) {
                uses(u, &defs[b], &mut ue[b]);
            }
            for &r in &inst.results {
                defs[b].insert(r.0 as usize);
            }
        }
        if let Some(t) = &block.term {
            for u in term_uses(t) {
                uses(u, &defs[b], &mut ue[b]);
            }
            succs[b] = t.edges().iter().map(|e| e.block.0 as usize).collect();
        }
    }
    let mut live_in = ue.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            let mut out = Bits::default();
            for &s in &succs[b] {
                let mut s_in = live_in[s].clone();
                for &p in &f.blocks[s].params {
                    s_in.remove(p.0 as usize);
                }
                out.union(&s_in);
            }
            for d in defs[b].iter() {
                out.remove(d);
            }
            changed |= live_in[b].union(&out);
        }
    }
    live_in
}

/// What a terminator reads, its edges' arguments included.
fn term_uses(t: &Term) -> Vec<Value> {
    let mut out = match t {
        Term::Br { cond, .. } => vec![*cond],
        Term::Ret(vs) => vs.clone(),
        Term::Unravel { ptr, len } => vec![*ptr, *len],
        _ => Vec::new(),
    };
    for e in t.edges() {
        out.extend(&e.args);
    }
    out
}

struct SlotLiveness {
    /// The slots live where each block starts.
    live_in: Vec<Bits>,
    /// For each slot, the blocks it's live in or used in.
    present: Vec<Bits>,
}

/// Where each slot holds something that matters: from where its value
/// starts (`life.start`) to its last use, through every address made from
/// it, and its `life.end`.
fn slot_liveness(f: &Func) -> SlotLiveness {
    let n = f.blocks.len();
    let nslots = f.slots.len();
    // The slots each value may be an address in.
    let mut derived: HashMap<Value, Bits> = HashMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        for block in &f.blocks {
            for inst in &block.insts {
                let from = match &inst.kind {
                    InstKind::Slot(s) => {
                        let mut b = Bits::default();
                        b.insert(*s as usize);
                        Some(b)
                    }
                    InstKind::Offset { ptr, .. } | InstKind::Index { base: ptr, .. } => derived.get(ptr).cloned(),
                    _ => None,
                };
                if let (Some(from), Some(&r)) = (from, inst.results.first()) {
                    changed |= derived.entry(r).or_default().union(&from);
                }
            }
            for e in block.term.iter().flat_map(|t| t.edges()) {
                for (a, p) in e.args.iter().zip(&f.blocks[e.block.0 as usize].params) {
                    if let Some(from) = derived.get(a).cloned() {
                        changed |= derived.entry(*p).or_default().union(&from);
                    }
                }
            }
        }
    }
    let mut ue = vec![Bits::default(); n];
    let mut kill = vec![Bits::default(); n];
    let mut seen = vec![Bits::default(); n];
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, block) in f.blocks.iter().enumerate() {
        let use_slots = |v: Value, kill: &Bits, ue: &mut Bits, seen: &mut Bits| {
            if let Some(d) = derived.get(&v) {
                for s in d.iter() {
                    seen.insert(s);
                    if !kill.contains(s) {
                        ue.insert(s);
                    }
                }
            }
        };
        for inst in &block.insts {
            match inst.kind {
                InstKind::Life { slot, end: false } => {
                    seen[b].insert(slot as usize);
                    if !ue[b].contains(slot as usize) {
                        kill[b].insert(slot as usize);
                    }
                }
                InstKind::Life { slot, end: true } => {
                    seen[b].insert(slot as usize);
                    if !kill[b].contains(slot as usize) {
                        ue[b].insert(slot as usize);
                    }
                }
                _ => {
                    for u in super::inst_uses(&inst.kind) {
                        use_slots(u, &kill[b], &mut ue[b], &mut seen[b]);
                    }
                }
            }
        }
        if let Some(t) = &block.term {
            for u in term_uses(t) {
                use_slots(u, &kill[b], &mut ue[b], &mut seen[b]);
            }
            succs[b] = t.edges().iter().map(|e| e.block.0 as usize).collect();
        }
        succs[b].extend(block.pads().iter().map(|p| p.0 as usize));
    }
    let mut live_in = ue.clone();
    let mut live_out = vec![Bits::default(); n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            let mut out = Bits::default();
            for &s in &succs[b] {
                out.union(&live_in[s]);
            }
            live_out[b] = out.clone();
            for k in kill[b].iter() {
                out.remove(k);
            }
            changed |= live_in[b].union(&out);
        }
    }
    let mut present = vec![Bits::default(); nslots];
    for b in 0..n {
        let mut here = live_in[b].clone();
        here.union(&live_out[b]);
        here.union(&seen[b]);
        for s in here.iter() {
            if s < nslots {
                present[s].insert(b);
            }
        }
    }
    SlotLiveness { live_in, present }
}

/// Where each value and slot first appears, in block order: the order
/// things are declared in, near enough.
struct FirstSeen {
    values: HashMap<Value, usize>,
    slots: HashMap<u32, usize>,
}

fn first_seen(f: &Func) -> FirstSeen {
    let mut out = FirstSeen { values: HashMap::new(), slots: HashMap::new() };
    let mut at = 0usize;
    for block in &f.blocks {
        for &p in &block.params {
            out.values.entry(p).or_insert(at);
            at += 1;
        }
        for inst in &block.insts {
            if let InstKind::Slot(s) | InstKind::Life { slot: s, .. } = inst.kind {
                out.slots.entry(s).or_insert(at);
            }
            for &r in &inst.results {
                out.values.entry(r).or_insert(at);
            }
            at += 1;
        }
    }
    out
}

/// Makes every use of a value that `defs` gives more than one definition
/// for (its own, and its reloads, by block) read the one that reaches it,
/// adding block parameters where they meet.
fn update_ssa(f: &mut Func, defs: &HashMap<Value, HashMap<usize, Value>>) -> Result<(), String> {
    if defs.is_empty() {
        return Ok(());
    }
    let n = f.blocks.len();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, block) in f.blocks.iter().enumerate() {
        for e in block.term.iter().flat_map(|t| t.edges()) {
            preds[e.block.0 as usize].push(b);
        }
    }
    for p in &mut preds {
        p.dedup();
    }
    let mut vars: Vec<Value> = defs.keys().copied().collect();
    vars.sort();
    for v in vars {
        let d = &defs[&v];
        let mut at_entry: HashMap<usize, Value> = HashMap::new();
        // Where `v` is defined in its own block: uses after it see it.
        let own = f.blocks.iter().enumerate().find_map(|(b, bl)| {
            if bl.params.contains(&v) {
                return Some((b, None));
            }
            bl.insts.iter().position(|i| i.results.contains(&v)).map(|i| (b, Some(i)))
        });
        for b in 0..n {
            // Reloads are at the start of their block: everything there
            // sees them.
            let reload = d.get(&b).copied().filter(|&nv| nv != v);
            let n_insts = f.blocks[b].insts.len();
            for i in 0..=n_insts {
                let reads: Vec<Value> = if i < n_insts {
                    super::inst_uses(&f.blocks[b].insts[i].kind).collect()
                } else {
                    f.blocks[b].term.as_ref().map(term_uses).unwrap_or_default()
                };
                if !reads.contains(&v) {
                    continue;
                }
                let seen = match (reload, own) {
                    (Some(nv), _) => nv,
                    (None, Some((ob, oi))) if ob == b && oi.is_none_or(|oi| oi < i) => v,
                    _ => read_entry(f, &preds, d, v, own, b, &mut at_entry)?,
                };
                if seen == v {
                    continue;
                }
                if i < n_insts {
                    super::build::rewrite(&mut f.blocks[b].insts[i].kind, |x| if x == v { seen } else { x });
                } else if let Some(t) = &mut f.blocks[b].term {
                    match t {
                        Term::Br { cond, .. } if *cond == v => *cond = seen,
                        Term::Ret(vs) => vs.iter_mut().filter(|x| **x == v).for_each(|x| *x = seen),
                        Term::Unravel { ptr, len } => {
                            if *ptr == v {
                                *ptr = seen;
                            }
                            if *len == v {
                                *len = seen;
                            }
                        }
                        _ => {}
                    }
                    for e in t.edges_mut() {
                        e.args.iter_mut().filter(|x| **x == v).for_each(|x| *x = seen);
                    }
                }
            }
        }
    }
    Ok(())
}

/// The definition of `v` that reaches the start of block `b`.
fn read_entry(
    f: &mut Func,
    preds: &[Vec<usize>],
    d: &HashMap<usize, Value>,
    v: Value,
    own: Option<(usize, Option<usize>)>,
    b: usize,
    memo: &mut HashMap<usize, Value>,
) -> Result<Value, String> {
    if let Some(&x) = memo.get(&b) {
        return Ok(x);
    }
    let read_end = |f: &mut Func, p: usize, memo: &mut HashMap<usize, Value>| -> Result<Value, String> {
        if let Some(&nv) = d.get(&p) {
            return Ok(nv);
        }
        if own.is_some_and(|(ob, _)| ob == p) {
            return Ok(v);
        }
        read_entry(f, preds, d, v, own, p, memo)
    };
    let ps = preds[b].clone();
    match ps.as_slice() {
        [] => Err(format!("v{} has no definition reaching b{b}", v.0)),
        [p] => {
            let x = read_end(f, *p, memo)?;
            memo.insert(b, x);
            Ok(x)
        }
        _ => {
            f.types.push(f.ty(v));
            let param = Value(f.types.len() as u32 - 1);
            f.blocks[b].params.push(param);
            memo.insert(b, param);
            for p in ps {
                let x = read_end(f, p, memo)?;
                for e in f.blocks[p].term.as_mut().expect("ends").edges_mut() {
                    if e.block.0 as usize == b {
                        e.args.push(x);
                    }
                }
            }
            Ok(param)
        }
    }
}
