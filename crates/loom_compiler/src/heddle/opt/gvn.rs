//! Global value numbering (pass 5 of design/vm.md §7): a computation that
//! an identical one dominates is replaced by that one's result, and so is a
//! load of memory nothing could have changed since it was loaded (or
//! stored).
//!
//! Over the dominator tree, with a table of what's been computed on the way
//! down. A trapping instruction (`x + y` checked, a bounds check) can go
//! too: the one that dominates it already trapped, or found nothing wrong
//! with the same operands. Loads are only kept along a path with no other
//! way in: a block that more than one edge reaches (a loop's head, where an
//! `if`'s arms meet) starts with no loads known, and so does what comes
//! after a call. A store or a copy forgets what it may change (see
//! `alias`), and a store is then known to have put its value there.

use std::collections::HashMap;

use super::alias::{Access, Alias};
use super::cfg::Cfg;
use crate::heddle::{BitOp, Cmp, FOp, Func, InstKind, IntOp, Mem, Module, VTy, Value};

/// Numbers every function's values.
pub fn gvn(module: &mut Module) {
    for f in &mut module.funcs {
        gvn_func(f);
        super::tidy::tidy(f);
    }
}

/// What a load or an element's address reads, as a key of the table of
/// known memory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MemKey {
    Load { ptr: Value, off: i32, mem: Mem },
    Elem { header: Value, off: i32, index: Value, scale: u64 },
}

impl MemKey {
    /// The bytes it reads.
    fn access(&self) -> Access {
        match self {
            MemKey::Load { ptr, off, mem } => Access { ptr: *ptr, off: i64::from(*off), size: mem.bytes() },
            // An element's address reads its list's buffer and length.
            MemKey::Elem { header, off, .. } => Access { ptr: *header, off: i64::from(*off), size: 16 },
        }
    }
}

/// How an instruction takes part.
enum Role {
    /// Computes its results from its operands alone (maybe trapping): one
    /// that an identical one dominates goes.
    Pure,
    /// Reads memory.
    Read(MemKey),
    /// Writes these bytes.
    Write(Access),
    /// May write memory a call can reach.
    Clobber,
    /// Neither: reads memory, or has an effect, without changing what a
    /// load would see.
    Other,
}

/// Natives that only compute their results from their arguments.
pub(super) const PURE_NATIVES: &[&str] = &[
    "int.add_chk",
    "int.sub_chk",
    "int.mul_chk",
    "int.cast_chk",
    "float.to_int_chk",
    "int.to_char_chk",
    "int.bits",
    "int.rotate",
    "float.math",
    "char.digit",
];

/// Natives that read memory, or make something new, but change nothing a
/// load could see.
pub(super) const QUIET_NATIVES: &[&str] = &[
    "str.eq",
    "str.cmp",
    "str.char_count",
    "str.find",
    "str.starts_with",
    "str.ends_with",
    "str.trim",
    "str.next_char",
    "str.next_line",
    "str.next_word",
    "str.hash",
    "str.parse_int",
    "str.parse_float",
    "map.find_str",
    "map.find_int",
    "mem.alloc",
    "mem.calloc",
];

