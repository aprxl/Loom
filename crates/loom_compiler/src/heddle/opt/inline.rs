//! Inlining (pass 1 of design/vm.md §7): small functions, and functions
//! called from one place, are copied into their callers, which saves a
//! call, a return and moving the arguments, and lets the passes after this
//! one see through the call.
//!
//! Bottom-up over the call graph: a function's callees are done before it
//! is, so what it inlines has had its own calls inlined already. A function
//! is never inlined into itself, and what an inlined copy calls stays a
//! call, so recursion keeps its calls: functions that call each other (drop
//! glue for a tree and for its list of children) are inlined one level.
//!
//! An inlined copy's instructions keep their locations, and say which call
//! they came from (a [`Site`]), so a backtrace still shows the function they
//! were in. A function with no locations at all (std's methods the compiler
//! writes, glue) reports a trap at the call, as it does when it's called.
//! What unwinds in the copy goes to the copy's landing pads, and from there
//! (or straight, where the callee had nothing to clean up) to the call's.

use crate::heddle::{Block, BlockId, Edge, Func, InstKind, Module, Site, Term, Value};

/// The most instructions (constants aside) a function inlined anywhere may
/// have.
const SMALL: usize = 40;
/// The most a function called from one place may have to be inlined there.
const ONCE: usize = 400;
/// How much a function may grow by inlining: to twice its size, or by
/// this many instructions, whichever is more.
const GROWTH: usize = 2 * SMALL;
/// The most frame bytes a function may gain from its callees' slots.
const SLOT_BYTES: u64 = 4096;

/// `module` with calls inlined (see the module's documentation), each
/// function tidied up after (see `tidy`).
pub fn inline(module: &mut Module) {
    let n = module.funcs.len();
    let mut calls = vec![0usize; n];
    let mut taken = vec![false; n];
    let mut callees: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, f) in module.funcs.iter().enumerate() {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match inst.kind {
                InstKind::Call { func, .. } => {
                    calls[func.0 as usize] += 1;
                    callees[i].push(func.0 as usize);
                }
                InstKind::FuncRef(func) => taken[func.0 as usize] = true,
                _ => {}
            }
        }
    }
    taken[module.entry.0 as usize] = true;
    for scc in sccs(&callees) {
        for &me in &scc {
            let mut f = std::mem::take(&mut module.funcs[me]);
            // How a callee may be inlined: within the caller's budget when
            // it's small, or whatever its size when this is its only call
            // (the code doesn't grow: the function itself goes unused).
            let eligible = |g: usize, funcs: &[Func]| -> Option<bool> {
                if g == me || !inlinable(&funcs[g]) {
                    return None;
                }
                let size = size(&funcs[g]);
                if calls[g] == 1 && !taken[g] && size <= ONCE { Some(false) } else { (size <= SMALL).then_some(true) }
            };
            inline_into(&mut f, &module.funcs, eligible);
            super::tidy::tidy(&mut f);
            module.funcs[me] = f;
        }
    }
}

/// The instructions that cost something: all but constants.
fn size(f: &Func) -> usize {
    f.blocks.iter().map(|b| b.insts.iter().filter(|i| !constant(&i.kind)).count() + 1).sum()
}

/// Whether calls of `f` may be inlined: it's finished (no suspensions or
/// slot lifetimes, which only a shuttle's body has before the frame pass),
/// and its entry block is only entered by calling it.
fn inlinable(f: &Func) -> bool {
    let finished = f.blocks.iter().all(|b| {
        !matches!(b.term, Some(Term::Suspend { .. }) | None)
            && b.insts.iter().all(|i| !matches!(i.kind, InstKind::Life { .. }))
    });
    let reentered = f.blocks.iter().any(|b| b.term.iter().flat_map(|t| t.edges()).any(|e| e.block.0 == 0));
    finished && !reentered && !f.blocks.is_empty()
}

fn constant(k: &InstKind) -> bool {
    matches!(k, InstKind::Iconst(_) | InstKind::Fconst(_) | InstKind::Str(_) | InstKind::Global(_))
}

