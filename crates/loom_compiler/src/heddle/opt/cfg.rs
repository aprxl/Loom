//! The control-flow facts the passes share: each block's predecessors, and
//! its dominators.
//!
//! Only normal edges count: landing pads, entered by unwinding, aren't
//! reached from the entry, so they have no dominator and no reverse
//! postorder number. A pass leaves them as they are, or handles them on
//! its own.

use crate::heddle::Func;

pub struct Cfg {
    pub preds: Vec<Vec<usize>>,
    /// Each reached block's immediate dominator (the entry's is itself).
    pub idom: Vec<Option<usize>>,
    /// The dominator tree: each block's children, in reverse postorder.
    pub children: Vec<Vec<usize>>,
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
        Cfg { preds, idom, children }
    }
}
