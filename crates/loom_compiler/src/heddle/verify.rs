//! Checks the SSA properties codegen relies on (design/vm.md §6.4):
//!
//! - every block ends in a terminator, and nothing jumps to the entry;
//! - every edge passes one argument, of the right type, per parameter;
//! - every value is defined exactly once;
//! - every use is dominated by its definition.

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
    // Whether the use at (block, index) sees `v`; the terminator is at
    // index `insts.len()`.
    let check = |v: Value, block: usize, index: usize| -> Result<(), String> {
        let &(db, di) = defs.get(&v).ok_or_else(|| format!("v{} is used in b{block} but never defined", v.0))?;
        if idom[block].is_none() {
            return Ok(()); // unreachable: nothing runs it
        }
        let ok = if db == block { di.is_none_or(|di| di < index) } else { dominates(db, block) };
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
    }
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
