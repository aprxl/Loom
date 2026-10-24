//! Range analysis (pass 7 of design/vm.md §7): an interval for every
//! integer value, and the checks it shows can't fire go: an overflow check
//! whose result always fits (the arithmetic wraps, which then never
//! happens), a cast's that always fits, a bounds check whose index is
//! always below the length, and a comparison whose answer is known.
//!
//! An interval is of the value's 64-bit pattern read signed or unsigned:
//! one with a negative end is read signed, one past `i64::MAX` unsigned,
//! and one in between either way (`view` reads it one way). Every value
//! starts unknown, and the walk goes over the blocks in reverse postorder
//! until nothing changes: an instruction's interval comes from its
//! operands', a block parameter's joins what each edge that runs passes
//! it, and after a few changes a loop head's parameter widens to
//! everything on the side it's growing. What the program has tested
//! narrows a value where the test dominates: in `while i < n { … }`'s body,
//! `i` is below `n`'s highest value, so `i + 1` fits whatever `n` is, and an
//! index the branch compared with the length itself is in bounds. So does a
//! bounds check that has passed, from there on: after `bounds j, 8`, `j` is
//! below 8, and so is an `i` that a branch showed below `j`.
//!
//! Widening sends a counter that stops below a bound to everything above
//! it; a few rounds of working each value out again, keeping what's
//! narrower, bring it back.

use super::cfg::Cfg;
use crate::heddle::{BitOp, Cmp, Func, InstKind, IntOp, Mem, Module, Policy, Term, Value};
use crate::ty::IntTy;

pub fn ranges(module: &mut Module) {
    for f in &mut module.funcs {
        ranges_func(f);
        super::tidy::tidy(f);
    }
}

/// An interval of integers, both ends included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Iv {
    lo: i128,
    hi: i128,
}

const I64: Iv = Iv { lo: i64::MIN as i128, hi: i64::MAX as i128 };
const U64: Iv = Iv { lo: 0, hi: u64::MAX as i128 };
/// Nothing known: any pattern, read either way.
const ANY: Iv = Iv { lo: i64::MIN as i128, hi: u64::MAX as i128 };

impl Iv {
    fn of(lo: i128, hi: i128) -> Iv {
        Iv { lo, hi }
    }

    fn one(v: i128) -> Iv {
        Iv { lo: v, hi: v }
    }

    fn ty(t: IntTy) -> Iv {
        Iv { lo: t.min(), hi: t.max() }
    }

    fn within(self, o: Iv) -> bool {
        o.lo <= self.lo && self.hi <= o.hi
    }

    fn meet(self, o: Iv) -> Iv {
        Iv { lo: self.lo.max(o.lo), hi: self.hi.min(o.hi) }
    }

    fn join(self, o: Iv) -> Iv {
        Iv { lo: self.lo.min(o.lo), hi: self.hi.max(o.hi) }
    }

    fn empty(self) -> bool {
        self.lo > self.hi
    }

    /// The pattern read signed or unsigned: itself when it's already read
    /// that way, everything otherwise.
    fn view(self, signed: bool) -> Iv {
        let whole = if signed { I64 } else { U64 };
        if self.within(whole) { self } else { whole }
    }

    /// Read as a value of `t` (which it is, by the IR's typing).
    fn as_ty(self, t: IntTy) -> Iv {
        let r = self.view(t.signed).meet(Iv::ty(t));
        if r.empty() { Iv::ty(t) } else { r }
    }
}

/// A test that holds where it dominates: `a cmp b`.
#[derive(Clone, Copy, Debug)]
struct Fact {
    a: Value,
    cmp: Cmp,
    unsigned: bool,
    b: Value,
    /// Where in its block it starts to hold: the first instruction after the
    /// check that made it (0 for a branch's, which holds all through).
    from: usize,
}

