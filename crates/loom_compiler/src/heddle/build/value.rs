//! Values while building: in registers or in memory, where locals live, and
//! moving values from one place to another.
//!
//! A value in registers is its parts ([`Val::Regs`]); one in memory is the
//! address it's at ([`Val::Mem`]). A value in memory that an expression
//! made (a call's result, a construction) is a *temporary*: it owns what's
//! in it, and a cleanup drops it at the end of the statement unless
//! something takes it first. Taking a value that owns memory from a place
//! (a variable, a field) copies its bytes and zeroes the place, and zero is
//! a value every drop leaves alone: so drops can run however control goes,
//! with nothing to track.

use loom_syntax::source::Span;

use super::repr::{Part, Repr};
use super::{Builder, Vals, Var};
use crate::heddle::{InstKind, Mem, VTy, Value};
use crate::hir::LocalId;
use crate::ty::Ty;

/// A value being worked with.
#[derive(Clone, Debug)]
pub(super) enum Val {
    /// In registers: one value per part.
    Regs(Vals),
    /// In memory, at this address.
    Mem(Value),
}

/// Where a local lives.
#[derive(Clone, Debug)]
pub(super) enum Storage {
    /// In registers, as SSA variables, one per part.
    Ssa(Vec<Var>),
    /// In a frame slot.
    Slot(u32),
    /// At an address that differs from time to time (a loop's item, a
    /// pattern's binding of part of a place), kept in a variable.
    Ref(Var),
    /// At a fixed address: a parameter given by address, or a captured
    /// variable's address from a lambda's environment.
    At(Value),
}

/// Something that can be written: a local's variables (or some of them),
/// or memory.
#[derive(Clone, Debug)]
pub(super) enum Place {
    Vars(Vec<Var>),
    At { ptr: Value, off: i64 },
}

impl<'a> Builder<'a> {
    // ------------------------------------------------------------ slots and addresses

    /// A new frame slot for a value of `ty`.
    pub(super) fn new_slot(&mut self, ty: Ty, span: Span) -> Option<u32> {
        let Some((size, align)) = self.size_align(ty) else {
            let shown = self.tcx().display(self.p, ty);
            return self.unsupported(span, &format!("values of type `{shown}`"));
        };
        Some(self.raw_slot(size, align))
    }

    /// A new frame slot of `size` bytes aligned to `align`.
    pub(super) fn raw_slot(&mut self, size: u64, align: u64) -> u32 {
        let n = self.f.slots.len() as u32;
        self.f.slots.push(crate::heddle::Slot { size, align });
        // In a shuttle's step, where its value starts and (at the latest)
        // stops mattering: the end of the statement (see `heddle::frame`).
        if self.f.shuttle.is_some() {
            self.emit(InstKind::Life { slot: n, end: false }, &[]);
            let id = self.add_cleanup(super::cleanup::CleanupKind::Life(n));
            if let Some(sb) = self.f.shuttle.as_mut() {
                sb.lives.insert(n, id);
            }
        }
        n
    }

    /// In a shuttle's step, temporary `ptr`'s value is gone (moved out):
    /// its slot stops mattering here, not at the end of the statement.
    pub(super) fn temp_gone(&mut self, ptr: Value) {
        let Some(slot) = self.slot_of(ptr) else { return };
        let Some(sb) = self.f.shuttle.as_mut() else { return };
        if !sb.temps.remove(&ptr) {
            return;
        }
        if let Some(id) = sb.lives.remove(&slot) {
            self.kill_cleanup(id);
            self.emit(InstKind::Life { slot, end: true }, &[]);
        }
    }

    /// In a shuttle's step, a local's slot `slot` matters until the end of
    /// its block: its address may be held by something until then.
    fn local_slot_lives(&mut self, slot: u32) {
        if self.f.shuttle.is_some() {
            self.add_scope_cleanup(super::cleanup::CleanupKind::Life(slot));
        }
    }

    /// The address of slot `n`.
    pub(super) fn slot_addr(&mut self, n: u32) -> Value {
        let v = self.emit1(InstKind::Slot(n), VTy::Ptr);
        self.f.slot_of.insert(v, n);
        v
    }

    /// `ptr + off`.
    pub(super) fn offset(&mut self, ptr: Value, off: i64) -> Value {
        if off == 0 {
            return ptr;
        }
        self.emit1(InstKind::Offset { ptr, off }, VTy::Ptr)
    }

