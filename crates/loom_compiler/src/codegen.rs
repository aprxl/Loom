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
//!    pointer, with no address in a register. A slot over 64 KiB (an array
//!    on the stack) lives on the heap instead, so a frame stays what 16-bit
//!    register numbers can reach: the frame has its pointer (and size), the
//!    prologue allocates it, and every way out frees it (a return, and the
//!    end of every landing pad, including one made just for that).
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
use loom_isa::{Func as IsaFunc, Image, IntSpec, LINK_SLOTS, Line, NO_SITE, Op, Site as IsaSite};
use loom_syntax::source::{SourceDb, Span};

use crate::heddle::{BitOp, Cmp, Edge, FOp, Func, InstKind, IntOp, Mem, Module, Policy, Term, Value, inst_uses};
use crate::ty::{FloatTy, IntTy};

/// The size over which a frame slot lives on the heap, in bytes. If a frame
/// is still too big for its registers, [`SMALL_BIG_SLOT`] is tried.
const BIG_SLOT: u64 = 64 * 1024;
const SMALL_BIG_SLOT: u64 = 1024;
/// The words of a big slot's cell in the frame: the heap block's address, a
/// word nobody uses, and its size in bytes (a list header's shape, which
/// `freebuf` frees the buffer of).
const CELL_WORDS: u64 = 3;
/// What an unwind in an instruction with no landing pad goes to, when the
/// function has big slots to free: the code at the end.
const FREE_PAD: usize = usize::MAX;

/// A frame needs more registers than a 16-bit number can name.
fn too_big(f: &Func) -> CodegenError {
    CodegenError::Limit(format!(
        "`{}` needs more than 65 535 registers in a frame: Treadle's limit (a function with that many values, or calls with that many arguments)",
        f.name
    ))
}

