//! Moves: a value that isn't `Copy` has one owner (see 03-memory, "Values
//! first").
//!
//! Putting such a value in an owning slot (a `let` or `var`, a `take`
//! argument, a return value, a field, an element, a box) moves it, and the
//! variable it came from can't be used again until it's given a new value.
//! A borrow (a read or `mut` parameter, a loop's item, a lambda's parameter
//! or captured variable) can't be moved out of at all, and neither can an
//! element of a list.
//!
//! The check runs over each function body once its types are known. It
//! follows the code: a variable moved on one branch of an `if` counts as
//! moved after it, and a loop's body is gone through twice, so a move in one
//! iteration is caught by a use in the next.
//!
//! Values that borrow a local are checked too (M0105): a slice of it, a
//! stream over it, or a lambda that uses it. Once the local moves or
//! changes, a borrow made before can't be used, and while a `for` loop goes
//! over a local, its body can't change it. (Borrows are otherwise
//! second-class, see 03-memory: this is the part that concerns locals.)
//!
//! It also finds loose ends (L0301): a variable holding a `once` value that
//! might still hold it when its scope ends, or when a `return`, `fail`, `?`
//! or `break` leaves the scope, unless a fallback `defer x => ...` ties it
//! off. (An unravel leaks it, by design.)

use std::collections::{HashMap, HashSet};

use loom_syntax::Diagnostic;
use loom_syntax::ast::{AssignOp, BinaryOp, ParamMode};
use loom_syntax::diag::{Code, codes};
use loom_syntax::source::Span;

use super::{Checker, Receiver};
use crate::hir::{Arg, Block, Expr, ExprKind, FnDef, GenericArg, HirId, LocalId, Pat, PatKind, StmtKind as HirStmt};
use crate::ty::{Ty, TyKind};
use loom_syntax::ast::FieldName;

/// What may have been moved at a point in the code.
#[derive(Clone, Debug, Default)]
struct Flow {
    /// Each moved place: its variable, its field path, and where it moved.
    moved: Vec<(LocalId, Vec<String>, Span)>,
    /// No code gets here (after a `return` or a `break`).
    dead: bool,
    /// Locals holding a `once` value that may not have been used up yet.
    loose: Vec<LocalId>,
    /// Borrows made stale: the borrowing local, what it borrows, and where
    /// that changed.
    stale: Vec<(LocalId, LocalId, Span)>,
}

impl Flow {
    fn merge(self, other: Flow) -> Flow {
        if self.dead {
            return other;
        }
        if other.dead {
            return self;
        }
        let mut moved = self.moved;
        for m in other.moved {
            if !moved.iter().any(|(l, p, _)| *l == m.0 && *p == m.1) {
                moved.push(m);
            }
        }
        let mut loose = self.loose;
        for l in other.loose {
            if !loose.contains(&l) {
                loose.push(l);
            }
        }
        let mut stale = self.stale;
        for s in other.stale {
            if !stale.iter().any(|(b, o, _)| *b == s.0 && *o == s.1) {
                stale.push(s);
            }
        }
        Flow { moved, dead: false, loose, stale }
    }
}

/// Why a variable can't be moved out of.
#[derive(Clone, Copy, Debug)]
enum Borrow {
    Param,
    LoopItem,
    LambdaParam,
}

/// Where a place starts.
#[derive(Clone, Debug)]
enum Root {
    Local(LocalId),
    /// A value made on the spot: moving out of it takes nothing from anyone.
    Temp,
}

#[derive(Default)]
struct MoveCx {
    flow: Flow,
    borrowed: HashMap<LocalId, Borrow>,
    /// Locals a pattern bound to part of a place: moving one moves out of
    /// that place's variable.
    parts_of: HashMap<LocalId, LocalId>,
    /// How many lambdas deep each local was declared.
    depth_of: HashMap<LocalId, u32>,
    depth: u32,
    /// The place each in-place dot-block's leading dots start from.
    subjects: HashMap<HirId, (Root, Vec<String>)>,
    /// For each level of depth: whether it's a `defer`'s body (or a lambda's).
    in_defer: Vec<bool>,
    /// For each `defer` or lambda body being gone through: how deep it
    /// starts, and the outer locals it uses.
    captures: Vec<(u32, Vec<LocalId>)>,
    /// The outer locals each lambda uses, by the lambda's id.
    lambda_caps: HashMap<HirId, Vec<LocalId>>,
    /// Locals holding a borrow (a slice, a stream, a function value), and
    /// the locals they borrow.
    borrows: HashMap<LocalId, Vec<LocalId>>,
    /// Locals a `for` loop being gone through goes over, with the loop.
    locked: Vec<(LocalId, Span)>,
    /// Locals a `defer` in an enclosing block still needs at its end, with
    /// where that `defer` is.
    held: Vec<(LocalId, Span)>,
    /// While a `match` guard is gone through: what it can't move, since the
    /// next arm needs it when the guard is false (the arm's bindings and the
    /// matched variable), with the guard.
    guarding: Vec<(LocalId, Span)>,
    /// The `once` locals declared in each block being gone through,
    /// innermost last.
    scopes: Vec<Vec<LocalId>>,
    /// Where the current function's (or lambda's) scopes start in `scopes`.
    fn_base: usize,
    /// Each loop being gone through, and how many scopes were open outside it.
    loops: Vec<(HirId, usize)>,
    /// Locals a fallback `defer` ties off.
    covered: HashSet<LocalId>,
    reported: HashSet<(&'static str, Span)>,
}

impl Checker<'_> {
    /// Checks the moves in the body of function `f`, whose types are known.
    pub(super) fn check_moves(&mut self, f: &FnDef, ret_unit: bool) {
        let Some(body) = &f.body else { return };
        let mut cx = MoveCx::default();
        cx.scopes.push(Vec::new());
        for param in &f.params {
            cx.depth_of.insert(param.local, 0);
            if param.mode != ParamMode::Take {
                cx.borrowed.insert(param.local, Borrow::Param);
            } else {
                // A `once` value taken is this function's to use up.
                self.declare(&mut cx, param.local);
            }
        }
        self.mv(&mut cx, body, !ret_unit);
        self.end_scope(&mut cx, end_of(body.span), "the function ends");
    }

