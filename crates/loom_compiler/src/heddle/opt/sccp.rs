//! Sparse conditional constant propagation (pass 4 of design/vm.md §7):
//! values that are constant on every path that runs are worked out (see
//! `fold`, which goes through the integer oracle), branches on them go one
//! way, and the blocks no path runs then go.
//!
//! Wegman and Zadeck's algorithm, over block parameters: a value starts
//! unknown (`Top`), and only moves down, to a constant or to varying; a
//! parameter is what every edge that runs passes it, and only edges that a
//! branch can take run. Landing pads run when a block that unwinds to them
//! does.
//!
//! Then a few identities: `x + 0`, `x * 1`, `x | 0`, `x << 0`, a cast to a
//! type that holds every value of the one it's from, `not not x`, and an
//! integer compared with itself, are what they are without computing.

use std::collections::HashSet;

use super::fold::fold;
use crate::heddle::{BitOp, Cmp, Edge, Func, Inst, InstKind, IntOp, Module, Term, VTy, Value};

pub fn sccp(module: &mut Module) {
    for f in &mut module.funcs {
        sccp_func(f);
        simplify(f);
        super::tidy::tidy(f);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lat {
    Top,
    Const(u64),
    Bottom,
}

fn meet(a: Lat, b: Lat) -> Lat {
    match (a, b) {
        (Lat::Top, x) | (x, Lat::Top) => x,
        (Lat::Const(x), Lat::Const(y)) if x == y => Lat::Const(x),
        _ => Lat::Bottom,
    }
}

/// Where a value is read: an instruction of a block, or its terminator.
#[derive(Clone, Copy)]
struct User {
    block: usize,
    inst: Option<usize>,
}

struct Sccp<'f> {
    f: &'f Func,
    lat: Vec<Lat>,
    users: Vec<Vec<User>>,
    run: Vec<bool>,
    edges: HashSet<(usize, usize)>,
    blocks: Vec<usize>,
    values: Vec<Value>,
}

impl Sccp<'_> {
    fn set(&mut self, v: Value, l: Lat) {
        let old = self.lat[v.0 as usize];
        let new = meet(old, l);
        if new != old {
            self.lat[v.0 as usize] = new;
            self.values.push(v);
        }
    }

    fn reach(&mut self, b: usize) {
        if !self.run[b] {
            self.run[b] = true;
            self.blocks.push(b);
        }
    }

    fn inst(&mut self, b: usize, i: usize) {
        let inst = &self.f.blocks[b].insts[i];
        let lat = &self.lat;
        let mut top = false;
        let operand = |v: Value| match lat[v.0 as usize] {
            Lat::Const(c) => Some(c),
            _ => None,
        };
        for u in crate::heddle::inst_uses(&inst.kind) {
            top |= lat[u.0 as usize] == Lat::Top;
        }
        let results = inst.results.clone();
        let out = match (&results[..], top) {
            // Not known yet: maybe a constant, once the operands are.
            ([_], true) if foldable(&inst.kind) => Lat::Top,
            ([_], _) => fold(&inst.kind, operand).map_or(Lat::Bottom, Lat::Const),
            _ => Lat::Bottom,
        };
        for r in results {
            self.set(r, out);
        }
    }

    fn term(&mut self, b: usize) {
        let Some(term) = &self.f.blocks[b].term else { return };
        let taken: Vec<usize> = match term {
            Term::Br { cond, .. } => match self.lat[cond.0 as usize] {
                Lat::Top => Vec::new(),
                Lat::Const(c) => vec![if c != 0 { 0 } else { 1 }],
                Lat::Bottom => vec![0, 1],
            },
            t => (0..t.edges().len()).collect(),
        };
        let edges: Vec<Edge> = term.edges().into_iter().cloned().collect();
        for k in taken {
            let e = &edges[k];
            let t = e.block.0 as usize;
            self.edges.insert((b, k));
            for (&p, &a) in self.f.blocks[t].params.iter().zip(&e.args) {
                let l = self.lat[a.0 as usize];
                self.set(p, l);
            }
            self.reach(t);
        }
    }
}

/// Whether `k` is a constant: known before its operands (it has none).
fn is_constant(k: &InstKind) -> bool {
    matches!(k, InstKind::Iconst(_) | InstKind::Fconst(_))
}

/// Whether `fold` may work `k` out, once its operands are known.
fn foldable(k: &InstKind) -> bool {
    use InstKind as K;
    matches!(
        k,
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
            | K::FloatCast { .. }
    )
}

