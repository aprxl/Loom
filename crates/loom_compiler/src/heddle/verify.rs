//! Checks the SSA properties codegen relies on (design/vm.md §6.4):
//!
//! - every block ends in a terminator, and nothing jumps to the entry;
//! - every edge passes one argument, of the right type, per parameter;
//! - every value is defined exactly once;
//! - every use is dominated by its definition;
//! - landing pads exist, only instructions that can unravel have one, and
//!   a pad reads no value but the function's parameters and its own.

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
    // What landing pads (reached only by unwinding) can see: the function's
    // parameters, and what the pads themselves compute.
    let pad_blocks: std::collections::HashSet<usize> = pad_reach(f);
    let params: std::collections::HashSet<Value> = f.params().iter().copied().collect();
    for &b in &pad_blocks {
        let block = &f.blocks[b];
        let mut seen: std::collections::HashSet<Value> = block.params.iter().copied().collect();
        for inst in &block.insts {
            for u in uses(&inst.kind) {
                if !seen.contains(&u) && !params.contains(&u) && !pad_defined(f, &pad_blocks, u) {
                    return Err(format!("the landing pad b{b} reads v{}, which isn't its own", u.0));
                }
            }
            seen.extend(inst.results.iter().copied());
        }
    }
    let dominates = |a: usize, mut b: usize| loop {
        if a == b {
            return true;
        }
        match idom[b] {
            Some(d) if d != b => b = d,
            _ => return false,
        }
    };
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

/// The values an instruction reads.
pub(crate) fn uses(k: &super::InstKind) -> Vec<Value> {
    use super::InstKind as K;
    match k {
        K::Undef | K::Iconst(_) | K::Fconst(_) | K::Str(_) => Vec::new(),
        K::Arith { a, b, .. }
        | K::Div { a, b, .. }
        | K::Rem { a, b, .. }
        | K::Shl { a, b, .. }
        | K::Shr { a, b, .. }
        | K::Bit { a, b, .. }
        | K::ICmp { a, b, .. }
        | K::FArith { a, b, .. }
        | K::FCmp { a, b, .. } => vec![*a, *b],
        K::Neg { a, .. }
        | K::BitNot { a, .. }
        | K::Not(a)
        | K::FNeg { a, .. }
        | K::IntCast { a, .. }
        | K::IntToFloat { a, .. }
        | K::FloatToInt { a, .. }
        | K::FloatCast { a, .. }
        | K::IntToChar { a, .. } => vec![*a],
        K::Call { args, .. } | K::Native { args, .. } => args.clone(),
        K::Slot(_) | K::Global(_) | K::FuncRef(_) => Vec::new(),
        K::Load { ptr, .. } | K::Offset { ptr, .. } => vec![*ptr],
        K::Store { ptr, val, .. } => vec![*ptr, *val],
        K::Copy { dst, src, .. } => vec![*dst, *src],
        K::Zero { dst, .. } => vec![*dst],
        K::Index { base, index, .. } => vec![*base, *index],
        K::Bounds { index, len } => vec![*index, *len],
        K::CallInd { callee, args } => std::iter::once(*callee).chain(args.iter().copied()).collect(),
    }
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
        let succs: Vec<usize> = f.blocks[b]
            .term
            .as_ref()
            .map(|t| t.edges().iter().map(|e| e.block.0 as usize).collect())
            .unwrap_or_default();
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