    /// Checks the moves in `e`, a constant's value or compile-time code,
    /// whose types are known.
    pub(super) fn check_value_moves(&mut self, e: &Expr) {
        let mut cx = MoveCx::default();
        cx.scopes.push(Vec::new());
        self.mv(&mut cx, e, true);
        self.end_scope(&mut cx, end_of(e.span), "the value is ready");
    }

    /// Goes through `e`. `owning`: its value goes to an owning slot.
    fn mv(&mut self, cx: &mut MoveCx, e: &Expr, owning: bool) {
        // Known while compiling (a `const for` item, `f.name`): the program
        // gets the value itself, so nothing is read or moved.
        if self.b.const_values.contains_key(&e.id) {
            return;
        }
        match &e.kind {
            ExprKind::Local(_) | ExprKind::LeadingDot(_) => self.mv_place(cx, e, owning),
            ExprKind::Field { base, .. } | ExprKind::ConstField { base, .. } => match self.place_of(cx, e) {
                Some(_) => self.mv_place(cx, e, owning),
                None => self.mv(cx, base, false),
            },
            ExprKind::Index { base, args } => {
                for arg in args {
                    if let GenericArg::Expr(i) = arg {
                        self.mv(cx, i, false);
                    }
                }
                match self.place_of(cx, e) {
                    Some(_) => self.mv_place(cx, e, owning),
                    None => self.mv(cx, base, false),
                }
            }
            ExprKind::Call { callee, args } => self.mv_call(cx, e, callee, args),
            ExprKind::Tuple(items) | ExprKind::Array(items) => {
                for item in items {
                    self.mv(cx, item, true);
                }
            }
            ExprKind::ArrayRepeat { value, count } => {
                self.mv(cx, value, true);
                self.mv(cx, count, false);
            }
            ExprKind::Template(parts) => {
                for part in parts {
                    if let crate::hir::TemplatePart::Interp { expr, .. } = part {
                        self.mv(cx, expr, false);
                    }
                }
            }
            ExprKind::Block(block) | ExprKind::Unsafe { body: block, .. } => self.mv_block(cx, block, owning),
            // `const if`: only the branch taken exists.
            ExprKind::If { then, else_, deferred: Some(_), .. } => {
                if self.b.const_ifs.get(&e.id) == Some(&true) {
                    self.mv_block(cx, then, owning);
                } else if let Some(else_) = else_.as_ref().filter(|_| self.b.const_ifs.contains_key(&e.id)) {
                    self.mv(cx, else_, owning);
                }
            }
            // `const for`: its body once per item, one after another, each
            // with that item's tables.
            ExprKind::For { body, is_const: true, .. } => {
                for id in self.b.unrolled.get(&e.id).cloned().unwrap_or_default() {
                    self.in_body(id, |this| this.mv_block(cx, body, false));
                }
            }
            ExprKind::If { cond, then, else_, .. } => {
                self.mv(cx, cond, false);
                let before = cx.flow.clone();
                self.mv_block(cx, then, owning);
                let after_then = std::mem::replace(&mut cx.flow, before);
                if let Some(else_) = else_ {
                    self.mv(cx, else_, owning);
                }
                let after_else = std::mem::take(&mut cx.flow);
                cx.flow = after_then.merge(after_else);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.mv(cx, scrutinee, false);
                let root = self.place_of(cx, scrutinee).map(|(r, _)| r);
                let before = cx.flow.clone();
                let mut out: Option<Flow> = None;
                for arm in arms {
                    cx.flow = before.clone();
                    self.bind_parts(cx, &arm.pat, root.as_ref());
                    if let Some(guard) = &arm.guard {
                        let mark = cx.guarding.len();
                        let mut locals = pat_locals(&arm.pat);
                        if let Some(Root::Local(l)) = root {
                            locals.push(l);
                        }
                        cx.guarding.extend(locals.into_iter().map(|l| (l, guard.span)));
                        self.mv(cx, guard, false);
                        cx.guarding.truncate(mark);
                    }
                    self.mv(cx, &arm.body, owning);
                    let flow = std::mem::take(&mut cx.flow);
                    out = Some(match out {
                        Some(o) => o.merge(flow),
                        None => flow,
                    });
                }
                cx.flow = out.unwrap_or(before);
            }
            ExprKind::For { pat, iter, body, .. } => {
                let iter_ty = self.b.expr_ty.get(&iter.id).copied().map(|t| self.deep_resolve(t));
                let is_seq = iter_ty.is_some_and(|t| self.seq(t).is_some());
                // A loop over a list reads it, and its items are borrowed
                // from it; one over a map or a set reads it and gets copies;
                // a stream's items are its own.
                let is_coll = iter_ty.is_some_and(|t| self.coll_parts(t).is_some());
                if is_seq || is_coll {
                    self.mv(cx, iter, false);
                } else {
                    let ranged = iter_ty.is_some_and(|t| matches!(self.tcx.kind(t), TyKind::Range(_)));
                    self.mv(cx, iter, !ranged);
                }
                let borrowed = is_seq && self.place_of(cx, iter).is_some();
                for local in pat_locals(pat) {
                    cx.depth_of.insert(local, cx.depth);
                    fresh(cx, local);
                    if borrowed {
                        cx.borrowed.insert(local, Borrow::LoopItem);
                    }
                }
                // A list, a map or a set is read as the loop goes, and so is
                // what a stream over one reads: none of them may change.
                let lazy = is_seq || is_coll || iter_ty.is_some_and(|t| self.is_borrowing_ty(t));
                let mut over = Vec::new();
                if lazy {
                    self.borrowed_roots(cx, iter, &mut over);
                }
                let locked = cx.locked.len();
                cx.locked.extend(over.into_iter().map(|l| (l, iter.span)));
                cx.loops.push((e.id, cx.scopes.len()));
                self.mv_loop(cx, |this, cx| this.mv_block(cx, body, false));
                cx.loops.pop();
                cx.locked.truncate(locked);
            }
            ExprKind::While { cond, body } => {
                cx.loops.push((e.id, cx.scopes.len()));
                self.mv_loop(cx, |this, cx| {
                    this.mv(cx, cond, false);
                    this.mv_block(cx, body, false);
                });
                cx.loops.pop();
            }
            ExprKind::Loop { body } => {
                cx.loops.push((e.id, cx.scopes.len()));
                self.mv_loop(cx, |this, cx| this.mv_block(cx, body, false));
                cx.loops.pop();
            }
            ExprKind::Break { value, target } => {
                if let Some(v) = value {
                    self.mv(cx, v, true);
                }
                self.leave_loop(cx, *target, e.span, "this `break` leaves");
                cx.flow.dead = true;
            }
            ExprKind::Continue { target } => {
                self.leave_loop(cx, *target, e.span, "this `continue` leaves");
                cx.flow.dead = true;
            }
            ExprKind::Return(value) => {
                if let Some(v) = value {
                    self.mv(cx, v, true);
                }
                self.leave_function(cx, e.span, "this returns");
                cx.flow.dead = true;
            }
            ExprKind::Fail(v) => {
                self.mv(cx, v, true);
                self.leave_function(cx, e.span, "this fails");
                cx.flow.dead = true;
            }
            ExprKind::Lambda { params, body } => {
                let saved = cx.flow.clone();
                let saved_base = std::mem::replace(&mut cx.fn_base, cx.scopes.len());
                cx.depth += 1;
                cx.in_defer.push(false);
                cx.captures.push((cx.depth, Vec::new()));
                for p in params {
                    cx.depth_of.insert(p.local, cx.depth);
                    cx.borrowed.insert(p.local, Borrow::LambdaParam);
                }
                self.mv(cx, body, true);
                let (_, caps) = cx.captures.pop().unwrap_or_default();
                cx.lambda_caps.insert(e.id, caps);
                cx.in_defer.pop();
                cx.depth -= 1;
                cx.fn_base = saved_base;
                cx.flow = saved;
            }
            ExprKind::Try(inner) => {
                self.mv(cx, inner, owning);
                // A `?` with no `mend` around it may leave the function.
                let exits = self.b.tries.get(&e.id).is_some_and(|t| !matches!(t.exit, super::Exit::Mend(..)));
                if exits {
                    let saved = cx.flow.clone();
                    self.leave_function(cx, e.span, "this `?` may return");
                    cx.flow = saved;
                }
            }
            ExprKind::Mend { expr, handler, binding } => {
                self.mv(cx, expr, owning);
                if let Some(b) = binding {
                    cx.depth_of.insert(*b, cx.depth);
                }
                let before = cx.flow.clone();
                self.mv(cx, handler, owning);
                let after = std::mem::replace(&mut cx.flow, before.clone());
                cx.flow = before.merge(after);
            }
            ExprKind::DotBlock { subject, body } => {
                let in_place = self.b.dot_blocks.get(&e.id) == Some(&true);
                // Edited in place, it's a use; as a value, it's taken.
                self.mv(cx, subject, !in_place && owning);
                let place = if in_place { self.place_of(cx, subject) } else { None };
                cx.subjects.insert(e.id, place.unwrap_or((Root::Temp, Vec::new())));
                self.mv_block(cx, body, false);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Cast { expr: operand, .. } => self.mv(cx, operand, false),
            ExprKind::Binary { op: BinaryOp::And | BinaryOp::Or, lhs, rhs } => {
                self.mv(cx, lhs, false);
                let before = cx.flow.clone();
                self.mv(cx, rhs, false);
                let after = std::mem::replace(&mut cx.flow, before.clone());
                cx.flow = before.merge(after);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.mv(cx, lhs, false);
                self.mv(cx, rhs, false);
            }
            ExprKind::Chain { first, rest } => {
                self.mv(cx, first, false);
                for (_, e) in rest {
                    self.mv(cx, e, false);
                }
            }
            ExprKind::Is { expr, pat } => {
                self.mv(cx, expr, false);
                let root = self.place_of(cx, expr).map(|(r, _)| r);
                self.bind_parts(cx, pat, root.as_ref());
            }
            ExprKind::Range { lo, hi, .. } => {
                for b in lo.iter().chain(hi.iter()) {
                    self.mv(cx, b, false);
                }
            }
            ExprKind::Assign { target, op, value } => {
                self.mv(cx, value, *op == AssignOp::Assign);
                self.assign_target(cx, target, *op, Some(value));
            }
            ExprKind::Map(pairs) => {
                for (key, value) in pairs {
                    self.mv(cx, key, true);
                    self.mv(cx, value, true);
                }
            }
            ExprKind::Yield { value, .. } => self.mv(cx, value, true),
            // It can only use its own locals.
            ExprKind::Const(inner) => self.mv(cx, inner, owning),
            ExprKind::Lit(_)
            | ExprKind::Def { .. }
            | ExprKind::Type(_)
            | ExprKind::Assoc { .. }
            | ExprKind::BuiltinCase(_)
            | ExprKind::Intrinsic(_)
            | ExprKind::Loom { .. }
            | ExprKind::Thread(_)
            | ExprKind::Pull
            | ExprKind::Deref(_)
            | ExprKind::Error => {}
        }
    }

    fn mv_block(&mut self, cx: &mut MoveCx, block: &Block, owning: bool) {
        let Some((last, rest)) = block.stmts.split_last() else { return };
        let held = cx.held.len();
        cx.scopes.push(Vec::new());
        for stmt in rest {
            self.mv_stmt(cx, stmt, false);
        }
        self.mv_stmt(cx, last, owning);
        // The block's defers have run.
        cx.held.truncate(held);
        self.end_scope(cx, end_of(block.span), "the scope ends here");
    }

    fn mv_stmt(&mut self, cx: &mut MoveCx, stmt: &crate::hir::Stmt, owning: bool) {
        match &stmt.kind {
            HirStmt::Let { pat, init, .. } => {
                self.mv(cx, init, true);
                // `let _ = tx` throws it away; `let Tx(moves) = tx` ties it off.
                if matches!(pat.kind, PatKind::Wild) {
                    self.dropped(cx, init);
                }
                for local in pat_locals(pat) {
                    cx.depth_of.insert(local, cx.depth);
                    fresh(cx, local);
                    self.declare(cx, local);
                }
                if let PatKind::Bind(l) = pat.kind {
                    self.new_borrow(cx, l, init);
                }
            }
            HirStmt::Expr(e) => {
                self.mv(cx, e, owning);
                if !owning && !matches!(e.kind, ExprKind::Assign { .. }) {
                    self.dropped(cx, e);
                }
            }
            HirStmt::Given { value, .. } => self.mv(cx, value, false),
            HirStmt::Spool { init, .. } => self.mv(cx, init, false),
            // The body runs at the end of the scope: what it moves, it
            // moves then. The fallback defer's own variable is its to move.
            HirStmt::Defer { binding, body } => {
                let saved = cx.flow.clone();
                if let Some(b) = binding {
                    cx.covered.insert(*b);
                }
                let saved_base = std::mem::replace(&mut cx.fn_base, cx.scopes.len());
                cx.depth += 1;
                cx.in_defer.push(true);
                cx.captures.push((cx.depth, Vec::new()));
                let own = binding.map(|b| (b, cx.depth_of.insert(b, cx.depth)));
                self.mv(cx, body, false);
                // What the body uses must still be there when it runs.
                for local in cx.captures.pop().unwrap_or_default().1 {
                    if Some(local) != *binding {
                        cx.held.push((local, stmt.span));
                    }
                }
                if let Some((b, before)) = own {
                    match before {
                        Some(d) => cx.depth_of.insert(b, d),
                        None => cx.depth_of.remove(&b),
                    };
                }
                cx.in_defer.pop();
                cx.depth -= 1;
                cx.fn_base = saved_base;
                cx.flow = saved;
            }
            HirStmt::Item(_) => {}
        }
    }

    /// A loop's body, twice: what one iteration moves, the next may use.
    fn mv_loop(&mut self, cx: &mut MoveCx, mut body: impl FnMut(&mut Self, &mut MoveCx)) {
        let before = cx.flow.clone();
        body(self, cx);
        let once = std::mem::take(&mut cx.flow);
        cx.flow = before.clone().merge(once);
        body(self, cx);
        let twice = std::mem::take(&mut cx.flow);
        // After the loop: it may not have run, or run and left by any way.
        cx.flow = before.merge(Flow { dead: false, ..twice });
    }

    fn mv_call(&mut self, cx: &mut MoveCx, call: &Expr, callee: &Expr, args: &[Arg]) {
        if self.b.closure_calls.contains(&call.id) {
            self.mv(cx, callee, false);
            for arg in args {
                self.mv(cx, &arg.value, false);
            }
            return;
        }
        // Building a value: `some(x)`, `Point(x, y)`, `.Circle(r)`.
        if matches!(callee.kind, ExprKind::BuiltinCase(_)) || self.b.constructs.contains_key(&call.id) {
            for arg in args {
                self.mv(cx, &arg.value, true);
            }
            return;
        }
        let Some(info) = self.b.calls.get(&call.id).cloned() else {
            for arg in args {
                self.mv(cx, &arg.value, false);
            }
            return;
        };
        // Worked out here if nothing asked for it yet: a generic method's
        // instance is only known once the body is finished (and never, when
        // its arguments couldn't be worked out, which was reported).
        let known = info.callee != super::generic::UNKNOWN;
        let sig = if known { self.fn_sig(info.callee) } else { None };
        let modes: Vec<ParamMode> = sig.map(|s| s.params.iter().map(|p| p.mode).collect()).unwrap_or_default();
        let method = match &callee.kind {
            ExprKind::Index { base, .. } => base.as_ref(),
            _ => callee,
        };
        let base = match (&info.receiver, &method.kind) {
            (Some(Receiver::Expr), ExprKind::Field { base, .. }) => Some(base.as_ref()),
            _ => None,
        };
        // In source order: the receiver, then the arguments.
        let mode_of = |source: super::ArgSource| {
            info.args.iter().position(|&s| s == source).and_then(|k| modes.get(k)).copied().unwrap_or(ParamMode::Read)
        };
        let recv_mode = mode_of(super::ArgSource::Receiver);
        if let Some(base) = base {
            // A stream made from a list or a range only reads it.
            let reads = matches!(
                self.b.streams.get(&call.id),
                Some(
                    super::IterSource::Seq(_)
                        | super::IterSource::Range(_)
                        | super::IterSource::MapPairs(..)
                        | super::IterSource::SetItems(_)
                )
            );
            let through = !info.through.is_empty();
            self.mv(cx, base, recv_mode == ParamMode::Take && !reads && !through);
        } else if recv_mode == ParamMode::Take
            && let Some(Receiver::Local(l)) = info.receiver
        {
            // `.consume()` in a method: takes `self`.
            self.move_local(cx, callee.id, l, &[], callee.span);
        }
        for (i, arg) in args.iter().enumerate() {
            let take = mode_of(super::ArgSource::Arg(i)) == ParamMode::Take;
            self.mv(cx, &arg.value, take);
        }
        // While a place is lent out as `mut`, nothing else in the call may
        // touch it: `swap(mut a, mut a)`. Different fields are fine.
        // A read of a value that isn't a table (a number, a string) is done
        // before the call starts, so it can't see the change.
        let mut places: Vec<(Root, Vec<String>, ParamMode, Span, bool)> = Vec::new();
        if let Some(base) = base
            && let Some((root, path)) = self.place_of(cx, base)
        {
            let shared = self.shares_table(base);
            places.push((root, path, recv_mode, base.span, shared));
        } else if let Some(Receiver::Local(l)) = info.receiver {
            places.push((Root::Local(l), Vec::new(), recv_mode, callee.span, true));
        }
        for (i, arg) in args.iter().enumerate() {
            if let Some((root, path)) = self.place_of(cx, &arg.value) {
                let shared = self.shares_table(&arg.value);
                places.push((root, path, mode_of(super::ArgSource::Arg(i)), arg.value.span, shared));
            }
            // A lambda uses what it captures whenever the callee calls it,
            // so it may reach a value lent as `mut` behind the callee's back.
            if matches!(arg.value.kind, ExprKind::Lambda { .. }) {
                for &l in cx.lambda_caps.get(&arg.value.id).into_iter().flatten() {
                    places.push((Root::Local(l), Vec::new(), ParamMode::Read, arg.value.span, true));
                }
            }
        }
        for (root, _, mode, span, _) in &places {
            if let (Root::Local(l), ParamMode::Mut) = (root, mode) {
                self.changes(cx, *l, *span);
            }
        }
        for (i, (ra, pa, ma, sa, share_a)) in places.iter().enumerate() {
            for (rb, pb, mb, sb, share_b) in &places[i + 1..] {
                let (Root::Local(a), Root::Local(b)) = (ra, rb) else { continue };
                let overlap = a == b && (pa.starts_with(pb) || pb.starts_with(pa));
                let conflict = match (*ma == ParamMode::Mut, *mb == ParamMode::Mut) {
                    (true, true) => true,
                    (true, false) => *share_b,
                    (false, true) => *share_a,
                    (false, false) => false,
                };
                if !overlap || !conflict {
                    continue;
                }
                let (lent, other) = if *ma == ParamMode::Mut { (*sa, *sb) } else { (*sb, *sa) };
                let name = self.p.local(*a).name.clone();
                self.report(
                    cx,
                    Diagnostic::error(
                        codes::M0104,
                        format!("`{name}` is lent out as `mut` and used again in the same call"),
                    )
                    .primary(other, "used here too")
                    .secondary(lent, "lent as `mut` here")
                    .help("a `mut` argument must be the only way the call reaches that value"),
                );
            }
        }
    }

    /// Reads or moves the place `e`.
    fn mv_place(&mut self, cx: &mut MoveCx, e: &Expr, owning: bool) {
        let Some((root, path)) = self.place_of(cx, e) else { return };
        // An index's base is read, and so is the index.
        if let ExprKind::Index { base, .. } = &e.kind
            && self.place_of(cx, base).is_none()
        {
            self.mv(cx, base, false);
        }
        let Root::Local(local) = root else { return };
        self.check_use(cx, local, &path, e.span);
        self.check_borrow(cx, local, e.span);
        capture(cx, local);
        if !owning || self.is_copy_expr(e) {
            return;
        }
        if path.iter().any(|k| k == "[]") {
            let name = self.p.local(local).name.clone();
            self.report(
                cx,
                Diagnostic::error(codes::M0103, format!("can't move a value out of an element of `{name}`"))
                    .primary(e.span, "this would take it out of the list")
                    .help("copy it (`x.copy()`), or take it out with `remove` or `pop`"),
            );
            return;
        }
        self.move_local(cx, e.id, local, &path, e.span);
    }

    /// Moves `local`'s place at `path`, or reports why it can't be.
    fn move_local(&mut self, cx: &mut MoveCx, id: HirId, local: LocalId, path: &[String], span: Span) {
        let at = span;
        let name = self.p.local(local).name.clone();
        let (owner, borrow) = match cx.parts_of.get(&local) {
            Some(&owner) => (owner, cx.borrowed.get(&owner).copied()),
            None => (local, cx.borrowed.get(&local).copied()),
        };
        let borrow = borrow.or_else(|| cx.borrowed.get(&local).copied());
        if let Some(b) = borrow {
            let owner_name = self.p.local(owner).name.clone();
            let why = match b {
                Borrow::Param => format!("`{owner_name}` is a parameter the caller still owns"),
                Borrow::LoopItem => format!("`{owner_name}` is borrowed from what the loop goes over"),
                Borrow::LambdaParam => format!("`{owner_name}` is lent to the lambda"),
            };
            self.report(
                cx,
                Diagnostic::error(codes::M0102, format!("can't move `{name}`: it's borrowed"))
                    .primary(span, "moved here")
                    .note(why)
                    .help("copy it (`x.copy()`), or take ownership with a `take` parameter"),
            );
            return;
        }
        let declared = cx.depth_of.get(&owner).copied().unwrap_or(0);
        if declared < cx.depth {
            let diag = if cx.in_defer.last() == Some(&true) {
                Diagnostic::error(codes::M0102, format!("can't move `{name}` in a `defer`"))
                    .primary(span, "moved here")
                    .note("the deferred code runs at the end of the scope, when the value may be gone")
                    .help(format!("to clean up `{name}` only while it's still here, write `defer {name} => …`"))
            } else {
                Diagnostic::error(codes::M0102, format!("can't move `{name}` out of the lambda that uses it"))
                    .primary(span, "moved here")
                    .note("a lambda borrows the variables it uses")
                    .help("copy it (`x.copy()`) instead")
            };
            self.report(cx, diag);
            return;
        }
        if let Some(&(_, at_defer)) = cx.held.iter().find(|(l, _)| *l == owner) {
            let owner_name = self.p.local(owner).name.clone();
            self.report(
                cx,
                Diagnostic::error(codes::M0102, format!("can't move `{owner_name}`: a `defer` still needs it"))
                    .primary(span, "moved here")
                    .secondary(at_defer, "this runs at the end of the scope and uses it")
                    .help(format!(
                        "to clean up `{owner_name}` only while it's still here, write `defer {owner_name} => …`"
                    )),
            );
            return;
        }
        if let Some(&(_, guard)) = cx.guarding.iter().find(|(l, _)| *l == owner || *l == local) {
            let name = self.p.local(local).name.clone();
            self.report(
                cx,
                Diagnostic::error(codes::M0102, format!("a guard can't move `{name}`"))
                    .primary(span, "moved here")
                    .secondary(guard, "when this is false, the next arm still needs it")
                    .help("copy it (`x.copy()`), or test something that only reads it"),
            );
            return;
        }
        if owner != local {
            cx.flow.moved.push((owner, Vec::new(), at));
        }
        cx.flow.moved.push((local, path.to_vec(), at));
        self.changes(cx, owner, at);
        // Moved away (or, for part of it, taken apart): used up.
        cx.flow.loose.retain(|&l| l != owner && l != local);
        // A fallback defer on `owner` does nothing once it has moved.
        self.b.moves.insert(id, owner);
    }

    /// Reports a use of `local` at `path` after (part of) it was moved.
    fn check_use(&mut self, cx: &mut MoveCx, local: LocalId, path: &[String], span: Span) {
        if cx.flow.dead {
            return;
        }
        let hit = cx
            .flow
            .moved
            .iter()
            .find(|(l, p, _)| *l == local && (p.starts_with(path) || path.starts_with(p)))
            .map(|(_, _, at)| *at);
        let Some(at) = hit else { return };
        let name = self.p.local(local).name.clone();
        // A stream is read once: its methods use it up, and there's no copy.
        let stream = self.b.local_ty.get(&local).copied().is_some_and(|t| {
            let t = self.deep_resolve(t);
            self.stream_item(t).is_some()
        });
        // The same spot, gone through again: a loop.
        let diag = if at == span {
            Diagnostic::error(codes::M0101, format!("`{name}` is moved again in the next iteration of a loop"))
                .primary(span, "moved here on the first iteration")
                .help("move it only once (then `break`), or give it a new value before the next iteration")
        } else if stream {
            Diagnostic::error(codes::M0101, format!("`{name}` is used after it was moved"))
                .primary(span, "used here")
                .secondary(at, "moved here: this used up the stream")
                .note("a stream is read once: each of its methods takes it")
                .help("make a new stream for each use (`xs.indexed()` again), or collect it into a list first")
        } else {
            Diagnostic::error(codes::M0101, format!("`{name}` is used after it was moved"))
                .primary(span, "used here")
                .secondary(at, "moved here")
                .help("copy it before it moves (`x.copy()`), or give it a new value first")
        };
        self.report(cx, diag);
        // Once is enough for each place.
        cx.flow.moved.retain(|(l, p, _)| !(*l == local && (p.starts_with(path) || path.starts_with(p))));
    }

    /// Whether values of `ty` can borrow what they were made from: slices,
    /// streams and function values.
    fn is_borrowing_ty(&mut self, ty: Ty) -> bool {
        let ty = self.deep_resolve(ty);
        match self.tcx.kind(ty) {
            TyKind::Fn(..) => true,
            TyKind::Adt(def, _) => Some(*def) == self.std.slice || Some(*def) == self.std.stream,
            _ => false,
        }
    }

    /// The locals the value of `e` borrows (and what those borrow in turn):
    /// the place a slice or a stream is made from, and what lambdas use.
    fn borrowed_roots(&mut self, cx: &MoveCx, e: &Expr, out: &mut Vec<LocalId>) {
        let add = |out: &mut Vec<LocalId>, l: LocalId| {
            let mut all = vec![l];
            all.extend(cx.borrows.get(&l).into_iter().flatten().copied());
            for l in all {
                if !out.contains(&l) {
                    out.push(l);
                }
            }
        };
        match &e.kind {
            ExprKind::Lambda { .. } => {
                for &l in cx.lambda_caps.get(&e.id).into_iter().flatten() {
                    add(out, l);
                }
            }
            ExprKind::Local(_) | ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::LeadingDot(_) => {
                match self.place_of(cx, e) {
                    Some((Root::Local(l), _)) => add(out, l),
                    Some((Root::Temp, _)) => {}
                    // `xs[a..b]`: a view of `xs`.
                    None => {
                        if let ExprKind::Index { base, .. } | ExprKind::Field { base, .. } = &e.kind {
                            self.borrowed_roots(cx, base, out);
                        }
                    }
                }
            }
            // A call that gives a borrow borrows what it was given.
            ExprKind::Call { callee, args } => {
                let Some(&ty) = self.b.expr_ty.get(&e.id) else { return };
                if !self.is_borrowing_ty(ty) {
                    return;
                }
                let method = match &callee.kind {
                    ExprKind::Index { base, .. } => base.as_ref(),
                    _ => callee.as_ref(),
                };
                if let ExprKind::Field { base, .. } = &method.kind {
                    self.borrowed_roots(cx, base, out);
                }
                for arg in args {
                    self.borrowed_roots(cx, &arg.value, out);
                }
            }
            _ => {}
        }
    }

    /// `target op= …`, once the value is gone through: `value` is it, when
    /// it's all `target`'s (not a part of a tuple's).
    fn assign_target(&mut self, cx: &mut MoveCx, target: &Expr, op: AssignOp, value: Option<&Expr>) {
        // `(a, b) = (b, a)`: each part gets a new value.
        if let (ExprKind::Tuple(items), AssignOp::Assign) = (&target.kind, op) {
            for item in items {
                self.assign_target(cx, item, op, None);
            }
            return;
        }
        if let Some((Root::Local(l), path)) = self.place_of(cx, target) {
            capture(cx, l);
            self.changes(cx, l, target.span);
            if let (true, AssignOp::Assign, Some(value)) = (path.is_empty(), op, value) {
                self.new_borrow(cx, l, value);
            }
        }
        match (op, self.place_of(cx, target)) {
            // A whole new value: what was moved out of it is back.
            (AssignOp::Assign, Some((Root::Local(l), path))) => {
                let parent = &path[..path.len().saturating_sub(1)];
                if !path.is_empty() {
                    self.check_use(cx, l, parent, target.span);
                }
                cx.flow.moved.retain(|(m, p, _)| !(*m == l && p.starts_with(&path)));
                // A new `once` value over one still loose loses it.
                if path.is_empty() && self.local_is_once(l) {
                    if cx.flow.loose.contains(&l) && !cx.covered.contains(&l) && !cx.flow.dead {
                        self.loose_end(cx, l, target.span, "...but it gets a new value here while still loose");
                    }
                    if !cx.flow.loose.contains(&l) {
                        cx.flow.loose.push(l);
                    }
                }
            }
            _ => self.mv(cx, target, false),
        }
    }

    /// `l` now holds `value`: what does it borrow?
    fn new_borrow(&mut self, cx: &mut MoveCx, l: LocalId, value: &Expr) {
        cx.flow.stale.retain(|(b, _, _)| *b != l);
        cx.borrows.remove(&l);
        let Some(&ty) = self.b.local_ty.get(&l) else { return };
        if !self.is_borrowing_ty(ty) {
            return;
        }
        let mut roots = Vec::new();
        self.borrowed_roots(cx, value, &mut roots);
        roots.retain(|&r| r != l);
        if !roots.is_empty() {
            cx.borrows.insert(l, roots);
        }
    }

    /// `l` moves or changes at `at`: the borrows of it made before go stale,
    /// and a loop going over it can't let that happen.
    fn changes(&mut self, cx: &mut MoveCx, l: LocalId, at: Span) {
        if cx.flow.dead {
            return;
        }
        let owner = cx.parts_of.get(&l).copied().unwrap_or(l);
        if let Some(&(_, over)) = cx.locked.iter().find(|(x, _)| *x == owner || *x == l) {
            let name = self.p.local(owner).name.clone();
            self.report(
                cx,
                Diagnostic::error(codes::M0105, format!("`{name}` changes while a loop goes over it"))
                    .primary(at, "changed here")
                    .secondary(over, "the loop reads it as it goes")
                    .help(format!(
                        "go over a copy (`{name}.copy()`), or collect the changes and make them after the loop"
                    )),
            );
        }
        let borrowers: Vec<LocalId> = cx
            .borrows
            .iter()
            .filter(|(_, roots)| roots.contains(&owner) || roots.contains(&l))
            .map(|(&b, _)| b)
            .collect();
        for b in borrowers {
            if !cx.flow.stale.iter().any(|(x, o, _)| *x == b && *o == owner) {
                cx.flow.stale.push((b, owner, at));
            }
        }
    }

    /// A use of `local`, which may hold a borrow gone stale.
    fn check_borrow(&mut self, cx: &mut MoveCx, local: LocalId, span: Span) {
        if cx.flow.dead {
            return;
        }
        let Some(&(_, owner, at)) = cx.flow.stale.iter().find(|(b, _, _)| *b == local) else { return };
        let (name, owner_name) = (self.p.local(local).name.clone(), self.p.local(owner).name.clone());
        self.report(
            cx,
            Diagnostic::error(
                codes::M0105,
                format!("`{name}` borrows `{owner_name}`, which changed after it was made"),
            )
            .primary(span, "used here")
            .secondary(at, format!("`{owner_name}` changes here"))
            .note(format!("`{name}` reads `{owner_name}` as it is now, not as it was"))
            .help(format!("make `{name}` after the change, or from a copy (`{owner_name}.copy()`)")),
        );
        cx.flow.stale.retain(|(b, _, _)| *b != local);
    }

    /// Records the locals a pattern binds from the place starting at `root`.
    fn bind_parts(&mut self, cx: &mut MoveCx, pat: &Pat, root: Option<&Root>) {
        for local in pat_locals(pat) {
            cx.depth_of.insert(local, cx.depth);
            fresh(cx, local);
            if let Some(Root::Local(owner)) = root {
                let owner = cx.parts_of.get(owner).copied().unwrap_or(*owner);
                cx.parts_of.insert(local, owner);
                // A binding that isn't `Copy` shares its table with the
                // place it came from: it borrows it.
                let shared = self.b.local_ty.get(&local).copied().is_some_and(|t| {
                    let t = self.deep_resolve(t);
                    self.is_table_backed(t) && !self.is_copy(t)
                });
                if shared {
                    cx.borrows.insert(local, vec![owner]);
                }
            }
        }
    }

    /// Where the place `e` starts, and the fields (or `[]` for an element)
    /// from there. `None` if `e` isn't a place.
    fn place_of(&self, cx: &MoveCx, e: &Expr) -> Option<(Root, Vec<String>)> {
        match &e.kind {
            ExprKind::Local(l) => Some((Root::Local(*l), Vec::new())),
            ExprKind::Field { base, field } => {
                let (root, mut path) = self.place_of(cx, base)?;
                path.push(match field {
                    FieldName::Named(n) => n.name.clone(),
                    FieldName::Index(i, _) => i.to_string(),
                });
                Some((root, path))
            }
            ExprKind::Index { base, args } => {
                let element =
                    matches!(args.as_slice(), [GenericArg::Expr(i)] if !matches!(i.kind, ExprKind::Range { .. }));
                if !element {
                    return None;
                }
                let (root, mut path) = self.place_of(cx, base)?;
                path.push("[]".into());
                Some((root, path))
            }
            ExprKind::LeadingDot(_) => {
                let access = self.b.fields.get(&e.id)?;
                let key = match &access.key {
                    super::warp::Key::Name(n) => n.clone(),
                    super::warp::Key::Pos(i) => (i - 1).to_string(),
                };
                let (root, mut path) = match access.base {
                    Receiver::Local(l) => (Root::Local(l), Vec::new()),
                    Receiver::Subject(id) => cx.subjects.get(&id).cloned().unwrap_or((Root::Temp, Vec::new())),
                    Receiver::Expr => return None,
                };
                path.push(key);
                Some((root, path))
            }
            // `v.(f)` is `v`'s field `f`, and `v.(c)` is `v` itself.
            ExprKind::ConstField { base, .. } => {
                let (root, mut path) = self.place_of(cx, base)?;
                if let Some(access) = self.b.fields.get(&e.id) {
                    path.push(match &access.key {
                        super::warp::Key::Name(n) => n.clone(),
                        super::warp::Key::Pos(i) => (i - 1).to_string(),
                    });
                }
                Some((root, path))
            }
            _ => None,
        }
    }

    /// Whether `e`'s value is a table the callee would share with it.
    fn shares_table(&mut self, e: &Expr) -> bool {
        let Some(&ty) = self.b.expr_ty.get(&e.id) else { return false };
        let ty = self.deep_resolve(ty);
        self.is_table_backed(ty)
    }

    fn is_copy_expr(&mut self, e: &Expr) -> bool {
        let Some(&ty) = self.b.expr_ty.get(&e.id) else { return true };
        let ty = self.deep_resolve(ty);
        if self.tcx.has_error(ty) || self.has_vars(ty) {
            return true;
        }
        self.is_copy(ty)
    }

    fn report(&mut self, cx: &mut MoveCx, diag: Diagnostic) {
        let code: Code = diag.code;
        let span = diag.labels.iter().find(|l| l.primary).map(|l| l.span);
        let Some(span) = span else { return };
        if cx.reported.insert((code.id, span))
            && !self.diags.iter().any(|d| d.code.id == code.id && d.labels.iter().any(|l| l.primary && l.span == span))
        {
            self.diags.push(diag);
        }
    }
}

/// `local` is used inside each lambda being gone through that it's declared
/// outside of: they capture it.
fn capture(cx: &mut MoveCx, local: LocalId) {
    let declared = cx.depth_of.get(&local).copied();
    for (depth, captured) in cx.captures.iter_mut() {
        if declared.is_some_and(|d| d < *depth) && !captured.contains(&local) {
            captured.push(local);
        }
    }
}

/// The locals a pattern binds.
fn pat_locals(pat: &Pat) -> Vec<LocalId> {
    let mut out = Vec::new();
    collect_locals(pat, &mut out);
    out
}

fn collect_locals(pat: &Pat, out: &mut Vec<LocalId>) {
    match &pat.kind {
        PatKind::Bind(l) => out.push(*l),
        PatKind::TypeTest { local, .. } => out.push(*local),
        PatKind::Twill { captures, .. } => out.extend(captures.iter().copied()),
        PatKind::Rest(Some(l)) => out.push(*l),
        PatKind::Tuple(ps) | PatKind::Slice(ps) | PatKind::Or(ps) => ps.iter().for_each(|p| collect_locals(p, out)),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => collect_locals(p, out),
        PatKind::Case { fields, .. } => fields.iter().flatten().for_each(|f| collect_locals(&f.pat, out)),
        PatKind::Range { .. }
        | PatKind::Lit { .. }
        | PatKind::Wild
        | PatKind::None
        | PatKind::Rest(None)
        | PatKind::ConstCase(_)
        | PatKind::Error => {}
    }
}

// ================================================================ loose ends

impl Checker<'_> {
    /// Whether local `l` holds a `once` value.
    fn local_is_once(&mut self, l: LocalId) -> bool {
        match self.b.local_ty.get(&l).copied() {
            Some(ty) => self.is_once(ty),
            None => false,
        }
    }

