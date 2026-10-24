//! Checks the SSA properties codegen relies on (design/vm.md §6.4):
//!
//! - every block ends in a terminator, and nothing jumps to the entry;
//! - every edge passes one argument, of the right type, per parameter;
//! - every value is defined exactly once;
//! - every use is dominated by its definition;
//! - landing pads exist, only instructions that can unravel have one, and
//!   a pad reads no value but the function's parameters, its own, and
//!   those computed before every instruction that unwinds to it.

use std::collections::HashMap;

use super::{BlockId, Func, Module, Value};

/// Checks every function of `module`.
pub fn verify(module: &Module) -> Result<(), String> {
    for f in &module.funcs {
        verify_func(f).map_err(|e| format!("in `{}`: {e}", f.name))?;
    }
    Ok(())
}

/// Where a value is defined: its block, and the instruction (`None` for a
/// block parameter).
type Def = (usize, Option<usize>);

fn verify_func(f: &Func) -> Result<(), String> {
    let n = f.blocks.len();
    let mut defs: HashMap<Value, Def> = HashMap::new();
    let mut define = |v: Value, at: Def| -> Result<(), String> {
        if v.0 as usize >= f.types.len() {
            return Err(format!("v{} has no type", v.0));
        }
        match defs.insert(v, at) {
            Some(_) => Err(format!("v{} is defined twice", v.0)),
            None => Ok(()),
        }
    };
    for (b, block) in f.blocks.iter().enumerate() {
        for &p in &block.params {
            define(p, (b, None))?;
        }
        for (i, inst) in block.insts.iter().enumerate() {
            for &r in &inst.results {
                define(r, (b, Some(i)))?;
            }
        }
    }
    let mut preds = vec![Vec::new(); n];
    for (b, block) in f.blocks.iter().enumerate() {
        for inst in &block.insts {
            if let Some(pad) = inst.unwind {
                if pad.0 as usize >= n || !f.blocks[pad.0 as usize].params.is_empty() {
                    return Err(format!("b{b} unwinds to b{}, which isn't a landing pad", pad.0));
                }
                if !inst.kind.can_unwind() {
                    return Err(format!("b{b} has a landing pad for an instruction that can't unravel"));
                }
            }
        }
        if let Some(pad) = block.term_unwind
            && (pad.0 as usize >= n || !f.blocks[pad.0 as usize].params.is_empty())
        {
            return Err(format!("b{b} unwinds to b{}, which isn't a landing pad", pad.0));
        }
        let term = block.term.as_ref().ok_or_else(|| format!("b{b} has no terminator"))?;
        for e in term.edges() {
            let target = f.blocks.get(e.block.0 as usize).ok_or_else(|| format!("b{b} jumps to a missing block"))?;
            if e.block == BlockId(0) {
                return Err(format!("b{b} jumps to the entry block"));
            }
            if e.args.len() != target.params.len() {
                return Err(format!(
                    "b{b} passes {} arguments to b{}, which takes {}",
                    e.args.len(),
                    e.block.0,
                    target.params.len()
                ));
            }
            for (&arg, &param) in e.args.iter().zip(&target.params) {
                if defs.contains_key(&arg) && f.ty(arg) != f.ty(param) {
                    return Err(format!("b{b} passes v{} to a parameter of another type", arg.0));
                }
            }
            preds[e.block.0 as usize].push(b);
        }
    }
    let idom = dominators(f, &preds);
    let dominates = |a: usize, mut b: usize| loop {
        if a == b {
            return true;
        }
        match idom[b] {
            Some(d) if d != b => b = d,
            _ => return false,
        }
    };
    // What landing pads (reached only by unwinding) can see: the function's
    // parameters, what the pads themselves compute, and what's computed
    // before every instruction that unwinds to them (an inlined function's
    // pads read its arguments).
    let pad_blocks: std::collections::HashSet<usize> = pad_reach(f);
    let params: std::collections::HashSet<Value> = f.params().iter().copied().collect();
    let sources = unwind_sources(f, &pad_blocks);
    for &b in &pad_blocks {
        let block = &f.blocks[b];
        let mut seen: std::collections::HashSet<Value> = block.params.iter().copied().collect();
        let mut reads: Vec<Value> = Vec::new();
        for inst in &block.insts {
            for u in uses(&inst.kind) {
                if !seen.contains(&u) {
                    reads.push(u);
                }
            }
            seen.extend(inst.results.iter().copied());
        }
        reads.extend(block.term.iter().flat_map(crate::heddle::opt::term_uses).filter(|u| !seen.contains(u)));
        for u in reads {
            if params.contains(&u) || pad_defined(f, &pad_blocks, u) {
                continue;
            }
            let Some(&(db, di)) = defs.get(&u) else {
                return Err(format!("v{} is used in b{b} but never defined", u.0));
            };
            // Before each instruction (or terminator, at the block's end)
            // that unwinds here. (Not checked in other pads, nor in blocks
            // nothing reaches.)
            let before = |&(sb, si): &(usize, usize)| {
                pad_blocks.contains(&sb)
                    || idom[sb].is_none()
                    || if db == sb { di.is_none_or(|di| di < si) } else { dominates(db, sb) }
            };
            if let Some((sb, si)) = sources[b].iter().find(|s| !before(s)) {
                return Err(format!(
                    "the landing pad b{b} reads v{}, which isn't there when b{sb}'s instruction {si} unwinds to it",
                    u.0
                ));
            }
        }
    }
    // Whether the use at (block, index) sees `v`; the terminator is at
    // index `insts.len()`.
    let check = |v: Value, block: usize, index: usize| -> Result<(), String> {
        let &(db, di) = defs.get(&v).ok_or_else(|| format!("v{} is used in b{block} but never defined", v.0))?;
        if idom[block].is_none() {
            return Ok(()); // unreachable: nothing runs it
        }
        let ok = if db == block {
            di.is_none_or(|di| di < index)
        } else {
            dominates(db, block) || pad_blocks.contains(&block)
        };
        if ok { Ok(()) } else { Err(format!("v{} is used in b{block} where its definition doesn't reach", v.0)) }
    };
    for (b, block) in f.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            for u in uses(&inst.kind) {
                check(u, b, i)?;
            }
        }
        let end = block.insts.len();
        match block.term.as_ref() {
            Some(super::Term::Br { cond, .. }) => check(*cond, b, end)?,
            Some(super::Term::Ret(vs)) => vs.iter().try_for_each(|&v| check(v, b, end))?,
            Some(super::Term::Unravel { ptr, len }) => {
                check(*ptr, b, end)?;
                check(*len, b, end)?;
            }
            _ => {}
        }
        if let Some(term) = &block.term {
            for e in term.edges() {
                e.args.iter().try_for_each(|&v| check(v, b, end))?;
            }
        }
    }
    Ok(())
}

