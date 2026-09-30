//! Heddle → Treadle bytecode (design/vm.md §8.4, §8.5).
//!
//! For each function:
//!
//! 1. **Layout**: blocks in reverse postorder from the entry, so a loop's
//!    head comes before its body and every cycle has a jump backwards.
//!    Backward jumps are `loop`, which counts a tick.
//! 2. **Fusion**: a comparison that's the last instruction of its block and
//!    only feeds the block's branch becomes a compare-and-branch. A small
//!    constant operand of `+`, `-` or such a branch becomes the
//!    instruction's immediate, and a constant with no other use is dropped.
//! 3. **Liveness and registers**: each value lives from its definition to
//!    its last use, stretched over the blocks it's live across; a linear
//!    scan gives each the lowest register free for all of that.
//! 4. **Frame**: the values' registers, the frame slots (memory for values
//!    that live in memory), two temporaries (for swaps at edges, and for a
//!    function value being called), then scratch: natives take their
//!    arguments there, and calls put their linkage and window there. A load
//!    or store at a slot's address is one at an offset from the frame
//!    pointer, with no address in a register.
//! 5. **Emission**: block arguments become parallel moves on their edge (on
//!    a small trampoline when the edge is one way out of a conditional
//!    branch), and jumps are patched once every block's place is known.
//!    Landing pads go after the code that runs normally, and each stretch
//!    of instructions that unwinds to one becomes an entry of the
//!    function's pad table.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;
use loom_isa::image::Pad;
use loom_isa::word::{cast_spec, word, word_i32};
use loom_isa::{Func as IsaFunc, Image, IntSpec, LINK_SLOTS, Line, Op};
use loom_syntax::source::{SourceDb, Span};

use crate::heddle::{BitOp, Cmp, Edge, FOp, Func, InstKind, IntOp, Mem, Module, Policy, Term, Value, inst_uses};
use crate::ty::{FloatTy, IntTy};

/// Generates the bytecode of `module`.
pub fn generate(db: &SourceDb, module: &Module) -> Result<Image, String> {
    let mut natives: IndexMap<&'static str, u16> = IndexMap::new();
    let mut files: IndexMap<String, u32> = IndexMap::new();
    let mut image = Image {
        strings: module.strings.clone(),
        fmts: module.fmts.clone(),
        data: module.data.clone(),
        entry: Some(module.entry.0),
        ..Image::default()
    };
    for f in &module.funcs {
        // A function whose conditional branches reach too far for a fused
        // compare-and-branch is emitted again without fusing.
        let out = match emit_func(db, f, true, &mut natives, &mut files) {
            Ok(out) => out,
            Err(Retry) => {
                emit_func(db, f, false, &mut natives, &mut files).map_err(|_| "a branch too far".to_string())?
            }
        };
        let out = out?;
        let entry = image.code.len() as u32;
        image.funcs.push(IsaFunc {
            name: f.name.clone(),
            entry,
            len: out.code.len() as u32,
            frame: out.frame,
            params: f.params().len() as u16,
            results: f.results.len() as u16,
            lines: out.lines,
            pads: out.pads,
        });
        image.code.extend(out.code);
    }
    image.natives = natives.into_keys().map(str::to_string).collect();
    image.files = files.into_keys().collect();
    Ok(image)
}

/// A fused branch's offset didn't fit in 16 bits.
struct Retry;

struct Emitted {
    code: Vec<u64>,
    frame: u16,
    lines: Vec<Line>,
    pads: Vec<Pad>,
}

/// Where a jump goes, before blocks have places.
#[derive(Clone, Copy, Debug)]
enum Label {
    Block(usize),
    Tramp(usize),
}

/// A jump to patch: the instruction's index, and whether its offset is the
/// 16-bit `c` field (a compare-and-branch) or the 32-bit `b:c`.
struct Fixup {
    at: usize,
    to: Label,
    short: bool,
}

struct Emitter<'m> {
    f: &'m Func,
    code: Vec<u64>,
    lines: Vec<Line>,
    reg: Vec<u16>,
    /// The temporary for breaking cycles of moves.
    tmp: u16,
    /// The temporary for a function value being called, out of the way of
    /// its arguments' moves; and for an address too far to fold.
    tmp2: u16,
    /// Each frame slot's byte offset from the frame pointer.
    slot_at: Vec<u32>,
    /// Values that are a frame slot's address: its number.
    slot_of: HashMap<Value, usize>,
    /// Stretches of code that unwind to a landing pad: where they start
    /// and end, and the pad's block.
    unwinds: Vec<(usize, usize, usize)>,
    /// Where natives' arguments go, and calls' linkage.
    scratch: u16,
    /// A call's window: past the linkage.
    window: u16,
    fixups: Vec<Fixup>,
    /// Instructions with a constant operand folded in, by block and index.
    folds: HashMap<(usize, usize), Fold>,
    /// Constants whose every use is folded: never emitted.
    dead: HashSet<Value>,
    /// Trampolines: an edge's moves and jump, placed after every block,
    /// with the layout place of the block the edge leaves.
    tramps: Vec<(usize, Edge)>,
    tramp_at: Vec<usize>,
    /// Each block's place in the layout, by block number.
    rank: Vec<usize>,
    order: Vec<usize>,
    natives: &'m mut IndexMap<&'static str, u16>,
    files: &'m mut IndexMap<String, u32>,
    db: &'m SourceDb,
    last_line: Option<(u32, u32, u32)>,
}

