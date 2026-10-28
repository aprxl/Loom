//! Threads: `loom` blocks and `thread` (design/vm.md §11).
//!
//! A loom block makes a group, a number kept in a slot of the frame, which
//! its threads (and the lambdas inside it) capture as a local of their own.
//! A cleanup ends the block, however it ends: it waits for the threads
//! (cancelling them first unless the block fell off its end), drops the
//! results nobody joined, frees the group and raises what failed.
//!
//! `thread e` is a function of its own, called by the VM when it starts the
//! task, with `(result address, environment)`: it writes its value where the
//! result goes, whatever its type. The environment is made on the heap, and
//! belongs to the task: a run of pointers to what it holds by reference,
//! then copies of what doesn't outlive the loom (typeck's `thread_copies`),
//! which a pointer in the run points at.

use loom_syntax::source::Span;

use super::cleanup::CleanupKind;
use super::value::{Place, Storage, Val};
use super::{Abi, Builder, FnKey};
use crate::heddle::{Cmp, InstKind, IntOp, Mem, VTy, Value};
use crate::hir::{self, HirId, LocalId};
use crate::ty::IntTy;
use crate::ty::{Ty, TyKind};
use crate::typeck::BodyId;

/// A thread waiting to be built: its expression, and what it captures.
#[derive(Clone, Debug)]
pub(super) struct ThreadInfo<'a> {
    pub expr: &'a hir::Expr,
    pub captures: Vec<LocalId>,
    /// The captures that move into the thread: what it owns, and drops.
    pub moves: Vec<LocalId>,
    /// What the environment owns (a moved capture that needs dropping): the
    /// offset it's at, and its type.
    pub owned: Vec<(u64, Ty)>,
}

/// A chunk task of a `loom for` over a range: the items `lo` to `lo + count`
/// run one after the other in one task, each as the thread's own function.
/// Its environment is the thread's, then `lo` and `count`.
#[derive(Clone, Copy, Debug)]
pub(super) struct ChunkEnv {
    lo: u64,
    count: u64,
    /// Where the item is copied for the body, and its type, if it reads it.
    item: Option<(u64, Ty)>,
    /// A `loom for` over a list or its chunks: where its items come from.
    src: Option<ChunkSrc>,
}

/// Where a chunk task finds its items, as offsets in its environment: the
/// elements' address, their number and the chunk width (for `chunks`), and
/// where the loop's index and item are copied for the body.
#[derive(Clone, Copy, Debug)]
pub(super) struct ChunkSrc {
    buf: u64,
    len: u64,
    width: Option<u64>,
    elem: Ty,
    index: Option<u64>,
    item: Option<(u64, Ty)>,
}

/// A `loom for` over something whose items are counted before the loop starts
/// and each worked out from its number: a list, slice or array (its elements),
/// or `chunks`/`chunks_mut` of one, either with `.indexed()`.
pub(super) struct Indexed<'a> {
    seq: &'a hir::Expr,
    /// The chunk width's expression, for `chunks` and `chunks_mut`.
    width: Option<&'a hir::Expr>,
    /// The patterns of the item's number (`.indexed()`) and of the item.
    index: Option<&'a hir::Pat>,
    item: Option<&'a hir::Pat>,
}

/// What a chunk task of an indexed `loom for` is given.
#[derive(Clone)]
struct SrcArgs {
    buf: Value,
    len: Value,
    width: Option<Value>,
    elem: Ty,
    index: Option<LocalId>,
    item: Option<LocalId>,
}

/// The parts of a `loom for` the builder needs to chunk it.
pub(super) struct LoomFor<'a> {
    pub pat: &'a hir::Pat,
    pub body: &'a hir::Block,
    pub thread: &'a hir::Expr,
    pub group: LocalId,
    pub int: IntTy,
    pub elem: Ty,
}

/// What a chunk task is spawned with.
struct ChunkArgs {
    /// The `loom for` pattern's locals, which the task sets itself.
    vars: Vec<LocalId>,
    lo: Value,
    count: Value,
    src: Option<SrcArgs>,
}

/// The `#builtin` methods of `Spun` the builder writes.
pub(super) fn writes_body(path: &str) -> bool {
    path == "std.sync.Spun.join"
}

