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
        if let Some(arg) = args.first() {
            self.check_block(body, expected);
            return self.unsupported(arg.span, "`loom(given: …)`");
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
        let atomic = Some(def) == self.std.atomic;
        if !atomic && Some(def) != self.std.mutex {
            return None;
        }
        let t = targs.first().copied()?;
        if args.len() != 1 || args[0].label.is_some() {
            let span = args.first().map_or(call.span, |a| a.span);
            let (name, example) = if atomic { ("Atomic", "Atomic[u64](0)") } else { ("Mutex", "Mutex[u64](0)") };
            self.error(codes::T0104, span, format!("`{name}[T]` is made from one value"), format!("write `{example}`"));
            self.check_args_alone(args);
            return Some(self.tcx.error);
        }
        self.check_coerce(&args[0].value, t);
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
            TyKind::Int(i) => matches!(i.bits, 8 | 16 | 32 | 64),
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

    pub(super) fn check_thread(&mut self, e: &Expr, body: &Expr) -> Ty {
        if self.looms == 0 {
            self.error(codes::C0403, e.span, "`thread` can only be used inside a `loom` block", "not inside a loom");
        }
        let ret = self.new_var(VarKind::Type);
        // The body is a function of its own, as a lambda's is.
        let saved_ret = self.ret.replace(ret);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_mends = std::mem::take(&mut self.mends);
        let saved_argument = std::mem::replace(&mut self.in_argument, false);
        let saved_defer = self.fence.take();
        // What the body evaluates to, and what its `return`s give, are the
        // thread's result.
        let ty = self.check_expr(body, None);
        if !self.unify(ret, ty) {
            self.mismatch(body.span, ret, ty);
        }
        self.fence = saved_defer;
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
