//! Global value numbering (pass 5 of design/vm.md §7): a computation that
//! an identical one dominates is replaced by that one's result, and so is a
//! load of memory nothing could have changed since it was loaded (or
//! stored).
//!
//! Over the dominator tree, with a table of what's been computed on the way
//! down. A trapping instruction (`x + y` checked, a bounds check) can go
//! too: the one that dominates it already trapped, or found nothing wrong
//! with the same operands. Loads are only kept along a path with no other
//! way in: a block that more than one edge reaches (where an `if`'s arms
//! meet) starts with no loads known, and so does what comes after a call.
//! A loop's head is the exception, when one block before the loop leads to
//! it: what was known there still is, but for what the loop may write (a
//! call in it may write anything but a slot whose address never escapes).
//! A store or a copy forgets what it may change (see `alias`), and a store
//! is then known to have put its value there. Memory in a slot (or the
//! program's data) is known by where it is, whichever of the values for
//! that address reads it.
//!
//! An element's address (`elem`), whose list's buffer and length are
//! known, becomes a check of the index against that length (gone when one
//! like it dominates it) and the address in that buffer: which range
//! analysis may then show can't fail. In a loop that can't change the
//! header of a list in the frame, they're loaded there, and LICM takes the
//! loads out of the loop.
//!
//! A map lookup (`map.find_str`, `map.find_int`) that one with the same map
//! and key dominates gives what that one gave, when nothing on the way could
//! have written memory: `if m.has(k) { m[k] += 1 }` looks `k` up once. Its
//! key is often loaded again after the first lookup (see `Role::Other`), so
//! a second table of known memory, which quiet natives leave alone, tells
//! when two loads read the same thing.

use std::collections::HashMap;

use super::alias::{Access, Alias, Root};
use super::cfg::Cfg;
use crate::heddle::{BitOp, Cmp, FOp, Func, InstKind, IntOp, Mem, Module, Term, VTy, Value};

/// Numbers every function's values.
pub fn gvn(module: &mut Module) {
    // What each function takes and gives, for calls through a known
    // function value.
    let shapes: Vec<(usize, usize)> = module.funcs.iter().map(|f| (f.params().len(), f.results.len())).collect();
    for f in &mut module.funcs {
        gvn_func(f);
        devirtualize(f, &shapes);
        super::tidy::tidy(f);
    }
}

/// A call through a function value that's a known function (a lambda
/// passed to a method inlined here, whose value GVN found in memory)
/// becomes a call of that function, which inlining can then copy in.
fn devirtualize(f: &mut Func, shapes: &[(usize, usize)]) {
    let mut func_of: HashMap<Value, crate::heddle::FuncId> = HashMap::new();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let (InstKind::FuncRef(g), [r]) = (&inst.kind, inst.results.as_slice()) {
            func_of.insert(*r, *g);
        }
    }
    for inst in f.blocks.iter_mut().flat_map(|b| &mut b.insts) {
        if let InstKind::CallInd { callee, args } = &inst.kind
            && let Some(&g) = func_of.get(callee)
            && shapes.get(g.0 as usize) == Some(&(args.len(), inst.results.len()))
        {
            inst.kind = InstKind::Call { func: g, args: args.clone() };
        }
    }
}

/// What a load reads, as a key of the table of known memory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MemKey {
    Load {
        ptr: Value,
        off: i32,
        mem: Mem,
    },
    /// `size` bytes at `ptr + off` are all zero (a `zero` wrote them).
    Zero {
        ptr: Value,
        off: i64,
        size: u64,
    },
}

impl MemKey {
    /// The same key, with the address it reads named by `name`.
    fn named(self, name: impl Fn(Value) -> Value) -> MemKey {
        match self {
            MemKey::Load { ptr, off, mem } => MemKey::Load { ptr: name(ptr), off, mem },
            MemKey::Zero { ptr, off, size } => MemKey::Zero { ptr: name(ptr), off, size },
        }
    }