/// How a loom's end was reached, for `loom.leave` and `loom.free`.
const NORMAL: u64 = 0;
const EARLY: u64 = 1;
const UNWIND: u64 = 2;

impl<'a> Builder<'a> {
    /// `loom { … }`: its group, and a cleanup that ends it.
    pub(super) fn loom_expr(
        &mut self,
        e: &'a hir::Expr,
        args: &'a [hir::Arg],
        block: &'a hir::Block,
        group: LocalId,
        ty: Ty,
    ) -> Option<Val> {
        let span = e.span;
        let g = match args.first() {
            None => self.emit1(InstKind::Native { name: "loom.new", imm: 0, args: Vec::new() }, VTy::Int),
            Some(arg) => {
                // `Crew(workers: n)`: its `workers` is the group's cap.
                let crew_ty = self.ty_of(&arg.value);
                let crew = self.expr(&arg.value)?;
                let pos = self.field_pos(crew_ty, None, &crate::typeck::Key::Name("workers".to_string()), span)?;
                let workers = self.field_of(&crew, &pos, span)?;
                let usize_ty = self.tcx().usize;
                let w = self.regs(workers, usize_ty, span)?[0];
                self.emit1(InstKind::Native { name: "loom.crew", imm: 0, args: vec![w] }, VTy::Int)
            }
        };
        let slot = self.raw_slot(8, 8);
        let at = self.slot_addr(slot);
        self.store(Mem::W64, at, 0, g);
        self.f.locals.insert(group, Storage::Slot(slot));
        // The cleanup is under the block's own locals, which are dropped
        // first: a thread can't be holding one (see typeck's capture rules).
        // The block's value is a temporary of the statement around it, not
        // of this scope, which ends before the value is used.
        let join = self.join(ty, span)?;
        let depth = self.open_scope();
        self.add_cleanup(CleanupKind::Loom { slot });
        let val = self.block_into(block, &join).map(|()| self.join_get(&join));
        self.close_scope(depth);
        val
    }

    /// What ends a loom: wait for its threads (cancelling them first when
    /// the end is early or an unwind), drop the results nobody took, free
    /// the group, and raise what failed (unless unwinding already).
    pub(super) fn emit_loom_end(&mut self, slot: u32) {
        let mode = if self.f.in_pad > 0 {
            UNWIND
        } else if self.f.closing {
            NORMAL
        } else {
            EARLY
        };
        let at = self.slot_addr(slot);
        let g = self.load(Mem::W64, at, 0, VTy::Int);
        let mode = self.iconst(mode);
        // The wait can unravel (a deadlock): then the loom's end is an
        // unwind, which cancels what's left and waits for it. This
        // cleanup isn't live while it runs, so it's put back for the wait.
        let again = (self.f.in_pad == 0).then(|| self.add_cleanup(CleanupKind::Loom { slot }));
        self.emit(InstKind::Native { name: "loom.leave", imm: 0, args: vec![g, mode] }, &[]);
        if again.is_some() {
            self.f.cleanups.pop();
        }
        // `loop { (f, p) = loom.leftover(g); if f == 0 { break }; f(p) }`
        let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.enter(head);
        let r = self.emit(InstKind::Native { name: "loom.leftover", imm: 0, args: vec![g] }, &[VTy::Int, VTy::Ptr]);
        let zero = self.iconst(0);
        let none = self.ucmp(Cmp::Eq, r[0], zero);
        self.br(none, done, body);
        self.enter(body);
        self.emit(InstKind::CallInd { callee: r[0], args: vec![r[1]] }, &[]);
        self.jump(head);
        self.enter(done);
        self.emit(InstKind::Native { name: "loom.free", imm: 0, args: vec![g, mode] }, &[]);
    }

    /// A `yield` or `pull`, where a cancelled thread stops (design/vm.md
    /// §11.2): unwinds if the running thread has been cancelled.
    pub(super) fn cancel_point(&mut self) {
        if self.has_threads {
            self.emit(InstKind::Native { name: "task.cancelled", imm: 0, args: Vec::new() }, &[]);
        }
    }