    // ------------------------------------------------------------ loads and stores

    pub(super) fn load_parts(&mut self, ptr: Value, off: i64, parts: &[Part]) -> Vals {
        parts
            .iter()
            .map(|p| self.emit1(InstKind::Load { mem: p.mem, ptr, off: (off + p.off as i64) as i32 }, p.vty))
            .collect()
    }

    pub(super) fn store_parts(&mut self, ptr: Value, off: i64, parts: &[Part], vals: &[Value]) {
        for (p, &v) in parts.iter().zip(vals) {
            self.emit(InstKind::Store { mem: p.mem, ptr, off: (off + p.off as i64) as i32, val: v }, &[]);
        }
    }

    /// The value of type `ty` at `ptr + off`: loaded, if it lives in
    /// registers.
    pub(super) fn load_val(&mut self, ptr: Value, off: i64, ty: Ty, span: Span) -> Option<Val> {
        match self.repr_of(ty, span)? {
            Repr::Regs(parts) => Some(Val::Regs(self.load_parts(ptr, off, &parts))),
            Repr::Mem { .. } => Some(Val::Mem(self.offset(ptr, off))),
        }
    }

    /// Writes `val`, of type `ty`, at `ptr + off`: its parts, or its bytes.
    /// Nothing is dropped or zeroed (see [`move_to`](Self::move_to)).
    pub(super) fn store_val(&mut self, ptr: Value, off: i64, ty: Ty, val: &Val, span: Span) -> Option<()> {
        match (self.repr_of(ty, span)?, val) {
            (Repr::Regs(parts), Val::Regs(vals)) => self.store_parts(ptr, off, &parts, vals),
            (Repr::Regs(parts), &Val::Mem(src)) => {
                let vals = self.load_parts(src, 0, &parts);
                self.store_parts(ptr, off, &parts, &vals);
            }
            (Repr::Mem { size, .. }, &Val::Mem(src)) => {
                let dst = self.offset(ptr, off);
                if size > 0 {
                    self.emit(InstKind::Copy { dst, src, size }, &[]);
                }
            }
            (Repr::Mem { .. }, Val::Regs(_)) => {
                return self.internal(span, "a value in registers for a type in memory");
            }
        }
        Some(())
    }

    /// `ty`'s representation, reporting a type Treadle can't hold.
    pub(super) fn repr_of(&mut self, ty: Ty, span: Span) -> Option<Repr> {
        match self.repr(ty) {
            Some(r) => Some(r),
            None => {
                let shown = self.tcx().display(self.p, ty);
                self.unsupported(span, &format!("values of type `{shown}`"))
            }
        }
    }

    /// The parts of `val`, a value of `ty`, which lives in registers.
    pub(super) fn regs(&mut self, val: Val, ty: Ty, span: Span) -> Option<Vals> {
        match val {
            Val::Regs(vals) => Some(vals),
            Val::Mem(ptr) => match self.repr_of(ty, span)? {
                Repr::Regs(parts) => Some(self.load_parts(ptr, 0, &parts)),
                Repr::Mem { .. } => self.internal(span, "the parts of a value in memory"),
            },
        }
    }

    /// An address holding `val`: its own, or a new slot's with its parts
    /// stored there. (The slot isn't a temporary: what's in registers owns
    /// nothing.)
    pub(super) fn addr_of(&mut self, val: Val, ty: Ty, span: Span) -> Option<Value> {
        match val {
            Val::Mem(ptr) => Some(ptr),
            Val::Regs(vals) => {
                let slot = self.new_slot(ty, span)?;
                let ptr = self.slot_addr(slot);
                let parts = self.repr_of(ty, span)?;
                self.store_parts(ptr, 0, parts.parts(), &vals);
                Some(ptr)
            }
        }
    }

    // ------------------------------------------------------------ temporaries and moves

    /// A new temporary of `ty`, to be filled: its address. Call
    /// [`own_temp`](Self::own_temp) once it holds a value.
    pub(super) fn temp(&mut self, ty: Ty, span: Span) -> Option<Value> {
        let slot = self.new_slot(ty, span)?;
        let ptr = self.slot_addr(slot);
        if let Some(sb) = self.f.shuttle.as_mut() {
            sb.temps.insert(ptr);
        }
        Some(ptr)
    }