    /// A new local of the innermost scope: loose, if it holds a `once` value.
    fn declare(&mut self, cx: &mut MoveCx, l: LocalId) {
        if !self.local_is_once(l) {
            return;
        }
        if let Some(scope) = cx.scopes.last_mut() {
            scope.push(l);
        }
        if !cx.flow.loose.contains(&l) {
            cx.flow.loose.push(l);
        }
    }

    /// Ends the innermost scope at `at`: its locals must be used up.
    fn end_scope(&mut self, cx: &mut MoveCx, at: Span, why: &str) {
        let Some(scope) = cx.scopes.pop() else { return };
        if !cx.flow.dead {
            for &l in &scope {
                if cx.flow.loose.contains(&l) && !cx.covered.contains(&l) {
                    self.loose_end(cx, l, at, &format!("...but it is still loose when {why}"));
                }
            }
        }
        cx.flow.loose.retain(|l| !scope.contains(l));
    }

    /// `return`, `fail` or `?` at `at`: every scope of the function ends.
    fn leave_function(&mut self, cx: &mut MoveCx, at: Span, why: &str) {
        self.leave_scopes(cx, cx.fn_base, at, why);
    }

    /// `break` or `continue` of loop `target`: the scopes inside it end.
    fn leave_loop(&mut self, cx: &mut MoveCx, target: HirId, at: Span, why: &str) {
        let Some(&(_, depth)) = cx.loops.iter().rev().find(|(id, _)| *id == target) else { return };
        self.leave_scopes(cx, depth.max(cx.fn_base), at, why);
    }