fn role(k: &InstKind) -> Role {
    use InstKind as K;
    match k {
        K::Arith { .. }
        | K::Div { .. }
        | K::Rem { .. }
        | K::Shl { .. }
        | K::Shr { .. }
        | K::Bit { .. }
        | K::Neg { .. }
        | K::BitNot { .. }
        | K::Not(_)
        | K::ICmp { .. }
        | K::FArith { .. }
        | K::FNeg { .. }
        | K::FCmp { .. }
        | K::IntCast { .. }
        | K::IntToFloat { .. }
        | K::FloatToInt { .. }
        | K::FloatCast { .. }
        | K::IntToChar { .. }
        | K::Offset { .. }
        | K::Index { .. }
        | K::Bounds { .. } => Role::Pure,
        K::Native { name, .. } if PURE_NATIVES.contains(name) => Role::Pure,
        K::Native { name, .. } if QUIET_NATIVES.contains(name) => Role::Other,
        K::Load { ptr, off, mem } => Role::Read(MemKey::Load { ptr: *ptr, off: *off, mem: *mem }),
        K::Elem { header, off, index, scale } => {
            Role::Read(MemKey::Elem { header: *header, off: *off, index: *index, scale: *scale })
        }
        K::Store { ptr, off, mem, .. } => Role::Write(Access { ptr: *ptr, off: i64::from(*off), size: mem.bytes() }),
        K::Copy { dst, size, .. } | K::Zero { dst, size } => Role::Write(Access { ptr: *dst, off: 0, size: *size }),
        K::Call { .. } | K::CallInd { .. } | K::Native { .. } | K::FreeBuf { .. } => Role::Clobber,
        // Each `undef` stands for itself. Constants and a slot's address
        // are cheapest made where they're used (codegen folds them into the
        // instruction, or makes them in the call window): one shared would
        // live in a register of its own, and be moved where it's needed.
        K::Undef
        | K::Iconst(_)
        | K::Fconst(_)
        | K::Str(_)
        | K::Global(_)
        | K::FuncRef(_)
        | K::Slot(_)
        | K::Life { .. } => Role::Other,
    }
}

/// `k` with commutative operands in order, so `a + b` and `b + a` are one
/// key.
fn canonical(mut k: InstKind) -> InstKind {
    use InstKind as K;
    let swap = |a: &mut Value, b: &mut Value| {
        if *b < *a {
            std::mem::swap(a, b);
        }
    };
    match &mut k {
        K::Arith { op: IntOp::Add | IntOp::Mul, a, b, .. }
        | K::Bit { op: BitOp::And | BitOp::Or | BitOp::Xor, a, b }
        | K::FArith { op: FOp::Add | FOp::Mul, a, b, .. }
        | K::ICmp { cmp: Cmp::Eq | Cmp::Ne, a, b, .. }
        | K::FCmp { cmp: Cmp::Eq | Cmp::Ne, a, b, .. } => swap(a, b),
        _ => {}
    }
    k
}

/// One change to a table, to undo when the walk leaves the block.
enum Undo {
    Expr((InstKind, Vec<VTy>)),
    /// What a key of known memory was before.
    Mem(MemKey, Option<Value>),
}

struct Gvn<'a> {
    alias: Alias,
    cfg: &'a Cfg,
    /// What each value was replaced with.
    repl: Vec<Value>,
    /// What's been computed, with its results' types (a zero address isn't
    /// a zero integer), and its results.
    exprs: HashMap<(InstKind, Vec<VTy>), Vec<Value>>,
    /// Known memory: what a load at each key gives.
    mem: HashMap<MemKey, Value>,
    undo: Vec<Undo>,
}

impl Gvn<'_> {
    fn find(&self, mut v: Value) -> Value {
        while self.repl[v.0 as usize] != v {
            v = self.repl[v.0 as usize];
        }
        v
    }

    fn remember(&mut self, key: MemKey, v: Value) {
        let old = self.mem.insert(key.clone(), v);
        self.undo.push(Undo::Mem(key, old));
    }

    /// Forgets the known memory `keep` says no to.
    fn forget(&mut self, keep: impl Fn(&Alias, &MemKey) -> bool) {
        let gone: Vec<MemKey> = self.mem.keys().filter(|k| !keep(&self.alias, k)).cloned().collect();
        for k in gone {
            let old = self.mem.remove(&k);
            self.undo.push(Undo::Mem(k, old));
        }
    }

    fn undo_to(&mut self, mark: usize) {
        while self.undo.len() > mark {
            match self.undo.pop().expect("above the mark") {
                Undo::Expr(k) => {
                    self.exprs.remove(&k);
                }
                Undo::Mem(k, Some(v)) => {
                    self.mem.insert(k, v);
                }
                Undo::Mem(k, None) => {
                    self.mem.remove(&k);
                }
            }
        }
    }
}

