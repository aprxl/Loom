//! Tidying a function up (pass 12 of design/vm.md §7, which other passes
//! also run when they're done): a block that only one jump reaches becomes
//! part of the block it jumps from (so an inlined function's comparison can
//! fuse with the caller's branch on it), instructions that only compute a
//! value nothing reads go, and so do blocks nothing reaches.

use std::collections::HashMap;

use crate::heddle::{BlockId, Func, InstKind, Module, Policy, Term, Value, rewrite};

/// Tidies every function of `module` up: the `cleanup` pass.
pub fn cleanup(module: &mut Module) {
    module.funcs.iter_mut().for_each(tidy);
}

/// Tidies `f` up (see the module's documentation).
pub fn tidy(f: &mut Func) {
    merge_chains(f);
    drop_dead(f);
    drop_unreached(f);
}

/// Removes the blocks nothing reaches, from the entry or by unwinding from
/// a block something does, and numbers the rest again, in order.
fn drop_unreached(f: &mut Func) {
    let n = f.blocks.len();
    let mut seen = vec![false; n];
    let mut stack = vec![0usize];
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let block = &f.blocks[b];
        stack.extend(block.term.iter().flat_map(|t| t.edges()).map(|e| e.block.0 as usize));
        stack.extend(block.pads().into_iter().map(|p| p.0 as usize));
    }
    if seen.iter().all(|&s| s) {
        return;
    }
    let mut new = vec![u32::MAX; n];
    let mut next = 0;
    for (b, &kept) in seen.iter().enumerate() {
        if kept {
            new[b] = next;
            next += 1;
        }
    }
    let to = |b: BlockId| BlockId(new[b.0 as usize]);
    let blocks = std::mem::take(&mut f.blocks);
    for (mut block, kept) in blocks.into_iter().zip(seen) {
        if !kept {
            continue;
        }
        for inst in &mut block.insts {
            inst.unwind = inst.unwind.map(to);
        }
        block.term_unwind = block.term_unwind.map(to);
        if let Some(t) = &mut block.term {
            for e in t.edges_mut() {
                e.block = to(e.block);
            }
        }
        f.blocks.push(block);
    }
}

/// Merges each block that only a `jump` reaches into the block it's in.
fn merge_chains(f: &mut Func) {
    let n = f.blocks.len();
    let mut preds = vec![0usize; n];
    let mut pads = vec![false; n];
    for b in &f.blocks {
        for e in b.term.iter().flat_map(|t| t.edges()) {
            preds[e.block.0 as usize] += 1;
        }
        for p in b.pads() {
            pads[p.0 as usize] = true;
        }
    }
    // Merging moves a block's edges to the block it joins: the counts hold.
    let mut to: HashMap<Value, Value> = HashMap::new();
    for a in 0..n {
        while let Some(Term::Jump(e)) = &f.blocks[a].term {
            let b = e.block.0 as usize;
            if b == a || b == 0 || preds[b] != 1 || pads[b] {
                break;
            }
            let Some(Term::Jump(edge)) = f.blocks[a].term.take() else { unreachable!("a jump") };
            let moved = std::mem::take(&mut f.blocks[b]);
            to.extend(moved.params.iter().copied().zip(edge.args));
            let block = &mut f.blocks[a];
            block.insts.extend(moved.insts);
            block.term = moved.term;
            block.term_span = moved.term_span;
            block.term_unwind = moved.term_unwind;
            // The merged block is never reached: an empty one that ends.
            f.blocks[b].term = Some(Term::Crash(0));
        }
    }
    if to.is_empty() {
        return;
    }
    let subst = |mut v: Value| {
        while let Some(&w) = to.get(&v) {
            v = w;
        }
        v
    };
    for block in &mut f.blocks {
        for inst in &mut block.insts {
            rewrite(&mut inst.kind, subst);
        }
        if let Some(t) = &mut block.term {
            super::rewrite_term(t, subst);
        }
    }
}

/// Whether `k` only computes its results: no effect, and it can't trap.
fn pure(k: &InstKind) -> bool {
    use InstKind as K;
    match k {
        K::Undef
        | K::Iconst(_)
        | K::Fconst(_)
        | K::Str(_)
        | K::Global(_)
        | K::Slot(_)
        | K::FuncRef(_)
        | K::Load { .. }
        | K::Offset { .. }
        | K::Index { .. }
        | K::Bit { .. }
        | K::Not(_)
        | K::BitNot { .. }
        | K::ICmp { .. }
        | K::FCmp { .. }
        | K::FArith { .. }
        | K::FNeg { .. }
        | K::IntToFloat { .. }
        | K::FloatCast { .. } => true,
        K::Arith { policy, .. } | K::IntCast { policy, .. } => *policy != Policy::Trap,
        _ => false,
    }
}

/// Drops pure instructions whose results nothing reads, until there are
/// none.
fn drop_dead(f: &mut Func) {
    loop {
        let mut read = vec![false; f.types.len()];
        for b in &f.blocks {
            for inst in &b.insts {
                for u in crate::heddle::inst_uses(&inst.kind) {
                    read[u.0 as usize] = true;
                }
            }
            for v in b.term.iter().flat_map(super::term_uses) {
                read[v.0 as usize] = true;
            }
        }
        let mut changed = false;
        for b in &mut f.blocks {
            let before = b.insts.len();
            b.insts
                .retain(|i| !(pure(&i.kind) && !i.results.is_empty() && i.results.iter().all(|r| !read[r.0 as usize])));
            changed |= b.insts.len() != before;
        }
        if !changed {
            return;
        }
    }
}
