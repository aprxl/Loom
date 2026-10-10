//! Interprocedural constants: a parameter every call of its function gives
//! the same constant is that constant inside the function, for the passes
//! after this one: `times_both(u, mut v, mut tmp)` always given lists of
//! 400 floats loops 400 times, which range analysis can see.
//!
//! Only functions nothing takes as a value (whose calls are all in sight),
//! and not the entry, nor a shuttle's body (its calls come from its
//! driver). The parameter stays: callers still pass it, and codegen gives
//! it no work.

use crate::fxhash::FxHashMap;

use crate::heddle::{Inst, InstKind, Module, Value};

pub fn ipcp(module: &mut Module) {
    let n = module.funcs.len();
    // For each function and parameter: the constant every call so far
    // passes (`Some`), or `None` once one passes something else.
    let mut seen: Vec<Option<Vec<Option<InstKind>>>> = vec![None; n];
    let mut barred = vec![false; n];
    barred[module.entry.0 as usize] = true;
    for f in &module.funcs {
        let consts: FxHashMap<Value, InstKind> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match (&i.kind, i.results.as_slice()) {
                (k @ (InstKind::Iconst(_) | InstKind::Fconst(_)), [r]) => Some((*r, k.clone())),
                _ => None,
            })
            .collect();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::FuncRef(g) => barred[g.0 as usize] = true,
                InstKind::Call { func, args } => {
                    let given: Vec<Option<InstKind>> = args.iter().map(|a| consts.get(a).cloned()).collect();
                    let slot = &mut seen[func.0 as usize];
                    *slot = Some(match slot.take() {
                        None => given,
                        Some(old) => old.into_iter().zip(given).map(|(o, g)| if o == g { o } else { None }).collect(),
                    });
                }
                _ => {}
            }
        }
    }
    for (i, f) in module.funcs.iter_mut().enumerate() {
        let Some(known) = &seen[i] else { continue };
        let suspends = f.blocks.iter().any(|b| matches!(b.term, Some(crate::heddle::Term::Suspend { .. })));
        if barred[i] || suspends || f.blocks.is_empty() {
            continue;
        }
        let mut repl: FxHashMap<Value, Value> = FxHashMap::default();
        let mut made = Vec::new();
        for (k, c) in known.iter().enumerate() {
            let (Some(c), Some(&p)) = (c, f.blocks[0].params.get(k)) else { continue };
            let v = Value(f.types.len() as u32);
            f.types.push(f.ty(p));
            made.push(Inst { kind: c.clone(), results: vec![v], span: None, unwind: None, site: None });
            repl.insert(p, v);
        }
        if made.is_empty() {
            continue;
        }
        let swap = |v: Value| repl.get(&v).copied().unwrap_or(v);
        for b in &mut f.blocks {
            for inst in &mut b.insts {
                crate::heddle::rewrite(&mut inst.kind, swap);
            }
            if let Some(t) = &mut b.term {
                super::rewrite_term(t, swap);
            }
        }
        f.blocks[0].insts.splice(0..0, made);
    }
}