fn gvn_func(f: &mut Func) {
    let cfg = Cfg::new(f);
    let mut g = Gvn {
        alias: Alias::new(f),
        cfg: &cfg,
        repl: (0..f.types.len() as u32).map(Value).collect(),
        exprs: HashMap::new(),
        mem: HashMap::new(),
        undo: Vec::new(),
    };
    // (block, the undo mark to go back to once its children are done).
    let mut stack: Vec<(usize, Option<usize>)> = vec![(0, None)];
    let mut dead: Vec<Vec<bool>> = f.blocks.iter().map(|b| vec![false; b.insts.len()]).collect();
    while let Some((b, done)) = stack.pop() {
        if let Some(mark) = done {
            g.undo_to(mark);
            continue;
        }
        stack.push((b, Some(g.undo.len())));
        // Memory is known only through a block's one way in.
        let only = g.cfg.preds[b].len() == 1 && g.cfg.idom[b] == Some(g.cfg.preds[b][0]);
        if !only {
            g.forget(|_, _| false);
        }
        visit(&mut g, f, b, &mut dead[b]);
        for &c in g.cfg.children[b].iter().rev() {
            stack.push((c, None));
        }
    }
    // Replace what went everywhere (landing pads too), and drop it.
    let repl: Vec<Value> = (0..f.types.len() as u32).map(|v| g.find(Value(v))).collect();
    for (block, dead) in f.blocks.iter_mut().zip(&dead) {
        let mut i = 0;
        block.insts.retain(|_| {
            i += 1;
            !dead[i - 1]
        });
        for inst in &mut block.insts {
            crate::heddle::rewrite(&mut inst.kind, |v| repl[v.0 as usize]);
        }
        if let Some(t) = &mut block.term {
            super::rewrite_term(t, |v| repl[v.0 as usize]);
        }
    }
}

/// Numbers block `b`'s instructions, marking those that go in `dead`.
fn visit(g: &mut Gvn, f: &Func, b: usize, dead: &mut [bool]) {
    for (i, inst) in f.blocks[b].insts.iter().enumerate() {
        let mut kind = inst.kind.clone();
        crate::heddle::rewrite(&mut kind, |v| g.find(v));
        match role(&kind) {
            Role::Pure => {
                let key = (canonical(kind), inst.results.iter().map(|&r| f.ty(r)).collect());
                if let Some(old) = g.exprs.get(&key) {
                    for (r, o) in inst.results.iter().zip(old.clone()) {
                        g.repl[r.0 as usize] = o;
                    }
                    dead[i] = true;
                } else {
                    g.exprs.insert(key.clone(), inst.results.clone());
                    g.undo.push(Undo::Expr(key));
                }
            }
            Role::Read(key) => match (g.mem.get(&key).copied(), inst.results.as_slice()) {
                // (A stored value of another type, as an address stored as
                // an integer, is loaded again.)
                (Some(old), [r]) if f.ty(old) == f.ty(*r) => {
                    g.repl[r.0 as usize] = old;
                    dead[i] = true;
                }
                (_, [r]) => g.remember(key, *r),
                _ => {}
            },
            Role::Write(access) => {
                g.forget(|alias, k| !alias.may_alias(access, k.access()));
                if let InstKind::Store { ptr, off, mem, val } = kind {
                    g.remember(MemKey::Load { ptr, off, mem }, val);
                }
            }
            Role::Clobber => g.forget(|_, _| false),
            // A loaded value kept past a call would have to be moved into
            // the next call's window, where loading it again puts it
            // straight: as cheap, and it needs no register of its own.
            Role::Other if matches!(kind, InstKind::Native { .. }) => g.forget(|_, _| false),
            Role::Other => {}
        }
    }
}
