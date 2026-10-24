//! What a counted loop leaves in memory (an addition to pass 5 of
//! design/vm.md §7): a loop that goes `p` from 0 below `bound` by one, and
//! adds one to a word of the frame each time round (a list's length, in
//! `for i in 0..n { xs.push(i) }`), leaves it `bound` more than it was.
//!
//! GVN can't know that: it forgets what a loop writes at its head. So this
//! says it, as a `store` of the value that's there already at the start of
//! the block the loop exits to, which GVN then keeps, and the checks of
//! every index below `n` against `xs`'s length go.
//!
//! Narrow on purpose. The loop's one way out is its header's test
//! `p < bound` (`p` starting at 0 and stepping by 1 on the one back edge,
//! `bound` made outside it, and unsigned, or a constant that isn't
//! negative); the word has exactly one writer in
//! the loop, `store word, load(word) + 1`, which every iteration but the
//! last passes once; and nothing else in the loop can reach the word (no
//! calls, and only natives that write nothing, or `mem.grow`, which writes
//! two other words of the header it's given).

use super::alias::{Alias, Root};
use super::cfg::{Cfg, Loop};
use super::gvn::{PURE_NATIVES, QUIET_NATIVES};
use crate::heddle::{Cmp, Func, Inst, InstKind, IntOp, Mem, Module, Policy, Term, VTy, Value};
use crate::ty::IntTy;

pub fn cells(module: &mut Module) {
    for f in &mut module.funcs {
        cells_func(f);
    }
}

/// What to put at the start of a loop's exit block.
struct Plan {
    exit: usize,
    slot: u32,
    /// The word's byte offset in the slot.
    off: i64,
    /// What the word was before the loop: `None` for zero.
    init: Option<Value>,
    bound: Value,
    /// How the loop's own addition was typed.
    ty: IntTy,
    span: Option<loom_syntax::source::Span>,
}

fn cells_func(f: &mut Func) {
    let cfg = Cfg::new(f);
    let loops = cfg.loops();
    if loops.is_empty() {
        return;
    }
    let alias = Alias::new(f);
    let n = f.types.len();
    // Where each value is made: its block and, for an instruction's result,
    // its place in it.
    let mut home = vec![usize::MAX; n];
    let mut def: Vec<Option<(usize, usize)>> = vec![None; n];
    for (b, block) in f.blocks.iter().enumerate() {
        for &p in &block.params {
            home[p.0 as usize] = b;
        }
        for (i, inst) in block.insts.iter().enumerate() {
            for &r in &inst.results {
                home[r.0 as usize] = b;
                def[r.0 as usize] = Some((b, i));
            }
        }
    }
    let cx = Cx { f, cfg: &cfg, loops: &loops, alias: &alias, home: &home, def: &def };
    let plans: Vec<Plan> = loops.iter().flat_map(|l| cx.plans(l)).collect();
    for plan in plans {
        let ptr = Value(f.types.len() as u32);
        f.types.push(VTy::Ptr);
        let mut insts = vec![Inst {
            kind: InstKind::Slot(plan.slot),
            results: vec![ptr],
            span: plan.span,
            unwind: None,
            site: None,
        }];
        let val = match plan.init {
            None => plan.bound,
            Some(init) => {
                let v = Value(f.types.len() as u32);
                f.types.push(VTy::Int);
                insts.push(Inst {
                    kind: InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: plan.ty, a: init, b: plan.bound },
                    results: vec![v],
                    span: plan.span,
                    unwind: None,
                    site: None,
                });
                v
            }
        };
        insts.push(Inst {
            kind: InstKind::Store { ptr, off: plan.off as i32, mem: Mem::W64, val },
            results: Vec::new(),
            span: plan.span,
            unwind: None,
            site: None,
        });
        f.blocks[plan.exit].insts.splice(0..0, insts);
    }
}

struct Cx<'a> {
    f: &'a Func,
    cfg: &'a Cfg,
    loops: &'a [Loop],
    alias: &'a Alias,
    home: &'a [usize],
    def: &'a [Option<(usize, usize)>],
}