    fn leave_scopes(&mut self, cx: &mut MoveCx, from: usize, at: Span, why: &str) {
        if cx.flow.dead {
            return;
        }
        let locals: Vec<LocalId> = cx.scopes.iter().skip(from).flatten().copied().collect();
        for l in locals {
            if cx.flow.loose.contains(&l) && !cx.covered.contains(&l) {
                self.loose_end(cx, l, at, &format!("...but it is still loose when {why}"));
            }
        }
    }

    /// An expression statement (or `let _ = …`) whose `once` value is
    /// thrown away.
    fn dropped(&mut self, cx: &mut MoveCx, e: &Expr) {
        if cx.flow.dead {
            return;
        }
        let Some(&ty) = self.b.expr_ty.get(&e.id) else { return };
        let ty = self.deep_resolve(ty);
        // A variable read here isn't thrown away: it's still in its variable.
        if matches!(e.kind, ExprKind::Local(_) | ExprKind::Field { .. } | ExprKind::LeadingDot(_)) {
            return;
        }
        if !self.is_once(ty) {
            return;
        }
        let shown = self.show(ty);
        let help = if self.burst_item(ty).is_some() {
            "go through it with `for`, or call `.drain()` to throw its items away on purpose"
        } else {
            "keep it in a variable and tie it off, or pass it to a function that takes it"
        };
        self.report(
            cx,
            Diagnostic::error(codes::L0301, format!("loose end: this `{shown}` is thrown away"))
                .primary(e.span, format!("`{shown}` is a `once` type: it must be used up exactly once"))
                .help(help),
        );
    }

