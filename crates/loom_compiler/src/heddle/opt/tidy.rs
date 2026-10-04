//! Tidying a function up (pass 12 of design/vm.md §7, which other passes
//! also run when they're done): a block that only one jump reaches becomes
//! part of the block it jumps from (so an inlined function's comparison can
//! fuse with the caller's branch on it), instructions that only compute a
//! value nothing reads go, and so do blocks nothing reaches.
//!
//! The last pass (`cleanup`) also moves an element's address down to the
//! loads and stores of it, a copy before each, where codegen makes each
//! pair one checked instruction (`ldelf`, `stelf`): in `xs[i] = a * b` the
//! address comes before the value it's given, and `xs[i] += 1` loads and
//! stores at one address.

use std::collections::HashMap;

use crate::heddle::{BlockId, Func, InstKind, Module, Policy, Term, Value, rewrite};

/// Tidies every function of `module` up: the `cleanup` pass.
pub fn cleanup(module: &mut Module) {
    for f in &mut module.funcs {
        tidy(f);
        sink_elems(f);
        sink_indexes(f);
        place_indexes(f);
    }
}

/// Moves each `elem` whose uses are all loads and stores of it later in its
/// block down to them, one copy before each: what's before the first can't
/// trap and has no effect, so its check runs where it would have, as far
/// as anything can tell; and what's between them writes no memory but the
/// element, so the copies after it check what's already checked.
fn sink_elems(f: &mut Func) {
    // Each slot's address, by slot (each use has a value of its own).
    let slot: HashMap<Value, u32> = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match (&i.kind, i.results.as_slice()) {
            (InstKind::Slot(n), [r]) => Some((*r, *n)),
            _ => None,
        })
        .collect();
    let same = |a: Value, b: Value| a == b || slot.get(&a).is_some_and(|n| slot.get(&b) == Some(n));
    let mut uses: HashMap<Value, usize> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            for u in crate::heddle::inst_uses(&inst.kind) {
                *uses.entry(u).or_default() += 1;
            }
        }
        for u in b.term.iter().flat_map(super::term_uses) {
            *uses.entry(u).or_default() += 1;
        }
    }
    for b in 0..f.blocks.len() {
        let mut i = 0;
        while i < f.blocks[b].insts.len() {
            let insts = &f.blocks[b].insts;
            let (InstKind::Elem { .. }, [r]) = (&insts[i].kind, insts[i].results.as_slice()) else {
                i += 1;
                continue;
            };
            let r = *r;
            let at: Vec<usize> = (i + 1..insts.len())
                .filter(|&k| match &insts[k].kind {
                    InstKind::Load { ptr, .. } => *ptr == r,
                    InstKind::Store { ptr, val, .. } => *ptr == r && *val != r,
                    _ => false,
                })
                .collect();
            let (Some(&first), Some(&last)) = (at.first(), at.last()) else {
                i += 1;
                continue;
            };
            let quiet_before = insts[i + 1..first].iter().all(|j| pure(&j.kind));
            let no_writes = insts[first..=last].iter().all(|j| match &j.kind {
                InstKind::Store { ptr, .. } => *ptr == r,
                k => !writes(k),
            });
            // A store whose element is read again soon after keeps its
            // address computed early: a fused store works it out late, and a
            // load after it that ran ahead has to run again.
            let reread = match (&insts[i].kind, &insts[last].kind) {
                (&InstKind::Elem { header, off, index, .. }, &InstKind::Store { off: at, .. }) => {
                    let rest = &insts[last + 1..];
                    rest.iter().enumerate().any(|(n, j)| match (&j.kind, j.results.as_slice()) {
                        (&InstKind::Elem { header: h, off: o, index: x, .. }, [e])
                            if same(h, header) && (o, x) == (off, index) =>
                        {
                            rest[n + 1..]
                                .iter()
                                .any(|l| matches!(l.kind, InstKind::Load { ptr, off, .. } if ptr == *e && off == at))
                        }
                        _ => false,
                    })
                }
                _ => false,
            };
            if reread
                || uses.get(&r) != Some(&at.len())
                || !quiet_before
                || !no_writes
                || (at.len() == 1 && first == i + 1)
            {
                i += 1;
                continue;
            }
            let elem = f.blocks[b].insts.remove(i);
            // From the last use up, so the places before stay put.
            for (n, &k) in at.iter().enumerate().rev() {
                let mut copy = elem.clone();
                let v = if n == 0 {
                    r
                } else {
                    let v = Value(f.types.len() as u32);
                    f.types.push(f.ty(r));
                    copy.results = vec![v];
                    v
                };
                let k = k - 1;
                if let InstKind::Load { ptr, .. } | InstKind::Store { ptr, .. } = &mut f.blocks[b].insts[k].kind {
                    *ptr = v;
                }
                f.blocks[b].insts.insert(k, copy);
            }
            // What came after it is at `i` now.
        }
    }
}

