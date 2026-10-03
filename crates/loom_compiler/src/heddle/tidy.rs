//! Tidying a function up after inlining: a block that only one jump
//! reaches becomes part of the block it jumps from (so an inlined
//! function's comparison can fuse with the caller's branch on it), and
//! instructions that only compute a value nothing reads go.

use std::collections::HashMap;

use super::{Func, InstKind, Policy, Term, Value, rewrite};

/// Tidies `f` up (see the module's documentation).
pub fn tidy(f: &mut Func) {
    merge_chains(f);
    drop_dead(f);
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
            rewrite_term(t, subst);
        }
    }
}

/// Replaces every value `t` reads with `f` of it.
fn rewrite_term(t: &mut Term, f: impl Fn(Value) -> Value) {
    match t {
        Term::Br { cond, .. } => *cond = f(*cond),
        Term::Ret(vs) => vs.iter_mut().for_each(|v| *v = f(*v)),
        Term::Unravel { ptr, len } => {
            *ptr = f(*ptr);
            *len = f(*len);
        }
        Term::Jump(_) | Term::UnravelMsg | Term::Crash(_) | Term::Resume => {}
    }
    for e in t.edges_mut() {
        e.args.iter_mut().for_each(|a| *a = f(*a));
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
                for u in super::inst_uses(&inst.kind) {
                    read[u.0 as usize] = true;
                }
            }
            if let Some(t) = &b.term {
                let mut mark = |v: Value| read[v.0 as usize] = true;
                match t {
                    Term::Br { cond, .. } => mark(*cond),
                    Term::Ret(vs) => vs.iter().for_each(|&v| mark(v)),
                    Term::Unravel { ptr, len } => {
                        mark(*ptr);
                        mark(*len);
                    }
                    _ => {}
                }
                for e in t.edges() {
                    e.args.iter().for_each(|&a| mark(a));
                }
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