    /// `thread body`: makes its environment, and starts the task. The value
    /// is the handle.
    pub(super) fn thread_expr(&mut self, e: &'a hir::Expr, group: Option<LocalId>) -> Option<Val> {
        self.spawn_thread(e, group, None)
    }

    /// Starts the task of thread expression `e`: its own, or with `chunk`,
    /// one that runs the thread for a run of items of a `loom for`.
    fn spawn_thread(&mut self, e: &'a hir::Expr, group: Option<LocalId>, chunk: Option<ChunkArgs>) -> Option<Val> {
        let span = e.span;
        let Some(group) = group else { return self.internal(span, "a thread outside a loom") };
        let result_ty = self.thread_result_ty(e)?;
        let captures = self.f.scan.captures.get(&e.id).cloned().unwrap_or_default();
        let copies = self.tables().thread_copies.get(&e.id).cloned().unwrap_or_default();
        let moves = self.tables().thread_moves.get(&e.id).cloned().unwrap_or_default();
        // The environment: a pointer for each capture, then the copies and
        // what moves in.
        let mut tail = 8 * captures.len() as u64;
        let mut copy_at = Vec::new();
        let mut owned = Vec::new();
        for &l in &captures {
            if copies.contains(&l) || moves.contains(&l) {
                let ty = self.local_ty(l);
                let (size, _) = self.size_align(ty)?;
                copy_at.push(Some((tail, size)));
                if moves.contains(&l) && self.needs_drop(ty) {
                    owned.push((tail, ty));
                }
                tail += size.div_ceil(8) * 8;
            } else {
                copy_at.push(None);
            }
        }
        let chunk_env = chunk.as_ref().map(|c| {
            let at = |l: LocalId| {
                let k = captures.iter().position(|&x| x == l)?;
                Some((copy_at[k]?.0, self.local_ty(l)))
            };
            let item = c.vars.first().copied().filter(|_| c.src.is_none()).and_then(at);
            let src = c.src.as_ref().map(|s| ChunkSrc {
                buf: tail + 16,
                len: tail + 24,
                width: s.width.map(|_| tail + 32),
                elem: s.elem,
                index: s.index.and_then(at).map(|(off, _)| off),
                item: s.item.and_then(at),
            });
            ChunkEnv { lo: tail, count: tail + 8, item, src }
        });
        let env_bytes = match &chunk {
            None => tail,
            Some(c) if c.src.is_some() => tail + 40,
            Some(_) => tail + 16,
        };
        let env = if env_bytes == 0 {
            self.iconst(0)
        } else {
            let (n, a) = (self.iconst(env_bytes), self.iconst(8));
            self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr)
        };
        for (k, &l) in captures.iter().enumerate() {
            if chunk.as_ref().is_some_and(|c| c.vars.contains(&l)) {
                // The chunk's task sets the item itself.
                let Some((off, _)) = copy_at[k] else {
                    return self.internal(span, "a loop variable that isn't copied");
                };
                let dst = self.offset(env, off as i64);
                self.store(Mem::W64, env, 8 * k as i64, dst);
                continue;
            }
            let place = self.local_place(l, span)?;
            let src = self.place_addr(&place, span)?;
            let pointer = match copy_at[k] {
                Some((off, size)) => {
                    let dst = self.offset(env, off as i64);
                    self.emit(InstKind::Copy { dst, src, size }, &[]);
                    // What moves in isn't the parent's to drop any more.
                    if moves.contains(&l) && self.needs_drop(self.local_ty(l)) {
                        self.emit(InstKind::Zero { dst: src, size }, &[]);
                    }
                    dst
                }
                None => src,
            };
            self.store(Mem::W64, env, 8 * k as i64, pointer);
        }
        let key = (self.f.body, e.id);
        let entry = match (&chunk, chunk_env) {
            (Some(c), Some(layout)) => {
                self.store(Mem::W64, env, layout.lo as i64, c.lo);
                self.store(Mem::W64, env, layout.count as i64, c.count);
                if let (Some(src), Some(at)) = (&c.src, layout.src) {
                    self.store(Mem::W64, env, at.buf as i64, src.buf);
                    self.store(Mem::W64, env, at.len as i64, src.len);
                    if let (Some(width), Some(off)) = (src.width, at.width) {
                        self.store(Mem::W64, env, off as i64, width);
                    }
                }
                self.chunks.insert(key, layout);
                FnKey::Chunk(key.0, key.1)
            }
            _ => FnKey::Thread(key.0, key.1),
        };
        let has_owned = !owned.is_empty();
        self.threads.insert(key, ThreadInfo { expr: e, captures, moves, owned });
        let entry = self.func_id(entry);
        let entry = self.emit1(InstKind::FuncRef(entry), VTy::Int);
        let drop_fn = if self.needs_drop(result_ty) {
            let func = self.glue(super::Glue::Drop, result_ty);
            self.emit1(InstKind::FuncRef(func), VTy::Int)
        } else {
            self.iconst(0)
        };
        let result_bytes = if self.is_valueless(result_ty) { 0 } else { self.size_align(result_ty)?.0 };
        let result_bytes = self.iconst(result_bytes);
        let env_bytes = self.iconst(env_bytes);
        let place = self.local_place(group, span)?;
        let at = self.place_addr(&place, span)?;
        let g = self.load(Mem::W64, at, 0, VTy::Int);
        // What a thread that never runs owns is dropped by its group.
        let env_drop = if has_owned {
            let func = self.func_id(FnKey::EnvDrop(self.f.body, e.id));
            self.emit1(InstKind::FuncRef(func), VTy::Int)
        } else {
            self.iconst(0)
        };
        let args = vec![g, entry, env, env_bytes, drop_fn, result_bytes, env_drop];
        let handle = self.emit1(InstKind::Native { name: "task.spawn", imm: 0, args }, VTy::Int);
        Some(Val::Regs(vec![handle]))
    }

    /// The `T` of a thread expression's `Spun[T]`.
    fn thread_result_ty(&mut self, e: &hir::Expr) -> Option<Ty> {
        let ty = self.ty_of(e);
        match self.tcx().kind(self.tcx().degauge(ty)).clone() {
            TyKind::Adt(_, args) if args.len() == 1 => Some(self.tcx().degauge(args[0])),
            _ => self.internal(e.span, "a thread whose type isn't a `Spun`"),
        }
    }

    pub(super) fn is_valueless(&self, ty: Ty) -> bool {
        matches!(self.tcx().kind(ty), TyKind::Unit | TyKind::Never)
    }

    /// The function a thread runs: `(result address, environment)`. It
    /// writes the value of the thread's expression at the address.
    pub(super) fn thread_func(&mut self, body: BodyId, id: HirId) -> Option<Vec<VTy>> {
        let info = self.threads.get(&(body, id)).cloned()?;
        let e = info.expr;
        let span = e.span;
        let hir::ExprKind::Thread { body: inner, .. } = &e.kind else {
            return self.internal(span, "a thread that isn't one");
        };
        let ret = self.thread_result_ty(e)?;
        self.f.scan = self.scan(body, inner);
        let abi = Abi { sret: Some(ret), env: true, params: Vec::new(), results: Vec::new() };
        let (sret, env, _) = self.entry_params(&abi);
        self.f.ret = Some(ret);
        self.f.sret = sret;
        let env = env?;
        for (k, &l) in info.captures.iter().enumerate() {
            let at = self.load(Mem::W64, env, 8 * k as i64, VTy::Ptr);
            if info.moves.contains(&l) {
                // Its own, in a slot of this frame, which it drops (pads
                // read only memory).
                let ty = self.local_ty(l);
                let (size, align) = self.size_align(ty)?;
                let slot = self.raw_slot(size.max(1), align.max(1));
                let dst = self.slot_addr(slot);
                self.emit(InstKind::Copy { dst, src: at, size }, &[]);
                self.f.locals.insert(l, Storage::Slot(slot));
                if self.needs_drop(ty) {
                    self.add_cleanup(CleanupKind::Drop { at: super::cleanup::Stable::Slot(slot), ty });
                }
            } else {
                self.f.locals.insert(l, Storage::At(at));
            }
        }
        let depth = self.open_scope();
        let valueless = self.is_valueless(ret);
        let val = self.expr(inner);
        if self.live() {
            let val = val?;
            let val = if valueless { Val::Regs(Vec::new()) } else { val };
            self.ret_val(val, span)?;
        }
        self.close_scope(depth);
        Some(Vec::new())
    }

    /// `loom for pat in lo..hi { body }` over an integer range, as the
    /// parent builds it: with one worker each item is a thread of its own
    /// (the reference schedule); with several, a task takes a run of items.
    /// `thread` is the body's `Spun` expression, the loop's only statement.
    pub(super) fn loom_for_range(
        &mut self,
        e: &'a hir::Expr,
        loop_: LoomFor<'a>,
        lo: &'a hir::Expr,
        hi: &'a hir::Expr,
    ) -> Option<()> {
        let LoomFor { pat, body, thread, group, int, elem } = loop_;
        let lo = self.value(lo)?[0];
        let hi = self.value(hi)?[0];
        // `n = if lo < hi { hi - lo } else { 0 }`
        let n = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(n, zero);
        let (some, sized) = (self.new_block(), self.new_block());
        let any = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: !int.signed, a: lo, b: hi }, VTy::Int);
        self.br(any, some, sized);
        self.enter(some);
        let size = self.uarith(IntOp::Sub, hi, lo);
        self.write(n, size);
        self.jump(sized);
        self.enter(sized);
        let n = self.read(n);
        let grain = self.emit1(InstKind::Native { name: "task.grain", imm: 0, args: vec![n] }, VTy::Int);
        let one = self.iconst(1);
        let single = self.ucmp(Cmp::Eq, grain, one);
        let (each, chunked, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(single, each, chunked);
        self.enter(each);
        self.range_loop(e, pat, body, lo, Some(hi), false, int, elem)?;
        if self.live() {
            self.jump(done);
        }
        self.enter(chunked);
        let var = match &pat.kind {
            hir::PatKind::Bind(l) => Some(*l),
            _ => None,
        };
        self.spawn_chunks(thread, group, n, lo, grain, var.into_iter().collect(), None)?;
        self.jump(done);
        self.enter(done);
        Some(())
    }

    /// Whether `iter` of a `loom for` can be counted before it starts and
    /// each item worked out from its number: a list, slice or array of
    /// elements that own nothing, or `chunks`/`chunks_mut` of one, with
    /// `.indexed()` or without. The patterns have to be names (or `_`) that
    /// the thread gets a copy of.
    pub(super) fn indexed_loom_for(
        &mut self,
        pat: &'a hir::Pat,
        iter: &'a hir::Expr,
        thread: &'a hir::Expr,
    ) -> Option<Indexed<'a>> {
        let (core, indexed) = match self.std_call(iter, "std.iter.Stream.indexed", 0) {
            Some((base, _)) => (base, true),
            None => (iter, false),
        };
        let pats: Vec<&'a hir::Pat> = match (&pat.kind, indexed) {
            (hir::PatKind::Tuple(items), true) if items.len() == 2 => items.iter().collect(),
            (_, false) => vec![pat],
            _ => return None,
        };
        let chunks = self
            .std_call(core, "std.slice.Slice.chunks", 1)
            .or_else(|| self.std_call(core, "std.slice.Slice.chunks_mut", 1));
        let (seq, width) = match chunks {
            Some((base, args)) => (base, Some(&args[0].value)),
            None => (core, None),
        };
        let seq_ty = self.coerced_ty(seq);
        let elem = self.seq_of(seq_ty)?.elem();
        let elem = self.tcx().degauge(elem);
        if width.is_none() && self.needs_drop(elem) {
            return None;
        }
        let item = *pats.last()?;
        let index = if indexed { Some(pats[0]) } else { None };
        let copies = self.tables().thread_copies.get(&thread.id)?;
        let captures = self.f.scan.captures.get(&thread.id);
        for p in index.into_iter().chain(Some(item)) {
            match &p.kind {
                hir::PatKind::Wild => {}
                hir::PatKind::Bind(l) => {
                    // An element a `mut` name stands for is the element itself.
                    if width.is_none() && std::ptr::eq(p, item) && self.p.local(*l).mutable {
                        return None;
                    }
                    if captures.is_some_and(|c| c.contains(l)) && !copies.contains(l) {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        Some(Indexed { seq, width, index, item: Some(item) })
    }

    /// The call `e` of the method at `path` with `args` arguments, on the
    /// expression before the dot; and its arguments.
    fn std_call(&self, e: &'a hir::Expr, path: &str, args: usize) -> Option<(&'a hir::Expr, &'a [hir::Arg])> {
        let hir::ExprKind::Call { callee, args: given } = &e.kind else { return None };
        let info = self.tables().calls.get(&e.id)?;
        if info.dynamic.is_some()
            || !info.through.is_empty()
            || info.receiver != Some(crate::typeck::Receiver::Expr)
            || given.len() != args
        {
            return None;
        }
        let hir::ExprKind::Field { base, .. } = &callee.kind else { return None };
        let def = self.typed.results.instance(info.callee).def;
        (self.p.def_path(def) == path).then_some((base, given.as_slice()))
    }

    /// Item number `k` of the sequence at `buf` of `len` elements: the
    /// element, or the view of the chunk (`width` elements, fewer at the end).
    fn indexed_item(
        &mut self,
        buf: Value,
        len: Value,
        width: Option<Value>,
        k: Value,
        elem: Ty,
        span: Span,
    ) -> Option<Val> {
        let Some(width) = width else {
            let at = self.elem_at(buf, k, elem)?;
            return self.load_val(at, 0, elem, span);
        };
        let start = self.uarith(IntOp::Mul, k, width);
        let rest = self.uarith(IntOp::Sub, len, start);
        let count = self.new_var(VTy::Int);
        self.write(count, width);
        let (short, join) = (self.new_block(), self.new_block());
        let fewer = self.ucmp(Cmp::Lt, rest, width);
        self.br(fewer, short, join);
        self.enter(short);
        self.write(count, rest);
        self.jump(join);
        self.enter(join);
        let ptr = self.elem_at(buf, start, elem)?;
        let count = self.read(count);
        Some(Val::Regs(vec![ptr, count]))
    }

    /// `loom for` over an [`Indexed`] source, as the parent builds it: with
    /// one worker each item is a thread of its own (the reference schedule);
    /// with several, a task takes a run of items and works each out itself.
    pub(super) fn loom_for_indexed(
        &mut self,
        e: &'a hir::Expr,
        source: Indexed<'a>,
        body: &'a hir::Block,
        thread: &'a hir::Expr,
        group: LocalId,
    ) -> Option<()> {
        let span = e.span;
        let Indexed { seq, width, index, item } = source;
        let (place, ty) = self.place_or_temp(seq)?;
        let v = self.read_place(&place, ty, span)?;
        let (buf, len) = self.view_of(&v, ty, span)?;
        let elem = self.tcx().degauge(self.seq_of(ty)?.elem());
        let (width, n) = match width {
            None => (None, len),
            Some(w) => {
                let width = self.value(w)?[0];
                let zero = self.iconst(0);
                let bad = self.ucmp(Cmp::Eq, width, zero);
                let (fail, ok) = (self.new_block(), self.new_block());
                self.br(bad, fail, ok);
                self.enter(fail);
                self.unravel_msg(&[super::text::Msg::Text("chunks of 0 elements")], span)?;
                self.enter(ok);
                // `ceil(len / width)`.
                let q = self.emit1(InstKind::Div { ty: super::seq::U64, a: len, b: width }, VTy::Int);
                let r = self.emit1(InstKind::Rem { ty: super::seq::U64, a: len, b: width }, VTy::Int);
                let more = self.ucmp(Cmp::Ne, r, zero);
                let n = self.uarith(IntOp::Add, q, more);
                (Some(width), n)
            }
        };
        let zero = self.iconst(0);
        let grain = self.emit1(InstKind::Native { name: "task.grain", imm: 0, args: vec![n] }, VTy::Int);
        let one = self.iconst(1);
        let single = self.ucmp(Cmp::Eq, grain, one);
        let (each, chunked, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(single, each, chunked);
        // One task for each item, started by the parent as it goes.
        self.enter(each);
        self.indexed_loop(e, (index, item), body, (buf, len, width, n), elem)?;
        if self.live() {
            self.jump(done);
        }
        self.enter(chunked);
        let local = |p: Option<&hir::Pat>| match p.map(|p| &p.kind) {
            Some(hir::PatKind::Bind(l)) => Some(*l),
            _ => None,
        };
        let (index, item) = (local(index), local(item));
        let src = SrcArgs { buf, len, width, elem, index, item };
        let vars = index.into_iter().chain(item).collect();
        self.spawn_chunks(thread, group, n, zero, grain, vars, Some(src))?;
        self.jump(done);
        self.enter(done);
        Some(())
    }

    /// The parent's own loop over an [`Indexed`] source: item `k` of the `n`,
    /// bound to the patterns, then the body.
    fn indexed_loop(
        &mut self,
        e: &'a hir::Expr,
        pats: (Option<&'a hir::Pat>, Option<&'a hir::Pat>),
        body: &'a hir::Block,
        (buf, len, width, n): (Value, Value, Option<Value>, Value),
        elem: Ty,
    ) -> Option<()> {
        let span = e.span;
        let k = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(k, zero);
        let (head, body_b, step, exit) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(k);
        let more = self.ucmp(Cmp::Lt, cur, n);
        self.br(more, body_b, exit);
        self.enter(body_b);
        let scopes = self.f.scopes.len();
        self.f.loops.push(super::LoopCtx {
            id: e.id,
            brk: exit,
            cont: step,
            result: super::LoopResult::None,
            scopes,
            brk_scopes: scopes,
        });
        let cur = self.read(k);
        let depth = self.open_scope();
        let mut done = Some(());
        if let Some(p) = pats.0 {
            let usize_ty = self.tcx().usize;
            done = self.bind_irrefutable(p, Val::Regs(vec![cur]), usize_ty);
        }
        if done.is_some()
            && let Some(p) = pats.1
            && let hir::PatKind::Bind(l) = &p.kind
        {
            let val = self.indexed_item(buf, len, width, cur, elem, span)?;
            let ty = self.local_ty(*l);
            done = self.bind_irrefutable(p, val, ty);
        }
        if done.is_some() {
            let join = self.join(self.tcx().unit, span)?;
            done = self.block_into(body, &join);
        }
        self.close_scope(depth);
        if self.live() && done.is_some() {
            self.jump(step);
        }
        self.f.loops.pop();
        self.enter(step);
        if self.live() {
            let cur = self.read(k);
            let one = self.iconst(1);
            let next = self.uarith(IntOp::Add, cur, one);
            self.write(k, next);
            self.jump(head);
        }
        self.seal(head);
        self.enter(exit);
        done
    }

    /// Spawns the tasks of a chunked `loom for` of `n` items numbered from
    /// `lo`, `grain` to a task, and leaves the builder after the last.
    #[allow(clippy::too_many_arguments)]
    fn spawn_chunks(
        &mut self,
        thread: &'a hir::Expr,
        group: LocalId,
        n: Value,
        lo: Value,
        grain: Value,
        vars: Vec<LocalId>,
        src: Option<SrcArgs>,
    ) -> Option<()> {
        let zero = self.iconst(0);
        // `start` is how many items have gone to tasks.
        let start = self.new_var(VTy::Int);
        self.write(start, zero);
        let (head, next, exit) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let at = self.read(start);
        let more = self.ucmp(Cmp::Lt, at, n);
        self.br(more, next, exit);
        self.enter(next);
        let at = self.read(start);
        let left = self.uarith(IntOp::Sub, n, at);
        let count = self.new_var(VTy::Int);
        self.write(count, grain);
        let (short, spawn) = (self.new_block(), self.new_block());
        let fewer = self.ucmp(Cmp::Lt, left, grain);
        self.br(fewer, short, spawn);
        self.enter(short);
        self.write(count, left);
        self.jump(spawn);
        self.enter(spawn);
        let first = self.uarith(IntOp::Add, lo, at);
        let count = self.read(count);
        self.spawn_thread(
            thread,
            Some(group),
            Some(ChunkArgs { vars: vars.clone(), lo: first, count, src: src.clone() }),
        )?;
        let at = self.read(start);
        let after = self.uarith(IntOp::Add, at, grain);
        self.write(start, after);
        self.jump(head);
        self.seal(head);
        self.enter(exit);
        Some(())
    }

    /// The function of a chunk task: `(result address, environment)`. It
    /// runs the thread's function for each item of the run, stopping at the
    /// next item if the task has been cancelled.
    pub(super) fn chunk_func(&mut self, body: BodyId, id: HirId) -> Option<Vec<VTy>> {
        let info = self.threads.get(&(body, id)).cloned()?;
        let chunk = *self.chunks.get(&(body, id))?;
        let span = info.expr.span;
        let abi = Abi { sret: Some(self.tcx().unit), env: true, params: Vec::new(), results: Vec::new() };
        let (sret, env, _) = self.entry_params(&abi);
        let (sret, env) = (sret?, env?);
        let lo = self.load(Mem::W64, env, chunk.lo as i64, VTy::Int);
        let count = self.load(Mem::W64, env, chunk.count as i64, VTy::Int);
        let thread = self.func_id(FnKey::Thread(body, id));
        let src = match chunk.src {
            Some(src) => {
                let buf = self.load(Mem::W64, env, src.buf as i64, VTy::Ptr);
                let len = self.load(Mem::W64, env, src.len as i64, VTy::Int);
                let width = src.width.map(|off| self.load(Mem::W64, env, off as i64, VTy::Int));
                Some((src, buf, len, width))
            }
            None => None,
        };
        let item = |this: &mut Self, k: Value| {
            let value = this.uarith(IntOp::Add, lo, k);
            if let Some((off, ty)) = chunk.item {
                let place = Place::At { ptr: env, off: off as i64 };
                this.write_place(&place, ty, Val::Regs(vec![value]), span)?;
            }
            if let Some((src, buf, len, width)) = src {
                if let Some(off) = src.index {
                    let place = Place::At { ptr: env, off: off as i64 };
                    let usize_ty = this.tcx().usize;
                    this.write_place(&place, usize_ty, Val::Regs(vec![value]), span)?;
                }
                if let Some((off, ty)) = src.item {
                    let val = this.indexed_item(buf, len, width, value, src.elem, span)?;
                    let place = Place::At { ptr: env, off: off as i64 };
                    this.write_place(&place, ty, val, span)?;
                }
            }
            this.emit(InstKind::Call { func: thread, args: vec![sret, env] }, &[]);
            Some(())
        };
        // The run has an item at least.
        let first = self.iconst(0);
        item(self, first)?;
        let one = self.iconst(1);
        self.count_loop(one, count, |this, k| {
            this.emit(InstKind::Native { name: "task.cancelled", imm: 0, args: Vec::new() }, &[]);
            item(this, k)
        })?;
        self.terminate(crate::heddle::Term::Ret(Vec::new()));
        Some(Vec::new())
    }

    /// What a thread that never ran owns, in its environment: dropped.
    pub(super) fn env_drop_func(&mut self, body: BodyId, id: HirId) -> Option<Vec<VTy>> {
        let info = self.threads.get(&(body, id)).cloned()?;
        let env = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.push(env);
        for (off, ty) in info.owned {
            let at = self.offset(env, off as i64);
            self.drop_at(at, ty);
        }
        self.terminate(crate::heddle::Term::Ret(Vec::new()));
        Some(Vec::new())
    }

    /// `Spun.join`: waits for the thread, which writes its result at the
    /// place this method's result goes.
    pub(super) fn spun_join(&mut self, handle: &Val, spun: Ty, ret: Ty, sret: Option<Value>, span: Span) -> Option<()> {
        let h = self.regs(handle.clone(), spun, span)?[0];
        let join = |this: &mut Self, dest: Value| {
            this.emit(InstKind::Native { name: "task.join", imm: 0, args: vec![h, dest] }, &[]);
        };
        match sret {
            Some(ptr) => {
                join(self, ptr);
                self.terminate(crate::heddle::Term::Ret(Vec::new()));
            }
            None if self.is_valueless(ret) => {
                let none = self.iconst(0);
                join(self, none);
                self.terminate(crate::heddle::Term::Ret(Vec::new()));
            }
            None => {
                let (size, align) = self.size_align(ret)?;
                let slot = self.raw_slot(size, align);
                let dest = self.slot_addr(slot);
                join(self, dest);
                let val = self.load_val(dest, 0, ret, span)?;
                let regs = self.regs(val, ret, span)?;
                self.terminate(crate::heddle::Term::Ret(regs));
            }
        }
        Some(())
    }
}