/// Whether a function has no locations at all: one the compiler wrote,
/// which reports a trap where it's called.
fn transparent(f: &Func) -> bool {
    f.blocks.iter().all(|b| b.term_span.is_none() && b.insts.iter().all(|i| i.span.is_none()))
}

/// The strongly connected components of the call graph, each after every
/// component it calls (Tarjan's order).
fn sccs(callees: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = callees.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut next = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // (node, which callee is next)
        let mut work = vec![(root, 0usize)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut i)) = work.last_mut() {
            if let Some(&w) = callees[v].get(*i) {
                *i += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                work.pop();
                if let Some(&(u, _)) = work.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut scc = Vec::new();
                    loop {
                        let w = stack.pop().expect("on the stack");
                        on_stack[w] = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    scc.sort_unstable();
                    out.push(scc);
                }
            }
        }
    }
    out
}

/// Inlines the calls in `f` of the functions of `funcs` that `eligible`
/// says may be (`Some(true)`: as long as `f` stays within its budget).
fn inline_into(f: &mut Func, funcs: &[Func], eligible: impl Fn(usize, &[Func]) -> Option<bool>) {
    let budget = size(f) * 2 + GROWTH;
    let slot_budget = f.slots.iter().map(|s| s.size).sum::<u64>() + SLOT_BYTES;
    let mut grown = size(f);
    // Blocks copied from a callee aren't looked at again: their calls were
    // decided when the callee was done. Nor are landing pads, which only
    // run when unwinding: a call there is cold.
    let mut copied = vec![true; f.blocks.len()];
    let mut stack = vec![0usize];
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut copied[b], false) {
            stack.extend(f.blocks[b].term.iter().flat_map(|t| t.edges()).map(|e| e.block.0 as usize));
        }
    }
    let mut b = 0;
    while b < f.blocks.len() {
        if copied[b] {
            b += 1;
            continue;
        }
        let found = f.blocks[b].insts.iter().position(|inst| match &inst.kind {
            InstKind::Call { func, .. } => {
                let g = func.0 as usize;
                let callee = &funcs[g];
                let fits = match eligible(g, funcs) {
                    None => false,
                    Some(counted) => !counted || grown + size(callee) <= budget,
                };
                callee.results.len() == inst.results.len()
                    && fits
                    && f.slots.iter().chain(&callee.slots).map(|s| s.size).sum::<u64>() <= slot_budget
            }
            _ => false,
        });
        let Some(i) = found else {
            b += 1;
            continue;
        };
        let InstKind::Call { func, .. } = &f.blocks[b].insts[i].kind else { unreachable!("found a call") };
        let callee = &funcs[func.0 as usize];
        if eligible(func.0 as usize, funcs) == Some(true) {
            grown += size(callee);
        }
        let added = inline_call(f, b, i, callee);
        // The rest of the block, where the call returns, then the copy.
        copied.push(false);
        copied.resize(copied.len() + added, true);
        b += 1;
    }
}