    /// The temporary at `ptr` (a slot's address) now holds a value of `ty`:
    /// dropped at the end of the statement unless it's taken first.
    pub(super) fn own_temp(&mut self, ptr: Value, ty: Ty) {
        if !self.needs_drop(ty) {
            return;
        }
        let Some(slot) = self.slot_of(ptr) else { return };
        let id = self.add_cleanup(super::cleanup::CleanupKind::Drop { at: super::cleanup::Stable::Slot(slot), ty });
        self.f.temps.insert(ptr, id);
    }

    /// The slot `ptr` is the address of, if it's one.
    pub(super) fn slot_of(&self, ptr: Value) -> Option<u32> {
        self.f.slot_of.get(&ptr).copied()
    }

    /// Moves `val`, a value of `ty`, to `ptr + off`, which holds nothing
    /// (or something already dropped). Taking it from a temporary means the
    /// temporary is no longer dropped; from a place, that the place is
    /// zeroed, if the value owns memory.
    pub(super) fn move_to(&mut self, ptr: Value, off: i64, val: Val, ty: Ty, span: Span) -> Option<()> {
        self.store_val(ptr, off, ty, &val, span)?;
        self.moved_from(&val, ty);
        Some(())
    }

    /// What's left after `val` was copied somewhere that now owns it: a
    /// temporary isn't dropped, and a place that owns memory is zeroed.
    pub(super) fn moved_from(&mut self, val: &Val, ty: Ty) {
        let Val::Mem(src) = *val else { return };
        self.temp_gone(src);
        if let Some(id) = self.f.temps.get(&src).copied().filter(|_| !self.f.viewed.contains(&src)) {
            self.f.temps.remove(&src);
            self.kill_cleanup(id);
        } else if self.needs_drop(ty)
            && let Some((size, _)) = self.size_align(ty)
        {
            self.emit(InstKind::Zero { dst: src, size }, &[]);
        }
    }

    /// `val`, a value of `ty`, as something the receiver owns: its parts, or
    /// the address of memory it's given (a temporary it takes over, or a
    /// fresh copy of a place, which is zeroed).
    pub(super) fn owned(&mut self, val: Val, ty: Ty, span: Span) -> Option<Val> {
        match val {
            Val::Regs(_) => Some(val),
            Val::Mem(src) => {
                if let Some(id) = self.f.temps.remove(&src) {
                    self.kill_cleanup(id);
                    return Some(Val::Mem(src));
                }
                if !self.needs_drop(ty) {
                    // Plain bytes: a copy, so the receiver can't change
                    // the original.
                    let dst = self.temp(ty, span)?;
                    self.store_val(dst, 0, ty, &val, span)?;
                    return Some(Val::Mem(dst));
                }
                let dst = self.temp(ty, span)?;
                self.move_to(dst, 0, val, ty, span)?;
                Some(Val::Mem(dst))
            }
        }
    }

    // ------------------------------------------------------------ locals

    /// Where local `l`, of type `ty`, lives: made the first time.
    pub(super) fn storage(&mut self, l: LocalId, span: Span) -> Option<Storage> {
        if let Some(s) = self.f.locals.get(&l) {
            return Some(s.clone());
        }
        let ty = self.local_ty(l);
        let s = match self.repr_of(ty, span)? {
            Repr::Regs(parts) if !self.f.scan.in_memory.contains(&l) => {
                let tys: Vec<VTy> = parts.iter().map(|p| p.vty).collect();
                let vars = self.new_vars(&tys);
                if let Some(sb) = self.f.shuttle.as_mut() {
                    for (&var, part) in vars.iter().zip(parts.iter()) {
                        sb.var_mem.insert(var, part.mem);
                    }
                }
                Storage::Ssa(vars)
            }
            _ => {
                let slot = self.new_slot(ty, span)?;
                self.local_slot_lives(slot);
                Storage::Slot(slot)
            }
        };
        self.f.locals.insert(l, s.clone());
        Some(s)
    }

    pub(super) fn local_ty(&self, l: LocalId) -> Ty {
        let ty = self.tables().local_ty.get(&l).copied().unwrap_or(self.tcx().error);
        self.tcx().degauge(ty)
    }