fn emit_func(
    db: &SourceDb,
    f: &Func,
    fuse: bool,
    natives: &mut IndexMap<&'static str, u16>,
    files: &mut IndexMap<String, u32>,
) -> Result<Result<Emitted, String>, Retry> {
    let order = layout(f);
    let mut rank = vec![usize::MAX; f.blocks.len()];
    for (i, &b) in order.iter().enumerate() {
        rank[b] = i;
    }
    let fused = if fuse { fusions(f) } else { HashSet::new() };
    let (folds, mut dead) = immediates(f, &fused);
    // Slot addresses only loads and stores use need no register.
    let (slot_of, folded_slots) = slot_uses(f);
    dead.extend(folded_slots);
    let skip: HashSet<Value> = fused.union(&dead).copied().collect();
    // The window's place comes from how many registers values need; a
    // second pass then lets values sit in it, which only frees registers.
    let (_, first) = allocate(f, &order, &skip, None);
    // The slots, after the values' registers, each at its alignment.
    let mut slot_at = Vec::with_capacity(f.slots.len());
    let mut bytes = u64::from(first) * 8;
    for s in &f.slots {
        bytes = bytes.div_ceil(s.align.max(1)) * s.align.max(1);
        slot_at.push(bytes);
        bytes += s.size;
    }
    let slot_words = bytes.div_ceil(8) - u64::from(first);
    let Some(tmp) = u16::try_from(u64::from(first) + slot_words).ok().filter(|&t| t < u16::MAX - 8) else {
        return Ok(Err(format!("`{}` needs more than 65535 registers", f.name)));
    };
    let Ok(slot_at) = slot_at.into_iter().map(u32::try_from).collect::<Result<Vec<u32>, _>>() else {
        return Ok(Err(format!("`{}` has too much in its frame", f.name)));
    };
    let tmp2 = tmp + 1;
    let scratch = tmp2 + 1;
    let window = scratch + LINK_SLOTS as u16;
    let (reg, _) = allocate(f, &order, &skip, Some(window));
    let mut frame = u32::from(scratch);
    let mut max = |n: u32| frame = frame.max(n);
    for b in &f.blocks {
        for inst in &b.insts {
            match &inst.kind {
                InstKind::Call { args, .. } | InstKind::CallInd { args, .. } => {
                    max(u32::from(window) + args.len().max(inst.results.len()) as u32);
                }
                InstKind::Native { args, .. } => max(u32::from(scratch) + args.len() as u32),
                _ => {}
            }
        }
        if matches!(b.term, Some(Term::Ret(ref vs)) if !vs.is_empty()) {
            let n = if let Some(Term::Ret(vs)) = &b.term { vs.len() } else { 0 };
            max(n as u32);
        }
    }
    max(f.params().len() as u32);
    let Ok(frame) = u16::try_from(frame) else {
        return Ok(Err(format!("`{}` needs more than 65535 registers", f.name)));
    };
    let mut e = Emitter {
        f,
        code: Vec::new(),
        lines: Vec::new(),
        reg,
        tmp,
        tmp2,
        slot_at,
        slot_of,
        unwinds: Vec::new(),
        scratch,
        window,
        fixups: Vec::new(),
        folds,
        dead,
        tramps: Vec::new(),
        tramp_at: Vec::new(),
        rank,
        order: order.clone(),
        natives,
        files,
        db,
        last_line: None,
    };
    let mut block_at = vec![0usize; f.blocks.len()];
    for (i, &b) in order.iter().enumerate() {
        block_at[b] = e.code.len();
        if let Err(msg) = e.block(b, i, &fused) {
            return Ok(Err(msg));
        }
    }
    // Trampolines go after every block, out of the way of fall-through.
    // Whether one's jump is a `loop` depends on its edge in the layout,
    // not on where the trampoline is.
    for k in 0..e.tramps.len() {
        e.tramp_at.push(e.code.len());
        let (from, edge) = e.tramps[k].clone();
        let span = f.blocks[order[from]].term_span;
        e.edge_moves(&edge, span);
        e.jump_to(from, edge.block.0 as usize, span, true);
    }
    // Each stretch of code with a landing pad, merged with the next when
    // they share it.
    let mut pads: Vec<Pad> = Vec::new();
    for &(start, end, pad) in &e.unwinds {
        let (start, end, pad) = (start as u32, end as u32, block_at[pad] as u32);
        match pads.last_mut() {
            Some(last) if last.end == start && last.pad == pad => last.end = end,
            _ if start < end => pads.push(Pad { start, end, pad }),
            _ => {}
        }
    }
    for fix in &e.fixups {
        let target = match fix.to {
            Label::Block(b) => block_at[b],
            Label::Tramp(t) => e.tramp_at[t],
        };
        let off = target as i64 - fix.at as i64;
        let w = e.code[fix.at];
        e.code[fix.at] = if fix.short {
            let Ok(off) = i16::try_from(off) else { return Err(Retry) };
            (w & 0x0000_FFFF_FFFF_FFFF) | u64::from(off as u16) << 48
        } else {
            let Ok(off) = i32::try_from(off) else {
                return Ok(Err(format!("`{}` is too big to jump across", f.name)));
            };
            (w & 0x0000_0000_FFFF_FFFF) | u64::from(off as u32) << 32
        };
    }
    Ok(Ok(Emitted { code: e.code, frame, lines: e.lines, pads }))
}

/// Blocks in reverse postorder from the entry, then the landing pads (and
/// what they jump to), each in reverse postorder from itself: out of the
/// way of the code that runs normally.
fn layout(f: &Func) -> Vec<usize> {
    let mut order = Vec::new();
    let mut seen = vec![false; f.blocks.len()];
    let mut roots = vec![0usize];
    let mut next_root = 0;
    while next_root < roots.len() {
        let root = roots[next_root];
        next_root += 1;
        if seen[root] {
            continue;
        }
        let mut post = Vec::new();
        let mut stack = vec![(root, 0usize)];
        seen[root] = true;
        while let Some((b, next)) = stack.pop() {
            let succs = succs(f, b);
            if next < succs.len() {
                stack.push((b, next + 1));
                let s = succs[next];
                if !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
            }
        }
        post.reverse();
        for &b in &post {
            roots.extend(f.blocks[b].pads().into_iter().map(|p| p.0 as usize));
        }
        order.extend(post);
    }
    order
}

/// The values that are frame slots' addresses, with their slots; and those
/// whose every use is a load or a store (at an offset that stays inside
/// the frame), which needs no register for the address.
fn slot_uses(f: &Func) -> (HashMap<Value, usize>, HashSet<Value>) {
    let mut slot_of = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let (InstKind::Slot(n), [r]) = (&inst.kind, inst.results.as_slice()) {
                slot_of.insert(*r, *n as usize);
            }
        }
    }
    let mut other: HashSet<Value> = HashSet::new();
    for b in &f.blocks {
        for inst in &b.insts {
            let folded = match &inst.kind {
                InstKind::Load { ptr, off, .. } if *off >= 0 => Some(*ptr),
                InstKind::Store { ptr, off, .. } if *off >= 0 => Some(*ptr),
                InstKind::Offset { ptr, off } if u32::try_from(*off).is_ok() => Some(*ptr),
                _ => None,
            };
            for u in inst_uses(&inst.kind) {
                if Some(u) != folded || matches!(&inst.kind, InstKind::Store { val, .. } if *val == u) {
                    other.insert(u);
                }
            }
        }
        for u in b.term.iter().flat_map(term_uses) {
            other.insert(u);
        }
    }
    let folded = slot_of.keys().copied().filter(|v| !other.contains(v)).collect();
    (slot_of, folded)
}

fn succs(f: &Func, b: usize) -> Vec<usize> {
    f.blocks[b].term.as_ref().map(|t| t.edges().iter().map(|e| e.block.0 as usize).collect()).unwrap_or_default()
}

/// The values read by an instruction, a terminator, and its edges.
fn term_uses(t: &Term) -> Vec<Value> {
    let mut out = match t {
        Term::Br { cond, .. } => vec![*cond],
        Term::Ret(vs) => vs.clone(),
        Term::Unravel { ptr, len } => vec![*ptr, *len],
        _ => Vec::new(),
    };
    for e in t.edges() {
        out.extend(&e.args);
    }
    out
}

/// Comparisons that become part of their block's branch.
fn fusions(f: &Func) -> HashSet<Value> {
    let mut uses: HashMap<Value, usize> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            for u in inst_uses(&inst.kind) {
                *uses.entry(u).or_default() += 1;
            }
        }
        for u in b.term.iter().flat_map(term_uses) {
            *uses.entry(u).or_default() += 1;
        }
    }
    let mut out = HashSet::new();
    for b in &f.blocks {
        let (Some(Term::Br { cond, .. }), Some(last)) = (&b.term, b.insts.last()) else { continue };
        if last.results == [*cond]
            && matches!(last.kind, InstKind::ICmp { .. } | InstKind::FCmp { .. })
            && uses.get(cond) == Some(&1)
        {
            out.insert(*cond);
        }
    }
    out
}

