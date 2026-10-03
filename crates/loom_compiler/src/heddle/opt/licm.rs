//! Loop-invariant code motion (pass 6 of design/vm.md §7): what a loop
//! computes the same on every iteration is computed once, before it.
//!
//! Each loop gets a *preheader*, a block every way into it goes through,
//! and what's invariant moves to its end: an instruction that can't trap
//! and has no effect, whose operands are all computed outside the loop (or
//! were moved out themselves). Inner loops are done first, so what moves out
//! of one can move on out of the loop around it.
//!
//! A load moves when nothing in the loop may write what it reads (see
//! `alias`; a call may write anything but a slot whose address never
//! escapes), and when loading early is harmless: it reads the frame or the
//! program's data, or it's among the first things the loop's head does,
//! which every way into the loop runs.

use super::alias::{Access, Alias, Root};
use super::cfg::Cfg;
use crate::heddle::{Block, BlockId, Edge, Func, InstKind, Module, Policy, Term, Value};

pub fn licm(module: &mut Module) {
    for f in &mut module.funcs {
        if add_preheaders(f) {
            licm_func(f);
            super::tidy::tidy(f);
        }
    }
}

/// Whether `k` computes its results from its operands alone, can't trap
/// and has no effect.
fn movable(k: &InstKind) -> bool {
    use InstKind as K;
    match k {
        K::Iconst(_)
        | K::Fconst(_)
        | K::Str(_)
        | K::Global(_)
        | K::FuncRef(_)
        | K::Slot(_)
        | K::Offset { .. }
        | K::Index { .. }
        | K::Bit { .. }
        | K::Not(_)
        | K::BitNot { .. }
        | K::ICmp { .. }
        | K::FArith { .. }
        | K::FNeg { .. }
        | K::FCmp { .. }
        | K::IntToFloat { .. }
        | K::FloatCast { .. } => true,
        K::Arith { policy, .. } | K::IntCast { policy, .. } => *policy != Policy::Trap,
        K::Native { name, .. } => *name == "float.math",
        _ => false,
    }
}

/// Gives every loop's head a preheader: one block that every edge from
/// outside the loop goes through, which only jumps to the head. Whether
/// the function has loops.
fn add_preheaders(f: &mut Func) -> bool {
    let cfg = Cfg::new(f);
    let loops = cfg.loops();
    for l in &loops {
        let h = l.header;
        let outside: Vec<usize> = cfg.preds[h].iter().copied().filter(|&p| !l.inside[p]).collect();
        // One way in, from a block that only goes here: already one.
        if let [p] = outside.as_slice()
            && cfg.succs[*p] == [h]
        {
            continue;
        }
        // A new block, with the head's parameters, which the edges from
        // outside go to instead.
        let pre = f.blocks.len();
        let tys: Vec<_> = f.blocks[h].params.iter().map(|&p| f.ty(p)).collect();
        let params: Vec<Value> = tys
            .into_iter()
            .map(|t| {
                f.types.push(t);
                Value(f.types.len() as u32 - 1)
            })
            .collect();
        for &p in &outside {
            if let Some(t) = &mut f.blocks[p].term {
                for e in t.edges_mut() {
                    if e.block.0 as usize == h {
                        e.block = BlockId(pre as u32);
                    }
                }
            }
        }
        let span = f.blocks[h].term_span;
        f.blocks.push(Block {
            params: params.clone(),
            term: Some(Term::Jump(Edge { block: BlockId(h as u32), args: params })),
            term_span: span,
            ..Block::default()
        });
    }
    !loops.is_empty()
}

fn licm_func(f: &mut Func) {
    let cfg = Cfg::new(f);
    let alias = Alias::new(f);
    let mut loops = cfg.loops();
    // Inner loops first.
    loops.sort_by_key(|l| l.blocks.len());
    // Where each value is made: its block.
    let mut home: Vec<usize> = vec![usize::MAX; f.types.len()];
    for (b, block) in f.blocks.iter().enumerate() {
        for &p in &block.params {
            home[p.0 as usize] = b;
        }
        for inst in &block.insts {
            for &r in &inst.results {
                home[r.0 as usize] = b;
            }
        }
    }
    for l in &loops {
        let h = l.header;
        let outside: Vec<usize> = cfg.preds[h].iter().copied().filter(|&p| !l.inside[p]).collect();
        let [pre] = outside.as_slice() else { continue };
        let pre = *pre;
        // What the loop may write.
        let mut writes: Vec<Access> = Vec::new();
        let mut calls = false;
        for &b in &l.blocks {
            for inst in &f.blocks[b].insts {
                match &inst.kind {
                    InstKind::Store { ptr, off, mem, .. } => {
                        writes.push(Access { ptr: *ptr, off: i64::from(*off), size: mem.bytes() });
                    }
                    InstKind::Copy { dst, size, .. } | InstKind::Zero { dst, size } => {
                        writes.push(Access { ptr: *dst, off: 0, size: *size });
                    }
                    InstKind::Native { name, .. } if super::gvn::PURE_NATIVES.contains(name) => {}
                    InstKind::Native { name, .. } if super::gvn::QUIET_NATIVES.contains(name) => {}
                    InstKind::Call { .. } | InstKind::CallInd { .. } | InstKind::Native { .. } => calls = true,
                    InstKind::FreeBuf { .. } => calls = true,
                    _ => {}
                }
            }
        }
        let unchanged = |access: Access| {
            let (root, _) = alias.root(access.ptr);
            let reachable = match root {
                Root::Slot(n) => alias.escapes[n as usize],
                _ => true,
            };
            !(calls && reachable) && writes.iter().all(|&w| !alias.may_alias(w, access))
        };
        let mut moved: Vec<crate::heddle::Inst> = Vec::new();
        for &b in &l.blocks {
            // In the head, until something that may trap or write: loads
            // there run on every way in.
            let mut first = b == h;
            let insts = std::mem::take(&mut f.blocks[b].insts);
            let mut kept = Vec::with_capacity(insts.len());
            for inst in insts {
                let invariant = crate::heddle::inst_uses(&inst.kind)
                    .iter()
                    .all(|u| home[u.0 as usize] == usize::MAX || !l.inside[home[u.0 as usize]]);
                let load = match &inst.kind {
                    InstKind::Load { ptr, off, mem } => {
                        let access = Access { ptr: *ptr, off: i64::from(*off), size: mem.bytes() };
                        let safe = first || matches!(alias.root(*ptr).0, Root::Slot(_) | Root::Data);
                        Some(safe && unchanged(access))
                    }
                    _ => None,
                };
                let hoist = invariant && load.unwrap_or_else(|| movable(&inst.kind));
                if hoist {
                    for &r in &inst.results {
                        home[r.0 as usize] = pre;
                    }
                    moved.push(inst);
                    continue;
                }
                if inst.kind.can_unwind() || !movable(&inst.kind) && !matches!(inst.kind, InstKind::Load { .. }) {
                    first = false;
                }
                kept.push(inst);
            }
            f.blocks[b].insts = kept;
        }
        f.blocks[pre].insts.extend(moved);
    }
}