    /// Reports local `l` as a loose end: its value isn't used up by `at`.
    fn loose_end(&mut self, cx: &mut MoveCx, l: LocalId, at: Span, label: &str) {
        let local = self.p.local(l);
        let (name, decl) = (local.name.clone(), local.span);
        let ty = self.b.local_ty.get(&l).copied().map(|t| self.deep_resolve(t));
        let shown = ty.map(|t| self.show(t)).unwrap_or_default();
        let ties = ty.map(|t| self.tie_offs(t, &name)).unwrap_or_default();
        let mut diag = Diagnostic::error(codes::L0301, format!("loose end: `{name}` is never tied off"))
            .primary(decl, format!("`{shown}` is a `once` type: it must be used up exactly once"))
            .secondary(at, label.to_string());
        if !ties.is_empty() {
            diag = diag.help(format!("tie it off with {}", ties.join(" or ")));
            let simple = ties.iter().find(|t| t.ends_with("()`")).or(ties.first());
            if let Some(first) = simple {
                diag =
                    diag.help(format!("or add `defer {name} => {}` right after it's created", first.trim_matches('`')));
            }
        } else {
            diag = diag.help(format!("pass it to a function that takes it, or add `defer {name} => …`"));
        }
        self.report(cx, diag);
    }

    /// The calls that use up a value of `ty` held in `name`: its warp's
    /// methods that take `self`, as `` `name.commit(..)` ``.
    fn tie_offs(&mut self, ty: Ty, name: &str) -> Vec<String> {
        let TyKind::Adt(def, _) = *self.tcx.kind(ty) else { return Vec::new() };
        let crate::hir::DefKind::Warp(w) = &self.p.def(def).kind else { return Vec::new() };
        let mut out = Vec::new();
        for &m in &w.members {
            if let crate::hir::DefKind::Fn(f) = &self.p.def(m).kind
                && f.params.first().is_some_and(|p| p.is_self && p.mode == ParamMode::Take)
            {
                let args = if f.params.len() > 1 { ".." } else { "" };
                out.push(format!("`{name}.{}({args})`", self.p.def(m).name));
            }
        }
        out
    }
}

/// A local bound anew (each time a loop's body runs, say): nothing has
/// moved out of this value yet.
fn fresh(cx: &mut MoveCx, local: LocalId) {
    cx.flow.moved.retain(|(l, _, _)| *l != local);
    cx.flow.stale.retain(|(b, _, _)| *b != local);
    cx.borrows.remove(&local);
}

/// Just the end of `span`: where a block's `}` is.
fn end_of(span: Span) -> Span {
    Span { lo: span.hi.saturating_sub(1), ..span }
}