impl Cx<'_> {
    fn kind(&self, v: Value) -> Option<&InstKind> {
        let (b, i) = self.def[v.0 as usize]?;
        Some(&self.f.blocks[b].insts[i].kind)
    }

    fn constant(&self, v: Value) -> Option<u64> {
        match self.kind(v)? {
            InstKind::Iconst(c) => Some(*c),
            _ => None,
        }
    }

    /// The words of the frame loop `l` counts, with what to leave them.
    fn plans(&self, l: &Loop) -> Vec<Plan> {
        let f = self.f;
        let h = l.header;
        // One way in, one way round.
        let outside: Vec<usize> =
            self.cfg.preds[h].iter().copied().filter(|&p| self.cfg.reached(p) && !l.inside[p]).collect();
        let latches: Vec<usize> =
            self.cfg.preds[h].iter().copied().filter(|&p| self.cfg.reached(p) && l.inside[p]).collect();
        let ([pre], [latch]) = (outside.as_slice(), latches.as_slice()) else { return Vec::new() };
        let (pre, latch) = (*pre, *latch);
        // The header: `p < bound`, in or out.
        let Some(Term::Br { cond, then, else_ }) = &f.blocks[h].term else { return Vec::new() };
        if !l.inside[then.block.0 as usize] || l.inside[else_.block.0 as usize] || !else_.args.is_empty() {
            return Vec::new();
        }
        let exit = else_.block.0 as usize;
        if self.cfg.preds[exit].as_slice() != [h] || !f.blocks[exit].params.is_empty() {
            return Vec::new();
        }
        let Some(&InstKind::ICmp { cmp: Cmp::Lt, unsigned, a: p, b: bound }) = self.kind(*cond) else {
            return Vec::new();
        };
        // Signed, it counts `bound` trips only when `bound` isn't negative.
        if !unsigned && !self.constant(bound).is_some_and(|c| (c as i64) >= 0) {
            return Vec::new();
        }
        let Some(pi) = f.blocks[h].params.iter().position(|&x| x == p) else { return Vec::new() };
        let enters = |from: usize| -> Option<Value> {
            f.blocks[from].term.as_ref()?.edges().find(|e| e.block.0 as usize == h).map(|e| e.args[pi])
        };
        let (Some(first), Some(next)) = (enters(pre), enters(latch)) else { return Vec::new() };
        if self.constant(first) != Some(0) {
            return Vec::new();
        }
        let steps = matches!(self.kind(next),
            Some(&InstKind::Arith { op: IntOp::Add, a, b, .. })
                if (a == p && self.constant(b) == Some(1)) || (b == p && self.constant(a) == Some(1)));
        let bound_home = self.home[bound.0 as usize];
        if !steps || bound_home == usize::MAX || l.inside[bound_home] {
            return Vec::new();
        }
        // The loop leaves by the header only.
        for &b in &l.blocks {
            let Some(t) = &f.blocks[b].term else { return Vec::new() };
            if matches!(t, Term::Ret(_) | Term::Suspend { .. }) {
                return Vec::new();
            }
            for e in t.edges() {
                let to = e.block.0 as usize;
                if !l.inside[to] && !(b == h && to == exit) {
                    return Vec::new();
                }
            }
        }
        // The words something adds one to.
        let mut out = Vec::new();
        for &sb in &l.blocks {
            for (si, inst) in f.blocks[sb].insts.iter().enumerate() {
                let InstKind::Store { ptr, off, mem: Mem::W64, val } = inst.kind else { continue };
                let (Root::Slot(slot), Some(base)) = self.alias.root(ptr) else { continue };
                let at = base + i64::from(off);
                // `val` is a load of the word, plus one.
                let Some(&InstKind::Arith { op: IntOp::Add, ty, a, b, .. }) = self.kind(val) else { continue };
                let x = if self.constant(b) == Some(1) {
                    a
                } else if self.constant(a) == Some(1) {
                    b
                } else {
                    continue;
                };
                let Some((lb, li)) = self.def[x.0 as usize] else { continue };
                let InstKind::Load { ptr: lp, off: lo, mem: Mem::W64 } = f.blocks[lb].insts[li].kind else { continue };
                if self.alias.root(lp) != (Root::Slot(slot), Some(at - i64::from(lo))) {
                    continue;
                }
                // Before the store, in the same trip round, and the store is
                // passed by every trip that gets to the end of the loop, once.
                let before = lb != h
                    && sb != h
                    && if lb == sb { li < si } else { self.cfg.dominates(lb, sb) }
                    && l.inside[lb]
                    && self.cfg.dominates(sb, latch);
                let inner = self.loops.iter().any(|o| {
                    o.header != h
                        && o.inside[sb]
                        && o.blocks.len() < l.blocks.len()
                        && o.blocks.iter().all(|&q| l.inside[q])
                });
                if !before || inner || !self.only_writer(l, (sb, si), slot, at) {
                    continue;
                }
                let Some(init) = self.before_loop(pre, slot, at) else { continue };
                out.push(Plan { exit, slot, off: at, init, bound, ty, span: f.blocks[h].term_span });
            }
        }
        out
    }

    /// Whether nothing in `l` but the store at `store` may write the eight
    /// bytes at `at` of `slot`.
    fn only_writer(&self, l: &Loop, store: (usize, usize), slot: u32, at: i64) -> bool {
        for &b in &l.blocks {
            for (i, inst) in self.f.blocks[b].insts.iter().enumerate() {
                if (b, i) == store {
                    continue;
                }
                if !self.leaves(&inst.kind, slot, at) {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `k` leaves the eight bytes at `at` of `slot` as they are.
    fn leaves(&self, k: &InstKind, slot: u32, at: i64) -> bool {
        let touches = |ptr: Value, off: i64, size: u64| -> bool {
            match self.alias.root(ptr) {
                (Root::Slot(s), Some(o)) if s == slot => {
                    let start = o.wrapping_add(off);
                    start < at + 8 && at < start + size as i64
                }
                (Root::Slot(s), None) if s == slot => true,
                (Root::Slot(_), _) => false,
                // An address that isn't in a slot reaches this one only if
                // the slot's address got out.
                _ => self.alias.escapes.get(slot as usize).copied().unwrap_or(true),
            }
        };
        match k {
            InstKind::Store { ptr, off, mem, .. } => !touches(*ptr, i64::from(*off), mem.bytes()),
            InstKind::Copy { dst, size, .. } | InstKind::Zero { dst, size } => !touches(*dst, 0, *size),
            InstKind::Native { name: "mem.grow", args, .. } => {
                // Words 0 and 2 of the header it's given.
                !touches(args[0], 0, 8) && !touches(args[0], 16, 8)
            }
            InstKind::Native { name, .. } => PURE_NATIVES.contains(name) || QUIET_NATIVES.contains(name),
            InstKind::Call { .. } | InstKind::CallInd { .. } | InstKind::FreeBuf { .. } => false,
            _ => true,
        }
    }

    /// What the eight bytes at `at` of `slot` hold on reaching the end of
    /// `pre`: `Some(None)` zero, `Some(Some(v))` the value `v`, `None`
    /// unknown. Searched back along straight-line code.
    fn before_loop(&self, mut b: usize, slot: u32, at: i64) -> Option<Option<Value>> {
        for _ in 0..8 {
            for inst in self.f.blocks[b].insts.iter().rev() {
                match &inst.kind {
                    InstKind::Store { ptr, off, mem: Mem::W64, val }
                        if self.alias.root(*ptr) == (Root::Slot(slot), Some(at - i64::from(*off))) =>
                    {
                        return Some(if self.constant(*val) == Some(0) { None } else { Some(*val) });
                    }
                    InstKind::Zero { dst, size } => {
                        if let (Root::Slot(s), Some(o)) = self.alias.root(*dst)
                            && s == slot
                            && o <= at
                            && at + 8 <= o + *size as i64
                        {
                            return Some(None);
                        }
                        if !self.leaves(&inst.kind, slot, at) {
                            return None;
                        }
                    }
                    k => {
                        if !self.leaves(k, slot, at) {
                            return None;
                        }
                    }
                }
            }
            let preds: Vec<usize> = self.cfg.preds[b].iter().copied().filter(|&p| self.cfg.reached(p)).collect();
            let [p] = preds.as_slice() else { return None };
            if !matches!(self.f.blocks[*p].term, Some(Term::Jump(_))) {
                return None;
            }
            b = *p;
        }
        None
    }
}