/// Inlines the call at instruction `i` of block `b`: the block ends by
/// jumping to the callee's copy, whose returns jump to a block with the
/// rest. How many blocks the copy added (after the rest's).
fn inline_call(f: &mut Func, b: usize, i: usize, callee: &Func) -> usize {
    // Split the block at the call: what follows it goes on in a block whose
    // parameters are the call's results.
    let rest = f.blocks[b].insts.split_off(i + 1);
    let call = f.blocks[b].insts.pop().expect("the call");
    let InstKind::Call { args, .. } = &call.kind else { unreachable!("a call") };
    let cont = BlockId(f.blocks.len() as u32);
    let after = Block {
        params: call.results.clone(),
        insts: rest,
        term: f.blocks[b].term.take(),
        term_span: f.blocks[b].term_span.take(),
        term_unwind: f.blocks[b].term_unwind.take(),
        term_site: f.blocks[b].term_site.take(),
    };
    f.blocks.push(after);
    // Where the copy's instructions say they are: the callee's own
    // locations in a frame of the call's, or, for a callee with none, the
    // call's.
    let shown = !transparent(callee);
    let site = f.sites.len() as u32;
    f.sites.push(Site { name: callee.name.clone(), call: call.span, parent: call.site });
    let site_base = f.sites.len() as u32;
    for s in &callee.sites {
        f.sites.push(Site {
            name: s.name.clone(),
            call: s.call,
            parent: Some(s.parent.map_or(site, |p| p + site_base)),
        });
    }
    let locate = |span: Option<loom_syntax::source::Span>, at: Option<u32>| match (at, shown) {
        (Some(k), _) => (span, Some(k + site_base)),
        (None, true) => (span, Some(site)),
        (None, false) => (call.span, call.site),
    };
    // The callee's blocks, each value given a new number (its parameters
    // are the arguments), its slots after the caller's.
    let base = f.blocks.len() as u32;
    let slot_base = f.slots.len() as u32;
    f.slots.extend(callee.slots.iter().cloned());
    let mut map: Vec<Option<Value>> = vec![None; callee.types.len()];
    for (p, a) in callee.params().iter().zip(args) {
        map[p.0 as usize] = Some(*a);
    }
    let new = |v: Value, f: &mut Func, map: &mut Vec<Option<Value>>| -> Value {
        *map[v.0 as usize].get_or_insert_with(|| {
            f.types.push(callee.types[v.0 as usize]);
            Value(f.types.len() as u32 - 1)
        })
    };
    let mut blocks = Vec::with_capacity(callee.blocks.len());
    for (k, cb) in callee.blocks.iter().enumerate() {
        let params = if k == 0 { Vec::new() } else { cb.params.iter().map(|&p| new(p, f, &mut map)).collect() };
        let mut insts = Vec::with_capacity(cb.insts.len());
        for inst in &cb.insts {
            let mut inst = inst.clone();
            for u in crate::heddle::inst_uses(&inst.kind) {
                new(u, f, &mut map);
            }
            crate::heddle::rewrite(&mut inst.kind, |v| map[v.0 as usize].expect("numbered just above"));
            if let InstKind::Slot(n) = &mut inst.kind {
                *n += slot_base;
            }
            inst.results = inst.results.iter().map(|&r| new(r, f, &mut map)).collect();
            (inst.span, inst.site) = locate(inst.span, inst.site);
            inst.unwind = match inst.unwind {
                Some(pad) => Some(BlockId(pad.0 + base)),
                None if inst.kind.can_unwind() => call.unwind,
                None => None,
            };
            insts.push(inst);
        }
        let mut term = cb.term.clone().expect("finished blocks end");
        super::rewrite_term(&mut term, |v| new(v, f, &mut map));
        for e in term.edges_mut() {
            e.block = BlockId(e.block.0 + base);
        }
        let mut unwind = cb.term_unwind.map(|p| BlockId(p.0 + base));
        match &term {
            // A return goes on after the call, with the results.
            Term::Ret(vs) => term = Term::Jump(Edge { block: cont, args: vs.clone() }),
            // Unwinding goes on at the call's landing pad, or out.
            Term::Resume => {
                if let Some(pad) = call.unwind {
                    term = Term::Jump(Edge { block: pad, args: Vec::new() });
                }
            }
            // An unravel, or a loop's back edge (where an interrupt is
            // noticed), unwinds where the call would.
            Term::Jump(_) | Term::Unravel { .. } | Term::UnravelMsg | Term::Crash(_) => {
                unwind = unwind.or(call.unwind);
            }
            Term::Br { .. } | Term::Suspend { .. } => {}
        }
        let (term_span, term_site) = locate(cb.term_span, cb.term_site);
        blocks.push(Block { params, insts, term: Some(term), term_span, term_unwind: unwind, term_site });
    }
    let added = blocks.len();
    f.blocks.extend(blocks);
    f.blocks[b].term = Some(Term::Jump(Edge { block: BlockId(base), args: Vec::new() }));
    f.blocks[b].term_span = call.span;
    f.blocks[b].term_site = call.site;
    added
}