/// The values an instruction reads, in no allocation: up to three of its own,
/// then a slice (a call's arguments).
pub(crate) struct Uses<'a> {
    fixed: [Value; 2],
    n: usize,
    i: usize,
    rest: &'a [Value],
}

impl Iterator for Uses<'_> {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        if self.i < self.n {
            self.i += 1;
            return Some(self.fixed[self.i - 1]);
        }
        let (&first, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(first)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.n - self.i + self.rest.len();
        (n, Some(n))
    }
}

impl ExactSizeIterator for Uses<'_> {}

/// The values an instruction reads.
pub(crate) fn uses(k: &super::InstKind) -> Uses<'_> {
    use super::InstKind as K;
    let none = Value(0);
    let (fixed, n, rest): ([Value; 2], usize, &[Value]) = match k {
        K::Undef | K::Iconst(_) | K::Fconst(_) | K::Str(_) => ([none; 2], 0, &[]),
        K::Arith { a, b, .. }
        | K::Div { a, b, .. }
        | K::Rem { a, b, .. }
        | K::Shl { a, b, .. }
        | K::Shr { a, b, .. }
        | K::Bit { a, b, .. }
        | K::ICmp { a, b, .. }
        | K::FArith { a, b, .. }
        | K::FCmp { a, b, .. } => ([*a, *b], 2, &[]),
        K::Neg { a, .. }
        | K::BitNot { a, .. }
        | K::Not(a)
        | K::FNeg { a, .. }
        | K::IntCast { a, .. }
        | K::IntToFloat { a, .. }
        | K::FloatToInt { a, .. }
        | K::FloatCast { a, .. }
        | K::IntToChar { a, .. } => ([*a, none], 1, &[]),
        K::Call { args, .. } | K::Native { args, .. } => ([none; 2], 0, args),
        K::Slot(_) | K::Global(_) | K::FuncRef(_) | K::Life { .. } => ([none; 2], 0, &[]),
        K::Load { ptr, .. } | K::Offset { ptr, .. } => ([*ptr, none], 1, &[]),
        K::Store { ptr, val, .. } => ([*ptr, *val], 2, &[]),
        K::Copy { dst, src, .. } => ([*dst, *src], 2, &[]),
        K::Zero { dst, .. } => ([*dst, none], 1, &[]),
        K::Index { base, index, .. } => ([*base, *index], 2, &[]),
        K::Bounds { index, len } => ([*index, *len], 2, &[]),
        K::Elem { header, index, .. } => ([*header, *index], 2, &[]),
        K::FreeBuf { header, .. } => ([*header, none], 1, &[]),
        K::CallInd { callee, args } => ([*callee, none], 1, args),
    };
    Uses { fixed, n, i: 0, rest }
}

