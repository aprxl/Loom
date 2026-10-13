//! Cleanups: the drops and `defer`s that run when a scope ends, however it
//! ends (design/vm.md §9.4).
//!
//! Each scope (a block, a statement's temporaries, a function) keeps its
//! cleanups on one stack. Leaving a scope normally, or by `break`,
//! `continue`, `return`, `?` or `fail`, emits the cleanups being left, the
//! latest first. Unwinding runs the same code from a *landing pad*: an
//! instruction that can unravel names the pad for the cleanups live where
//! it is. A pad runs the latest cleanup and goes on to the pad for the rest,
//! and the last one ends with `resume`. Pads are made when first needed and
//! shared by every instruction with the same live cleanups; they read only
//! memory (frame slots, and addresses the function was given), so they
//! work from wherever they're reached.
//!
//! A cleanup made dead (a temporary that was moved away) is left out of
//! pads made from then on, and isn't emitted when its scope ends.
//!
//! Cleanups are decided while building, and code runs conditionally: the
//! right side of an `and`, a `mend`'s left side before a `?` jumps out. So
//! every slot a drop is registered for is zero whenever it holds nothing:
//! it's zeroed when the function starts, a cleanup zeroes it after
//! dropping, and taking a value from it zeroes it. Dropping zero does
//! nothing, so a cleanup that runs for a value that was never made (or is
//! gone) is harmless.

use loom_syntax::source::Span;

use super::Builder;
use crate::heddle::{BlockId, InstKind, Term, VTy, Value};
use crate::hir;
use crate::ty::Ty;
use crate::typeck::BodyId;

/// An address a landing pad can compute on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stable {
    Slot(u32),
    /// A function parameter's value: an address the caller gave.
    Param(Value),
}

#[derive(Clone, Debug)]
pub(super) enum CleanupKind<'a> {
    /// Drops the value of `ty` at `at`.
    Drop { at: Stable, ty: Ty },
    /// Runs a `defer`'s body, whose tables are `tables`. A fallback
    /// `defer x =>` runs only while the byte in slot `flag` is set.
    Defer { body: &'a hir::Expr, tables: BodyId, flag: Option<u32>, span: Span },
    /// In a shuttle's step: slot `n`'s value matters until here (see
    /// `heddle::frame`).
    Life(u32),
    /// Frees the box of a shuttle at this address (see `shuttle.rs`).
    FreeShuttle(Value),
    /// Frees the `size` bytes, aligned to 8, at this address: a stream's
    /// box (see `stream.rs`).
    Free { at: Value, size: u64 },
    /// Finishes the projection (a shuttle of type `ty`) in slot `slot`,
    /// which lent a place for the statement, then drops it; unwinding, it
    /// only drops it (see `shuttle.rs`).
    Lend { slot: u32, ty: Ty },
    /// Ends the loom block whose group is in this slot (see `thread.rs`).
    Loom { slot: u32 },
    /// Gives back the lock of the mutex whose address is in this slot (see
    /// `sync.rs`).
    Unlock { slot: u32 },
}

#[derive(Clone, Debug)]
pub(super) struct Cleanup<'a> {
    pub id: u32,
    pub kind: CleanupKind<'a>,
    pub live: bool,
}

impl<'a> Builder<'a> {
    /// Adds a cleanup at the top of the stack; its id.
    pub(super) fn add_cleanup(&mut self, kind: CleanupKind<'a>) -> u32 {
        self.note_owned_slot(&kind);
        let id = self.f.next_cleanup;
        self.f.next_cleanup += 1;
        self.f.cleanups.push(super::cleanup::Cleanup { id, kind, live: true });
        id
    }

    /// Adds a cleanup at the end of the innermost block scope, under the
    /// current statement's temporaries: a local declared by the statement.
    pub(super) fn add_scope_cleanup(&mut self, kind: CleanupKind<'a>) -> u32 {
        self.note_owned_slot(&kind);
        let id = self.f.next_cleanup;
        self.f.next_cleanup += 1;
        // Just under the statement's scope, which then starts after it.
        match self.f.stmt_start.filter(|&depth| depth >= 1 && depth <= self.f.scopes.len()) {
            Some(depth) => {
                let at = self.f.scopes[depth - 1];
                self.f.cleanups.insert(at, Cleanup { id, kind, live: true });
                // Everything above moved up one: the statement's scope and
                // the scopes opened inside it (a loop's body, say) start
                // after the new cleanup.
                for start in &mut self.f.scopes[depth - 1..] {
                    if *start >= at {
                        *start += 1;
                    }
                }
            }
            None => self.f.cleanups.push(Cleanup { id, kind, live: true }),
        }
        id
    }