/// A constant operand folded into an instruction as its immediate.
#[derive(Clone, Copy, Debug)]
enum Fold {
    /// The second operand is the constant.
    B(i16),
    /// The first is, and the operation is flipped (`k + x` is `x + k`, and
    /// `k < x` is `x > k`).
    A(i16),
}

/// The value of each integer constant.
fn constants(f: &Func) -> HashMap<Value, i64> {
    let mut out = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let (InstKind::Iconst(bits), [r]) = (&inst.kind, inst.results.as_slice()) {
                out.insert(*r, *bits as i64);
            }
        }
    }
    out
}

/// Which instructions take a constant operand as their immediate, and the
/// constants nothing else reads. An immediate is 16 bits, sign-extended to
/// the 64-bit pattern of the register, so a constant fits when its pattern
/// is a sign-extended `i16` (for any integer type).
fn immediates(f: &Func, fused: &HashSet<Value>) -> (HashMap<(usize, usize), Fold>, HashSet<Value>) {
    let consts = constants(f);
    let small = |v: Value| consts.get(&v).and_then(|&k| i16::try_from(k).ok());
    let mut folds = HashMap::new();
    let mut folded: HashMap<Value, usize> = HashMap::new();
    for (bi, b) in f.blocks.iter().enumerate() {
        for (ii, inst) in b.insts.iter().enumerate() {
            let fold = match &inst.kind {
                // Only the widths with their own ops have immediate forms.
                InstKind::Arith { ty, .. } if int_width(*ty).is_none() => None,
                InstKind::Arith { op: IntOp::Add, policy: Policy::Trap | Policy::Wrap, a, b, .. } => {
                    match (small(*b), small(*a)) {
                        (Some(k), _) => Some((Fold::B(k), *b)),
                        (None, Some(k)) => Some((Fold::A(k), *a)),
                        _ => None,
                    }
                }
                InstKind::Arith { op: IntOp::Sub, policy: Policy::Trap, b, .. } => small(*b).map(|k| (Fold::B(k), *b)),
                // `x -% k` is `x +% -k`.
                InstKind::Arith { op: IntOp::Sub, policy: Policy::Wrap, b, .. } => {
                    small(*b).and_then(|k| k.checked_neg()).map(|k| (Fold::B(k), *b))
                }
                InstKind::ICmp { a, b, .. } if inst.results.first().is_some_and(|r| fused.contains(r)) => {
                    match (small(*b), small(*a)) {
                        (Some(k), _) => Some((Fold::B(k), *b)),
                        (None, Some(k)) => Some((Fold::A(k), *a)),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some((fold, v)) = fold {
                folds.insert((bi, ii), fold);
                *folded.entry(v).or_default() += 1;
            }
        }
    }
    let mut uses: HashMap<Value, usize> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            for u in inst_uses(&inst.kind) {
                *uses.entry(u).or_default() += 1;
            }
        }
        for u in b.term.iter().flat_map(term_uses) {
            *uses.entry(u).or_default() += 1;
        }
    }
    let dead = folded.into_iter().filter(|(v, n)| uses.get(v) == Some(n)).map(|(v, _)| v).collect();
    (folds, dead)
}

/// How a value is used, for register hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Argument `k` of the call at this position.
    CallArg(usize, u16),
    /// Result `i` of a `ret`.
    Ret(u16),
    Other,
}