fn negate(c: Cmp) -> Cmp {
    match c {
        Cmp::Eq => Cmp::Ne,
        Cmp::Ne => Cmp::Eq,
        Cmp::Lt => Cmp::Ge,
        Cmp::Le => Cmp::Gt,
        Cmp::Gt => Cmp::Le,
        Cmp::Ge => Cmp::Lt,
    }
}

/// `a cmp b` as `b cmp' a`.
fn flip(c: Cmp) -> Cmp {
    match c {
        Cmp::Eq | Cmp::Ne => c,
        Cmp::Lt => Cmp::Gt,
        Cmp::Le => Cmp::Ge,
        Cmp::Gt => Cmp::Lt,
        Cmp::Ge => Cmp::Le,
    }
}

struct Ranges<'f> {
    f: &'f Func,
    cfg: Cfg,
    /// Each value's interval, as computed where it's made (`None`: not yet).
    iv: Vec<Option<Iv>>,
    /// The constant each value is, when it is one.
    consts: Vec<Option<u64>>,
    /// The tests each block adds to its dominator's (those that hold on its
    /// one way in).
    facts: Vec<Vec<Fact>>,
    /// How many times each value's interval has grown (to widen).
    grown: Vec<u8>,
    /// Whether any value was widened (and may be narrowed again).
    widened: bool,
    /// Each reached block's chain of dominators, itself first.
    chains: Vec<Vec<usize>>,
}

