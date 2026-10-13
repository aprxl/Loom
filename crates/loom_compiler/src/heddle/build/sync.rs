//! `Atomic[T]` (std.sync, design/vm.md §11).
//!
//! An atomic is `T`'s bytes in memory (a slot, or inside whatever holds it),
//! and every operation is a native `atomic.*` on its address: opaque to the
//! optimizer (a `load` isn't hoisted out of a loop, or forwarded from an
//! earlier store), and one place for several workers to make real atomic
//! operations of.

use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, int_spec};
use crate::heddle::{InstKind, Mem, Term, VTy, Value};
use crate::hir;
use crate::ty::{Ty, TyKind};

/// The `#builtin` methods of `Atomic` the builder writes.
pub(super) fn writes_body(path: &str) -> bool {
    path.starts_with("std.sync.Atomic.")
}

impl<'a> Builder<'a> {
    /// `Atomic[T](value)`: a temporary holding the value.
    pub(super) fn sync_new(&mut self, e: &'a hir::Expr, args: &'a [hir::Arg], ty: Ty) -> Option<Val> {
        let span = e.span;
        let t = self.atomic_elem(ty, span)?;
        let Some(arg) = args.first() else { return self.internal(span, "an atomic made from nothing") };
        let value = self.expr(&arg.value)?;
        let at = self.temp(ty, span)?;
        // A mutex has its lock word first (free), then the value.
        let off = if self.is_mutex(ty) { 8 } else { 0 };
        if off != 0 {
            let free = self.iconst(0);
            self.store(Mem::W64, at, 0, free);
        }
        // What the value owns is the mutex's now.
        self.move_to(at, off, value, t, span)?;
        Some(Val::Mem(at))
    }

    fn is_mutex(&self, ty: Ty) -> bool {
        matches!(self.tcx().kind(self.tcx().degauge(ty)), TyKind::Adt(def, _)
            if self.p.def_path(*def) == "std.sync.Mutex")
    }

    /// `m.lock()`: takes the lock (waiting while another thread has it),
    /// and is the value's place until the statement ends, however it ends:
    /// a cleanup gives the lock back.
    pub(super) fn mutex_lock(&mut self, e: &'a hir::Expr) -> Option<(super::value::Place, Ty)> {
        let span = e.span;
        let hir::ExprKind::Call { callee, args } = &e.kind else { return None };
        let info = self.tables().calls.get(&e.id)?.clone();
        let sig = self.typed.results.sigs.get(&info.callee)?.clone();
        let modes: Vec<loom_syntax::ast::ParamMode> = sig.params.iter().map(|p| p.mode).collect();
        let recv_ty = self.plain_ty(sig.params.first()?.ty);
        let t = self.atomic_elem(recv_ty, span)?;
        let worked = self.call_args(callee, args, &info, &modes, Some(recv_ty))?;
        let at = match worked.into_iter().next()? {
            super::call::Arg::Place(place) => self.place_addr(&place, span)?,
            super::call::Arg::Val(v, ty) => self.addr_of(v, ty, span)?,
        };
        self.emit(InstKind::Native { name: "mutex.lock", imm: 0, args: vec![at] }, &[]);
        // The mutex's address, kept in memory for the cleanup, which an
        // unwind runs from a landing pad (they read only memory).
        let slot = self.raw_slot(8, 8);
        let slot_at = self.slot_addr(slot);
        self.store(Mem::W64, slot_at, 0, at);
        self.add_cleanup(super::cleanup::CleanupKind::Unlock { slot });
        let value = self.offset(at, 8);
        Some((super::value::Place::At { ptr: value, off: 0 }, t))
    }

    /// Gives back the lock of the mutex whose address is in `slot`.
    pub(super) fn emit_unlock(&mut self, slot: u32) {
        let at = self.slot_addr(slot);
        let m = self.load(Mem::W64, at, 0, VTy::Ptr);
        self.emit(InstKind::Native { name: "mutex.unlock", imm: 0, args: vec![m] }, &[]);
    }

    /// The `T` of an `Atomic[T]`.
    fn atomic_elem(&mut self, ty: Ty, span: Span) -> Option<Ty> {
        match self.tcx().kind(self.tcx().degauge(ty)).clone() {
            TyKind::Adt(_, args) if args.len() == 1 => Some(self.tcx().degauge(args[0])),
            _ => self.internal(span, "an atomic whose type isn't an `Atomic[T]`"),
        }
    }

    /// The body of an `Atomic` method: a native on the address of `self`.
    pub(super) fn atomic_method(&mut self, path: &str, args: &[Val], tys: &[Ty], span: Span) -> Option<()> {
        let Val::Mem(at) = args[0] else { return self.internal(span, "an atomic in registers") };
        let t = self.atomic_elem(tys[0], span)?;
        // A `bool` is a byte, as a `u8` is.
        let spec = match self.int_of(t) {
            Some(int) => int_spec(int).pack(),
            None => loom_isa::IntSpec { bits: 8, signed: false, size: false }.pack(),
        };
        let spec = self.iconst(spec);
        let value = |this: &mut Self, k: usize| -> Option<Value> { Some(this.regs(args[k].clone(), t, span)?[0]) };
        let name = path.rsplit('.').next().unwrap_or_default();
        let (native, native_args, results): (&'static str, Vec<Value>, usize) = match name {
            "load" => ("atomic.load", vec![at, spec], 1),
            "store" => ("atomic.store", vec![at, value(self, 1)?, spec], 0),
            "swap" => ("atomic.swap", vec![at, value(self, 1)?, spec], 1),
            "compare_exchange" => {
                let (expected, desired) = (value(self, 1)?, value(self, 2)?);
                ("atomic.cas", vec![at, expected, desired, spec], 2)
            }
            "add" => ("atomic.add", vec![at, value(self, 1)?, spec], 1),
            "sub" => ("atomic.sub", vec![at, value(self, 1)?, spec], 1),
            _ => return self.internal(span, &format!("no body for `Atomic.{name}`")),
        };
        let out = self.emit(InstKind::Native { name: native, imm: 0, args: native_args }, &vec![VTy::Int; results]);
        self.terminate(Term::Ret(out));
        Some(())
    }
}