/// A register for each value but those in `skip`; and how many registers
/// they take.
///
/// Linear scan, over each value's span from its definition to its last
/// use, stretched over the blocks it's live across (a comparison fused into
/// its branch is read at its own position, the one before the branch, and
/// nothing is made in between). A value gets, when
/// they're free for all of its span:
///
/// 1. with `window` (the call window's first register), a register of the
///    window: a call's result stays where the call left it, and a value
///    only passed to a call is made where the call wants it. Only when no
///    call or native runs in between, since a call's frame is laid over the
///    window;
/// 2. `r{i}`, for a value `ret` returns as its `i`-th;
/// 3. the register of a value an edge passes to it, or of the parameter it
///    passes itself to, so the edge moves nothing;
/// 4. otherwise, the lowest free register.
fn allocate(f: &Func, order: &[usize], skip: &HashSet<Value>, window: Option<u16>) -> (Vec<u16>, u16) {
    let n = f.types.len();
    // Positions: each block's parameters at its start, then each
    // instruction, then the terminator.
    let mut start = vec![0usize; f.blocks.len()];
    let mut end = vec![0usize; f.blocks.len()];
    let mut pos = 0;
    for &b in order {
        start[b] = pos;
        pos += 1 + f.blocks[b].insts.len();
        end[b] = pos;
        pos += 1;
    }
    // Liveness: backward, to a fixpoint.
    let mut live_in: Vec<BTreeSet<Value>> = vec![BTreeSet::new(); f.blocks.len()];
    let mut live_out: Vec<BTreeSet<Value>> = vec![BTreeSet::new(); f.blocks.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().rev() {
            let block = &f.blocks[b];
            let mut out = BTreeSet::new();
            for s in succs(f, b) {
                let params: HashSet<Value> = f.blocks[s].params.iter().copied().collect();
                out.extend(live_in[s].iter().filter(|v| !params.contains(v)));
            }
            // What a landing pad reads must survive every instruction that
            // may unwind to it.
            for pad in block.pads() {
                out.extend(live_in[pad.0 as usize].iter().copied());
            }
            let mut live = out.clone();
            for u in block.term.iter().flat_map(term_uses) {
                live.insert(u);
            }
            for inst in block.insts.iter().rev() {
                for r in &inst.results {
                    live.remove(r);
                }
                live.extend(inst_uses(&inst.kind));
            }
            for p in &block.params {
                live.remove(p);
            }
            if live != live_in[b] || out != live_out[b] {
                live_in[b] = live;
                live_out[b] = out;
                changed = true;
            }
        }
    }
    // Each value's span of positions, and what hints need.
    let mut span: Vec<Option<(usize, usize)>> = vec![None; n];
    let mut touch = |v: Value, p: usize| {
        let s = &mut span[v.0 as usize];
        *s = Some(match *s {
            None => (p, p),
            Some((a, b)) => (a.min(p), b.max(p)),
        });
    };
    let mut calls = Vec::new();
    let mut natives = Vec::new();
    let mut result_of: HashMap<Value, (usize, u16)> = HashMap::new();
    let mut uses: HashMap<Value, Vec<Use>> = HashMap::new();
    let mut partners: HashMap<Value, Vec<Value>> = HashMap::new();
    for &b in order {
        let block = &f.blocks[b];
        for &p in &block.params {
            touch(p, start[b]);
        }
        for &v in &live_in[b] {
            touch(v, start[b]);
        }
        for &v in &live_out[b] {
            touch(v, end[b]);
        }
        for (i, inst) in block.insts.iter().enumerate() {
            let at = start[b] + 1 + i;
            for u in inst_uses(&inst.kind) {
                touch(u, at);
            }
            for &r in &inst.results {
                touch(r, at);
            }
            match &inst.kind {
                InstKind::Call { args, .. } | InstKind::CallInd { args, .. } => {
                    calls.push(at);
                    for (k, &a) in args.iter().enumerate() {
                        uses.entry(a).or_default().push(Use::CallArg(at, k as u16));
                    }
                    for (j, &r) in inst.results.iter().enumerate() {
                        result_of.insert(r, (at, j as u16));
                    }
                    if let InstKind::CallInd { callee, .. } = &inst.kind {
                        uses.entry(*callee).or_default().push(Use::Other);
                    }
                }
                other => {
                    if matches!(other, InstKind::Native { .. }) {
                        natives.push(at);
                    }
                    for u in inst_uses(other) {
                        uses.entry(u).or_default().push(Use::Other);
                    }
                }
            }
        }
        let Some(term) = &block.term else { continue };
        for u in term_uses(term) {
            touch(u, end[b]);
        }
        match term {
            Term::Ret(vs) => {
                for (i, &v) in vs.iter().enumerate() {
                    uses.entry(v).or_default().push(Use::Ret(i as u16));
                }
            }
            Term::Br { cond, .. } => uses.entry(*cond).or_default().push(Use::Other),
            Term::Unravel { ptr, len } => {
                uses.entry(*ptr).or_default().push(Use::Other);
                uses.entry(*len).or_default().push(Use::Other);
            }
            _ => {}
        }
        for e in term.edges() {
            for (&param, &arg) in f.blocks[e.block.0 as usize].params.iter().zip(&e.args) {
                partners.entry(param).or_default().push(arg);
                partners.entry(arg).or_default().push(param);
                uses.entry(arg).or_default().push(Use::Other);
            }
        }
    }
    // Whether a call (or native) runs strictly between `lo` and `hi`.
    let between = |list: &[usize], lo: usize, hi: usize| {
        let i = list.partition_point(|&p| p <= lo);
        list.get(i).is_some_and(|&p| p < hi)
    };

    let mut reg = vec![0u16; n];
    let mut assigned = vec![false; n];
    let params = f.params();
    let mut used = params.len() as u16;
    for (i, &p) in params.iter().enumerate() {
        reg[p.0 as usize] = i as u16;
        assigned[p.0 as usize] = true;
    }
    let mut values: Vec<(usize, usize, Value)> = (0..n)
        .filter_map(|i| {
            let v = Value(i as u32);
            let (a, b) = span[i]?;
            (!skip.contains(&v)).then_some((a, b, v))
        })
        .collect();
    values.sort();
    let param_set: HashSet<Value> = params.iter().copied().collect();
    // Registers in use, with when they come free.
    let mut active: Vec<(usize, u16)> =
        values.iter().filter(|(_, _, v)| param_set.contains(v)).map(|&(_, end, v)| (end, reg[v.0 as usize])).collect();
    let mut free: BTreeSet<u16> = BTreeSet::new();
    for &(s, e, v) in &values {
        if param_set.contains(&v) {
            continue;
        }
        // A value whose last use is where this one is made can give it its
        // register: every instruction reads its operands before it writes
        // (a call's arguments are in its window before it runs, and its
        // results come out after).
        active.retain(|&(end, r)| {
            if end <= s {
                // Window registers aren't the pool's.
                if window.is_none_or(|w| r < w) {
                    free.insert(r);
                }
                false
            } else {
                true
            }
        });
        let busy = |r: u16, active: &[(usize, u16)]| active.iter().any(|&(_, a)| a == r);
        let mut choice = None;
        if let Some(w) = window
            && !between(&calls, s, e)
            && !between(&natives, s, e)
        {
            let at_call_end =
                |k: u16| matches!(uses.get(&v).map(Vec::as_slice), Some([Use::CallArg(at, j)]) if *at == e && *j == k);
            let slot = match result_of.get(&v) {
                Some(&(at, j)) if at == s => Some(j),
                _ => match uses.get(&v).map(Vec::as_slice) {
                    Some([Use::CallArg(_, k)]) if at_call_end(*k) => Some(*k),
                    _ => None,
                },
            };
            if let Some(k) = slot
                && !busy(w + k, &active)
            {
                choice = Some(w + k);
            }
        }
        if choice.is_none() {
            // A register of the pool, if it's free: below `used` and in
            // `free`, or past `used` (the ones in between come free).
            let mut hinted: Vec<u16> = Vec::new();
            for u in uses.get(&v).into_iter().flatten() {
                if let Use::Ret(i) = u {
                    hinted.push(*i);
                }
            }
            for p in partners.get(&v).into_iter().flatten() {
                if assigned[p.0 as usize] && window.is_none_or(|w| reg[p.0 as usize] < w) {
                    hinted.push(reg[p.0 as usize]);
                }
            }
            for h in hinted {
                if h < used && free.remove(&h) {
                    choice = Some(h);
                    break;
                }
                if h >= used && !busy(h, &active) {
                    free.extend(used..h);
                    used = h + 1;
                    choice = Some(h);
                    break;
                }
            }
        }
        let r = match choice {
            Some(r) => r,
            None => match free.pop_first() {
                Some(r) => r,
                None => {
                    used += 1;
                    used - 1
                }
            },
        };
        reg[v.0 as usize] = r;
        assigned[v.0 as usize] = true;
        active.push((e, r));
    }
    (reg, used)
}

fn int_width(t: IntTy) -> Option<usize> {
    match t.bits {
        8 => Some(0),
        16 => Some(1),
        32 => Some(2),
        64 => Some(3),
        _ => None,
    }
}

fn spec(t: IntTy) -> u64 {
    IntSpec { bits: t.bits, signed: t.signed, size: t.size }.pack()
}

/// The op for `t` from a family laid out as `[i8, i16, i32, i64, u8, u16,
/// u32, u64]`, or the family's generic form (with its extension word).
fn by_width(family: [Op; 8], generic: Op, t: IntTy) -> (Op, Option<u64>) {
    match int_width(t) {
        Some(w) => (family[w + if t.signed { 0 } else { 4 }], None),
        None => (generic, Some(spec(t))),
    }
}