    /// A slot a drop is registered for is zeroed when the function starts.
    fn note_owned_slot(&mut self, kind: &CleanupKind<'a>) {
        if let CleanupKind::Drop { at: Stable::Slot(n), ty } = *kind
            && !self.f.owned_slots.iter().any(|&(s, _)| s == n)
        {
            self.f.owned_slots.push((n, ty));
        }
    }

    /// Zeroes, at the start of the function, every slot a drop was
    /// registered for (see the module's documentation).
    pub(super) fn zero_owned_slots(&mut self) {
        let mut insts = Vec::new();
        for (n, ty) in std::mem::take(&mut self.f.owned_slots) {
            let Some((size, _)) = self.size_align(ty) else { continue };
            let ptr = self.new_value(VTy::Ptr);
            insts.push(crate::heddle::Inst {
                kind: InstKind::Slot(n),
                results: vec![ptr],
                span: None,
                unwind: None,
                site: None,
            });
            insts.push(crate::heddle::Inst {
                kind: InstKind::Zero { dst: ptr, size },
                results: Vec::new(),
                span: None,
                unwind: None,
                site: None,
            });
        }
        let entry = &mut self.f.blocks[0].insts;
        entry.splice(0..0, insts);
    }

    /// Takes cleanup `id` out: what it would clean up belongs elsewhere now.
    pub(super) fn kill_cleanup(&mut self, id: u32) {
        if let Some(c) = self.f.cleanups.iter_mut().find(|c| c.id == id) {
            c.live = false;
        }
    }

    /// Opens a scope; its depth, for [`close_scope`](Self::close_scope).
    pub(super) fn open_scope(&mut self) -> usize {
        self.f.scopes.push(self.f.cleanups.len());
        self.f.scopes.len()
    }

    /// Ends the innermost scope (at `depth`): emits its live cleanups, if
    /// control reaches its end, and forgets them.
    pub(super) fn close_scope(&mut self, depth: usize) {
        debug_assert_eq!(self.f.scopes.len(), depth);
        let start = self.f.scopes.pop().unwrap_or(0);
        if self.live() {
            let saved = std::mem::replace(&mut self.f.closing, true);
            self.run_cleanups(start);
            self.f.closing = saved;
        }
        self.f.cleanups.truncate(start);
    }

    /// Emits every live cleanup from index `from` of the stack up, the
    /// latest first, leaving them on the stack: for leaving scopes early.
    pub(super) fn run_cleanups(&mut self, from: usize) {
        let mut i = self.f.cleanups.len();
        while i > from {
            i -= 1;
            if !self.live() {
                return;
            }
            let c = self.f.cleanups[i].clone();
            if c.live {
                // While it runs, only what's under it is live.
                let saved = self.f.cleanups.split_off(i);
                self.emit_cleanup(&c.kind);
                self.f.cleanups.extend(saved);
            }
        }
    }

    /// Where the scope at `depth` (as [`open_scope`](Self::open_scope)
    /// gave) starts on the cleanup stack.
    pub(super) fn scope_start(&self, depth: usize) -> usize {
        self.f.scopes.get(depth - 1).copied().unwrap_or(0)
    }

    // ------------------------------------------------------------ landing pads

    /// The landing pad for the cleanups live now, if any are.
    pub(super) fn pad(&mut self) -> Option<BlockId> {
        let live: Vec<usize> = (0..self.f.cleanups.len()).filter(|&i| self.f.cleanups[i].live).collect();
        if live.is_empty() {
            return None;
        }
        Some(self.pad_for(&live))
    }

    /// The pad that runs the cleanups at `live` (indices into the stack),
    /// the last first.
    fn pad_for(&mut self, live: &[usize]) -> BlockId {
        let key: Vec<u32> = live.iter().map(|&i| self.f.cleanups[i].id).collect();
        if let Some(&b) = self.f.pads.get(&key) {
            return b;
        }
        let (&last, rest) = live.split_last().expect("not empty");
        let outer = if rest.is_empty() { None } else { Some(self.pad_for(rest)) };
        let block = self.new_block();
        self.f.sealed[block.0 as usize] = true;
        // Build it off to the side, with only the cleanups under this one
        // live: an unwind inside it goes to the next pad out.
        let saved_cur = self.f.cur.replace(block);
        let saved_span = self.f.span;
        let saved_stack = std::mem::take(&mut self.f.cleanups);
        let saved_scopes = std::mem::take(&mut self.f.scopes);
        let saved_stmt = self.f.stmt_start.take();
        let saved_loops = std::mem::take(&mut self.f.loops);
        let saved_temps = std::mem::take(&mut self.f.temps);
        self.f.cleanups = rest.iter().map(|&i| saved_stack[i].clone()).collect();
        let kind = saved_stack[last].kind.clone();
        self.f.in_pad += 1;
        self.emit_cleanup(&kind);
        self.f.in_pad -= 1;
        if self.live() {
            match outer {
                Some(o) => self.jump(o),
                None => self.terminate(Term::Resume),
            }
        }
        self.f.cur = saved_cur;
        self.f.span = saved_span;
        self.f.cleanups = saved_stack;
        self.f.scopes = saved_scopes;
        self.f.stmt_start = saved_stmt;
        self.f.loops = saved_loops;
        self.f.temps = saved_temps;
        self.f.pads.insert(key, block);
        block
    }