    /// The bytes it reads.
    fn access(&self) -> Access {
        match self {
            MemKey::Load { ptr, off, mem } => Access { ptr: *ptr, off: i64::from(*off), size: mem.bytes() },
            MemKey::Zero { ptr, off, size } => Access { ptr: *ptr, off: *off, size: *size },
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
        K::Store { ptr, off, mem, .. } => Role::Write(Access { ptr: *ptr, off: i64::from(*off), size: mem.bytes() }),
        K::Copy { dst, size, .. } | K::Zero { dst, size } => Role::Write(Access { ptr: *dst, off: 0, size: *size }),
        K::Call { .. } | K::CallInd { .. } | K::Native { .. } | K::FreeBuf { .. } => Role::Clobber,
        // Each `undef` stands for itself. Constants and a slot's address
        // are cheapest made where they're used (codegen folds them into the
        // instruction, or makes them in the call window): one shared would
        // live in a register of its own, and be moved where it's needed. So
        // is an element's address: a load or a store of it is one checked
        // instruction (`ldelf`), where a shared one is the address and then
        // a plain load or store at it.
        K::Elem { .. }
        | K::Undef
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

/// Natives that look a key up in a map: they read the map, and change
/// nothing.
const LOOKUPS: &[&str] = &["map.find_str", "map.find_int"];

/// An argument of a lookup, as its key: constants by value (GVN leaves each
/// constant its own value).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Arg {
    Const(u64),
    Value(Value),
}

type FindKey = (&'static str, u16, Vec<Arg>);

/// One change to a table, to undo when the walk leaves the block.
enum Undo {
    Expr((InstKind, Vec<VTy>)),
    /// What a key of known memory was before.
    Mem(MemKey, Option<Value>),
    /// The same for the memory quiet natives leave known.
    Stable(MemKey, Option<Value>),
    /// A lookup made (to forget), or forgotten (to know again).
    Find(FindKey, Option<Vec<Value>>),
}

/// What a loop may write: memory known before it that none of this
/// touches is known at its head too.
struct LoopWrites {
    writes: Vec<Access>,
    /// A call, or anything else that may write what it can reach.
    clobbers: bool,
}

impl LoopWrites {
    fn keeps(&self, alias: &Alias, k: &MemKey) -> bool {
        let a = k.access();
        let private = matches!(alias.root(a.ptr), (Root::Slot(n), _) if !alias.escapes[n as usize]);
        (private || !self.clobbers) && self.writes.iter().all(|w| !alias.may_alias(*w, a))
    }
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
    /// Known memory that natives which write nothing don't forget: which
    /// value a load at each key read first.
    stable: HashMap<MemKey, Value>,
    /// For a load of what an earlier one read: that one (for lookups' keys
    /// alone; the load stays).
    same: HashMap<Value, Value>,
    /// The constant each value is, when it is one.
    consts: HashMap<Value, u64>,
    /// For an address in a slot or the program's data (which GVN leaves
    /// unshared, like constants): the first value that is that address.
    addrs: HashMap<Value, Value>,
    /// The lookups made, and their results.
    finds: HashMap<FindKey, Vec<Value>>,
    /// The `elem`s whose list's buffer and length are known, by (block,
    /// instruction): the length to check the index against (none when a
    /// check like it dominates), and the buffer.
    split: HashMap<(usize, usize), (Option<Value>, Value)>,
    /// Of those, the ones whose buffer and length are loaded there.
    loads: HashMap<(usize, usize), (Value, Value)>,
    /// What each loop may write, and the loops each block is in.
    loops: Vec<LoopWrites>,
    within: Vec<Vec<usize>>,
    /// The types of the values made here, numbered from `types.len()`.
    fresh: Vec<VTy>,
    base: u32,
    undo: Vec<Undo>,
}

impl Gvn<'_> {
    /// A new value, of type `ty`.
    fn fresh(&mut self, ty: VTy) -> Value {
        let v = Value(self.base + self.fresh.len() as u32);
        self.fresh.push(ty);
        self.repl.push(v);
        v
    }

    fn find(&self, mut v: Value) -> Value {
        while self.repl[v.0 as usize] != v {
            v = self.repl[v.0 as usize];
        }
        v
    }

    /// Whether a `zero` known to hold wrote all of `access`'s bytes.
    fn zeroed(&self, access: Access) -> bool {
        let (root, off) = self.alias.root(access.ptr);
        let Some(off) = off.map(|o| o.wrapping_add(access.off)) else { return false };
        self.mem.keys().any(|k| match k {
            MemKey::Zero { ptr, off: zoff, size } => match self.alias.root(*ptr) {
                (r, Some(start)) if r == root => {
                    let start = start + zoff;
                    off >= start && off + access.size as i64 <= start + *size as i64
                }
                _ => false,
            },
            _ => false,
        })
    }

    /// When `k` is a zeroed stretch `access` writes part of, at offsets
    /// known from the same root: the parts before and after it.
    fn zero_left(&self, k: &MemKey, access: Access) -> Option<Vec<MemKey>> {
        let MemKey::Zero { ptr, off, size } = *k else { return None };
        let (zr, zstart) = self.alias.root(ptr);
        let (wr, wstart) = self.alias.root(access.ptr);
        let (zstart, wstart) = (zstart? + off, wstart? + access.off);
        let (zend, wend) = (zstart + size as i64, wstart + access.size as i64);
        if zr != wr || wend <= zstart || zend <= wstart {
            return None;
        }
        let mut out = Vec::new();
        if wstart > zstart {
            out.push(MemKey::Zero { ptr, off, size: (wstart - zstart) as u64 });
        }
        if zend > wend {
            out.push(MemKey::Zero { ptr, off: off + (wend - zstart), size: (zend - wend) as u64 });
        }
        Some(out)
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

    /// Forgets what a native that writes nothing makes not worth knowing:
    /// a loaded value kept past it would have to be moved into the next
    /// call's window, where loading it again puts it straight, as cheap and
    /// with no register of its own. A constant (made where it's used) and
    /// zeroed memory are still worth it.
    fn forget_loaded(&mut self) {
        let gone: Vec<MemKey> = self
            .mem
            .iter()
            .filter(|(k, v)| !matches!(k, MemKey::Zero { .. }) && !self.consts.contains_key(&self.find(**v)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            let old = self.mem.remove(&k);
            self.undo.push(Undo::Mem(k, old));
        }
    }

    /// Forgets the stable memory `keep` says no to, and every lookup (a
    /// write may change a map).
    fn forget_stable(&mut self, keep: impl Fn(&Alias, &MemKey) -> bool) {
        let gone: Vec<MemKey> = self.stable.keys().filter(|k| !keep(&self.alias, k)).cloned().collect();
        for k in gone {
            let old = self.stable.remove(&k);
            self.undo.push(Undo::Stable(k, old));
        }
        for (k, v) in self.finds.drain().collect::<Vec<_>>() {
            self.undo.push(Undo::Find(k, Some(v)));
        }
    }

    /// `v` as a lookup's argument.
    fn arg(&self, v: Value) -> Arg {
        let v = self.find(v);
        match self.consts.get(&v) {
            Some(&c) => Arg::Const(c),
            None => Arg::Value(self.same.get(&v).copied().unwrap_or_else(|| self.addr(v))),
        }
    }

    /// `v`, or the first value that's the same address.
    fn addr(&self, v: Value) -> Value {
        self.addrs.get(&v).copied().unwrap_or(v)
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
                Undo::Stable(k, Some(v)) => {
                    self.stable.insert(k, v);
                }
                Undo::Stable(k, None) => {
                    self.stable.remove(&k);
                }
                Undo::Find(k, Some(v)) => {
                    self.finds.insert(k, v);
                }
                Undo::Find(k, None) => {
                    self.finds.remove(&k);
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
        stable: HashMap::new(),
        same: HashMap::new(),
        consts: f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match (&i.kind, i.results.as_slice()) {
                (InstKind::Iconst(c), [r]) => Some((*r, *c)),
                _ => None,
            })
            .collect(),
        finds: HashMap::new(),
        addrs: HashMap::new(),
        split: HashMap::new(),
        loads: HashMap::new(),
        loops: Vec::new(),
        within: vec![Vec::new(); f.blocks.len()],
        fresh: Vec::new(),
        base: f.types.len() as u32,
        undo: Vec::new(),
    };
    let mut first: HashMap<(Root, i64), Value> = HashMap::new();
    for r in f.blocks.iter().flat_map(|b| &b.insts).flat_map(|i| &i.results) {
        if let (root @ (Root::Slot(_) | Root::Data), Some(off)) = g.alias.root(*r) {
            g.addrs.insert(*r, *first.entry((root, off)).or_insert(*r));
        }
    }
    // What each loop may write; by its head, when one block before it
    // leads there.
    let mut heads: HashMap<usize, usize> = HashMap::new();
    for l in cfg.loops() {
        let outside: Vec<usize> = cfg.preds[l.header].iter().copied().filter(|&p| !l.inside[p]).collect();
        if outside == [cfg.idom[l.header].unwrap_or(usize::MAX)] {
            heads.insert(l.header, g.loops.len());
        }
        for &b in &l.blocks {
            g.within[b].push(g.loops.len());
        }
        let mut w = LoopWrites { writes: Vec::new(), clobbers: false };
        for &b in &l.blocks {
            for inst in &f.blocks[b].insts {
                match role(&inst.kind) {
                    Role::Write(a) => w.writes.push(a),
                    Role::Clobber => {
                        w.clobbers = true;
                        if let InstKind::FreeBuf { header, off, .. } = inst.kind {
                            w.writes.push(Access { ptr: header, off: i64::from(off), size: 24 });
                        }
                    }
                    _ => {}
                }
            }
            if matches!(f.blocks[b].term, Some(Term::Suspend { .. })) {
                w.clobbers = true;
            }
        }
        g.loops.push(w);
    }
    // (block, the undo mark to go back to once its children are done).
    let mut stack: Vec<(usize, Option<usize>)> = vec![(0, None)];
    let mut dead: Vec<Vec<bool>> = f.blocks.iter().map(|b| vec![false; b.insts.len()]).collect();
    // Loads of memory known to be zero, which become constants.
    let mut zero = dead.clone();
    while let Some((b, done)) = stack.pop() {
        if let Some(mark) = done {
            g.undo_to(mark);
            continue;
        }
        stack.push((b, Some(g.undo.len())));
        // Memory is known only through a block's one way in.
        let only = g.cfg.preds[b].len() == 1 && g.cfg.idom[b] == Some(g.cfg.preds[b][0]);
        if let Some(&l) = heads.get(&b) {
            let w = std::mem::replace(&mut g.loops[l], LoopWrites { writes: Vec::new(), clobbers: false });
            g.forget(|alias, k| w.keeps(alias, k));
            g.forget_stable(|alias, k| w.keeps(alias, k));
            g.loops[l] = w;
        } else if !only {
            g.forget(|_, _| false);
            g.forget_stable(|_, _| false);
        }
        visit(&mut g, f, b, &mut dead[b], &mut zero[b]);
        for &c in g.cfg.children[b].iter().rev() {
            stack.push((c, None));
        }
    }
    // Replace what went everywhere (landing pads too), and drop it.
    f.types.extend(g.fresh.iter().copied());
    let repl: Vec<Value> = (0..f.types.len() as u32).map(|v| g.find(Value(v))).collect();
    for (b, ((block, dead), zero)) in f.blocks.iter_mut().zip(&dead).zip(&zero).enumerate() {
        for (inst, &z) in block.insts.iter_mut().zip(zero) {
            if z {
                let floats = matches!(f.types[inst.results[0].0 as usize], VTy::F32 | VTy::F64);
                inst.kind = if floats { InstKind::Fconst(0) } else { InstKind::Iconst(0) };
            }
        }
        let old = std::mem::take(&mut block.insts);
        for (i, inst) in old.into_iter().enumerate() {
            if dead[i] {
                continue;
            }
            match (g.split.get(&(b, i)), &inst.kind) {
                (Some(&(len, buf)), &InstKind::Elem { header, off, index, scale }) => {
                    if let Some(&(buf, count)) = g.loads.get(&(b, i)) {
                        for (at, mem_off, r) in [(header, off, buf), (header, off + 8, count)] {
                            block.insts.push(crate::heddle::Inst {
                                kind: InstKind::Load { ptr: at, off: mem_off, mem: Mem::W64 },
                                results: vec![r],
                                unwind: None,
                                ..inst.clone()
                            });
                        }
                    }
                    if let Some(len) = len {
                        block.insts.push(crate::heddle::Inst {
                            kind: InstKind::Bounds { index, len },
                            results: Vec::new(),
                            ..inst.clone()
                        });
                    }
                    block.insts.push(crate::heddle::Inst {
                        kind: InstKind::Index { base: buf, index, scale },
                        unwind: None,
                        ..inst
                    });
                }
                _ => block.insts.push(inst),
            }
        }
        for inst in &mut block.insts {
            crate::heddle::rewrite(&mut inst.kind, |v| repl[v.0 as usize]);
        }
        if let Some(t) = &mut block.term {
            super::rewrite_term(t, |v| repl[v.0 as usize]);
        }
    }
}

/// The buffer and the length of the list whose header is at `header +
/// off`, for the `elem` that's instruction `i` of block `b`: known, or new
/// values for loads put just before it (see `loads`), when it's in loops
/// none of which can change them and the header is in the frame (where
/// LICM takes the loads out of them all, to run once).
fn elem_parts(g: &mut Gvn, b: usize, i: usize, header: Value, off: i32) -> Option<(Value, Value)> {
    let at = g.addr(header);
    let keys = (MemKey::Load { ptr: at, off, mem: Mem::W64 }, MemKey::Load { ptr: at, off: off + 8, mem: Mem::W64 });
    if let (Some(&buf), Some(&len)) = (g.mem.get(&keys.0), g.mem.get(&keys.1)) {
        return Some((buf, len));
    }
    let framed = matches!(g.alias.root(header), (Root::Slot(_), Some(_)));
    let kept = |l: &usize| g.loops[*l].keeps(&g.alias, &keys.0) && g.loops[*l].keeps(&g.alias, &keys.1);
    if !framed || g.within[b].is_empty() || !g.within[b].iter().all(kept) {
        return None;
    }
    let (buf, len) = (g.fresh(VTy::Ptr), g.fresh(VTy::Int));
    g.remember(keys.0, buf);
    g.remember(keys.1, len);
    g.loads.insert((b, i), (buf, len));
    Some((buf, len))
}

/// Numbers block `b`'s instructions, marking those that go in `dead`.
fn visit(g: &mut Gvn, f: &Func, b: usize, dead: &mut [bool], zero: &mut [bool]) {
    for (i, inst) in f.blocks[b].insts.iter().enumerate() {
        let mut kind = inst.kind.clone();
        crate::heddle::rewrite(&mut kind, |v| g.find(v));
        let role = match role(&kind) {
            Role::Read(key) => Role::Read(key.named(|v| g.addr(v))),
            other => other,
        };
        match role {
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
                (_, [r]) => {
                    match g.stable.get(&key).copied() {
                        Some(old) if f.ty(old) == f.ty(*r) => {
                            g.same.insert(*r, old);
                        }
                        _ => {
                            let old = g.stable.insert(key.clone(), *r);
                            g.undo.push(Undo::Stable(key.clone(), old));
                        }
                    }
                    // From memory a `zero` wrote: it's 0.
                    if let MemKey::Load { .. } = key
                        && g.zeroed(key.access())
                    {
                        zero[i] = true;
                    }
                    g.remember(key, *r);
                }
                _ => {}
            },
            // Zeroing what's known to be zero already does nothing.
            Role::Write(access) if matches!(kind, InstKind::Zero { .. }) && g.zeroed(access) => dead[i] = true,
            // Nor does freeing the buffer of a list whose capacity is zero.
            Role::Clobber
                if matches!(kind, InstKind::FreeBuf { header, off, .. }
                    if g.zeroed(Access { ptr: header, off: i64::from(off) + 16, size: 8 })) =>
            {
                dead[i] = true;
            }
            Role::Write(access) => {
                // What's left zero around the bytes written, when they're
                // at known places in a zeroed stretch.
                let kept: Vec<MemKey> = g.mem.keys().filter_map(|k| g.zero_left(k, access)).flatten().collect();
                g.forget(|alias, k| !alias.may_alias(access, k.access()));
                g.forget_stable(|alias, k| !alias.may_alias(access, k.access()));
                for k in kept {
                    let at = k.access().ptr;
                    g.remember(k, at);
                }
                match kind {
                    InstKind::Store { ptr, off, mem, val } => {
                        g.remember(MemKey::Load { ptr: g.addr(ptr), off, mem }, val)
                    }
                    InstKind::Zero { dst, size } => g.remember(MemKey::Zero { ptr: g.addr(dst), off: 0, size }, dst),
                    _ => {}
                }
            }
            Role::Clobber => {
                g.forget(|_, _| false);
                g.forget_stable(|_, _| false);
            }
            // An element of a list whose buffer and length are known, or
            // can be loaded where a loop can't change them.
            Role::Other
                if let InstKind::Elem { header, off, index, .. } = kind
                    && let Some((buf, len)) = elem_parts(g, b, i, header, off) =>
            {
                let key = (InstKind::Bounds { index, len }, Vec::new());
                let checked = g.exprs.contains_key(&key);
                if !checked {
                    g.exprs.insert(key.clone(), Vec::new());
                    g.undo.push(Undo::Expr(key));
                }
                g.split.insert((b, i), ((!checked).then_some(len), buf));
            }
            Role::Other
                if let InstKind::Native { name, imm, args } = &kind
                    && LOOKUPS.contains(name) =>
            {
                let key: FindKey = (name, *imm, args.iter().map(|&a| g.arg(a)).collect());
                match g.finds.get(&key) {
                    Some(old) if old.len() == inst.results.len() => {
                        for (r, o) in inst.results.iter().zip(old.clone()) {
                            g.repl[r.0 as usize] = o;
                        }
                        dead[i] = true;
                    }
                    _ => {
                        g.finds.insert(key.clone(), inst.results.clone());
                        g.undo.push(Undo::Find(key, None));
                        g.forget_loaded();
                    }
                }
            }
            Role::Other if matches!(kind, InstKind::Native { .. }) => g.forget_loaded(),
            Role::Other => {}
        }
    }
}