impl Emitter<'_> {
    fn r(&self, v: Value) -> u16 {
        self.reg[v.0 as usize]
    }

    fn line(&mut self, span: Option<Span>) {
        let Some(span) = span else { return };
        let at = self.db.line_col(span);
        let name = self.db.file(span.file).name().to_string();
        let next = self.files.len() as u32;
        let file = *self.files.entry(name).or_insert(next);
        let key = (file, at.line, at.col);
        if self.last_line != Some(key) {
            self.last_line = Some(key);
            self.lines.push(Line { pc: self.code.len() as u32, file, line: at.line, column: at.col });
        }
    }

    fn put(&mut self, span: Option<Span>, w: u64) {
        self.line(span);
        self.code.push(w);
    }

    fn put_ext(&mut self, span: Option<Span>, w: u64, ext: Option<u64>) {
        self.put(span, w);
        if let Some(ext) = ext {
            self.code.push(ext);
        }
    }

    fn mov(&mut self, span: Option<Span>, dst: u16, src: u16) {
        if dst != src {
            self.put(span, word(Op::Mov, dst, src, 0));
        }
    }

    /// Moves every `src` into its `dst` at once: each source is read before
    /// it's overwritten.
    fn parallel(&mut self, span: Option<Span>, moves: Vec<(u16, u16)>) {
        let mut pending: Vec<(u16, u16)> = moves.into_iter().filter(|(d, s)| d != s).collect();
        while !pending.is_empty() {
            let ready = pending.iter().position(|&(d, _)| !pending.iter().any(|&(_, s)| s == d));
            match ready {
                Some(i) => {
                    let (d, s) = pending.remove(i);
                    self.mov(span, d, s);
                }
                None => {
                    // Only cycles are left: move one source out of the way.
                    let (_, s) = pending[0];
                    self.mov(span, self.tmp, s);
                    for m in &mut pending {
                        if m.1 == s {
                            m.1 = self.tmp;
                        }
                    }
                }
            }
        }
    }

    fn native(&mut self, name: &'static str) -> u16 {
        let next = self.natives.len() as u16;
        *self.natives.entry(name).or_insert(next)
    }

    fn edge_moves(&mut self, e: &Edge, span: Option<Span>) {
        let params = &self.f.blocks[e.block.0 as usize].params;
        let moves = params.iter().zip(&e.args).map(|(&p, &a)| (self.r(p), self.r(a))).collect();
        self.parallel(span, moves);
    }

    /// A jump from the block at layout place `from` to block `to`: nothing
    /// if it's next (unless `always`), `loop` if it goes backwards.
    fn jump_to(&mut self, from: usize, to: usize, span: Option<Span>, always: bool) {
        let back = self.rank[to] <= from;
        if !always && !back && self.order.get(from + 1) == Some(&to) {
            return;
        }
        let at = self.code.len();
        self.put(span, word_i32(if back { Op::Loop } else { Op::Jmp }, 0, 0));
        self.fixups.push(Fixup { at, to: Label::Block(to), short: false });
    }

    /// Where a conditional branch at layout place `from` jumps for `e`: its
    /// block, or a trampoline when the edge moves values or goes backwards.
    fn cond_target(&mut self, from: usize, e: &Edge) -> Label {
        let target = e.block.0 as usize;
        let params = &self.f.blocks[target].params;
        let moves_nothing = params.iter().zip(&e.args).all(|(&p, &a)| self.r(p) == self.r(a));
        if moves_nothing && self.rank[target] > from {
            Label::Block(target)
        } else {
            self.tramps.push((from, e.clone()));
            Label::Tramp(self.tramps.len() - 1)
        }
    }

    fn block(&mut self, b: usize, place: usize, fused: &HashSet<Value>) -> Result<(), String> {
        let f = self.f;
        let block = &f.blocks[b];
        for (i, inst) in block.insts.iter().enumerate() {
            if inst.results.first().is_some_and(|r| fused.contains(r) || self.dead.contains(r)) {
                continue;
            }
            let fold = self.folds.get(&(b, i)).copied();
            let start = self.code.len();
            self.inst(inst, fold)?;
            if let Some(pad) = inst.unwind {
                self.unwinds.push((start, self.code.len(), pad.0 as usize));
            }
        }
        let span = block.term_span;
        let term_start = self.code.len();
        match block.term.as_ref().expect("finished blocks end") {
            Term::Jump(e) => {
                self.edge_moves(e, span);
                self.jump_to(place, e.block.0 as usize, span, false);
            }
            Term::Br { cond, then, else_ } => {
                let fused_cmp = block.insts.last().filter(|i| fused.contains(cond) && i.results == [*cond]);
                let then_block = then.block.0 as usize;
                // When `then` comes next, jump away only when the condition
                // fails, and fall into it.
                let then_moves =
                    self.f.blocks[then_block].params.iter().zip(&then.args).any(|(&p, &a)| self.r(p) != self.r(a));
                let falls_into_then =
                    !then_moves && self.order.get(place + 1) == Some(&then_block) && self.rank[then_block] > place;
                let (edge, holds) = if falls_into_then { (else_, false) } else { (then, true) };
                let label = self.cond_target(place, edge);
                let at = self.code.len();
                let fold = self.folds.get(&(b, block.insts.len().wrapping_sub(1))).copied();
                match fused_cmp.map(|i| &i.kind) {
                    Some(k) => {
                        let w = branch_word(k, holds, fold, self);
                        self.put(span, w);
                        self.fixups.push(Fixup { at, to: label, short: true });
                    }
                    None => {
                        let op = if holds { Op::Jnz } else { Op::Jz };
                        self.put(span, word_i32(op, self.r(*cond), 0));
                        self.fixups.push(Fixup { at, to: label, short: false });
                    }
                }
                if !falls_into_then {
                    self.edge_moves(else_, span);
                    self.jump_to(place, else_.block.0 as usize, span, false);
                }
            }
            Term::Ret(vs) => {
                let moves = vs.iter().enumerate().map(|(i, &v)| (i as u16, self.r(v))).collect();
                self.parallel(span, moves);
                self.put(span, word(Op::Ret, 0, vs.len() as u16, 0));
            }
            Term::Unravel { ptr, len } => self.put(span, word(Op::Unravel, self.r(*ptr), self.r(*len), 0)),
            Term::UnravelMsg => {
                let n = self.native("msg.unravel");
                self.put(span, word(Op::CallN, self.scratch, n, 0));
                self.put(span, word(Op::Crash, 1, 0, 0));
            }
            Term::Crash(k) => self.put(span, word(Op::Crash, *k, 0, 0)),
            Term::Resume => self.put(span, word(Op::Resume, 0, 0, 0)),
        }
        if let Some(pad) = block.term_unwind {
            self.unwinds.push((term_start, self.code.len(), pad.0 as usize));
        }
        Ok(())
    }

    /// The load or store op for `mem`, with a register address or at an
    /// offset from the frame pointer.
    fn mem_op(mem: Mem, store: bool, frame: bool) -> Op {
        use Op as O;
        match (mem, store, frame) {
            (Mem::I8, false, false) => O::Ld8S,
            (Mem::U8, false, false) => O::Ld8U,
            (Mem::I16, false, false) => O::Ld16S,
            (Mem::U16, false, false) => O::Ld16U,
            (Mem::I32, false, false) => O::Ld32S,
            (Mem::U32, false, false) => O::Ld32U,
            (Mem::W64, false, false) => O::Ld64,
            (Mem::I8, false, true) => O::LdF8S,
            (Mem::U8, false, true) => O::LdF8U,
            (Mem::I16, false, true) => O::LdF16S,
            (Mem::U16, false, true) => O::LdF16U,
            (Mem::I32, false, true) => O::LdF32S,
            (Mem::U32, false, true) => O::LdF32U,
            (Mem::W64, false, true) => O::LdF64,
            (Mem::I8 | Mem::U8, true, false) => O::St8,
            (Mem::I16 | Mem::U16, true, false) => O::St16,
            (Mem::I32 | Mem::U32, true, false) => O::St32,
            (Mem::W64, true, false) => O::St64,
            (Mem::I8 | Mem::U8, true, true) => O::StF8,
            (Mem::I16 | Mem::U16, true, true) => O::StF16,
            (Mem::I32 | Mem::U32, true, true) => O::StF32,
            (Mem::W64, true, true) => O::StF64,
        }
    }

    /// Where a load or a store at `ptr + off` reaches: a byte offset from
    /// the frame pointer when `ptr` is a slot's address; otherwise a
    /// register and an `i16` offset, adding a far one into `tmp2` first.
    fn address(&mut self, span: Option<Span>, ptr: Value, off: i32) -> Result<(u16, i16), u32> {
        if let Some(&slot) = self.slot_of.get(&ptr)
            && off >= 0
        {
            return Err(self.slot_at[slot] + off as u32);
        }
        match i16::try_from(off) {
            Ok(off) => Ok((self.r(ptr), off)),
            Err(_) => {
                self.put_ext(span, word(Op::Ldk, self.tmp2, 0, 0), Some(off as i64 as u64));
                self.put(span, word(Op::AddWU64, self.tmp2, self.r(ptr), self.tmp2));
                Ok((self.tmp2, 0))
            }
        }
    }

    fn inst(&mut self, inst: &crate::heddle::Inst, fold: Option<Fold>) -> Result<(), String> {
        use InstKind as K;
        let span = inst.span;
        let dst = inst.results.first().map(|&v| self.r(v)).unwrap_or(0);
        let abc = |this: &Self, op: Op, a: Value, b: Value| word(op, dst, this.r(a), this.r(b));
        if let (K::Arith { op, policy, ty, a, b }, Some(fold)) = (&inst.kind, fold) {
            // `x + k`, `k + x`, `x - k`, `x -% k` (as `x +% -k`).
            let (x, k) = match fold {
                Fold::B(k) => (*a, k),
                Fold::A(k) => (*b, k),
            };
            use Op as O;
            let family = match (op, policy) {
                (IntOp::Add, Policy::Trap) => [
                    O::AddTKI8,
                    O::AddTKI16,
                    O::AddTKI32,
                    O::AddTKI64,
                    O::AddTKU8,
                    O::AddTKU16,
                    O::AddTKU32,
                    O::AddTKU64,
                ],
                (IntOp::Sub, Policy::Trap) => [
                    O::SubTKI8,
                    O::SubTKI16,
                    O::SubTKI32,
                    O::SubTKI64,
                    O::SubTKU8,
                    O::SubTKU16,
                    O::SubTKU32,
                    O::SubTKU64,
                ],
                _ => [
                    O::AddWKI8,
                    O::AddWKI16,
                    O::AddWKI32,
                    O::AddWKI64,
                    O::AddWKU8,
                    O::AddWKU16,
                    O::AddWKU32,
                    O::AddWKU64,
                ],
            };
            if let Some(w) = int_width(*ty) {
                let op = family[w + if ty.signed { 0 } else { 4 }];
                self.put(span, word(op, dst, self.r(x), k as u16));
                return Ok(());
            }
            return Err(format!("an immediate for a `{}`-bit integer", ty.bits));
        }
        match &inst.kind {
            K::Undef => {}
            K::Iconst(bits) => match i32::try_from(*bits as i64) {
                Ok(small) => self.put(span, word_i32(Op::Ldi, dst, small)),
                Err(_) => self.put_ext(span, word(Op::Ldk, dst, 0, 0), Some(*bits)),
            },
            K::Fconst(bits) => self.put_ext(span, word(Op::Ldk, dst, 0, 0), Some(*bits)),
            K::Str(n) => self.put(span, word_i32(Op::Ldstr, dst, *n as i32)),
            K::Arith { op, policy, ty, a, b } => {
                use Op as O;
                let (family, generic) = match (op, policy) {
                    (IntOp::Add, Policy::Trap) => (
                        [O::AddTI8, O::AddTI16, O::AddTI32, O::AddTI64, O::AddTU8, O::AddTU16, O::AddTU32, O::AddTU64],
                        O::AddTG,
                    ),
                    (IntOp::Add, Policy::Wrap) => (
                        [O::AddWI8, O::AddWI16, O::AddWI32, O::AddWI64, O::AddWU8, O::AddWU16, O::AddWU32, O::AddWU64],
                        O::AddWG,
                    ),
                    (IntOp::Add, Policy::Sat) => (
                        [O::AddSI8, O::AddSI16, O::AddSI32, O::AddSI64, O::AddSU8, O::AddSU16, O::AddSU32, O::AddSU64],
                        O::AddSG,
                    ),
                    (IntOp::Sub, Policy::Trap) => (
                        [O::SubTI8, O::SubTI16, O::SubTI32, O::SubTI64, O::SubTU8, O::SubTU16, O::SubTU32, O::SubTU64],
                        O::SubTG,
                    ),
                    (IntOp::Sub, Policy::Wrap) => (
                        [O::SubWI8, O::SubWI16, O::SubWI32, O::SubWI64, O::SubWU8, O::SubWU16, O::SubWU32, O::SubWU64],
                        O::SubWG,
                    ),
                    (IntOp::Sub, Policy::Sat) => (
                        [O::SubSI8, O::SubSI16, O::SubSI32, O::SubSI64, O::SubSU8, O::SubSU16, O::SubSU32, O::SubSU64],
                        O::SubSG,
                    ),
                    (IntOp::Mul, Policy::Trap) => (
                        [O::MulTI8, O::MulTI16, O::MulTI32, O::MulTI64, O::MulTU8, O::MulTU16, O::MulTU32, O::MulTU64],
                        O::MulTG,
                    ),
                    (IntOp::Mul, Policy::Wrap) => (
                        [O::MulWI8, O::MulWI16, O::MulWI32, O::MulWI64, O::MulWU8, O::MulWU16, O::MulWU32, O::MulWU64],
                        O::MulWG,
                    ),
                    (IntOp::Mul, Policy::Sat) => (
                        [O::MulSI8, O::MulSI16, O::MulSI32, O::MulSI64, O::MulSU8, O::MulSU16, O::MulSU32, O::MulSU64],
                        O::MulSG,
                    ),
                };
                let (op, ext) = by_width(family, generic, *ty);
                self.put_ext(span, abc(self, op, *a, *b), ext);
            }
            K::Div { ty, a, b } => {
                use Op as O;
                let family = [O::DivI8, O::DivI16, O::DivI32, O::DivI64, O::DivU8, O::DivU16, O::DivU32, O::DivU64];
                let (op, ext) = by_width(family, O::DivG, *ty);
                self.put_ext(span, abc(self, op, *a, *b), ext);
            }
            K::Rem { ty, a, b } => self.put(span, abc(self, if ty.signed { Op::RemS } else { Op::RemU }, *a, *b)),
            K::Shl { ty, a, b } => {
                use Op as O;
                let family = [O::ShlI8, O::ShlI16, O::ShlI32, O::ShlI64, O::ShlU8, O::ShlU16, O::ShlU32, O::ShlU64];
                let (op, ext) = by_width(family, O::ShlG, *ty);
                self.put_ext(span, abc(self, op, *a, *b), ext);
            }
            K::Shr { ty, a, b } => {
                use Op as O;
                let family = [O::ShrI8, O::ShrI16, O::ShrI32, O::ShrI64, O::ShrU8, O::ShrU16, O::ShrU32, O::ShrU64];
                let (op, ext) = by_width(family, O::ShrG, *ty);
                self.put_ext(span, abc(self, op, *a, *b), ext);
            }
            K::Bit { op, a, b } => {
                let op = match op {
                    BitOp::And => Op::And,
                    BitOp::Or => Op::Or,
                    BitOp::Xor => Op::Xor,
                };
                self.put(span, abc(self, op, *a, *b));
            }
            K::Neg { ty, a } => {
                if !ty.signed {
                    return Err(format!("negating an unsigned value in `{}`", self.f.name));
                }
                let (op, ext) = match int_width(*ty) {
                    Some(w) => ([Op::NegI8, Op::NegI16, Op::NegI32, Op::NegI64][w], None),
                    None => (Op::NegG, Some(spec(*ty))),
                };
                self.put_ext(span, word(op, dst, self.r(*a), 0), ext);
            }
            K::BitNot { ty, a } => {
                let (op, ext) = match (ty.signed, int_width(*ty)) {
                    (true, _) => (Op::BnotS, None),
                    (false, Some(w)) => ([Op::BnotU8, Op::BnotU16, Op::BnotU32, Op::BnotU64][w], None),
                    (false, None) => (Op::BnotG, Some(spec(*ty))),
                };
                self.put_ext(span, word(op, dst, self.r(*a), 0), ext);
            }
            K::Not(a) => self.put(span, word(Op::Not, dst, self.r(*a), 0)),
            K::ICmp { cmp, unsigned, a, b } => {
                let (op, a, b) = int_compare(*cmp, *unsigned, *a, *b);
                self.put(span, abc(self, op, a, b));
            }
            K::FArith { op, ty, a, b } => {
                let f64_ = *ty == FloatTy::F64;
                let op = match (op, f64_) {
                    (FOp::Add, true) => Op::AddF64,
                    (FOp::Sub, true) => Op::SubF64,
                    (FOp::Mul, true) => Op::MulF64,
                    (FOp::Div, true) => Op::DivF64,
                    (FOp::Rem, true) => Op::RemF64,
                    (FOp::Add, false) => Op::AddF32,
                    (FOp::Sub, false) => Op::SubF32,
                    (FOp::Mul, false) => Op::MulF32,
                    (FOp::Div, false) => Op::DivF32,
                    (FOp::Rem, false) => Op::RemF32,
                };
                self.put(span, abc(self, op, *a, *b));
            }
            K::FNeg { ty, a } => {
                let op = if *ty == FloatTy::F64 { Op::NegF64 } else { Op::NegF32 };
                self.put(span, word(op, dst, self.r(*a), 0));
            }
            K::FCmp { cmp, ty, a, b } => {
                let (op, a, b) = float_compare(*cmp, *ty, *a, *b);
                self.put(span, abc(self, op, a, b));
            }
            K::IntCast { policy, to, from, a } => {
                let op = match policy {
                    Policy::Trap => Op::CastT,
                    Policy::Wrap => Op::CastW,
                    Policy::Sat => Op::CastS,
                };
                let ext = cast_spec(int_spec(*to), int_spec(*from));
                self.put_ext(span, word(op, dst, self.r(*a), 0), Some(ext));
            }
            K::IntToFloat { from, to, a } => {
                let u64_ = !from.signed && from.bits == 64;
                let op = match (to, u64_) {
                    (FloatTy::F64, false) => Op::ItofS64,
                    (FloatTy::F64, true) => Op::ItofU64,
                    (FloatTy::F32, false) => Op::ItofS32,
                    (FloatTy::F32, true) => Op::ItofU32,
                };
                self.put(span, word(op, dst, self.r(*a), 0));
            }
            K::FloatToInt { sat, from, to, a } => {
                let op = match (from, sat) {
                    (FloatTy::F64, false) => Op::FtoiT64,
                    (FloatTy::F64, true) => Op::FtoiS64,
                    (FloatTy::F32, false) => Op::FtoiT32,
                    (FloatTy::F32, true) => Op::FtoiS32,
                };
                self.put_ext(span, word(op, dst, self.r(*a), 0), Some(spec(*to)));
            }
            K::FloatCast { to, a } => {
                let op = if *to == FloatTy::F32 { Op::F64toF32 } else { Op::F32toF64 };
                self.put(span, word(op, dst, self.r(*a), 0));
            }
            K::IntToChar { from, a } => self.put_ext(span, word(Op::Itoc, dst, self.r(*a), 0), Some(spec(*from))),
            K::Call { func, args } => {
                // A value may already be in the window (maybe at another
                // argument's place): all at once.
                let moves = args.iter().enumerate().map(|(i, &a)| (self.window + i as u16, self.r(a))).collect();
                self.parallel(span, moves);
                self.put(span, word_i32(Op::Call, self.window, func.0 as i32));
                for (i, &r) in inst.results.iter().enumerate() {
                    self.mov(span, self.r(r), self.window + i as u16);
                }
            }
            K::Native { name, imm, args } => {
                for (i, &a) in args.iter().enumerate() {
                    self.mov(span, self.scratch + i as u16, self.r(a));
                }
                let n = self.native(name);
                self.put(span, word(Op::CallN, self.scratch, n, *imm));
                for (i, &r) in inst.results.iter().enumerate() {
                    self.mov(span, self.r(r), self.scratch + i as u16);
                }
            }
            K::Slot(n) => {
                let at = self.slot_at[*n as usize];
                self.put(span, word_i32(Op::Lea, dst, at as i32));
            }
            K::Global(off) => self.put(span, word_i32(Op::Gaddr, dst, *off as i32)),
            K::Offset { ptr, off } => {
                if let (Some(&slot), Ok(at)) = (self.slot_of.get(ptr), u32::try_from(*off)) {
                    // An address inside a frame slot: one `lea`.
                    self.put(span, word_i32(Op::Lea, dst, (self.slot_at[slot] + at) as i32));
                } else if let Ok(k) = i16::try_from(*off) {
                    self.put(span, word(Op::AddWKU64, dst, self.r(*ptr), k as u16));
                } else {
                    self.put_ext(span, word(Op::Ldk, self.tmp2, 0, 0), Some(*off as u64));
                    self.put(span, word(Op::AddWU64, dst, self.r(*ptr), self.tmp2));
                }
            }
            K::Load { mem, ptr, off } => match self.address(span, *ptr, *off) {
                Ok((base, off)) => self.put(span, word(Self::mem_op(*mem, false, false), dst, base, off as u16)),
                Err(at) => self.put(span, word_i32(Self::mem_op(*mem, false, true), dst, at as i32)),
            },
            K::Store { mem, ptr, off, val } => {
                let v = self.r(*val);
                match self.address(span, *ptr, *off) {
                    Ok((base, off)) => self.put(span, word(Self::mem_op(*mem, true, false), base, v, off as u16)),
                    Err(at) => self.put(span, word_i32(Self::mem_op(*mem, true, true), v, at as i32)),
                }
            }
            K::Copy { dst: d, src, size } => {
                self.put_ext(span, word(Op::Copy, self.r(*d), self.r(*src), 0), Some(*size));
            }
            K::Zero { dst: d, size } => self.put_ext(span, word(Op::Zero, self.r(*d), 0, 0), Some(*size)),
            K::Index { base, index, scale } => {
                self.put_ext(span, word(Op::Idx, dst, self.r(*base), self.r(*index)), Some(*scale));
            }
            K::Bounds { index, len } => self.put(span, word(Op::Bounds, self.r(*index), self.r(*len), 0)),
            K::FuncRef(func) => self.put(span, word_i32(Op::Ldfn, dst, func.0 as i32)),
            K::CallInd { callee, args } => {
                // The function's number goes out of the window's way first.
                self.mov(span, self.tmp2, self.r(*callee));
                let moves = args.iter().enumerate().map(|(i, &a)| (self.window + i as u16, self.r(a))).collect();
                self.parallel(span, moves);
                let (params, results) = (args.len(), inst.results.len());
                if params > 255 || results > 255 {
                    return Err(format!("a function value called with {params} arguments in `{}`", self.f.name));
                }
                self.put(span, word(Op::CallR, self.window, self.tmp2, ((params as u16) << 8) | results as u16));
                for (i, &r) in inst.results.iter().enumerate() {
                    self.mov(span, self.r(r), self.window + i as u16);
                }
            }
        }
        Ok(())
    }
}