    // ------------------------------------------------------------ what cleanups do

    fn stable_addr(&mut self, at: Stable) -> Value {
        match at {
            Stable::Slot(n) => self.slot_addr(n),
            Stable::Param(v) => v,
        }
    }

    fn emit_cleanup(&mut self, kind: &CleanupKind<'a>) {
        match kind {
            CleanupKind::Drop { at, ty } => {
                let ptr = self.stable_addr(*at);
                self.drop_at(ptr, *ty);
                // Back to holding nothing.
                if let Stable::Slot(_) = at
                    && let Some((size, _)) = self.size_align(*ty)
                {
                    self.emit(InstKind::Zero { dst: ptr, size }, &[]);
                }
            }
            CleanupKind::Defer { body, tables, flag, span } => {
                // User code: a loom left inside it is left early, even when
                // the scope's end is what runs this.
                let saved_closing = std::mem::replace(&mut self.f.closing, false);
                let saved_body = std::mem::replace(&mut self.f.body, *tables);
                let saved_span = self.f.span.replace(*span);
                let run = match flag {
                    Some(slot) => {
                        let ptr = self.slot_addr(*slot);
                        let set = self.load(crate::heddle::Mem::U8, ptr, 0, VTy::Int);
                        let (yes, done) = (self.new_block(), self.new_block());
                        self.br(set, yes, done);
                        self.enter(yes);
                        Some(done)
                    }
                    None => None,
                };
                // The body is a statement of its own: its temporaries go at its end.
                let depth = self.open_scope();
                let done_body = self.expr(body).is_some();
                let _ = done_body;
                self.close_scope(depth);
                if let Some(done) = run {
                    if self.live() {
                        self.jump(done);
                    }
                    self.enter(done);
                }
                self.f.span = saved_span;
                self.f.body = saved_body;
                self.f.closing = saved_closing;
            }
            CleanupKind::Life(n) => {
                self.emit(InstKind::Life { slot: *n, end: true }, &[]);
            }
            CleanupKind::FreeShuttle(b) => self.free_shuttle_box(*b),
            CleanupKind::Lend { slot, ty } => self.finish_lend(*slot, *ty),
            CleanupKind::Loom { slot } => self.emit_loom_end(*slot),
            CleanupKind::Unlock { slot } => self.emit_unlock(*slot),
            CleanupKind::Free { at, size } => {
                let (n, a) = (self.iconst(*size), self.iconst(8));
                self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![*at, n, a] }, &[]);
            }
        }
    }

    /// Drops the value of `ty` at `ptr`.
    pub(super) fn drop_at(&mut self, ptr: Value, ty: Ty) {
        if !self.needs_drop(ty) {
            return;
        }
        // Text, and a list of plain data: freeing the buffer, here.
        let ty = self.tcx().degauge(ty);
        if self.is_string(ty) {
            self.free_buffer(ptr, 1, 1);
            return;
        }
        if let Some(crate::typeck::Seq::List(elem)) = self.seq_of(ty)
            && !self.needs_drop(elem)
            && let Some((size, align)) = self.size_align(elem)
        {
            self.free_buffer(ptr, size, align);
            return;
        }
        let func = self.glue(super::Glue::Drop, ty);
        let pad = self.pad();
        let span = self.f.span;
        let b = self.cur_block();
        self.f.blocks[b.0 as usize].insts.push(crate::heddle::Inst {
            kind: InstKind::Call { func, args: vec![ptr] },
            results: Vec::new(),
            span,
            unwind: pad,
            site: None,
        });
    }

    /// The span of the statement being built, for messages.
    pub(super) fn here(&self) -> Span {
        self.f.span.or(self.main_span).expect("set before anything is built")
    }
}
