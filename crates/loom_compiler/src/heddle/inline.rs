//! Inlining: small functions with no locations of their own (std's methods
//! the compiler writes, and glue) are copied into their callers, which
//! saves a call, a return and moving the arguments (design/vm.md §17's
//! "inline the small builtins").
//!
//! Only functions that are transparent anyway: a trap in one is reported at
//! the call into it, which is where an inlined copy's instructions report
//! too (they take the call's location), and they unwind to the call's
//! landing pad. One level: what an inlined function calls stays a call.

use super::{Block, BlockId, Edge, Func, InstKind, Module, Term, Value};

/// The most instructions (constants aside) a function inlined may have.
const SMALL: usize = 20;

/// `module` with the calls of its small transparent functions inlined, and
/// each function tidied up after (see `tidy`).
pub fn inline(module: &Module) -> Module {
    let small: Vec<bool> = module.funcs.iter().map(inlinable).collect();
    let mut out = module.clone();
    for (i, f) in out.funcs.iter_mut().enumerate() {
        inline_into(f, i, module, &small);
        super::tidy::tidy(f);
    }
    out
}

/// Whether calls of `f` may be inlined: no locations, nothing to clean up
/// when it unravels, small, and an entry block nothing jumps back to.
fn inlinable(f: &Func) -> bool {
    let transparent = f.blocks.iter().all(|b| {
        b.term_span.is_none()
            && b.term_unwind.is_none()
            && b.insts.iter().all(|i| i.span.is_none() && i.unwind.is_none())
            && !matches!(b.term, Some(Term::Resume) | None)
    });
    let size: usize = f.blocks.iter().map(|b| b.insts.iter().filter(|i| !constant(&i.kind)).count()).sum();
    let reentered = f.blocks.iter().any(|b| b.term.iter().flat_map(|t| t.edges()).any(|e| e.block.0 == 0));
    transparent && size <= SMALL && !reentered
}

fn constant(k: &InstKind) -> bool {
    matches!(k, InstKind::Iconst(_) | InstKind::Fconst(_) | InstKind::Str(_) | InstKind::Global(_))
}

/// Inlines the calls in `f` (function `me` of `module`) of the functions
/// `small` says may be.
fn inline_into(f: &mut Func, me: usize, module: &Module, small: &[bool]) {
    // Blocks copied from a callee aren't looked at again: what an inlined
    // function calls stays a call (or recursive glue would never end).
    let mut copied = vec![false; f.blocks.len()];
    let mut b = 0;
    while b < f.blocks.len() {
        if copied[b] {
            b += 1;
            continue;
        }
        let found = f.blocks[b].insts.iter().position(|inst| match &inst.kind {
            InstKind::Call { func, .. } => {
                let g = func.0 as usize;
                g != me && small[g] && module.funcs[g].results.len() == inst.results.len()
            }
            _ => false,
        });
        let Some(i) = found else {
            b += 1;
            continue;
        };
        // Split the block at the call: what follows it goes on in a block
        // whose parameters are the call's results.
        let rest = f.blocks[b].insts.split_off(i + 1);
        let call = f.blocks[b].insts.pop().expect("the call");
        let InstKind::Call { func, args } = &call.kind else { unreachable!("found a call") };
        let callee = &module.funcs[func.0 as usize];
        let cont = BlockId(f.blocks.len() as u32);
        let after = Block {
            params: call.results.clone(),
            insts: rest,
            term: f.blocks[b].term.take(),
            term_span: f.blocks[b].term_span.take(),
            term_unwind: f.blocks[b].term_unwind.take(),
        };
        f.blocks.push(after);
        copied.push(false);
        // The callee's blocks, each value given a new number (its
        // parameters are the arguments), its slots after the caller's.
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
                for u in super::inst_uses(&inst.kind) {
                    new(u, f, &mut map);
                }
                super::rewrite(&mut inst.kind, |v| map[v.0 as usize].expect("numbered just above"));
                if let InstKind::Slot(n) = &mut inst.kind {
                    *n += slot_base;
                }
                inst.results = inst.results.iter().map(|&r| new(r, f, &mut map)).collect();
                inst.span = call.span;
                inst.unwind = call.unwind;
                insts.push(inst);
            }
            let mut term = cb.term.clone().expect("finished blocks end");
            match &mut term {
                Term::Br { cond, .. } => *cond = new(*cond, f, &mut map),
                Term::Unravel { ptr, len } => {
                    *ptr = new(*ptr, f, &mut map);
                    *len = new(*len, f, &mut map);
                }
                Term::Ret(_)
                | Term::Jump(_)
                | Term::UnravelMsg
                | Term::Crash(_)
                | Term::Resume
                | Term::Suspend { .. } => {}
            }
            for e in term.edges_mut() {
                e.block = BlockId(e.block.0 + base);
                for a in &mut e.args {
                    *a = new(*a, f, &mut map);
                }
            }
            // A return goes on after the call, with the results.
            if let Term::Ret(vs) = &term {
                let args = vs.iter().map(|&v| new(v, f, &mut map)).collect();
                term = Term::Jump(Edge { block: cont, args });
            }
            // An unravel in it goes where one in the call would.
            let unravels = matches!(term, Term::Unravel { .. } | Term::UnravelMsg | Term::Crash(_));
            blocks.push(Block {
                params,
                insts,
                term: Some(term),
                term_span: call.span,
                term_unwind: if unravels { call.unwind } else { None },
            });
        }
        copied.resize(copied.len() + blocks.len(), true);
        f.blocks.extend(blocks);
        f.blocks[b].term = Some(Term::Jump(Edge { block: BlockId(base), args: Vec::new() }));
        f.blocks[b].term_span = call.span;
        b += 1;
    }
}
