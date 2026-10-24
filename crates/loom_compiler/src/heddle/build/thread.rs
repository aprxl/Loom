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
    /// The `loom for` pattern's local, if it has one.
    var: Option<LocalId>,
    lo: Value,
    count: Value,
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
            let item = c.var.and_then(|v| {
                let k = captures.iter().position(|&l| l == v)?;
                Some((copy_at[k]?.0, self.local_ty(v)))
            });
            ChunkEnv { lo: tail, count: tail + 8, item }
        });
        let env_bytes = if chunk.is_some() { tail + 16 } else { tail };
        let env = if env_bytes == 0 {
            self.iconst(0)
        } else {
            let (n, a) = (self.iconst(env_bytes), self.iconst(8));
            self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr)
        };
        for (k, &l) in captures.iter().enumerate() {
            if chunk.as_ref().is_some_and(|c| c.var == Some(l)) {
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
        self.spawn_thread(thread, Some(group), Some(ChunkArgs { var, lo: first, count }))?;
        let at = self.read(start);
        let after = self.uarith(IntOp::Add, at, grain);
        self.write(start, after);
        self.jump(head);
        self.seal(head);
        self.enter(exit);
        self.jump(done);
        self.enter(done);
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
        let item = |this: &mut Self, k: Value| {
            if let Some((off, ty)) = chunk.item {
                let value = this.uarith(IntOp::Add, lo, k);
                let place = Place::At { ptr: env, off: off as i64 };
                this.write_place(&place, ty, Val::Regs(vec![value]), span)?;
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