fn int_spec(t: IntTy) -> IntSpec {
    IntSpec { bits: t.bits, signed: t.signed, size: t.size }
}

/// The op and operand order for an integer comparison giving a `bool`.
fn int_compare(cmp: Cmp, unsigned: bool, a: Value, b: Value) -> (Op, Value, Value) {
    let (lt, le) = if unsigned { (Op::LtU, Op::LeU) } else { (Op::LtS, Op::LeS) };
    match cmp {
        Cmp::Eq => (Op::Eq, a, b),
        Cmp::Ne => (Op::Ne, a, b),
        Cmp::Lt => (lt, a, b),
        Cmp::Le => (le, a, b),
        Cmp::Gt => (lt, b, a),
        Cmp::Ge => (le, b, a),
    }
}

fn float_compare(cmp: Cmp, ty: FloatTy, a: Value, b: Value) -> (Op, Value, Value) {
    let [eq, ne, lt, le] = match ty {
        FloatTy::F64 => [Op::EqF64, Op::NeF64, Op::LtF64, Op::LeF64],
        FloatTy::F32 => [Op::EqF32, Op::NeF32, Op::LtF32, Op::LeF32],
    };
    match cmp {
        Cmp::Eq => (eq, a, b),
        Cmp::Ne => (ne, a, b),
        Cmp::Lt => (lt, a, b),
        Cmp::Le => (le, a, b),
        Cmp::Gt => (lt, b, a),
        Cmp::Ge => (le, b, a),
    }
}

