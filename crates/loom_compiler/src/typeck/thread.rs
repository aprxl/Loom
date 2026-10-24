//! Threads: `loom { … thread e … }`.
//!
//! A loom block is a scope its threads can't outlive, and its value is its
//! last expression's, as for any block. `thread e` runs `e` as a function of
//! its own (so `return` leaves the thread, and `?` can't reach a `mend`
//! outside it) and gives a `Spun[T]` for the `T` that `e` has. `join` is a
//! method of `Spun`, declared in `std.sync`.
//!
//! `thread` belongs to the innermost enclosing loom, found lexically: a
//! lambda in a loom's block is inside it, a function item isn't.

use loom_syntax::diag::codes;

use super::Checker;
use super::infer::VarKind;
use crate::hir::{Arg, Block, Expr};
use crate::ty::{Ty, TyKind};
use loom_syntax::source::Span;

impl Checker<'_> {
    pub(super) fn check_loom(&mut self, args: &[Arg], body: &Block, expected: Option<Ty>) -> Ty {
        match args {
            [] => {}
            [arg]
                if arg.is_given
                    && let Some(def) = self.std.crew =>
            {
                let crew = self.tcx.intern(TyKind::Adt(def, Vec::new()));
                self.check_coerce(&arg.value, crew);
            }
            _ => {
                let span = args.first().map_or(body.span, |a| a.span);
                self.error(
                    codes::T0104,
                    span,
                    "a `loom` takes one `given` crew",
                    "write `loom(given: Crew(workers: 2))`",
                );
                self.check_args_alone(args);
                self.check_block(body, expected);
                return self.tcx.error;
            }
        }
        self.looms += 1;
        let ty = self.check_block(body, expected);
        self.looms -= 1;
        // Its threads are done with when it ends, so a handle can't be its value.
        let resolved = self.resolve(ty);
        if self.mentions_spun(resolved) {
            let span = body.stmts.last().map_or(body.span, |s| s.span);
            self.error(
                codes::C0404,
                span,
                "a thread's handle can't leave its `loom` block",
                "the threads are done when the block ends: `join` it, or end the block with `()`",
            );
        }
        ty
    }

    /// `Atomic[T](value)`: the value is a `T`, which an atomic can hold.
    /// `None` when `ty` isn't a type of `std.sync` with a constructor.
    pub(super) fn check_sync_new(&mut self, call: &Expr, ty: Ty, args: &[Arg]) -> Option<Ty> {
        let TyKind::Adt(def, targs) = self.tcx.kind(ty).clone() else { return None };
        // `Arena(capacity)`.
        if Some(def) == self.std.arena || Some(def) == self.std.shared_arena {
            if args.len() != 1 || args[0].label.is_some() {
                let span = args.first().map_or(call.span, |a| a.span);
                self.error(codes::T0104, span, "`Arena` is made from its capacity", "write `Arena(1.kib())`");
                self.check_args_alone(args);
                return Some(self.tcx.error);
            }
            self.check_coerce(&args[0].value, self.tcx.usize);
            self.b.sync_new.insert(call.id);
            return Some(ty);
        }
        // `Pool[T](slots)`.
        if Some(def) == self.std.pool {
            let t = targs.first().copied()?;
            if args.len() != 1 || args[0].label.is_some() {
                let span = args.first().map_or(call.span, |a| a.span);
                self.error(codes::T0104, span, "`Pool[T]` is made from its slot count", "write `Pool[Particle](100)`");
                self.check_args_alone(args);
                return Some(self.tcx.error);
            }
            let resolved = self.resolve(t);
            if self.owns_heap(resolved).is_some()
                || matches!(self.tcx.kind(resolved), TyKind::Adt(d, _) if Some(*d) == self.std.string)
            {
                self.unsupported(call.span, "a `Pool` of text, lists or boxes");
            }
            self.check_coerce(&args[0].value, self.tcx.usize);
            self.b.sync_new.insert(call.id);
            return Some(ty);
        }
        let atomic = Some(def) == self.std.atomic;
        let pipe = Some(def) == self.std.pipe;
        if !atomic && !pipe && Some(def) != self.std.mutex {
            return None;
        }
        let t = targs.first().copied()?;
        if args.len() != 1 || args[0].label.is_some() {
            let span = args.first().map_or(call.span, |a| a.span);
            let (name, example, what) = match (atomic, pipe) {
                (true, _) => ("Atomic", "Atomic[u64](0)", "one value"),
                (_, true) => ("Pipe", "Pipe[u64](4)", "its capacity"),
                _ => ("Mutex", "Mutex[u64](0)", "one value"),
            };
            self.error(codes::T0104, span, format!("`{name}[T]` is made from {what}"), format!("write `{example}`"));
            self.check_args_alone(args);
            return Some(self.tcx.error);
        }
        // A pipe is made from its capacity; the others from their value.
        let want = if pipe { self.tcx.usize } else { t };
        self.check_coerce(&args[0].value, want);
        if pipe {
            let resolved = self.resolve(t);
            if self.is_once(resolved) {
                self.unsupported(call.span, "a `Pipe` of a `once` type");
            }
        }
        self.b.sync_new.insert(call.id);
        if atomic {
            self.atomic_holds(t, args[0].span);
        }
        Some(ty)
    }

    /// Reports an `Atomic` of something but an integer of 8, 16, 32 or 64
    /// bits, or `bool`.
    pub(super) fn atomic_holds(&mut self, t: Ty, span: Span) {
        let t = self.resolve(t);
        let fine = match self.tcx.kind(t) {
            TyKind::Int(i) => i.size || matches!(i.bits, 8 | 16 | 32 | 64),
            TyKind::Bool | TyKind::IntVar(_) | TyKind::Var(_) | TyKind::Error => true,
            _ => false,
        };
        if !fine {
            let shown = self.show(t);
            self.unsupported(span, &format!("`Atomic[{shown}]`: only integers of 8, 16, 32 or 64 bits, and `bool`"));
        }
    }

    /// Whether a value of `ty` holds a `Spun` somewhere.
    pub(super) fn mentions_spun(&mut self, ty: Ty) -> bool {
        let ty = self.deep_resolve(ty);
        match self.tcx.kind(ty).clone() {
            TyKind::Adt(def, args) => Some(def) == self.std.spun || args.iter().any(|&t| self.mentions_spun(t)),
            TyKind::Tuple(items) | TyKind::Union(items) => items.iter().any(|&t| self.mentions_spun(t)),
            TyKind::Optional(t) | TyKind::Array(t, _) | TyKind::Range(t) => self.mentions_spun(t),
            TyKind::Result(a, b) => self.mentions_spun(a) || self.mentions_spun(b),
            _ => false,
        }
    }

    pub(super) fn check_thread(&mut self, e: &Expr, body: &Expr, item: bool) -> Ty {
        if self.looms == 0 {
            self.error(codes::C0403, e.span, "`thread` can only be used inside a `loom` block", "not inside a loom");
        }
        // An iteration of a `loom for` has no value.
        let ret = if item { self.tcx.unit } else { self.new_var(VarKind::Type) };
        // The body is a function of its own, as a lambda's is.
        let saved_ret = self.ret.replace(ret);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_mends = std::mem::take(&mut self.mends);
        let saved_argument = std::mem::replace(&mut self.in_argument, false);
        let saved_defer = self.fence.take();
        let saved_item = std::mem::replace(&mut self.in_item, item);
        // What the body evaluates to, and what its `return`s give, are the
        // thread's result.
        let ty = self.check_expr(body, None);
        // A body that never ends (it unravels) has no value to give: `()`.
        let ty = if matches!(self.tcx.kind(self.resolve(ty)), TyKind::Never) { self.tcx.unit } else { ty };
        if !self.unify(ret, ty) {
            self.mismatch(body.span, ret, ty);
        }
        self.fence = saved_defer;
        self.in_item = saved_item;
        self.in_argument = saved_argument;
        self.mends = saved_mends;
        self.loops = saved_loops;
        self.ret = saved_ret;
        match self.std.spun {
            Some(spun) => self.tcx.intern(TyKind::Adt(spun, vec![ret])),
            None => self.tcx.error,
        }
    }
}
