//! What each function may write through each of its parameters (mod/ref
//! summaries), so a caller can keep what it knows of a frame slot across a
//! call that's given the slot's address: a list's length across
//! `times(mut out)`, which only writes the list's elements.
//!
//! For each parameter, the byte ranges (from where it points) that the
//! function, or what it passes the parameter on to, may write; or anything
//! (`Writes::Any`) once the parameter goes where it can't be followed: into
//! memory, out of the function, a block's parameter, a call through a
//! function value, a native that writes. Writes through a list's buffer
//! aren't writes through the parameter: the buffer is memory of its own.
//! Over the call graph, from nothing until nothing changes (recursion);
//! what still changes after a few rounds is `Any`.

use super::alias::{Alias, Root};
use crate::heddle::{Func, InstKind, Module, Value};

/// What a function may write through one parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writes {
    /// These byte ranges, (offset, size), from where it points.
    Ranges(Vec<(i64, u64)>),
    Any,
}

impl Writes {
    fn add(&mut self, off: i64, size: u64) {
        if let Writes::Ranges(rs) = self
            && !rs.contains(&(off, size))
        {
            rs.push((off, size));
            rs.sort_unstable();
        }
    }

    /// Whether it may write any of `[off, off + size)`.
    pub fn touches(&self, off: i64, size: u64) -> bool {
        match self {
            Writes::Any => true,
            Writes::Ranges(rs) => rs.iter().any(|&(o, n)| o < off + size as i64 && off < o + n as i64),
        }
    }
}

/// Natives that write nothing (and keep no address they're given).
fn quiet(name: &str) -> bool {
    super::gvn::QUIET_NATIVES.contains(&name) || super::gvn::PURE_NATIVES.contains(&name)
}

/// The summaries of every function of `module`, by function and parameter;
/// `aliases` are the functions' (`Alias::new`).
pub fn summaries_with(module: &Module, aliases: &[Alias]) -> Vec<Vec<Writes>> {
    let mut out: Vec<Vec<Writes>> =
        module.funcs.iter().map(|f| vec![Writes::Ranges(Vec::new()); f.params().len()]).collect();
    for round in 0.. {
        let mut changed = false;
        for (i, f) in module.funcs.iter().enumerate() {
            let new = summarize(f, &aliases[i], &out);
            if new != out[i] {
                changed = true;
                out[i] = if round >= 8 {
                    new.into_iter().zip(&out[i]).map(|(n, o)| if n == *o { n } else { Writes::Any }).collect()
                } else {
                    new
                };
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// `f`'s summary, given its callees' (`all`).
fn summarize(f: &Func, alias: &Alias, all: &[Vec<Writes>]) -> Vec<Writes> {
    let params = f.params();
    let mut out = vec![Writes::Ranges(Vec::new()); params.len()];
    // Which parameter a value points into, and where (when known).
    let from = |v: Value| -> Option<(usize, Option<i64>)> {
        match alias.root(v) {
            (Root::Value(p), off) => params.iter().position(|&q| q == p).map(|k| (k, off)),
            _ => None,
        }
    };
    for b in &f.blocks {
        for inst in &b.insts {
            use InstKind as K;
            // The uses followed: what's written through, and calls.
            let followed: Vec<Value> = match &inst.kind {
                K::Store { ptr, off, mem, .. } => {
                    if let Some((k, o)) = from(*ptr) {
                        match o {
                            Some(o) => out[k].add(o + i64::from(*off), mem.bytes()),
                            None => out[k] = Writes::Any,
                        }
                    }
                    // What's stored is followed no further: it escapes.
                    vec![*ptr]
                }
                K::Copy { dst, src, size } => {
                    if let Some((k, o)) = from(*dst) {
                        match o {
                            Some(o) => out[k].add(o, *size),
                            None => out[k] = Writes::Any,
                        }
                    }
                    vec![*dst, *src]
                }
                K::Zero { dst, size } => {
                    if let Some((k, o)) = from(*dst) {
                        match o {
                            Some(o) => out[k].add(o, *size),
                            None => out[k] = Writes::Any,
                        }
                    }
                    vec![*dst]
                }
                K::FreeBuf { header, off, .. } => {
                    if let Some((k, o)) = from(*header) {
                        match o {
                            Some(o) => out[k].add(o + i64::from(*off), 24),
                            None => out[k] = Writes::Any,
                        }
                    }
                    vec![*header]
                }
                K::Call { func, args } => {
                    for (j, &a) in args.iter().enumerate() {
                        if let Some((k, o)) = from(a) {
                            match (&all[func.0 as usize][j], o) {
                                (Writes::Ranges(rs), Some(o)) => {
                                    for &(ro, rn) in rs {
                                        out[k].add(o + ro, rn);
                                    }
                                }
                                (Writes::Ranges(rs), None) if rs.is_empty() => {}
                                _ => out[k] = Writes::Any,
                            }
                        }
                    }
                    args.clone()
                }
                K::Native { name, args, .. } if quiet(name) => args.clone(),
                // Words 0 and 2 of the header it's given.
                K::Native { name: "mem.grow", args, .. } => {
                    if let Some((k, o)) = from(args[0]) {
                        match o {
                            Some(o) => {
                                out[k].add(o, 8);
                                out[k].add(o + 16, 8);
                            }
                            None => out[k] = Writes::Any,
                        }
                    }
                    vec![args[0]]
                }
                // Reading, and deriving addresses, write nothing.
                K::Load { .. } | K::Offset { .. } | K::Index { .. } | K::Elem { .. } | K::Bounds { .. } => {
                    crate::heddle::inst_uses(&inst.kind).collect()
                }
                _ => Vec::new(),
            };
            for v in crate::heddle::inst_uses(&inst.kind) {
                if !followed.contains(&v)
                    && let Some((k, _)) = from(v)
                {
                    out[k] = Writes::Any;
                }
            }
            // A stored pointer goes where it can't be followed.
            if let K::Store { val, .. } = &inst.kind
                && let Some((k, _)) = from(*val)
            {
                out[k] = Writes::Any;
            }
        }
        for v in b.term.iter().flat_map(super::term_uses) {
            if let Some((k, _)) = from(v) {
                out[k] = Writes::Any;
            }
        }
    }
    out
}