    /// The place local `l` is.
    pub(super) fn local_place(&mut self, l: LocalId, span: Span) -> Option<Place> {
        Some(match self.storage(l, span)? {
            Storage::Ssa(vars) => Place::Vars(vars),
            Storage::Slot(n) => Place::At { ptr: self.slot_addr(n), off: 0 },
            Storage::Ref(var) => Place::At { ptr: self.read(var), off: 0 },
            Storage::At(ptr) => Place::At { ptr, off: 0 },
        })
    }

    /// The value in `place`, of type `ty`: its parts, or its address.
    pub(super) fn read_place(&mut self, place: &Place, ty: Ty, span: Span) -> Option<Val> {
        match place {
            Place::Vars(vars) => Some(Val::Regs(self.read_all(vars))),
            Place::At { ptr, off } => self.load_val(*ptr, *off, ty, span),
        }
    }

    /// Writes `val` into `place`, without dropping what was there.
    pub(super) fn write_place(&mut self, place: &Place, ty: Ty, val: Val, span: Span) -> Option<()> {
        match place {
            Place::Vars(vars) => {
                let vals = self.regs(val, ty, span)?;
                self.write_all(vars, &vals);
                Some(())
            }
            Place::At { ptr, off } => self.move_to(*ptr, *off, val, ty, span),
        }
    }

    /// The address of `place`: for a place in registers, there's none.
    pub(super) fn place_addr(&mut self, place: &Place, span: Span) -> Option<Value> {
        match place {
            Place::At { ptr, off } => Some(self.offset(*ptr, *off)),
            Place::Vars(_) => self.internal(span, "the address of a local in registers"),
        }
    }

    /// Binds local `l` to `val`, a value its declaration owns from here.
    pub(super) fn bind_owned(&mut self, l: LocalId, val: Val, span: Span) -> Option<()> {
        let ty = self.local_ty(l);
        // A temporary in memory becomes the local's own storage.
        if let Val::Mem(ptr) = val
            && !self.f.locals.contains_key(&l)
            && let Some(slot) = self.slot_of(ptr)
            && let Some(id) = self.f.temps.remove(&ptr)
        {
            self.kill_cleanup(id);
            self.f.locals.insert(l, Storage::Slot(slot));
            self.local_slot_lives(slot);
            self.drop_local_at_scope_end(l, slot, ty);
            return Some(());
        }
        let storage = self.storage(l, span)?;
        let place = self.local_place(l, span)?;
        self.write_place(&place, ty, val, span)?;
        if let Storage::Slot(slot) = storage {
            self.drop_local_at_scope_end(l, slot, ty);
        }
        Some(())
    }

    /// Local `l` in slot `slot` is dropped when its scope ends.
    fn drop_local_at_scope_end(&mut self, _l: LocalId, slot: u32, ty: Ty) {
        if self.needs_drop(ty) {
            self.add_scope_cleanup(super::cleanup::CleanupKind::Drop { at: super::cleanup::Stable::Slot(slot), ty });
        }
    }

    /// Binds local `l` to the value at `ptr`, which it borrows: it's a view
    /// of that memory, dropped with it and never on its own.
    pub(super) fn bind_ref(&mut self, l: LocalId, ptr: Value) {
        let var = match self.f.locals.get(&l) {
            Some(Storage::Ref(var)) => *var,
            _ => {
                let var = self.new_var(VTy::Ptr);
                self.f.locals.insert(l, Storage::Ref(var));
                var
            }
        };
        self.write(var, ptr);
    }

    /// Reports a bug in the builder.
    pub(super) fn internal<T>(&mut self, span: Span, what: &str) -> Option<T> {
        // For finding where, with RUST_BACKTRACE=1.
        if std::env::var_os("LOOM_INTERNAL_PANIC").is_some() {
            panic!("internal error: {what}");
        }
        self.diags.push(
            loom_syntax::Diagnostic::error(loom_syntax::diag::codes::U0102, format!("internal error: {what}"))
                .primary(span, "while building this for Treadle")
                .note("this is a bug in Loom, not in your program"),
        );
        None
    }

    /// A load of one part of `mem` at `ptr + off`.
    pub(super) fn load(&mut self, mem: Mem, ptr: Value, off: i64, vty: VTy) -> Value {
        self.emit1(InstKind::Load { mem, ptr, off: off as i32 }, vty)
    }

    pub(super) fn store(&mut self, mem: Mem, ptr: Value, off: i64, val: Value) {
        self.emit(InstKind::Store { mem, ptr, off: off as i32, val }, &[]);
    }
}
