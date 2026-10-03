//! The control-flow facts the passes share: each block's successors and
//! predecessors, reverse postorder, dominators, and loops.
//!
//! Only normal edges count: landing pads, entered by unwinding, aren't
//! reached from the entry, so they have no dominator and no reverse
//! postorder number. A pass leaves them as they are, or handles them on
//! its own.

use crate::heddle::Func;

pub struct Cfg {
    pub succs: Vec<Vec<usize>>,
    pub preds: Vec<Vec<usize>>,
    /// The blocks the entry reaches, in reverse postorder.
    pub rpo: Vec<usize>,
    /// Each block's place in `rpo` (`usize::MAX` if the entry doesn't
    /// reach it).
    pub rank: Vec<usize>,
    /// Each reached block's immediate dominator (the entry's is itself).
    pub idom: Vec<Option<usize>>,
    /// The dominator tree: each block's children, in reverse postorder.
    pub children: Vec<Vec<usize>>,
    /// Each block's entry and exit numbers in a walk of the dominator tree:
    /// `a` dominates `b` when `b`'s numbers are inside `a`'s.
    pre: Vec<u32>,
    post: Vec<u32>,
}

impl Cfg {
    pub fn new(f: &Func) -> Cfg {
        let n = f.blocks.len();
        let succs: Vec<Vec<usize>> = f
            .blocks
            .iter()
            .map(|b| b.term.iter().flat_map(|t| t.edges()).map(|e| e.block.0 as usize).collect())
            .collect();
        let mut preds = vec![Vec::new(); n];
        for (b, ss) in succs.iter().enumerate() {
            for &s in ss {
                if !preds[s].contains(&b) {
                    preds[s].push(b);
                }
            }
        }
        // Reverse postorder from the entry, iteratively.
        let mut post_order = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        let mut stack = vec![(0usize, 0usize)];
        if n > 0 {
            seen[0] = true;
        }
        while let Some(&mut (b, ref mut i)) = stack.last_mut() {
            if let Some(&s) = succs[b].get(*i) {
                *i += 1;
                if !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post_order.push(b);
                stack.pop();
            }
        }
        let rpo: Vec<usize> = post_order.into_iter().rev().collect();
        let mut rank = vec![usize::MAX; n];
        for (i, &b) in rpo.iter().enumerate() {
            rank[b] = i;
        }
        // Dominators: Cooper, Harvey and Kennedy.
        let mut idom: Vec<Option<usize>> = vec![None; n];
        if n > 0 {
            idom[0] = Some(0);
        }
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new: Option<usize> = None;
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
        let mut children = vec![Vec::new(); n];
        for &b in rpo.iter().skip(1) {
            if let Some(d) = idom[b] {
                children[d].push(b);
            }
        }
        let (mut pre, mut post) = (vec![0u32; n], vec![0u32; n]);
        let mut clock = 0;
        let mut walk = vec![(0usize, 0usize)];
        if n > 0 {
            pre[0] = clock;
            clock += 1;
        }
        while let Some(&mut (b, ref mut i)) = walk.last_mut() {
            if let Some(&c) = children[b].get(*i) {
                *i += 1;
                pre[c] = clock;
                clock += 1;
                walk.push((c, 0));
            } else {
                post[b] = clock;
                clock += 1;
                walk.pop();
            }
        }
        Cfg { succs, preds, rpo, rank, idom, children, pre, post }
    }

    /// Whether the entry reaches `b` by normal edges.
    pub fn reached(&self, b: usize) -> bool {
        self.rank[b] != usize::MAX
    }

    /// Whether `a` dominates `b` (every block dominates itself). Only for
    /// blocks the entry reaches.
    pub fn dominates(&self, a: usize, b: usize) -> bool {
        self.reached(a) && self.reached(b) && self.pre[a] <= self.pre[b] && self.post[b] <= self.post[a]
    }

    /// The natural loops: for each block some edge goes back to (one that
    /// dominates where the edge comes from), the blocks of its loop, header
    /// first. Loops that share a header are one. Innermost last isn't
    /// promised: sort by size when it matters.
    pub fn loops(&self) -> Vec<Loop> {
        let mut out: Vec<Loop> = Vec::new();
        for &h in &self.rpo {
            let latches: Vec<usize> =
                self.preds[h].iter().copied().filter(|&p| self.reached(p) && self.dominates(h, p)).collect();
            if latches.is_empty() {
                continue;
            }
            let mut inside = vec![false; self.succs.len()];
            inside[h] = true;
            let mut blocks = vec![h];
            let mut stack = latches;
            while let Some(b) = stack.pop() {
                if std::mem::replace(&mut inside[b], true) {
                    continue;
                }
                blocks.push(b);
                stack.extend(self.preds[b].iter().copied().filter(|&p| self.reached(p)));
            }
            blocks[1..].sort_by_key(|&b| self.rank[b]);
            out.push(Loop { header: h, blocks, inside });
        }
        out
    }
}

/// A natural loop.
pub struct Loop {
    pub header: usize,
    /// Its blocks, the header first, then in reverse postorder.
    pub blocks: Vec<usize>,
    /// Whether each block of the function is in it.
    pub inside: Vec<bool>,
}
