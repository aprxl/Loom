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
//! Nor can a borrow outlive what it borrows (M0106): a lambda that captures
//! a local is passed as an argument or kept in a local, never returned or
//! stored, and so is a stream or a slice made from a local. Treadle runs a
//! lambda over its enclosing frame, which is gone once the function returns
//! (design/vm.md §10.9).
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

/// Where a value goes that mustn't outlive the locals it borrows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    /// Out of the function: returned, failed with, or yielded.
    Out,
    /// Into something that owns it: a field, an element, a tuple, a
    /// `take` argument, a case's payload.
    Stored,
}

/// Where a place starts.
#[derive(Clone, Debug)]
enum Root {
    Local(LocalId),
    /// A value made on the spot: moving out of it takes nothing from anyone.
    Temp,
}

/// A thread spawned in a loom whose handle hasn't been joined: the locals
/// it holds by reference (design/vm.md §11.3).
struct LiveThread {
    /// The `thread` expression.
    span: Span,
    reads: Vec<LocalId>,
    writes: Vec<LocalId>,
    /// The variable holding its `Spun`, if it was bound to one.
    handle: Option<LocalId>,
}

#[derive(Default)]
struct MoveCx {
    flow: Flow,
    /// What projections called in the statement being gone through
    /// borrow until it ends: each local, how, and where.
    lent: Vec<(LocalId, ParamMode, Span)>,
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
    /// For each `thread` body being gone through: how deep it starts, and
    /// the outer locals it changes.
    writes: Vec<(u32, Vec<(LocalId, Span)>)>,
    /// For each `loom` block being gone through: the scope depth of its
    /// block. A local declared at it or outside is captured by reference;
    /// a narrower one (a loop variable, a lambda's parameter) is copied.
    looms: Vec<usize>,
    /// The threads of the looms being gone through that aren't joined yet.
    live: Vec<LiveThread>,
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
    /// How many blocks deep each local was declared: a borrow can't be
    /// kept in a variable that outlives what it borrows.
    scope_of: HashMap<LocalId, usize>,
    /// The locals each local's lambda (or a stream holding one) captures.
    captures_of: HashMap<LocalId, Vec<LocalId>>,
    /// The locals each loop item is borrowed from.
    item_roots: HashMap<LocalId, Vec<LocalId>>,
    /// Items of a loop over a `#disjoint` stream (`chunks_mut`): views that
    /// don't overlap, so each iteration's thread may change its own.
    disjoint_items: HashSet<LocalId>,
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
            cx.scope_of.insert(param.local, cx.scopes.len());
            if param.mode != ParamMode::Take {
                cx.borrowed.insert(param.local, Borrow::Param);
            } else {
                // A `once` value taken is this function's to use up.
                self.declare(&mut cx, param.local);
            }
        }
        self.mv(&mut cx, body, !ret_unit);
        if !ret_unit {
            self.check_escape(&cx, body, Escape::Out);
        }
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
        // What a projection lends is someone else's: it can be read or
        // changed, but not taken.
        if owning && self.lent(e) && !self.is_copy_expr(e) {
            self.report(
                cx,
                Diagnostic::error(codes::M0103, "can't move a value out of what a projection lends")
                    .primary(e.span, "this would take it out of its place")
                    .help("copy it (`x.copy()`)"),
            );
            return self.mv(cx, e, false);
        }
        match &e.kind {
            ExprKind::Local(_) | ExprKind::LeadingDot(_) => self.mv_place(cx, e, owning),
            ExprKind::Deref(inner) => match self.place_of(cx, e) {
                Some(_) => self.mv_place(cx, e, owning),
                None => self.mv(cx, inner, false),
            },
            ExprKind::Unary { op: loom_syntax::ast::UnaryOp::Alloc, operand } => {
                self.mv(cx, operand, true);
                self.check_escape(cx, operand, Escape::Stored);
            }
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
                    self.check_escape(cx, item, Escape::Stored);
                }
            }
            ExprKind::ArrayRepeat { value, count } => {
                self.mv(cx, value, true);
                self.check_escape(cx, value, Escape::Stored);
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
                    // In the loop's body, a block deeper.
                    cx.scope_of.insert(local, cx.scopes.len() + 1);
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
                let disjoint = self.iter_is_disjoint(iter);
                for local in pat_locals(pat) {
                    cx.item_roots.insert(local, over.clone());
                    if disjoint {
                        cx.disjoint_items.insert(local);
                    }
                }
                cx.locked.extend(over.into_iter().map(|l| (l, iter.span)));
                cx.loops.push((e.id, cx.scopes.len()));
                self.mv_loop(cx, |this, cx| {
                    // Each iteration binds the pattern anew: what the last
                    // one moved out of it is gone with it.
                    for local in pat_locals(pat) {
                        fresh(cx, local);
                    }
                    this.mv_block(cx, body, false)
                });
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
                    self.check_escape(cx, v, Escape::Out);
                    if !cx.looms.is_empty() && self.value_is_handle(v) {
                        self.handle_escapes(cx, v.span, "returned");
                    }
                }
                self.leave_function(cx, e.span, "this returns");
                cx.flow.dead = true;
            }
            ExprKind::Fail(v) => {
                self.mv(cx, v, true);
                self.check_escape(cx, v, Escape::Out);
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
                    cx.scope_of.insert(p.local, cx.scopes.len() + 1);
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
            // Its threads are joined before it ends, so what they hold is
            // free again after it.
            ExprKind::Loom { body, .. } => {
                let live = cx.live.len();
                cx.looms.push(cx.scopes.len() + 1);
                self.mv_block(cx, body, owning);
                cx.looms.pop();
                cx.live.truncate(live);
            }
            // A function of its own, like a lambda's body: it holds what it
            // captures by reference until it's joined.
            ExprKind::Thread { body, .. } => self.mv_thread(cx, e, body),
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
                    cx.scope_of.insert(*b, cx.scopes.len() + 1);
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
                // A handle can't be kept in a variable that outlives its loom.
                if let (Some(&by_ref), true) = (cx.looms.last(), self.value_is_handle(value))
                    && let Some((Root::Local(l), _)) = self.place_of(cx, target)
                    && cx.scope_of.get(&l).is_some_and(|&s| s < by_ref)
                {
                    self.handle_escapes(cx, target.span, "kept in a variable declared outside it");
                }
                if *op == AssignOp::Assign {
                    self.check_assigned_escape(cx, target, value);
                }
                self.assign_target(cx, target, *op, Some(value));
            }
            ExprKind::Map(pairs) => {
                for (key, value) in pairs {
                    self.mv(cx, key, true);
                    self.mv(cx, value, true);
                    self.check_escape(cx, key, Escape::Stored);
                    self.check_escape(cx, value, Escape::Stored);
                }
            }
            // A projection lends the place: it's used, not moved.
            ExprKind::Yield { place: Some(_), value } => self.mv(cx, value, false),
            ExprKind::Yield { value, .. } => {
                self.mv(cx, value, true);
                self.check_escape(cx, value, Escape::Out);
            }
            // It can only use its own locals.
            ExprKind::Const(inner) => self.mv(cx, inner, owning),
            ExprKind::Lit(_)
            | ExprKind::Def { .. }
            | ExprKind::Type(_)
            | ExprKind::Assoc { .. }
            | ExprKind::BuiltinCase(_)
            | ExprKind::Intrinsic(_)
            | ExprKind::Pull
            | ExprKind::Error => {}
        }
    }

    /// `thread body`: see the rules in design/vm.md §11.3. (A method of its
    /// own, so the frame of `mv`, which recurses, stays small.)
    #[inline(never)]
    fn mv_thread(&mut self, cx: &mut MoveCx, e: &Expr, body: &Expr) {
        let saved = cx.flow.clone();
        let saved_base = std::mem::replace(&mut cx.fn_base, cx.scopes.len());
        cx.depth += 1;
        cx.in_defer.push(false);
        cx.captures.push((cx.depth, Vec::new()));
        cx.writes.push((cx.depth, Vec::new()));
        self.mv(cx, body, true);
        let (_, caps) = cx.captures.pop().unwrap_or_default();
        let (_, written) = cx.writes.pop().unwrap_or_default();
        cx.in_defer.pop();
        cx.depth -= 1;
        cx.fn_base = saved_base;
        cx.flow = saved;
        // Only what's declared outside the loom block is shared: the
        // block's own locals are dropped before its guard waits.
        let by_ref = cx.looms.last().copied().unwrap_or(0);
        let held = |cx: &MoveCx, l: &LocalId| cx.scope_of.get(l).is_some_and(|&s| s < by_ref);
        let writes: Vec<LocalId> = written.iter().map(|w| w.0).filter(|l| held(cx, l)).collect();
        let mut reads: Vec<LocalId> =
            caps.iter().copied().filter(|l| held(cx, l)).filter(|l| !writes.contains(l)).collect();
        // A disjoint view it changes holds what it's a view of, which the
        // parent mustn't touch until it's joined.
        for &l in caps.iter().filter(|l| cx.disjoint_items.contains(l)) {
            for r in Self::roots_of(cx, l) {
                if held(cx, &r) && !reads.contains(&r) {
                    reads.push(r);
                }
            }
        }
        // What doesn't outlive the loom is copied into the thread:
        // a `Copy` value, or a borrow of what does.
        let mut copies = Vec::new();
        let mut moves = Vec::new();
        let narrower: Vec<LocalId> = caps.iter().copied().filter(|l| !held(cx, l)).collect();
        for l in narrower {
            let name = self.p.local(l).name.clone();
            let changed = written.iter().find(|w| w.0 == l).map(|w| w.1);
            let borrow = cx.borrowed.contains_key(&l) || cx.parts_of.contains_key(&l) || cx.borrows.contains_key(&l);
            let ty = self.b.local_ty.get(&l).copied();
            let copyable = borrow || ty.is_some_and(|t| self.is_copy(t));
            // What a copied borrow borrows has to outlive the loom.
            let mut roots: Vec<LocalId> = cx.item_roots.get(&l).cloned().unwrap_or_default();
            roots.extend(cx.borrows.get(&l).into_iter().flatten().copied());
            roots.extend(cx.parts_of.get(&l).copied());
            let inside = roots.into_iter().find(|r| !held(cx, r) && cx.scope_of.contains_key(r));
            if let Some(root) = inside {
                let root = self.p.local(root).name.clone();
                self.report(
                    cx,
                    Diagnostic::error(
                        codes::C0405,
                        format!("a thread can't keep `{name}`, which borrows `{root}`: it doesn't outlive the loom"),
                    )
                    .primary(e.span, "this thread uses it")
                    .help("declare what it borrows outside the `loom`"),
                );
            } else if let Some(at) = changed {
                self.report(
                    cx,
                    Diagnostic::error(
                        codes::C0405,
                        format!("`{name}` doesn't outlive the loom, so a thread gets a copy of it"),
                    )
                    .primary(at, "this changes the thread's copy, which nobody sees")
                    .help("declare it outside the `loom`, or return the new value from the thread"),
                );
            } else if !copyable {
                // What it owns goes to the thread, and the parent is done
                // with it.
                moves.push(l);
                cx.flow.moved.push((l, Vec::new(), e.span));
                cx.flow.loose.retain(|&x| x != l);
            } else {
                copies.push(l);
            }
        }
        self.b.thread_copies.insert(e.id, copies);
        self.b.thread_moves.insert(e.id, moves);
        cx.lambda_caps.insert(e.id, caps);
        cx.live.push(LiveThread { span: e.span, reads, writes, handle: None });
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
        let lent = cx.lent.len();
        self.mv_stmt_inner(cx, stmt, owning);
        cx.lent.truncate(lent);
    }

    fn mv_stmt_inner(&mut self, cx: &mut MoveCx, stmt: &crate::hir::Stmt, owning: bool) {
        match &stmt.kind {
            HirStmt::Let { pat, init, .. } => {
                let live = cx.live.len();
                self.mv(cx, init, true);
                // `let a = thread …`: `a` joins it.
                if let (ExprKind::Thread { .. }, PatKind::Bind(l), true) = (&init.kind, &pat.kind, cx.live.len() > live)
                    && let Some(t) = cx.live.last_mut()
                {
                    t.handle = Some(*l);
                }
                // `let _ = tx` throws it away; `let Tx(moves) = tx` ties it off.
                if matches!(pat.kind, PatKind::Wild) {
                    self.dropped(cx, init);
                }
                for local in pat_locals(pat) {
                    cx.depth_of.insert(local, cx.depth);
                    cx.scope_of.insert(local, cx.scopes.len());
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
            HirStmt::Given { local, value } => {
                let alias =
                    matches!(&value.kind, ExprKind::Local(l) if self.p.local(*l).kind == crate::hir::LocalKind::Spool);
                self.mv(cx, value, !alias);
                if let Some(local) = local {
                    cx.depth_of.insert(*local, cx.depth);
                    cx.scope_of.insert(*local, cx.scopes.len());
                    fresh(cx, *local);
                    self.declare(cx, *local);
                }
            }
            HirStmt::Spool { local, init } => {
                self.mv(cx, init, true);
                cx.depth_of.insert(*local, cx.depth);
                cx.scope_of.insert(*local, cx.scopes.len());
                fresh(cx, *local);
                self.declare(cx, *local);
            }
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
                self.check_escape(cx, &arg.value, Escape::Stored);
            }
            return;
        }
        let Some(info) = self.b.calls.get(&call.id).cloned() else {
            for arg in args {
                let stream = self.is_stream_expr(&arg.value);
                self.mv(cx, &arg.value, stream);
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
        // A lambda's parameters are the items of the receiver, borrowed from
        // what the receiver is made from, as a loop's are.
        let item_roots = base.map(|base| self.receiver_roots(cx, base)).unwrap_or_default();
        for (i, arg) in args.iter().enumerate() {
            if let ExprKind::Lambda { params, .. } = &arg.value.kind
                && !item_roots.is_empty()
            {
                for p in params {
                    let ty = self.b.local_ty.get(&p.local).copied();
                    if ty.is_some_and(|t| self.holds_borrow(t)) {
                        cx.item_roots.insert(p.local, item_roots.clone());
                    }
                }
            }
            // A stream given to a call is used up, whatever the parameter:
            // `a.chain(b)` reads `b` to its end.
            let take = mode_of(super::ArgSource::Arg(i)) == ParamMode::Take;
            let stream = self.is_stream_expr(&arg.value);
            self.mv(cx, &arg.value, take || stream);
            // Taken, it's the callee's to keep: `fs.push(x => x * k)`.
            if take {
                self.check_escape(cx, &arg.value, Escape::Stored);
            }
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
                let through = self.local_is_view(*l);
                self.changes_via(cx, *l, *span, through);
            }
        }
        // A projection keeps what it borrows until the statement ends: the
        // place it lent may be inside it.
        for (root, _, mode, span, _) in &places {
            let Root::Local(l) = root else { continue };
            let earlier = cx.lent.iter().find(|(o, m, _)| o == l && (*m == ParamMode::Mut || *mode == ParamMode::Mut));
            if let Some(&(_, _, at)) = earlier {
                let name = self.p.local(*l).name.clone();
                self.report(
                    cx,
                    Diagnostic::error(
                        codes::M0104,
                        format!("`{name}` is lent to a projection for this whole statement"),
                    )
                    .primary(*span, "used here again")
                    .secondary(at, "a projection borrows it here")
                    .help("use what the projection lends in a statement of its own"),
                );
            }
        }
        if self.b.lends.contains_key(&call.id) {
            for (root, _, mode, span, _) in &places {
                if let Root::Local(l) = root
                    && *mode != ParamMode::Take
                {
                    cx.lent.push((*l, *mode, *span));
                }
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

    /// Whether `e` is a place a projection lends, or part of one.
    fn lent(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Call { .. } => self.b.lends.contains_key(&e.id),
            ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::ConstField { base, .. } => {
                self.lent(base)
            }
            _ => false,
        }
    }

    /// Reads or moves the place `e`.
    /// Whether `e` is a stream (and not something made into one).
    fn is_stream_expr(&self, e: &Expr) -> bool {
        self.b.expr_ty.get(&e.id).is_some_and(|&t| self.stream_item(t).is_some())
    }

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
        self.thread_conflict(cx, cx.parts_of.get(&local).copied().unwrap_or(local), false, e.span);
        capture(cx, local);
        // A thread's handle given to something (`a.join()`): it's joined, and
        // what it held is free.
        if owning && path.is_empty() {
            cx.live.retain(|t| t.handle != Some(local));
        }
        if owning && path.is_empty() && self.p.local(local).kind == crate::hir::LocalKind::Spool {
            let name = self.p.local(local).name.clone();
            self.report(
                cx,
                Diagnostic::error(codes::M0107, format!("the spool `{name}` can't be moved"))
                    .primary(e.span, "a spool stays where it was made")
                    .help(format!("lend it: `given: {name}`")),
            );
            return;
        }
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
        // What was moved into a thread (a local of the loom block, or an item
        // of a `loom for`) is the thread's: it may move it on.
        let into_thread = cx.writes.last().is_some_and(|w| w.0 == cx.depth)
            && cx.looms.last().is_some_and(|&inside| cx.scope_of.get(&owner).is_some_and(|&s| s >= inside));
        if declared < cx.depth && !into_thread {
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
        // A thread's handle given up (`a.join()`): what it held is free.
        if path.is_empty() {
            cx.live.retain(|t| t.handle != Some(local));
        }
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
            // A `ref warp` holds borrows: it lives in locals and parameters.
            TyKind::Adt(def, _) => {
                Some(*def) == self.std.slice
                    || Some(*def) == self.std.stream
                    || matches!(&self.p.def(*def).kind, crate::hir::DefKind::Warp(w)
                        if w.modifier == loom_syntax::ast::WarpModifier::Ref)
            }
            _ => false,
        }
    }

    /// The locals the items of `recv` (the receiver of a method call) are
    /// borrowed from: its roots, if it is a list, a slice, a map, a set or
    /// a stream.
    fn receiver_roots(&mut self, cx: &MoveCx, recv: &Expr) -> Vec<LocalId> {
        let Some(ty) = self.b.expr_ty.get(&recv.id).copied().map(|t| self.deep_resolve(t)) else { return Vec::new() };
        let mut roots = Vec::new();
        if self.seq(ty).is_some() || self.coll_parts(ty).is_some() || self.is_borrowing_ty(ty) {
            self.borrowed_roots(cx, recv, &mut roots);
        }
        roots
    }

    /// Whether a value of `ty` can be, or hold, a borrow of something else:
    /// text and slices, and anything that isn't `Copy` (an item of a list is
    /// borrowed). A number is a copy of its item.
    fn holds_borrow(&mut self, ty: Ty) -> bool {
        let ty = self.deep_resolve(ty);
        match self.tcx.kind(ty).clone() {
            TyKind::Str => true,
            TyKind::Adt(def, _) if Some(def) == self.std.slice => true,
            TyKind::Tuple(items) => items.iter().any(|&t| self.holds_borrow(t)),
            TyKind::Optional(t) => self.holds_borrow(t),
            _ => !self.is_copy(ty),
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

    /// The locals that lambdas in the value of `e` capture: a lambda's own,
    /// a local's that holds one, and those of the lambdas a call that gives
    /// a borrow (a stream) was given. A captured local lives in the frame,
    /// so it counts even when it's a parameter.
    fn captured_roots(&mut self, cx: &MoveCx, e: &Expr, out: &mut Vec<LocalId>) {
        let add = |out: &mut Vec<LocalId>, l: LocalId| {
            if !out.contains(&l) {
                out.push(l);
            }
        };
        match &e.kind {
            ExprKind::Lambda { .. } => {
                for &l in cx.lambda_caps.get(&e.id).into_iter().flatten() {
                    add(out, l);
                }
            }
            ExprKind::Local(l) => {
                for &c in cx.captures_of.get(l).into_iter().flatten() {
                    add(out, c);
                }
            }
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
                    self.captured_roots(cx, base, out);
                }
                for arg in args {
                    self.captured_roots(cx, &arg.value, out);
                }
            }
            _ => {}
        }
    }

    /// The locals the value of `e` can't outlive, if it's a borrow (a
    /// function value, a slice or a stream): what its lambdas capture, and
    /// the local variables it's a view of. (A parameter's elements are its
    /// caller's, so a view of them can go anywhere the caller's can.)
    fn escaping_roots(&mut self, cx: &MoveCx, e: &Expr) -> Vec<LocalId> {
        let Some(&ty) = self.b.expr_ty.get(&e.id) else { return Vec::new() };
        if !self.is_borrowing_ty(ty) {
            return Vec::new();
        }
        let mut roots = Vec::new();
        self.captured_roots(cx, e, &mut roots);
        let mut viewed = Vec::new();
        self.borrowed_roots(cx, e, &mut viewed);
        for l in viewed {
            let param = matches!(self.p.local(l).kind, crate::hir::LocalKind::Param | crate::hir::LocalKind::SelfParam);
            // A local holding a borrow isn't what's borrowed: what it holds is.
            let holder = matches!(e.kind, ExprKind::Local(x) if x == l);
            if !param && !holder && !roots.contains(&l) {
                roots.push(l);
            }
        }
        roots
    }

    /// The expressions whose values `e`'s value is: itself, or the tails
    /// of its blocks and branches.
    fn value_leaves<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        let tail = |b: &'e Block| match b.stmts.last().map(|s| &s.kind) {
            Some(HirStmt::Expr(t)) => Some(t),
            _ => None,
        };
        match &e.kind {
            ExprKind::Block(b) | ExprKind::Unsafe { body: b, .. } => {
                if let Some(t) = tail(b) {
                    Self::value_leaves(t, out);
                }
            }
            ExprKind::If { then, else_, .. } => {
                if let Some(t) = tail(then) {
                    Self::value_leaves(t, out);
                }
                if let Some(e) = else_ {
                    Self::value_leaves(e, out);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    Self::value_leaves(&arm.body, out);
                }
            }
            ExprKind::Mend { expr, handler, .. } => {
                Self::value_leaves(expr, out);
                Self::value_leaves(handler, out);
            }
            _ => out.push(e),
        }
    }

    /// Reports the value of `e` going `into` somewhere it would outlive a
    /// local it borrows (M0106).
    fn check_escape(&mut self, cx: &MoveCx, e: &Expr, into: Escape) {
        let mut leaves = Vec::new();
        Self::value_leaves(e, &mut leaves);
        for leaf in leaves {
            let roots = self.escaping_roots(cx, leaf);
            if let Some(&root) = roots.first() {
                let what = match into {
                    Escape::Out => "returned",
                    Escape::Stored => "stored",
                };
                self.report_escape(leaf, root, what);
            }
        }
    }

    /// `target = value`: a borrow can go into a variable that ends no later
    /// than what it borrows, and into anything else (a field, an element, a
    /// `mut` parameter, which is its caller's) not at all.
    fn check_assigned_escape(&mut self, cx: &MoveCx, target: &Expr, value: &Expr) {
        let mut leaves = Vec::new();
        Self::value_leaves(value, &mut leaves);
        for leaf in leaves {
            let roots = self.escaping_roots(cx, leaf);
            let Some(&first) = roots.first() else { continue };
            let problem = match &target.kind {
                ExprKind::Local(t)
                    if !matches!(
                        self.p.local(*t).kind,
                        crate::hir::LocalKind::Param | crate::hir::LocalKind::SelfParam
                    ) =>
                {
                    let depth = cx.scope_of.get(t).copied().unwrap_or(0);
                    roots
                        .iter()
                        .find(|r| cx.scope_of.get(r).copied().unwrap_or(0) > depth)
                        .map(|&r| (r, "kept in a variable that outlives it"))
                }
                ExprKind::Local(_) => Some((first, "stored in the caller's variable")),
                _ => Some((first, "stored")),
            };
            if let Some((root, what)) = problem {
                self.report_escape(leaf, root, what);
            }
        }
    }

    /// M0106 for `e`, whose value borrows `root`.
    fn report_escape(&mut self, e: &Expr, root: LocalId, what: &str) {
        let name = self.p.local(root).name.clone();
        let thing =
            if matches!(e.kind, ExprKind::Lambda { .. }) { "a lambda that uses" } else { "a value that borrows" };
        let diag = Diagnostic::error(codes::M0106, format!("{thing} `{name}` can't be {what}"))
            .primary(e.span, format!("{what} here, it would outlive `{name}`"))
            .secondary(self.p.local(root).span, format!("`{name}` is here"))
            .note("a lambda borrows the variables it uses; a slice or a stream, what it's made from")
            .help("pass it as an argument or keep it in a local, or have it take what it needs as parameters");
        let already =
            self.diags.iter().any(|d| d.code == codes::M0106 && d.labels.iter().any(|l| l.primary && l.span == e.span));
        if !already {
            self.diags.push(diag);
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
            // An element written through a view changes what it views.
            let through = !path.is_empty() && self.local_is_view(l);
            self.changes_via(cx, l, target.span, through);
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
        cx.captures_of.remove(&l);
        let Some(&ty) = self.b.local_ty.get(&l) else { return };
        if !self.is_borrowing_ty(ty) {
            return;
        }
        let mut captured = Vec::new();
        self.captured_roots(cx, value, &mut captured);
        captured.retain(|&r| r != l);
        if !captured.is_empty() {
            cx.captures_of.insert(l, captured);
        }
        let mut roots = Vec::new();
        self.borrowed_roots(cx, value, &mut roots);
        roots.retain(|&r| r != l);
        if !roots.is_empty() {
            cx.borrows.insert(l, roots);
        }
    }

    /// What the view in `l` is a view of: the locals it was made from.
    fn roots_of(cx: &MoveCx, l: LocalId) -> Vec<LocalId> {
        let mut roots: Vec<LocalId> = cx.item_roots.get(&l).cloned().unwrap_or_default();
        for r in cx.borrows.get(&l).into_iter().flatten().chain(cx.parts_of.get(&l)) {
            if !roots.contains(r) {
                roots.push(*r);
            }
        }
        roots
    }

    /// Whether local `l` is a view (a slice).
    fn local_is_view(&mut self, l: LocalId) -> bool {
        let Some(&ty) = self.b.local_ty.get(&l) else { return false };
        let ty = self.deep_resolve(ty);
        matches!(self.tcx.kind(ty), TyKind::Adt(def, _) if Some(*def) == self.std.slice)
    }

    /// Whether a loop goes over a `#disjoint` stream: one of its methods
    /// (std marks `chunks_mut`) gives pieces that don't overlap.
    fn iter_is_disjoint(&mut self, e: &Expr) -> bool {
        let ExprKind::Call { callee, .. } = &e.kind else { return false };
        if let Some(info) = self.b.calls.get(&e.id) {
            let def = self.r.instance(info.callee).def;
            // Only std's word is taken for it.
            if self.p.def_path(def).starts_with("std.") && self.p.def(def).attrs.iter().any(|a| a.name == "disjoint") {
                return true;
            }
        }
        match &callee.kind {
            ExprKind::Field { base, .. } => self.iter_is_disjoint(base),
            _ => false,
        }
    }

    /// Whether the value of `e` holds a thread's `Spun`.
    fn value_is_handle(&mut self, e: &Expr) -> bool {
        let ty = self.b.expr_ty.get(&e.id).copied();
        ty.is_some_and(|t| self.mentions_spun(t))
    }

    fn handle_escapes(&mut self, cx: &mut MoveCx, at: Span, how: &str) {
        if cx.flow.dead {
            return;
        }
        self.report(
            cx,
            Diagnostic::error(codes::C0404, "a thread's handle can't leave its `loom` block")
                .primary(at, format!("{how} here"))
                .note("the threads are done when the block ends"),
        );
    }

    /// `local` is read (or changed, with `write`) at `at`: a thread spawned
    /// earlier that's still going holds it. A thread may change what no other
    /// holds, and the rest only read what only they read.
    fn thread_conflict(&mut self, cx: &mut MoveCx, local: LocalId, write: bool, at: Span) {
        if cx.flow.dead || cx.live.is_empty() {
            return;
        }
        let in_thread = !cx.writes.is_empty();
        let found = cx.live.iter().find_map(|t| {
            if t.writes.contains(&local) {
                Some((t.span, true))
            } else if write && t.reads.contains(&local) {
                Some((t.span, false))
            } else {
                None
            }
        });
        let Some((thread, changes)) = found else { return };
        // `hits += 1` reads and changes: say it once.
        if self.diags.iter().any(|d| d.code.id.starts_with("C04") && d.labels.iter().any(|l| l.primary && l.span == at))
        {
            return;
        }
        let name = self.p.local(local).name.clone();
        let diag = if write && changes && in_thread {
            Diagnostic::error(codes::C0401, format!("`{name}` is changed by more than one thread"))
                .primary(at, "changed here")
                .secondary(thread, "another thread changes it")
                .help("use `Atomic`, or produce a value per thread and combine them")
        } else {
            let (what, held) = if changes { ("used", "changes") } else { ("changed", "reads") };
            Diagnostic::error(codes::C0402, format!("`{name}` is {what} while a thread {held} it"))
                .primary(at, format!("{what} here"))
                .secondary(thread, format!("this thread {held} it, and hasn't been joined"))
                .help("`join` the thread first, or give it its own copy")
        };
        self.report(cx, diag);
    }

    /// `l` moves or changes at `at`: the borrows of it made before go stale,
    /// and a loop going over it can't let that happen.
    fn changes(&mut self, cx: &mut MoveCx, l: LocalId, at: Span) {
        self.changes_via(cx, l, at, false);
    }

    /// `changes`, where `through_view`: an element of a view is written,
    /// which changes what it's a view of and not the view.
    fn changes_via(&mut self, cx: &mut MoveCx, l: LocalId, at: Span, through_view: bool) {
        if cx.flow.dead {
            return;
        }
        let owner = cx.parts_of.get(&l).copied().unwrap_or(l);
        // What threads are told it changed: the view's roots (none, for a
        // view of a `#disjoint` stream: the iterations don't overlap, and
        // the loop keeps everyone else off the list until it ends).
        let changed: Vec<LocalId> = if !through_view {
            vec![owner]
        } else if cx.disjoint_items.contains(&owner) {
            Vec::new()
        } else {
            Self::roots_of(cx, owner)
        };
        for &c in &changed {
            self.thread_conflict(cx, c, true, at);
            let declared = cx.depth_of.get(&c).copied();
            for (depth, written) in cx.writes.iter_mut() {
                if declared.is_some_and(|d| d < *depth) && !written.iter().any(|(w, _)| *w == c) {
                    written.push((c, at));
                }
            }
        }
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
            // In the arm (or the rest of the condition's block), a block deeper.
            cx.scope_of.insert(local, cx.scopes.len() + 1);
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

    /// Where the place `e` starts, and the fields (or `[]` for an element,
    /// `^` for a pointer's target) from there. `None` if `e` isn't a place.
    fn place_of(&self, cx: &MoveCx, e: &Expr) -> Option<(Root, Vec<String>)> {
        match &e.kind {
            ExprKind::Local(l) => Some((Root::Local(*l), Vec::new())),
            ExprKind::Deref(base) => {
                let (root, mut path) = self.place_of(cx, base)?;
                path.push("^".into());
                Some((root, path))
            }
            ExprKind::Field { base, field } => {
                let (root, mut path) = self.place_of(cx, base)?;
                if self.derefs(base) {
                    path.push("^".into());
                }
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
pub(super) fn pat_locals(pat: &Pat) -> Vec<LocalId> {
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