/// Why a module has no bytecode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodegenError {
    /// The program is over one of Treadle's limits (registers in a frame,
    /// distance of a jump, arguments of a function value…): not a bug.
    Limit(String),
    /// Heddle that codegen doesn't know what to do with: a bug in Loom.
    Bug(String),
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodegenError::Limit(m) | CodegenError::Bug(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CodegenError {}

impl From<String> for CodegenError {
    fn from(m: String) -> Self {
        CodegenError::Bug(m)
    }
}

impl From<&str> for CodegenError {
    fn from(m: &str) -> Self {
        CodegenError::Bug(m.to_string())
    }
}

/// Generates the bytecode of `module`.
pub fn generate(db: &SourceDb, module: &Module) -> Result<Image, CodegenError> {
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
        let hoisted = hoist_constants(f);
        let f = hoisted.as_ref().unwrap_or(f);
        // A function whose conditional branches reach too far for a fused
        // compare-and-branch is emitted again without fusing.
        let out = match emit_func(db, f, true, &mut natives, &mut files) {
            Ok(out) => out,
            Err(Retry) => emit_func(db, f, false, &mut natives, &mut files)
                .map_err(|_| CodegenError::Bug(format!("`{}` has a branch that reaches too far", f.name)))?,
        };
        let out = out?;
        let entry = image.code.len() as u32;
        let sites = f
            .sites
            .iter()
            .map(|s| IsaSite {
                name: s.name.clone(),
                call: s.call.map(|span| {
                    let at = db.line_col(span);
                    let name = db.file(span.file).name().to_string();
                    let next = files.len() as u32;
                    (*files.entry(name).or_insert(next), at.line, at.col)
                }),
                parent: s.parent.unwrap_or(NO_SITE),
            })
            .collect();
        image.funcs.push(IsaFunc {
            name: f.name.clone(),
            entry,
            len: out.code.len() as u32,
            frame: out.frame,
            params: f.params().len() as u16,
            results: f.results.len() as u16,
            lines: out.lines,
            pads: out.pads,
            sites,
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
    /// Each frame slot's byte offset from the frame pointer (a big slot's is
    /// its cell's).
    slot_at: Vec<u32>,
    /// Which slots are on the heap (see [`BIG_SLOT`]).
    big: Vec<bool>,
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
    /// Elements' addresses only the next instruction, a load or a store,
    /// uses: made by that one.
    elems: HashSet<Value>,
    /// Values nothing reads: a call's results that aren't, or a block's
    /// parameters. Their registers may be another value's, so they aren't
    /// written.
    unused: HashSet<Value>,
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
    last_line: Option<(u32, u32, u32, u32)>,
    /// The inlined call the instruction being emitted came from.
    site: Option<u32>,
}

fn emit_func(
    db: &SourceDb,
    f: &Func,
    fuse: bool,
    natives: &mut IndexMap<&'static str, u16>,
    files: &mut IndexMap<String, u32>,
) -> Result<Result<Emitted, CodegenError>, Retry> {
    let order = layout(f);
    let mut rank = vec![usize::MAX; f.blocks.len()];
    for (i, &b) in order.iter().enumerate() {
        rank[b] = i;
    }
    let fused = if fuse { fusions(f) } else { HashSet::new() };
    let (folds, dead) = immediates(f);
    let elems = elem_accesses(f);
    // The slots, after the values' registers, each at its alignment: the big
    // ones as a cell, whose block is on the heap (what is big gets smaller
    // when the frame doesn't fit its registers otherwise).
    let mut laid = None;
    for limit in [BIG_SLOT, SMALL_BIG_SLOT] {
        let big: Vec<bool> = f.slots.iter().map(|s| s.size > limit).collect();
        // Slot addresses only loads and stores use need no register (not a
        // big slot's: its address is loaded from its cell).
        let (slot_of, folded_slots) = slot_uses(f, &big);
        let mut dead = dead.clone();
        dead.extend(folded_slots);
        let skip: HashSet<Value> = fused.union(&dead).chain(&elems).copied().collect();
        // The window's place comes from how many registers values need; a
        // second pass then lets values sit in it, which only frees registers.
        let Some((_, first)) = allocate(f, &order, &skip, None) else { break };
        let mut slot_at = Vec::with_capacity(f.slots.len());
        let mut bytes = u64::from(first) * 8;
        for (s, &is_big) in f.slots.iter().zip(&big) {
            let (size, align) = if is_big { (CELL_WORDS * 8, 8) } else { (s.size, s.align.max(1)) };
            bytes = bytes.div_ceil(align) * align;
            slot_at.push(bytes);
            bytes += size;
        }
        let slot_words = bytes.div_ceil(8) - u64::from(first);
        if let Some(tmp) = u16::try_from(u64::from(first) + slot_words).ok().filter(|&t| t < u16::MAX - 8) {
            laid = Some((big, slot_at, tmp, slot_of, dead, skip));
            break;
        }
    }
    let Some((big, slot_at, tmp, slot_of, dead, skip)) = laid else {
        return Ok(Err(too_big(f)));
    };
    let Ok(slot_at) = slot_at.into_iter().map(u32::try_from).collect::<Result<Vec<u32>, _>>() else {
        return Ok(Err(CodegenError::Limit(format!("`{}` has more than 4 GiB in its frame", f.name))));
    };
    let tmp2 = tmp + 1;
    let scratch = tmp2 + 1;
    let window = scratch + LINK_SLOTS as u16;
    let Some((reg, _)) = allocate(f, &order, &skip, Some(window)) else {
        return Ok(Err(too_big(f)));
    };
    let mut frame = u32::from(scratch);
    let mut max = |n: u32| frame = frame.max(n);
    for b in &f.blocks {
        for inst in &b.insts {
            match &inst.kind {
                InstKind::Call { args, .. } | InstKind::CallInd { args, .. } => {
                    max(u32::from(window) + args.len().max(inst.results.len()) as u32);
                }
                InstKind::Native { args, .. } => {
                    max(u32::from(scratch) + args.len().max(inst.results.len()) as u32);
                }
                _ => {}
            }
        }
        if matches!(b.term, Some(Term::Ret(ref vs)) if !vs.is_empty()) {
            let n = if let Some(Term::Ret(vs)) = &b.term { vs.len() } else { 0 };
            max(n as u32);
        }
    }
    max(f.params().len() as u32);
    if big.contains(&true) {
        // The prologue's `mem.alloc`.
        max(u32::from(scratch) + 2);
    }
    let Ok(frame) = u16::try_from(frame) else {
        return Ok(Err(too_big(f)));
    };
    let mut read: HashSet<Value> = HashSet::new();
    for b in &f.blocks {
        for inst in &b.insts {
            read.extend(inst_uses(&inst.kind));
        }
        read.extend(b.term.iter().flat_map(term_uses));
    }
    let unused = f
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(i, b)| {
            b.insts
                .iter()
                .flat_map(|i| i.results.iter().copied())
                .chain(b.params.iter().copied().filter(move |_| i > 0))
        })
        .filter(|r| !read.contains(r))
        .collect();
    let mut e = Emitter {
        f,
        code: Vec::new(),
        lines: Vec::new(),
        reg,
        tmp,
        tmp2,
        slot_at,
        big,
        slot_of,
        unwinds: Vec::new(),
        scratch,
        window,
        fixups: Vec::new(),
        folds,
        dead,
        elems,
        unused,
        tramps: Vec::new(),
        tramp_at: Vec::new(),
        rank,
        order: order.clone(),
        natives,
        files,
        db,
        last_line: None,
        site: None,
    };
    let mut block_at = vec![0usize; f.blocks.len()];
    e.prologue();
    for (i, &b) in order.iter().enumerate() {
        block_at[b] = e.code.len();
        if let Err(error) = e.block(b, i, &fused) {
            return Ok(Err(error));
        }
    }
    // Trampolines go after every block, out of the way of fall-through.
    // Whether one's jump is a `loop` depends on its edge in the layout,
    // not on where the trampoline is.
    for k in 0..e.tramps.len() {
        e.tramp_at.push(e.code.len());
        let (from, edge) = e.tramps[k].clone();
        let span = f.blocks[order[from]].term_span;
        e.site = f.blocks[order[from]].term_site;
        let start = e.code.len();
        e.edge_moves(&edge, span);
        e.jump_to(from, edge.block.0 as usize, span, true);
        // A loop's back edge notices an interrupt: from here too, it
        // unwinds to its block's landing pad.
        if let Some(pad) = f.blocks[order[from]].term_unwind {
            e.unwinds.push((start, e.code.len(), pad.0 as usize));
        }
    }
    // What unwinds without a pad of its own still has the big slots to free.
    e.site = None;
    let free_pad = e.code.len();
    if e.big.contains(&true) {
        e.free_big(None);
        e.put(None, word(Op::Resume, 0, 0, 0));
    }
    // Each stretch of code with a landing pad, merged with the next when
    // they share it.
    let mut pads: Vec<Pad> = Vec::new();
    for &(start, end, pad) in &e.unwinds {
        let at = if pad == FREE_PAD { free_pad } else { block_at[pad] };
        let (start, end, pad) = (start as u32, end as u32, at as u32);
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
                return Ok(Err(CodegenError::Limit(format!("`{}` is too big to jump across", f.name))));
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
fn slot_uses(f: &Func, big: &[bool]) -> (HashMap<Value, usize>, HashSet<Value>) {
    let mut slot_of = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let (InstKind::Slot(n), [r]) = (&inst.kind, inst.results.as_slice())
                && !big[*n as usize]
            {
                slot_of.insert(*r, *n as usize);
            }
        }
    }
    let mut other: HashSet<Value> = HashSet::new();
    for b in &f.blocks {
        for inst in &b.insts {
            let folded = match &inst.kind {
                InstKind::Load { ptr, off, .. } if *off >= 0 => Some(*ptr),
                InstKind::Elem { header, off, .. } if *off >= 0 => Some(*header),
                InstKind::FreeBuf { header, off, .. } if *off >= 0 => Some(*header),
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

/// `f` with the constants made inside loops (numbers, text and data
/// addresses) made once, at the start, instead of on every iteration; or
/// `None` when it has none. Each is still used only where it was, which the
/// entry block comes before. (Not every constant: one a cold path uses would
/// cost every call.)
fn hoist_constants(f: &Func) -> Option<Func> {
    let looped = in_loops(f);
    let constant =
        |k: &InstKind| matches!(k, InstKind::Iconst(_) | InstKind::Fconst(_) | InstKind::Str(_) | InstKind::Global(_));
    if !(1..f.blocks.len()).any(|b| looped[b] && f.blocks[b].insts.iter().any(|i| constant(&i.kind))) {
        return None;
    }
    let mut f = f.clone();
    // What's made at the start: the constants of the entry block and of
    // loops, then addresses at an offset from them (a constant's value
    // after its flag), which a loop would work out again too.
    let mut made: HashSet<Value> = HashSet::new();
    for (b, block) in f.blocks.iter().enumerate() {
        if b == 0 || looped[b] {
            made.extend(block.insts.iter().filter(|i| constant(&i.kind)).flat_map(|i| i.results.iter().copied()));
        }
    }
    let offset = |k: &InstKind| matches!(k, InstKind::Offset { ptr, .. } if made.contains(ptr));
    let (mut lifted, mut offsets) = (Vec::new(), Vec::new());
    for (block, _) in f.blocks.iter_mut().zip(&looped).skip(1).filter(|(_, l)| **l) {
        let mut keep = Vec::new();
        for inst in std::mem::take(&mut block.insts) {
            if constant(&inst.kind) {
                lifted.push(inst);
            } else if offset(&inst.kind) {
                offsets.push(inst);
            } else {
                keep.push(inst);
            }
        }
        block.insts = keep;
    }
    // The entry block itself may be a loop's; its constants come first too.
    let entry = std::mem::take(&mut f.blocks[0].insts);
    let (mut first, rest): (Vec<_>, Vec<_>) = entry.into_iter().partition(|i| constant(&i.kind));
    first.extend(lifted);
    first.extend(offsets);
    first.extend(rest);
    f.blocks[0].insts = first;
    Some(f)
}

/// Whether each block is on a cycle of the control flow graph: strongly
/// connected components (Kosaraju's), those of more than one block or with
/// an edge to itself.
fn in_loops(f: &Func) -> Vec<bool> {
    let n = f.blocks.len();
    let next: Vec<Vec<usize>> = (0..n).map(|b| succs(f, b)).collect();
    let mut prev = vec![Vec::new(); n];
    for (b, ss) in next.iter().enumerate() {
        for &s in ss {
            prev[s].push(b);
        }
    }
    // Finishing order, iteratively.
    let mut seen = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for root in 0..n {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut stack = vec![(root, 0)];
        while let Some(&mut (b, ref mut i)) = stack.last_mut() {
            if let Some(&s) = next[b].get(*i) {
                *i += 1;
                if !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                order.push(b);
                stack.pop();
            }
        }
    }
    let mut comp = vec![usize::MAX; n];
    let mut size = Vec::new();
    for &root in order.iter().rev() {
        if comp[root] != usize::MAX {
            continue;
        }
        let c = size.len();
        size.push(0);
        comp[root] = c;
        let mut stack = vec![root];
        while let Some(b) = stack.pop() {
            size[c] += 1;
            for &p in &prev[b] {
                if comp[p] == usize::MAX {
                    comp[p] = c;
                    stack.push(p);
                }
            }
        }
    }
    (0..n).map(|b| size[comp[b]] > 1 || next[b].contains(&b)).collect()
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
            && matches!(last.kind, InstKind::ICmp { .. } | InstKind::FCmp { .. } | InstKind::Not(_))
            && uses.get(cond) == Some(&1)
        {
            out.insert(*cond);
        }
    }
    out
}

/// Elements' addresses (`elem`) whose only use is as the address of the
/// load or store right after, which the access then works out itself.
fn elem_accesses(f: &Func) -> HashSet<Value> {
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
        for pair in b.insts.windows(2) {
            let (InstKind::Elem { scale, .. }, [r]) = (&pair[0].kind, pair[0].results.as_slice()) else { continue };
            let next = match &pair[1].kind {
                InstKind::Load { ptr, off, .. } => *ptr == *r && (0..=0xFFFF).contains(off),
                InstKind::Store { ptr, off, val, .. } => *ptr == *r && *val != *r && (0..=0xFFFF).contains(off),
                _ => false,
            };
            if next && uses.get(r) == Some(&1) && u32::try_from(*scale).is_ok() {
                out.insert(*r);
            }
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
    /// A bounds check's length is this constant.
    Len(u32),
    /// A bounds check of a constant index below a constant length: nothing.
    Gone,
}

impl Fold {
    /// The operand that isn't the constant, and the constant.
    fn split(self, a: Value, b: Value) -> (Value, i16) {
        match self {
            Fold::B(k) => (a, k),
            Fold::A(k) => (b, k),
            Fold::Len(_) | Fold::Gone => unreachable!("only bounds checks fold these"),
        }
    }
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
fn immediates(f: &Func) -> (HashMap<(usize, usize), Fold>, HashSet<Value>) {
    let consts = constants(f);
    let small = |v: Value| consts.get(&v).and_then(|&k| i16::try_from(k).ok());
    let mask = |v: Value| consts.get(&v).and_then(|&k| u16::try_from(k).ok());
    // Values below a constant: an unsigned `x % k`, or `x & m`.
    let mut below: HashMap<Value, u64> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            let bound = match &inst.kind {
                InstKind::Rem { ty, b, .. } if !ty.signed => consts.get(b).map(|&k| k as u64),
                InstKind::Bit { op: BitOp::And, a, b } => {
                    consts.get(b).or_else(|| consts.get(a)).and_then(|&m| (m as u64).checked_add(1))
                }
                _ => None,
            };
            if let (Some(k), [r]) = (bound, inst.results.as_slice()) {
                below.insert(*r, k);
            }
        }
    }
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
                InstKind::ICmp { a, b, .. } => match (small(*b), small(*a)) {
                    (Some(k), _) => Some((Fold::B(k), *b)),
                    (None, Some(k)) => Some((Fold::A(k), *a)),
                    _ => None,
                },
                // A mask: zero-extended.
                InstKind::Bit { a, b, .. } => match (mask(*b), mask(*a)) {
                    (Some(k), _) => Some((Fold::B(k as i16), *b)),
                    (None, Some(k)) => Some((Fold::A(k as i16), *a)),
                    _ => None,
                },
                InstKind::Shl { ty, b, .. } if int_width(*ty).is_some() => {
                    small(*b).filter(|&k| k >= 0 && (k as u32) < u32::from(ty.bits)).map(|k| (Fold::B(k), *b))
                }
                InstKind::Shr { ty, b, .. } => {
                    small(*b).filter(|&k| k >= 0 && (k as u32) < u32::from(ty.bits)).map(|k| (Fold::B(k), *b))
                }
                InstKind::Div { b, .. } | InstKind::Rem { b, .. } => {
                    small(*b).filter(|&k| k > 0).map(|k| (Fold::B(k), *b))
                }
                InstKind::Bounds { index, len } => match (consts.get(index), consts.get(len)) {
                    (Some(&i), Some(&n)) if (i as u64) < (n as u64) => {
                        *folded.entry(*index).or_default() += 1;
                        Some((Fold::Gone, *len))
                    }
                    // `x % k` (or `x & m`, unsigned) is below `k` (`m + 1`).
                    (_, Some(&n)) if below.get(index).is_some_and(|&k| k <= n as u64) => Some((Fold::Gone, *len)),
                    (_, Some(&n)) => u32::try_from(n as u64).ok().map(|n| (Fold::Len(n), *len)),
                    _ => None,
                },
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
    let mut dead: HashSet<Value> = folded.into_iter().filter(|(v, n)| uses.get(v) == Some(n)).map(|(v, _)| v).collect();
    // And constants nothing reads at all.
    dead.extend(consts.keys().filter(|v| !uses.contains_key(v)));
    (folds, dead)
}

/// How a value is used, for register hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Argument `k` of the call at this position.
    CallArg(usize, u16),
    /// Argument `k` of the native called at this position.
    NativeArg(usize, u16),
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
fn allocate(f: &Func, order: &[usize], skip: &HashSet<Value>, window: Option<u16>) -> Option<(Vec<u16>, u16)> {
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
    let mut native_result_of: HashMap<Value, (usize, u16)> = HashMap::new();
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
                InstKind::Native { args, .. } => {
                    natives.push(at);
                    for (k, &a) in args.iter().enumerate() {
                        uses.entry(a).or_default().push(Use::NativeArg(at, k as u16));
                    }
                    for (j, &r) in inst.results.iter().enumerate() {
                        native_result_of.insert(r, (at, j as u16));
                    }
                }
                other => {
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
                // Window registers aren't the pool's, nor the natives'
                // scratch ones below them.
                if window.is_none_or(|w| r < w - LINK_SLOTS as u16) {
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
            let at_native_end = |k: u16| matches!(uses.get(&v).map(Vec::as_slice), Some([Use::NativeArg(at, j)]) if *at == e && *j == k);
            // The natives' window is the call window's linkage and on.
            let scratch = w - LINK_SLOTS as u16;
            let slot = match (result_of.get(&v), native_result_of.get(&v)) {
                (Some(&(at, j)), _) if at == s => Some(w + j),
                (_, Some(&(at, j))) if at == s => Some(scratch + j),
                _ => match uses.get(&v).map(Vec::as_slice) {
                    Some([Use::CallArg(_, k)]) if at_call_end(*k) => Some(w + *k),
                    Some([Use::NativeArg(_, k)]) if at_native_end(*k) => Some(scratch + *k),
                    _ => None,
                },
            };
            if let Some(r) = slot
                && !busy(r, &active)
            {
                choice = Some(r);
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
                // Not a register of the windows: a call or a native writes those.
                if assigned[p.0 as usize] && window.is_none_or(|w| reg[p.0 as usize] < w - LINK_SLOTS as u16) {
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
                    used = h.checked_add(1)?;
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
                    used = used.checked_add(1)?;
                    used - 1
                }
            },
        };
        reg[v.0 as usize] = r;
        assigned[v.0 as usize] = true;
        active.push((e, r));
    }
    Some((reg, used))
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

    /// Where the next instruction is from: a location, in a call inlined
    /// there (`site`) or in the function's own code.
    fn line(&mut self, span: Option<Span>) {
        let Some(span) = span else { return };
        let at = self.db.line_col(span);
        let name = self.db.file(span.file).name().to_string();
        let next = self.files.len() as u32;
        let file = *self.files.entry(name).or_insert(next);
        let site = self.site.unwrap_or(NO_SITE);
        let key = (file, at.line, at.col, site);
        if self.last_line != Some(key) {
            self.last_line = Some(key);
            self.lines.push(Line { pc: self.code.len() as u32, file, line: at.line, column: at.col, site });
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

    fn has_big(&self) -> bool {
        self.big.contains(&true)
    }

    /// Allocates the heap block of each big slot, and puts its address and
    /// size in the slot's cell (see [`BIG_SLOT`]).
    fn prologue(&mut self) {
        for n in 0..self.big.len() {
            if !self.big[n] {
                continue;
            }
            let (size, align) = (self.f.slots[n].size, self.f.slots[n].align.max(1));
            let at = self.slot_at[n] as i32;
            self.put_ext(None, word(Op::Ldk, self.scratch, 0, 0), Some(size));
            self.put(None, word_i32(Op::Ldi, self.scratch + 1, align as i32));
            let alloc = self.native("mem.alloc");
            self.put(None, word(Op::CallN, self.scratch, alloc, 0));
            self.put(None, word_i32(Op::StF64, self.scratch, at));
            self.put_ext(None, word(Op::Ldk, self.tmp, 0, 0), Some(size));
            self.put(None, word_i32(Op::StF64, self.tmp, at + 16));
        }
    }

    /// Frees the heap block of each big slot: at a return, and at the end
    /// of an unwind through this function.
    fn free_big(&mut self, span: Option<Span>) {
        for n in 0..self.big.len() {
            if !self.big[n] {
                continue;
            }
            let log = self.f.slots[n].align.max(1).trailing_zeros() as u16;
            let ext = (u64::from(self.slot_at[n]) << 32) | 1;
            self.put_ext(span, word(Op::FreeBufF, log, 0, 0), Some(ext));
        }
    }

    fn native(&mut self, name: &'static str) -> u16 {
        // (A few dozen natives exist, and the verifier names each one.)
        debug_assert!(self.natives.len() < usize::from(u16::MAX), "more natives than a `calln` can number");
        let next = self.natives.len() as u16;
        *self.natives.entry(name).or_insert(next)
    }

    fn edge_moves(&mut self, e: &Edge, span: Option<Span>) {
        let params = &self.f.blocks[e.block.0 as usize].params;
        let moves = params
            .iter()
            .zip(&e.args)
            .filter(|(p, _)| !self.unused.contains(p))
            .map(|(&p, &a)| (self.r(p), self.r(a)))
            .collect();
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
    /// block, or a trampoline when the edge moves values. (One that jumps
    /// backwards counts a tick, as `loop` does.)
    fn cond_target(&mut self, from: usize, e: &Edge) -> Label {
        let target = e.block.0 as usize;
        let params = &self.f.blocks[target].params;
        let moves_nothing =
            params.iter().zip(&e.args).all(|(&p, &a)| self.unused.contains(&p) || self.r(p) == self.r(a));
        if moves_nothing {
            Label::Block(target)
        } else {
            self.tramps.push((from, e.clone()));
            Label::Tramp(self.tramps.len() - 1)
        }
    }

    /// Emits block `b`'s instructions, then its terminator.
    fn block(&mut self, b: usize, place: usize, fused: &HashSet<Value>) -> Result<(), CodegenError> {
        self.insts(b, fused)?;
        self.term(b, place, fused)
    }

    /// Whether a jump from block `b` to `h` goes back to the head of a loop
    /// that can be *rotated*: the jump runs the head's test itself and
    /// branches back into the loop, which saves a jump (`loop`) on every
    /// iteration. The head has to end in a branch, and be short, with
    /// nothing in it that can unwind.
    fn rotates(&self, b: usize, place: usize, h: usize, fused: &HashSet<Value>) -> bool {
        let head = &self.f.blocks[h];
        if h == b || self.rank[h] > place || !matches!(head.term, Some(Term::Br { .. })) {
            return false;
        }
        let emitted = head
            .insts
            .iter()
            .filter(|i| !i.results.first().is_some_and(|r| fused.contains(r) || self.dead.contains(r)))
            .count();
        emitted <= 4
            && head
                .insts
                .iter()
                .all(|i| !i.kind.can_unwind() && !matches!(i.kind, InstKind::Call { .. } | InstKind::CallInd { .. }))
    }

    /// Emits block `b`'s instructions.
    fn insts(&mut self, b: usize, fused: &HashSet<Value>) -> Result<(), CodegenError> {
        let f = self.f;
        let block = &f.blocks[b];
        for (i, inst) in block.insts.iter().enumerate() {
            if inst
                .results
                .first()
                .is_some_and(|r| fused.contains(r) || self.dead.contains(r) || self.elems.contains(r))
            {
                continue;
            }
            let fold = self.folds.get(&(b, i)).copied();
            let start = self.code.len();
            // A load or a store at an element the instruction before gave.
            let elem = match &inst.kind {
                InstKind::Load { ptr, .. } | InstKind::Store { ptr, .. } if self.elems.contains(ptr) => {
                    Some(&block.insts[i - 1])
                }
                _ => None,
            };
            self.site = elem.unwrap_or(inst).site;
            let unwind = match elem {
                Some(e) => {
                    self.elem_access(e, inst)?;
                    e.unwind.or(inst.unwind)
                }
                None => {
                    self.inst(inst, fold)?;
                    inst.unwind
                }
            };
            match unwind {
                Some(pad) => self.unwinds.push((start, self.code.len(), pad.0 as usize)),
                None if self.has_big() => self.unwinds.push((start, self.code.len(), FREE_PAD)),
                None => {}
            }
        }
        Ok(())
    }

    /// Emits the branch that ends block `b`, at layout place `place` (its
    /// own, or that of a back edge that rotates its loop).
    fn branch(&mut self, b: usize, place: usize, fused: &HashSet<Value>, span: Option<Span>) {
        let block = &self.f.blocks[b];
        let Some(Term::Br { cond, then, else_ }) = &block.term else { unreachable!("a branch") };
        let fused_cmp = block.insts.last().filter(|i| fused.contains(cond) && i.results == [*cond]);
        let then_block = then.block.0 as usize;
        // When `then` comes next, jump away only when the condition fails,
        // and fall into it.
        let then_moves = self.f.blocks[then_block]
            .params
            .iter()
            .zip(&then.args)
            .any(|(&p, &a)| !self.unused.contains(&p) && self.r(p) != self.r(a));
        let falls_into_then =
            !then_moves && self.order.get(place + 1) == Some(&then_block) && self.rank[then_block] > place;
        let (edge, holds) = if falls_into_then { (else_, false) } else { (then, true) };
        let label = self.cond_target(place, edge);
        let at = self.code.len();
        let fold = self.folds.get(&(b, block.insts.len().wrapping_sub(1))).copied();
        match fused_cmp.map(|i| &i.kind) {
            // `not x`: a branch on `x` the other way.
            Some(InstKind::Not(x)) => {
                let op = if holds { Op::Jz } else { Op::Jnz };
                self.put(span, word_i32(op, self.r(*x), 0));
                self.fixups.push(Fixup { at, to: label, short: false });
            }
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

    /// Emits block `b`'s terminator.
    fn term(&mut self, b: usize, place: usize, fused: &HashSet<Value>) -> Result<(), CodegenError> {
        let f = self.f;
        let block = &f.blocks[b];
        let span = block.term_span;
        self.site = block.term_site;
        let term_start = self.code.len();
        match block.term.as_ref().expect("finished blocks end") {
            Term::Jump(e) if self.rotates(b, place, e.block.0 as usize, fused) => {
                // The loop's head again, here: its parameters, its
                // instructions, and its branch, back into the loop.
                let h = e.block.0 as usize;
                self.edge_moves(e, span);
                self.insts(h, fused)?;
                let head = &f.blocks[h];
                self.site = head.term_site;
                self.branch(h, place, fused, head.term_span);
            }
            Term::Jump(e) => {
                self.edge_moves(e, span);
                self.jump_to(place, e.block.0 as usize, span, false);
            }
            Term::Br { .. } => self.branch(b, place, fused, span),
            Term::Ret(vs) => {
                self.free_big(span);
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
            Term::Resume => {
                self.free_big(span);
                self.put(span, word(Op::Resume, 0, 0, 0));
            }
            Term::Suspend { .. } => {
                return Err(CodegenError::Bug(format!("a suspension left in `{}`", self.f.name)));
            }
        }
        match block.term_unwind {
            Some(pad) => self.unwinds.push((term_start, self.code.len(), pad.0 as usize)),
            None if self.has_big() && matches!(block.term, Some(Term::Unravel { .. } | Term::UnravelMsg)) => {
                self.unwinds.push((term_start, self.code.len(), FREE_PAD));
            }
            None => {}
        }
        Ok(())
    }

    /// The load or store `access` at the element `elem` gives, in one
    /// instruction, reported where `elem` is (it's what can unravel).
    fn elem_access(&mut self, elem: &crate::heddle::Inst, access: &crate::heddle::Inst) -> Result<(), CodegenError> {
        use Op as O;
        let InstKind::Elem { header, off: hoff, index, scale } = &elem.kind else {
            return Err("an element access without its element".into());
        };
        let span = elem.span.or(access.span);
        let (mem, off, reg, store) = match &access.kind {
            InstKind::Load { mem, off, .. } => (*mem, *off, self.r(access.results[0]), false),
            InstKind::Store { mem, off, val, .. } => (*mem, *off, self.r(*val), true),
            _ => return Err("an element access that isn't a load or a store".into()),
        };
        let ops = match (mem, store) {
            (Mem::I8, false) => (O::LdElF8S, O::LdEl8S),
            (Mem::U8, false) => (O::LdElF8U, O::LdEl8U),
            (Mem::I16, false) => (O::LdElF16S, O::LdEl16S),
            (Mem::U16, false) => (O::LdElF16U, O::LdEl16U),
            (Mem::I32, false) => (O::LdElF32S, O::LdEl32S),
            (Mem::U32, false) => (O::LdElF32U, O::LdEl32U),
            (Mem::W64, false) => (O::LdElF64, O::LdEl64),
            (Mem::I8 | Mem::U8, true) => (O::StElF8, O::StEl8),
            (Mem::I16 | Mem::U16, true) => (O::StElF16, O::StEl16),
            (Mem::I32 | Mem::U32, true) => (O::StElF32, O::StEl32),
            (Mem::W64, true) => (O::StElF64, O::StEl64),
        };
        let index = self.r(*index);
        match self.address(span, *header, *hoff) {
            Err(at) => {
                let ext = (u64::from(at) << 32) | *scale;
                self.put_ext(span, word(ops.0, reg, index, off as u16), Some(ext));
            }
            Ok((base, k)) => {
                let base = if k == 0 {
                    base
                } else {
                    self.put(span, word(O::AddWKU64, self.tmp2, base, k as u16));
                    self.tmp2
                };
                let ext = ((off as u64) << 32) | *scale;
                self.put_ext(span, word(ops.1, reg, base, index), Some(ext));
            }
        }
        Ok(())
    }

    /// `inst` with a constant operand as its immediate (see [`immediates`]),
    /// if it's one of the instructions that take one here.
    fn inst_k(&mut self, inst: &crate::heddle::Inst, fold: Fold) -> Result<bool, CodegenError> {
        use InstKind as K;
        use Op as O;
        let span = inst.span;
        let dst = inst.results.first().map(|&v| self.r(v)).unwrap_or(0);
        let w = match &inst.kind {
            K::Bit { op, a, b } => {
                let (x, k) = fold.split(*a, *b);
                let op = match op {
                    BitOp::And => O::AndK,
                    BitOp::Or => O::OrK,
                    BitOp::Xor => O::XorK,
                };
                word(op, dst, self.r(x), k as u16)
            }
            K::Shl { ty, a, b } => {
                let (x, k) = fold.split(*a, *b);
                let family =
                    [O::ShlKI8, O::ShlKI16, O::ShlKI32, O::ShlKI64, O::ShlKU8, O::ShlKU16, O::ShlKU32, O::ShlKU64];
                let w = int_width(*ty).ok_or("a shift by a constant of an odd width")?;
                word(family[w + if ty.signed { 0 } else { 4 }], dst, self.r(x), k as u16)
            }
            K::Shr { ty, a, b } => {
                let (x, k) = fold.split(*a, *b);
                word(if ty.signed { O::ShrKS } else { O::ShrKU }, dst, self.r(x), k as u16)
            }
            K::Div { ty, a, b } => {
                let (x, k) = fold.split(*a, *b);
                word(if ty.signed { O::DivKS } else { O::DivKU }, dst, self.r(x), k as u16)
            }
            K::Rem { ty, a, b } => {
                let (x, k) = fold.split(*a, *b);
                word(if ty.signed { O::RemKS } else { O::RemKU }, dst, self.r(x), k as u16)
            }
            K::ICmp { cmp, unsigned, a, b } => {
                let (x, k) = fold.split(*a, *b);
                // `k < x` is `x > k`.
                let cmp = match (fold, cmp) {
                    (Fold::A(_), Cmp::Lt) => Cmp::Gt,
                    (Fold::A(_), Cmp::Le) => Cmp::Ge,
                    (Fold::A(_), Cmp::Gt) => Cmp::Lt,
                    (Fold::A(_), Cmp::Ge) => Cmp::Le,
                    (_, other) => *other,
                };
                let op = match (cmp, unsigned) {
                    (Cmp::Eq, _) => O::EqK,
                    (Cmp::Ne, _) => O::NeK,
                    (Cmp::Lt, false) => O::LtSK,
                    (Cmp::Le, false) => O::LeSK,
                    (Cmp::Gt, false) => O::GtSK,
                    (Cmp::Ge, false) => O::GeSK,
                    (Cmp::Lt, true) => O::LtUK,
                    (Cmp::Le, true) => O::LeUK,
                    (Cmp::Gt, true) => O::GtUK,
                    (Cmp::Ge, true) => O::GeUK,
                };
                word(op, dst, self.r(x), k as u16)
            }
            K::Bounds { index, .. } => match fold {
                Fold::Gone => return Ok(true),
                Fold::Len(n) => word_i32(O::BoundsK, self.r(*index), n as i32),
                _ => return Ok(false),
            },
            _ => return Ok(false),
        };
        self.put(span, w);
        Ok(true)
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

    fn inst(&mut self, inst: &crate::heddle::Inst, fold: Option<Fold>) -> Result<(), CodegenError> {
        use InstKind as K;
        let span = inst.span;
        let dst = inst.results.first().map(|&v| self.r(v)).unwrap_or(0);
        let abc = |this: &Self, op: Op, a: Value, b: Value| word(op, dst, this.r(a), this.r(b));
        if let (K::Arith { op, policy, ty, a, b }, Some(fold)) = (&inst.kind, fold) {
            // `x + k`, `k + x`, `x - k`, `x -% k` (as `x +% -k`).
            let (x, k) = fold.split(*a, *b);
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
            return Err(format!("an immediate for a `{}`-bit integer", ty.bits).into());
        }
        if let Some(fold) = fold
            && self.inst_k(inst, fold)?
        {
            return Ok(());
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
                    return Err(format!("negating an unsigned value in `{}`", self.f.name).into());
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
                    if !self.unused.contains(&r) {
                        self.mov(span, self.r(r), self.window + i as u16);
                    }
                }
            }
            K::Native { name, imm, args } => {
                // At once: an argument may sit where another goes.
                let moves = args.iter().enumerate().map(|(i, &a)| (self.scratch + i as u16, self.r(a))).collect();
                self.parallel(span, moves);
                let n = self.native(name);
                self.put(span, word(Op::CallN, self.scratch, n, *imm));
                for (i, &r) in inst.results.iter().enumerate() {
                    if !self.unused.contains(&r) {
                        self.mov(span, self.r(r), self.scratch + i as u16);
                    }
                }
            }
            K::Slot(n) => {
                let at = self.slot_at[*n as usize];
                // A big slot's address is in its cell.
                let op = if self.big[*n as usize] { Op::LdF64 } else { Op::Lea };
                self.put(span, word_i32(op, dst, at as i32));
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
            K::FreeBuf { header, off, size, align } => {
                let log = align.max(&1).trailing_zeros() as u16;
                match self.address(span, *header, *off) {
                    Err(at) => {
                        let ext = (u64::from(at) << 32) | *size;
                        self.put_ext(span, word(Op::FreeBufF, log, 0, 0), Some(ext));
                    }
                    Ok((reg, k)) => {
                        let reg = if k == 0 {
                            reg
                        } else {
                            self.put(span, word(Op::AddWKU64, self.tmp2, reg, k as u16));
                            self.tmp2
                        };
                        self.put_ext(span, word(Op::FreeBuf, reg, log, 0), Some(*size));
                    }
                }
            }
            K::Elem { header, off, index, scale } => match self.address(span, *header, *off) {
                // In the frame: one `elemf`.
                Err(at) => {
                    let ext = (u64::from(at) << 32) | *scale;
                    self.put_ext(span, word(Op::ElemF, dst, self.r(*index), 0), Some(ext));
                }
                Ok((reg, 0)) => self.put_ext(span, word(Op::Elem, dst, reg, self.r(*index)), Some(*scale)),
                Ok((reg, k)) => {
                    self.put(span, word(Op::AddWKU64, self.tmp2, reg, k as u16));
                    self.put_ext(span, word(Op::Elem, dst, self.tmp2, self.r(*index)), Some(*scale));
                }
            },
            K::FuncRef(func) => self.put(span, word_i32(Op::Ldfn, dst, func.0 as i32)),
            K::Life { .. } => return Err(CodegenError::Bug(format!("a slot's lifetime left in `{}`", self.f.name))),
            K::CallInd { callee, args } => {
                // The function's number goes out of the window's way first.
                self.mov(span, self.tmp2, self.r(*callee));
                let moves = args.iter().enumerate().map(|(i, &a)| (self.window + i as u16, self.r(a))).collect();
                self.parallel(span, moves);
                let (params, results) = (args.len(), inst.results.len());
                if params > 255 || results > 255 {
                    return Err(CodegenError::Limit(format!(
                        "a function value called with {params} arguments and {results} results in `{}`: Treadle's calls through function values take at most 255 of each",
                        self.f.name
                    )));
                }
                self.put(span, word(Op::CallR, self.window, self.tmp2, ((params as u16) << 8) | results as u16));
                for (i, &r) in inst.results.iter().enumerate() {
                    if !self.unused.contains(&r) {
                        self.mov(span, self.r(r), self.window + i as u16);
                    }
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
        let (x, k) = fold.split(*a, *b);
        let cmp = match (fold, cmp) {
            (Fold::A(_), Cmp::Lt) => Cmp::Gt,
            (Fold::A(_), Cmp::Le) => Cmp::Ge,
            (Fold::A(_), Cmp::Gt) => Cmp::Lt,
            (Fold::A(_), Cmp::Ge) => Cmp::Le,
            (_, other) => *other,
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