fn sccp_func(f: &mut Func) {
    let n = f.types.len();
    let mut users: Vec<Vec<User>> = vec![Vec::new(); n];
    for (b, block) in f.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            for u in crate::heddle::inst_uses(&inst.kind) {
                users[u.0 as usize].push(User { block: b, inst: Some(i) });
            }
        }
        for u in block.term.iter().flat_map(super::term_uses) {
            users[u.0 as usize].push(User { block: b, inst: None });
        }
    }
    let mut s = Sccp {
        f,
        lat: vec![Lat::Top; n],
        users,
        run: vec![false; f.blocks.len()],
        edges: HashSet::new(),
        blocks: Vec::new(),
        values: Vec::new(),
    };
    for &p in f.params() {
        s.lat[p.0 as usize] = Lat::Bottom;
    }
    s.reach(0);
    loop {
        if let Some(b) = s.blocks.pop() {
            for pad in f.blocks[b].pads() {
                s.reach(pad.0 as usize);
            }
            for i in 0..f.blocks[b].insts.len() {
                s.inst(b, i);
            }
            s.term(b);
        } else if let Some(v) = s.values.pop() {
            for k in 0..s.users[v.0 as usize].len() {
                let u = s.users[v.0 as usize][k];
                if !s.run[u.block] {
                    continue;
                }
                match u.inst {
                    Some(i) => s.inst(u.block, i),
                    None => s.term(u.block),
                }
            }
        } else {
            break;
        }
    }
    let (lat, run) = (s.lat, s.run);
    // Constants made where they're known, and branches that go one way.
    let mut subst: Vec<Value> = (0..n as u32).map(Value).collect();
    for (b, block) in f.blocks.iter_mut().enumerate() {
        if !run[b] {
            continue;
        }
        let mut made = Vec::new();
        if b != 0 {
            for &p in &block.params {
                if let Lat::Const(c) = lat[p.0 as usize] {
                    let ty = f.types[p.0 as usize];
                    f.types.push(ty);
                    let v = Value(f.types.len() as u32 - 1);
                    subst[p.0 as usize] = v;
                    made.push(constant(ty, c, v, block.term_span));
                }
            }
        }
        for inst in &mut block.insts {
            if let [r] = inst.results.as_slice()
                && let Lat::Const(c) = lat[r.0 as usize]
                && !is_constant(&inst.kind)
            {
                let ty = f.types[r.0 as usize];
                inst.kind = constant(ty, c, *r, None).kind;
                inst.unwind = None;
            }
        }
        block.insts.splice(0..0, made);
        if let Some(Term::Br { cond, then, else_ }) = &block.term
            && let Lat::Const(c) = lat[cond.0 as usize]
        {
            let e = if c != 0 { then.clone() } else { else_.clone() };
            block.term = Some(Term::Jump(e));
        }
    }
    for block in &mut f.blocks {
        for inst in &mut block.insts {
            crate::heddle::rewrite(&mut inst.kind, |v| subst[v.0 as usize]);
        }
        if let Some(t) = &mut block.term {
            super::rewrite_term(t, |v| subst[v.0 as usize]);
        }
    }
}

/// An instruction making constant `bits` as `v`, of type `ty`.
fn constant(ty: VTy, bits: u64, v: Value, span: Option<loom_syntax::source::Span>) -> Inst {
    let kind = match ty {
        VTy::F32 | VTy::F64 => InstKind::Fconst(bits),
        VTy::Int | VTy::Ptr => InstKind::Iconst(bits),
    };
    Inst { kind, results: vec![v], span, unwind: None, site: None }
}

/// The identities (see the module's documentation): each result that's
/// one of its operands is replaced by it, and the instruction goes; an
/// integer compared with itself becomes a constant.
fn simplify(f: &mut Func) {
    let mut consts: Vec<Option<u64>> = vec![None; f.types.len()];
    let mut nots: Vec<Option<Value>> = vec![None; f.types.len()];
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        match (&inst.kind, inst.results.as_slice()) {
            (InstKind::Iconst(c), [r]) => consts[r.0 as usize] = Some(*c),
            (InstKind::Not(a), [r]) => nots[r.0 as usize] = Some(*a),
            _ => {}
        }
    }
    let k = |v: Value| consts[v.0 as usize];
    let mut subst: Vec<Value> = (0..f.types.len() as u32).map(Value).collect();
    let mut changed = false;
    for block in &mut f.blocks {
        block.insts.retain_mut(|inst| {
            let [r] = inst.results.as_slice() else { return true };
            use InstKind as K;
            let same = match &inst.kind {
                K::Arith { op: IntOp::Add | IntOp::Sub, a, b, .. } if k(*b) == Some(0) => Some(*a),
                K::Arith { op: IntOp::Add, a, b, .. } if k(*a) == Some(0) => Some(*b),
                K::Arith { op: IntOp::Mul, a, b, .. } if k(*b) == Some(1) => Some(*a),
                K::Arith { op: IntOp::Mul, a, b, .. } if k(*a) == Some(1) => Some(*b),
                K::Div { a, b, .. } if k(*b) == Some(1) => Some(*a),
                K::Bit { op: BitOp::Or | BitOp::Xor, a, b } if k(*b) == Some(0) => Some(*a),
                K::Bit { op: BitOp::Or | BitOp::Xor, a, b } if k(*a) == Some(0) => Some(*b),
                K::Bit { op: BitOp::And, a, b } if k(*b) == Some(u64::MAX) => Some(*a),
                K::Bit { op: BitOp::And, a, b } if k(*a) == Some(u64::MAX) => Some(*b),
                K::Shl { a, b, .. } | K::Shr { a, b, .. } if k(*b) == Some(0) => Some(*a),
                K::IntCast { from, to, a, .. } if from.widens_to(*to) => Some(*a),
                K::Not(a) => nots[a.0 as usize],
                _ => None,
            };
            if let Some(x) = same {
                subst[r.0 as usize] = x;
                changed = true;
                return false;
            }
            if let K::ICmp { cmp, a, b, .. } = &inst.kind
                && a == b
            {
                inst.kind = K::Iconst(u64::from(matches!(cmp, Cmp::Eq | Cmp::Le | Cmp::Ge)));
                inst.unwind = None;
            }
            true
        });
    }
    if !changed {
        return;
    }
    let find = |mut v: Value| {
        while subst[v.0 as usize] != v {
            v = subst[v.0 as usize];
        }
        v
    };
    for block in &mut f.blocks {
        for inst in &mut block.insts {
            crate::heddle::rewrite(&mut inst.kind, find);
        }
        if let Some(t) = &mut block.term {
            super::rewrite_term(t, find);
        }
    }
}