impl Ranges<'_> {
    /// `v`'s interval where it's made: a constant read signed.
    fn base(&self, v: Value) -> Iv {
        match self.consts[v.0 as usize] {
            Some(c) => Iv::one(i128::from(c as i64)),
            None => self.iv[v.0 as usize].unwrap_or(ANY),
        }
    }

    /// `v` read signed or unsigned: a constant as that.
    fn read(&self, v: Value, signed: bool) -> Iv {
        match self.consts[v.0 as usize] {
            Some(c) if !signed => Iv::one(i128::from(c)),
            _ => self.base(v).view(signed),
        }
    }

    /// The tests that hold at instruction `pos` of block `b` (past its end
    /// for the terminator): the dominators' all, and the block's own from
    /// where they start to.
    fn facts_at(&self, b: usize, pos: usize) -> impl Iterator<Item = &Fact> {
        self.chains[b]
            .iter()
            .copied()
            .enumerate()
            .flat_map(move |(n, b)| self.facts[b].iter().filter(move |f| n > 0 || f.from <= pos))
    }

    /// `v` at instruction `pos` of block `b`, read signed or unsigned,
    /// narrowed by the tests that hold there.
    fn at(&self, v: Value, b: usize, pos: usize, signed: bool) -> Iv {
        self.at_depth(v, b, pos, signed, 1)
    }

    /// [`at`](Self::at), with `depth` more times a test's other side may be
    /// narrowed the same way (a value below one that a check has just
    /// shown below the length is below the length too).
    fn at_depth(&self, v: Value, b: usize, pos: usize, signed: bool, depth: u8) -> Iv {
        let mut r = self.read(v, signed);
        for fact in self.facts_at(b, pos) {
            let (cmp, other) = if fact.a == v {
                (fact.cmp, fact.b)
            } else if fact.b == v {
                (flip(fact.cmp), fact.a)
            } else {
                continue;
            };
            let s = !fact.unsigned;
            let mut x = r.view(s);
            let o = if depth > 0 { self.at_depth(other, b, pos, s, depth - 1) } else { self.read(other, s) };
            match cmp {
                Cmp::Lt => x.hi = x.hi.min(o.hi - 1),
                Cmp::Le => x.hi = x.hi.min(o.hi),
                Cmp::Gt => x.lo = x.lo.max(o.lo + 1),
                Cmp::Ge => x.lo = x.lo.max(o.lo),
                Cmp::Eq => x = x.meet(o),
                Cmp::Ne if o.lo == o.hi && x.lo == o.lo => x.lo += 1,
                Cmp::Ne if o.lo == o.hi && x.hi == o.lo => x.hi -= 1,
                Cmp::Ne => {}
            }
            // An empty one is a path that never runs: keep what's known.
            if !x.empty() {
                r = x.view(signed);
            }
        }
        r
    }

    /// Whether a test holds in block `b` (`Some(false)`: its opposite does).
    fn decide(&self, b: usize, pos: usize, cmp: Cmp, unsigned: bool, a: Value, c: Value) -> Option<bool> {
        let s = !unsigned;
        // The same test, or its opposite, already made.
        for fact in self.facts_at(b, pos) {
            if fact.unsigned != unsigned && !(self.nonneg(a, b, pos) && self.nonneg(c, b, pos)) {
                continue;
            }
            let k = if (fact.a, fact.b) == (a, c) {
                fact.cmp
            } else if (fact.a, fact.b) == (c, a) {
                flip(fact.cmp)
            } else {
                continue;
            };
            if implies(k, cmp) {
                return Some(true);
            }
            if implies(k, negate(cmp)) {
                return Some(false);
            }
        }
        let (x, y) = (self.at(a, b, pos, s), self.at(c, b, pos, s));
        let holds = |cmp: Cmp| match cmp {
            Cmp::Lt => x.hi < y.lo,
            Cmp::Le => x.hi <= y.lo,
            Cmp::Gt => x.lo > y.hi,
            Cmp::Ge => x.lo >= y.hi,
            Cmp::Eq => x.lo == x.hi && y.lo == y.hi && x.lo == y.lo,
            Cmp::Ne => x.hi < y.lo || y.hi < x.lo,
        };
        if holds(cmp) {
            Some(true)
        } else if holds(negate(cmp)) {
            Some(false)
        } else {
            None
        }
    }

    /// Whether `v` is never negative in block `b`, read signed.
    fn nonneg(&self, v: Value, b: usize, pos: usize) -> bool {
        self.at(v, b, pos, true).lo >= 0
    }

    /// The interval of what instruction `i` of block `b` makes (one result
    /// that's an integer), and whether its check, if it has one, can't fire.
    fn eval(&self, b: usize, pos: usize, k: &InstKind) -> (Iv, bool) {
        use InstKind as K;
        match *k {
            K::Arith { op, policy, ty, a, b: c } => {
                let (x, y) = (self.at(a, b, pos, ty.signed).as_ty(ty), self.at(c, b, pos, ty.signed).as_ty(ty));
                let exact = match op {
                    IntOp::Add => Some(Iv::of(x.lo + y.lo, x.hi + y.hi)),
                    IntOp::Sub => Some(Iv::of(x.lo - y.hi, x.hi - y.lo)),
                    IntOp::Mul => {
                        let ps = [
                            x.lo.checked_mul(y.lo),
                            x.lo.checked_mul(y.hi),
                            x.hi.checked_mul(y.lo),
                            x.hi.checked_mul(y.hi),
                        ];
                        ps.iter()
                            .try_fold(None::<Iv>, |acc, p| {
                                let p = Iv::one((*p)?);
                                Some(Some(acc.map_or(p, |a| a.join(p))))
                            })
                            .flatten()
                    }
                };
                let fits = exact.filter(|e| e.within(Iv::ty(ty)));
                match (policy, fits) {
                    (_, Some(e)) => (e, true),
                    (Policy::Sat, None) => {
                        let e = exact.unwrap_or(Iv::ty(ty));
                        (Iv::of(ty.saturate(e.lo), ty.saturate(e.hi)), false)
                    }
                    _ => (Iv::ty(ty), false),
                }
            }
            K::IntCast { policy, to, from, a } => {
                let x = self.at(a, b, pos, from.signed).as_ty(from);
                if x.within(Iv::ty(to)) {
                    (x, true)
                } else if policy == Policy::Sat {
                    (Iv::of(to.saturate(x.lo), to.saturate(x.hi)), false)
                } else if policy == Policy::Trap {
                    (x.meet(Iv::ty(to)), false)
                } else {
                    (Iv::ty(to), false)
                }
            }
            K::Div { ty, a, b: c } if !ty.signed => {
                let (x, y) = (self.at(a, b, pos, false).as_ty(ty), self.at(c, b, pos, false).as_ty(ty));
                if y.lo >= 1 { (Iv::of(x.lo / y.hi, x.hi / y.lo), false) } else { (Iv::ty(ty), false) }
            }
            K::Rem { ty, a, b: c } => {
                let (x, y) = (self.at(a, b, pos, ty.signed).as_ty(ty), self.at(c, b, pos, ty.signed).as_ty(ty));
                if x.lo >= 0 && y.lo >= 1 { (Iv::of(0, x.hi.min(y.hi - 1)), false) } else { (Iv::ty(ty), false) }
            }
            K::Shr { ty, a, b: c } => {
                let (x, y) = (self.at(a, b, pos, ty.signed).as_ty(ty), self.at(c, b, pos, false));
                if x.lo >= 0 && y.lo == y.hi && (0..i128::from(ty.bits)).contains(&y.lo) {
                    (Iv::of(x.lo >> y.lo, x.hi >> y.lo), false)
                } else {
                    (Iv::ty(ty), false)
                }
            }
            K::Bit { op, a, b: c } => {
                let (x, y) = (self.at(a, b, pos, true), self.at(c, b, pos, true));
                match op {
                    BitOp::And if x.lo >= 0 && y.lo >= 0 => (Iv::of(0, x.hi.min(y.hi)), false),
                    BitOp::And if x.lo >= 0 => (Iv::of(0, x.hi), false),
                    BitOp::And if y.lo >= 0 => (Iv::of(0, y.hi), false),
                    BitOp::Or | BitOp::Xor if x.lo >= 0 && y.lo >= 0 => {
                        let top = x.hi.max(y.hi);
                        (Iv::of(0, (1i128 << (128 - top.leading_zeros())) - 1), false)
                    }
                    _ => (ANY, false),
                }
            }
            K::ICmp { .. } | K::FCmp { .. } | K::Not(_) => (Iv::of(0, 1), false),
            K::Load { mem, .. } => (
                match mem {
                    Mem::I8 => Iv::of(-128, 127),
                    Mem::U8 => Iv::of(0, 255),
                    Mem::I16 => Iv::of(-32768, 32767),
                    Mem::U16 => Iv::of(0, 65535),
                    Mem::I32 => Iv::of(i128::from(i32::MIN), i128::from(i32::MAX)),
                    Mem::U32 => Iv::of(0, i128::from(u32::MAX)),
                    Mem::W64 => ANY,
                },
                false,
            ),
            K::IntToChar { .. } => (Iv::of(0, 0x10FFFF), false),
            _ => (ANY, false),
        }
    }

    /// Works out every interval.
    fn solve(&mut self) {
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.cfg.rpo.len() {
                let b = self.cfg.rpo[i];
                // Parameters: what every edge that runs passes.
                for (n, &p) in self.f.blocks[b].params.iter().enumerate() {
                    let mut joined: Option<Iv> = None;
                    for &q in self.cfg.preds[b].iter().filter(|&&q| self.cfg.reached(q)) {
                        let Some(t) = &self.f.blocks[q].term else { continue };
                        for e in t.edges().filter(|e| e.block.0 as usize == b) {
                            let a = e.args[n];
                            if self.consts[a.0 as usize].is_none() && self.iv[a.0 as usize].is_none() {
                                continue;
                            }
                            let x = self.at_edge(a, q);
                            joined = Some(joined.map_or(x, |j| j.join(x)));
                        }
                    }
                    if let Some(mut new) = joined
                        && self.iv[p.0 as usize] != Some(new)
                    {
                        if let Some(old) = self.iv[p.0 as usize] {
                            new = new.join(old);
                            if new != old {
                                let g = &mut self.grown[p.0 as usize];
                                *g = g.saturating_add(1);
                                if *g > 2 {
                                    self.widened = true;
                                    if new.hi > old.hi {
                                        new.hi = ANY.hi;
                                    }
                                    if new.lo < old.lo {
                                        new.lo = ANY.lo;
                                    }
                                }
                            }
                        }
                        if self.iv[p.0 as usize] != Some(new) {
                            self.iv[p.0 as usize] = Some(new);
                            changed = true;
                        }
                    }
                }
                for (i, inst) in self.f.blocks[b].insts.iter().enumerate() {
                    if let [r] = inst.results.as_slice()
                        && self.consts[r.0 as usize].is_none()
                    {
                        let (new, _) = self.eval(b, i, &inst.kind);
                        if self.iv[r.0 as usize] != Some(new) {
                            self.iv[r.0 as usize] = Some(new);
                            changed = true;
                        }
                    }
                }
            }
        }
        // Widening overshoots (a counter that stops where a test says it
        // does goes to everything above). What every value is now is a bound
        // that holds, so working each value out again from the others, and
        // keeping what's narrower than before, only tightens: a few rounds.
        for _ in 0..if self.widened { 3 } else { 0 } {
            let mut changed = false;
            for i in 0..self.cfg.rpo.len() {
                let b = self.cfg.rpo[i];
                for (n, &p) in self.f.blocks[b].params.iter().enumerate() {
                    let mut joined: Option<Iv> = None;
                    for &q in self.cfg.preds[b].iter().filter(|&&q| self.cfg.reached(q)) {
                        let Some(t) = &self.f.blocks[q].term else { continue };
                        for e in t.edges().filter(|e| e.block.0 as usize == b) {
                            let a = e.args[n];
                            if self.consts[a.0 as usize].is_none() && self.iv[a.0 as usize].is_none() {
                                continue;
                            }
                            let x = self.at_edge(a, q);
                            joined = Some(joined.map_or(x, |j| j.join(x)));
                        }
                    }
                    if let (Some(new), Some(old)) = (joined, self.iv[p.0 as usize]) {
                        let new = new.meet(old);
                        if !new.empty() && new != old {
                            self.iv[p.0 as usize] = Some(new);
                            changed = true;
                        }
                    }
                }
                for (i, inst) in self.f.blocks[b].insts.iter().enumerate() {
                    if let [r] = inst.results.as_slice()
                        && self.consts[r.0 as usize].is_none()
                        && let Some(old) = self.iv[r.0 as usize]
                    {
                        let new = self.eval(b, i, &inst.kind).0.meet(old);
                        if !new.empty() && new != old {
                            self.iv[r.0 as usize] = Some(new);
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// `a` at the end of block `q`, as it's passed on an edge: narrowed both
    /// ways it can be read, and kept as the narrower.
    fn at_edge(&self, a: Value, q: usize) -> Iv {
        let (s, u) = (self.at(a, q, usize::MAX, true), self.at(a, q, usize::MAX, false));
        let base = self.base(a);
        // The narrower that's still a reading of the pattern.
        [s, u, base].into_iter().min_by_key(|x| x.hi - x.lo).unwrap_or(ANY)
    }
}

/// Whether `a k b` holding means `a c b` does.
fn implies(k: Cmp, c: Cmp) -> bool {
    k == c
        || matches!((k, c), (Cmp::Lt, Cmp::Le | Cmp::Ne) | (Cmp::Gt, Cmp::Ge | Cmp::Ne) | (Cmp::Eq, Cmp::Le | Cmp::Ge))
}

fn ranges_func(f: &mut Func) {
    let cfg = Cfg::new(f);
    let n = f.types.len();
    let mut consts = vec![None; n];
    let mut def = vec![None; n];
    for (b, block) in f.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            if let [r] = inst.results.as_slice() {
                def[r.0 as usize] = Some((b, i));
                if let InstKind::Iconst(c) = inst.kind {
                    consts[r.0 as usize] = Some(c);
                }
            }
        }
    }
    // The test each block's one way in makes.
    let mut facts = vec![Vec::new(); f.blocks.len()];
    for b in 0..f.blocks.len() {
        if !cfg.reached(b) {
            continue;
        }
        if let Some(Term::Br { cond, then, else_ }) = &f.blocks[b].term
            && then.block != else_.block
            && let Some((db, di)) = def[cond.0 as usize]
            && let InstKind::ICmp { cmp, unsigned, a, b: c } = f.blocks[db].insts[di].kind
        {
            for (e, holds) in [(then, cmp), (else_, negate(cmp))] {
                let t = e.block.0 as usize;
                if cfg.preds[t].len() == 1 {
                    facts[t].push(Fact { a, cmp: holds, unsigned, b: c, from: 0 });
                }
            }
        }
    }
    // A check that passed: what it checked holds from there on (and where
    // that dominates), since a failed one doesn't go on.
    for (b, block) in f.blocks.iter().enumerate() {
        if !cfg.reached(b) {
            continue;
        }
        for (i, inst) in block.insts.iter().enumerate() {
            if let InstKind::Bounds { index, len } = inst.kind {
                facts[b].push(Fact { a: index, cmp: Cmp::Lt, unsigned: true, b: len, from: i + 1 });
            }
        }
    }
    // Unknown until worked out: one-result instructions and the parameters
    // of blocks past the entry. Anything else could be anything.
    let mut iv = vec![Some(ANY); n];
    for (b, block) in f.blocks.iter().enumerate().filter(|&(b, _)| cfg.reached(b)) {
        if b != 0 {
            block.params.iter().for_each(|p| iv[p.0 as usize] = None);
        }
        for inst in &block.insts {
            if let [r] = inst.results.as_slice() {
                iv[r.0 as usize] = None;
            }
        }
    }
    let chains = (0..f.blocks.len())
        .map(|mut b| {
            let mut chain = vec![b];
            if cfg.reached(b) {
                while let Some(d) = cfg.idom[b].filter(|&d| d != b) {
                    chain.push(d);
                    b = d;
                }
            }
            chain
        })
        .collect();
    let mut r = Ranges { f, cfg, iv, consts, facts, grown: vec![0; n], widened: false, chains };
    r.solve();
    // What goes: (block, instruction) and what becomes of it.
    enum Change {
        Wrap,
        Drop,
        Const(bool),
    }
    let mut changes: Vec<(usize, usize, Change)> = Vec::new();
    for &b in &r.cfg.rpo {
        for (i, inst) in r.f.blocks[b].insts.iter().enumerate() {
            use InstKind as K;
            match inst.kind {
                K::Arith { policy: Policy::Trap, .. } | K::IntCast { policy: Policy::Trap, .. }
                    if r.eval(b, i, &inst.kind).1 =>
                {
                    changes.push((b, i, Change::Wrap));
                }
                K::Bounds { index, len } if r.decide(b, i, Cmp::Lt, true, index, len) == Some(true) => {
                    changes.push((b, i, Change::Drop));
                }
                K::ICmp { cmp, unsigned, a, b: c } => {
                    if let Some(k) = r.decide(b, i, cmp, unsigned, a, c) {
                        changes.push((b, i, Change::Const(k)));
                    }
                }
                _ => {}
            }
        }
    }
    drop(r);
    for (b, i, change) in changes.into_iter().rev() {
        let inst = &mut f.blocks[b].insts[i];
        match change {
            Change::Wrap => match &mut inst.kind {
                InstKind::Arith { policy, .. } | InstKind::IntCast { policy, .. } => {
                    *policy = Policy::Wrap;
                    inst.unwind = None;
                }
                _ => unreachable!("only checked arithmetic and casts"),
            },
            Change::Const(k) => inst.kind = InstKind::Iconst(u64::from(k)),
            Change::Drop => {
                f.blocks[b].insts.remove(i);
            }
        }
    }
}