/// Moves each `index` whose uses are all loads and stores of it later in
/// its block down to them, a copy before each (`ldx`, `stx` in codegen),
/// with the `bounds` of the same index just before it, if there is one, in
/// front of the first (`ldxc`, `stxc`): when what's before the first can't
/// trap and has no effect, the check runs where it would have, as far as
/// anything can tell. An `index` reads no memory, so its copies can go
/// anywhere after it.
fn sink_indexes(f: &mut Func) {
    let mut uses: HashMap<Value, usize> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            for u in crate::heddle::inst_uses(&inst.kind) {
                *uses.entry(u).or_default() += 1;
            }
        }
        for u in b.term.iter().flat_map(super::term_uses) {
            *uses.entry(u).or_default() += 1;
        }
    }
    for b in 0..f.blocks.len() {
        let mut i = 0;
        while i < f.blocks[b].insts.len() {
            let insts = &f.blocks[b].insts;
            let (&InstKind::Index { index, .. }, [r]) = (&insts[i].kind, insts[i].results.as_slice()) else {
                i += 1;
                continue;
            };
            let r = *r;
            let check = i > 0 && matches!(insts[i - 1].kind, InstKind::Bounds { index: k, .. } if k == index);
            let at: Vec<usize> = (i + 1..insts.len())
                .filter(|&k| match &insts[k].kind {
                    InstKind::Load { ptr, .. } => *ptr == r,
                    InstKind::Store { ptr, val, .. } => *ptr == r && *val != r,
                    _ => false,
                })
                .collect();
            let Some(&first) = at.first() else {
                i += 1;
                continue;
            };
            if uses.get(&r) != Some(&at.len())
                || (check && !insts[i + 1..first].iter().all(|j| pure(&j.kind)))
                || (at.len() == 1 && first == i + 1)
            {
                i += 1;
                continue;
            }
            let start = if check { i - 1 } else { i };
            let group: Vec<_> = f.blocks[b].insts.drain(start..=i).collect();
            let gone = group.len();
            for (n, &k) in at.iter().enumerate().rev() {
                let k = k - gone;
                let mut copy = group.last().expect("the index").clone();
                let v = if n == 0 {
                    r
                } else {
                    let v = Value(f.types.len() as u32);
                    f.types.push(f.ty(r));
                    copy.results = vec![v];
                    v
                };
                if let InstKind::Load { ptr, .. } | InstKind::Store { ptr, .. } = &mut f.blocks[b].insts[k].kind {
                    *ptr = v;
                }
                f.blocks[b].insts.insert(k, copy);
                if n == 0 && check {
                    f.blocks[b].insts.insert(k, group[0].clone());
                }
            }
            // What came after them is at `start` now.
            i = start;
        }
    }
}

/// Moves each `index` whose one use is a load or a store of it, but not
/// just after it (LICM took it out of a loop, or it was made in another
/// block), to just before that use: there the two are one instruction
/// (`ldx`), as cheap as the load alone, and a `bounds` of the same index
/// just before it fuses too. Its operands were made before it, so they
/// still are.
fn place_indexes(f: &mut Func) {
    let mut uses: HashMap<Value, usize> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            for u in crate::heddle::inst_uses(&inst.kind) {
                *uses.entry(u).or_default() += 1;
            }
        }
        for u in b.term.iter().flat_map(super::term_uses) {
            *uses.entry(u).or_default() += 1;
        }
    }
    // The places just after each `index`.
    let mut after: HashMap<Value, (usize, usize)> = HashMap::new();
    for (b, block) in f.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            if let (InstKind::Index { .. }, [r]) = (&inst.kind, inst.results.as_slice())
                && uses.get(r) == Some(&1)
            {
                after.insert(*r, (b, i + 1));
            }
        }
    }
    let mut moving: HashMap<Value, crate::heddle::Inst> = HashMap::new();
    for (b, block) in f.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let ptr = match &inst.kind {
                InstKind::Load { ptr, .. } => *ptr,
                InstKind::Store { ptr, val, .. } if ptr != val => *ptr,
                _ => continue,
            };
            if let Some(&(db, di)) = after.get(&ptr)
                && (db, di) != (b, i)
            {
                moving.insert(ptr, f.blocks[db].insts[di - 1].clone());
            }
        }
    }
    if moving.is_empty() {
        return;
    }
    for block in &mut f.blocks {
        let old = std::mem::take(&mut block.insts);
        for inst in old {
            if inst.results.first().is_some_and(|r| moving.contains_key(r)) {
                continue;
            }
            if let InstKind::Load { ptr, .. } | InstKind::Store { ptr, .. } = &inst.kind
                && let Some(idx) = moving.get(ptr)
            {
                block.insts.push(idx.clone());
            }
            block.insts.push(inst);
        }
    }
}

/// Whether `k` may write memory.
fn writes(k: &InstKind) -> bool {
    use InstKind as K;
    matches!(
        k,
        K::Store { .. }
            | K::Copy { .. }
            | K::Zero { .. }
            | K::Call { .. }
            | K::CallInd { .. }
            | K::Native { .. }
            | K::FreeBuf { .. }
    )
}

/// Tidies `f` up (see the module's documentation).
pub fn tidy(f: &mut Func) {
    // Blocks nothing reaches first: their jumps would count.
    drop_unreached(f);
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