/// The blocks only unwinding reaches: every landing pad, and what they
/// jump to.
fn pad_reach(f: &Func) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    let mut stack: Vec<usize> = f.blocks.iter().flat_map(|b| b.pads()).map(|p| p.0 as usize).collect();
    while let Some(b) = stack.pop() {
        if b >= f.blocks.len() || !out.insert(b) {
            continue;
        }
        for e in f.blocks[b].term.iter().flat_map(|t| t.edges()) {
            stack.push(e.block.0 as usize);
        }
        stack.extend(f.blocks[b].pads().into_iter().map(|p| p.0 as usize));
    }
    out
}

/// For each block only unwinding reaches, every place that unwinds to it
/// (straight, or to a pad that jumps to it): a block, and the index of the
/// instruction (the block's length for its terminator).
fn unwind_sources(f: &Func, pads: &std::collections::HashSet<usize>) -> Vec<Vec<(usize, usize)>> {
    let mut out = vec![Vec::new(); f.blocks.len()];
    for (b, block) in f.blocks.iter().enumerate() {
        let mut add = |pad: BlockId, at: usize| {
            // Every block the pad reaches by jumping.
            let mut stack = vec![pad.0 as usize];
            let mut seen = std::collections::HashSet::new();
            while let Some(x) = stack.pop() {
                if x >= f.blocks.len() || !pads.contains(&x) || !seen.insert(x) {
                    continue;
                }
                out[x].push((b, at));
                stack.extend(f.blocks[x].term.iter().flat_map(|t| t.edges()).map(|e| e.block.0 as usize));
            }
        };
        for (i, inst) in block.insts.iter().enumerate() {
            if let Some(pad) = inst.unwind {
                add(pad, i);
            }
        }
        if let Some(pad) = block.term_unwind {
            add(pad, block.insts.len());
        }
    }
    out
}

/// Whether `v` is defined in one of the landing pads.
fn pad_defined(f: &Func, pads: &std::collections::HashSet<usize>, v: Value) -> bool {
    pads.iter().any(|&b| {
        let block = &f.blocks[b];
        block.params.contains(&v) || block.insts.iter().any(|i| i.results.contains(&v))
    })
}

/// The immediate dominator of each block reachable from the entry (the
/// entry's is itself), by Cooper, Harvey and Kennedy's iteration.
fn dominators(f: &Func, preds: &[Vec<usize>]) -> Vec<Option<usize>> {
    let n = f.blocks.len();
    // Reverse postorder from the entry.
    let mut order = Vec::new();
    let mut seen = vec![false; n];
    let mut stack = vec![(0usize, 0usize)];
    seen[0] = true;
    while let Some((b, next)) = stack.pop() {
        let succs: Vec<usize> =
            f.blocks[b].term.as_ref().map(|t| t.edges().map(|e| e.block.0 as usize).collect()).unwrap_or_default();
        if next < succs.len() {
            stack.push((b, next + 1));
            let s = succs[next];
            if !seen[s] {
                seen[s] = true;
                stack.push((s, 0));
            }
        } else {
            order.push(b);
        }
    }
    order.reverse();
    let mut rank = vec![usize::MAX; n];
    for (i, &b) in order.iter().enumerate() {
        rank[b] = i;
    }
    let mut idom: Vec<Option<usize>> = vec![None; n];
    idom[0] = Some(0);
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().skip(1) {
            let mut new = None;
            for &p in &preds[b] {
                if idom[p].is_none() {
                    continue;
                }
                new = Some(match new {
                    None => p,
                    Some(q) => {
                        let (mut x, mut y) = (p, q);
                        while x != y {
                            while rank[x] > rank[y] {
                                x = idom[x].expect("processed");
                            }
                            while rank[y] > rank[x] {
                                y = idom[y].expect("processed");
                            }
                        }
                        x
                    }
                });
            }
            if new.is_some() && idom[b] != new {
                idom[b] = new;
                changed = true;
            }
        }
    }
    idom
}
