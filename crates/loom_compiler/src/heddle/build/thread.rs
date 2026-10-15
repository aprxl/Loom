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
use super::value::{Storage, Val};
use super::{Abi, Builder, FnKey};
use crate::heddle::{Cmp, InstKind, Mem, VTy, Value};
use crate::hir::{self, HirId, LocalId};
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
        if let Some(arg) = args.first() {
            return self.unsupported(arg.span, "`loom(given: …)`");
        }
        let slot = self.raw_slot(8, 8);
        let g = self.emit1(InstKind::Native { name: "loom.new", imm: 0, args: Vec::new() }, VTy::Int);
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
    pub(super) fn thread_expr(&mut self, e: &'a hir::Expr, group: Option<LocalId>, body: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        let Some(group) = group else { return self.internal(span, "a thread outside a loom") };
        let result_ty = self.thread_result_ty(e)?;
        let captures = self.f.scan.captures.get(&e.id).cloned().unwrap_or_default();
        let copies = self.tables().thread_copies.get(&e.id).cloned().unwrap_or_default();
        let moves = self.tables().thread_moves.get(&e.id).cloned().unwrap_or_default();
        let _ = body;
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
        let env_bytes = tail;
        let env = if env_bytes == 0 {
            self.iconst(0)
        } else {
            let (n, a) = (self.iconst(env_bytes), self.iconst(8));
            self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr)
        };
        for (k, &l) in captures.iter().enumerate() {
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
        let has_owned = !owned.is_empty();
        self.threads.insert((self.f.body, e.id), ThreadInfo { expr: e, captures, moves, owned });
        let entry = self.func_id(FnKey::Thread(self.f.body, e.id));
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