/// The compare-and-branch that jumps when comparison `k` holds, or with
/// `holds` false, when it doesn't (its offset patched later). An integer
/// comparison negates by swapping (`not a < b` is `b <= a`); a float one
/// needs the negated ops, which also jump for NaN. With `fold`, one
/// operand is a constant, the op's immediate.
fn branch_word(k: &InstKind, holds: bool, fold: Option<Fold>, e: &Emitter) -> u64 {
    if let (InstKind::ICmp { cmp, unsigned, a, b }, Some(fold)) = (k, fold) {
        // `x cmp k`, with `k cmp x` flipped around.
        let (x, k, cmp) = match fold {
            Fold::B(k) => (*a, k, *cmp),
            Fold::A(k) => (
                *b,
                k,
                match cmp {
                    Cmp::Lt => Cmp::Gt,
                    Cmp::Le => Cmp::Ge,
                    Cmp::Gt => Cmp::Lt,
                    Cmp::Ge => Cmp::Le,
                    other => *other,
                },
            ),
        };
        let cmp = if holds {
            cmp
        } else {
            match cmp {
                Cmp::Eq => Cmp::Ne,
                Cmp::Ne => Cmp::Eq,
                Cmp::Lt => Cmp::Ge,
                Cmp::Le => Cmp::Gt,
                Cmp::Gt => Cmp::Le,
                Cmp::Ge => Cmp::Lt,
            }
        };
        let op = match (cmp, unsigned) {
            (Cmp::Eq, _) => Op::JEqK,
            (Cmp::Ne, _) => Op::JNeK,
            (Cmp::Lt, false) => Op::JLtSK,
            (Cmp::Le, false) => Op::JLeSK,
            (Cmp::Gt, false) => Op::JGtSK,
            (Cmp::Ge, false) => Op::JGeSK,
            (Cmp::Lt, true) => Op::JLtUK,
            (Cmp::Le, true) => Op::JLeUK,
            (Cmp::Gt, true) => Op::JGtUK,
            (Cmp::Ge, true) => Op::JGeUK,
        };
        return word(op, e.r(x), k as u16, 0);
    }
    let (op, a, b) = match k {
        InstKind::ICmp { cmp, unsigned, a, b } => {
            let (lt, le) = if *unsigned { (Op::JLtU, Op::JLeU) } else { (Op::JLtS, Op::JLeS) };
            let (a, b) = (*a, *b);
            match (cmp, holds) {
                (Cmp::Eq, true) | (Cmp::Ne, false) => (Op::JEq, a, b),
                (Cmp::Ne, true) | (Cmp::Eq, false) => (Op::JNe, a, b),
                (Cmp::Lt, true) | (Cmp::Ge, false) => (lt, a, b),
                (Cmp::Le, true) | (Cmp::Gt, false) => (le, a, b),
                (Cmp::Gt, true) | (Cmp::Le, false) => (lt, b, a),
                (Cmp::Ge, true) | (Cmp::Lt, false) => (le, b, a),
            }
        }
        InstKind::FCmp { cmp, ty, a, b } => {
            let [eq, ne, lt, le, nlt, nle] = match ty {
                FloatTy::F64 => [Op::JEqF64, Op::JNeF64, Op::JLtF64, Op::JLeF64, Op::JNLtF64, Op::JNLeF64],
                FloatTy::F32 => [Op::JEqF32, Op::JNeF32, Op::JLtF32, Op::JLeF32, Op::JNLtF32, Op::JNLeF32],
            };
            let (a, b) = (*a, *b);
            match (cmp, holds) {
                (Cmp::Eq, true) | (Cmp::Ne, false) => (eq, a, b),
                (Cmp::Ne, true) | (Cmp::Eq, false) => (ne, a, b),
                (Cmp::Lt, true) => (lt, a, b),
                (Cmp::Le, true) => (le, a, b),
                (Cmp::Gt, true) => (lt, b, a),
                (Cmp::Ge, true) => (le, b, a),
                (Cmp::Lt, false) => (nlt, a, b),
                (Cmp::Le, false) => (nle, a, b),
                (Cmp::Gt, false) => (nlt, b, a),
                (Cmp::Ge, false) => (nle, b, a),
            }
        }
        _ => unreachable!("only comparisons are fused"),
    };
    word(op, e.r(a), e.r(b), 0)
}
