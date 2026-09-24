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

use std::collections::{HashMap, HashSet};

use loom_syntax::Diagnostic;
use loom_syntax::ast::{AssignOp, BinaryOp, ParamMode};
use loom_syntax::diag::{Code, codes};
use loom_syntax::source::Span;

use super::{Checker, Receiver};
use crate::hir::{Arg, Block, Expr, ExprKind, FnDef, GenericArg, HirId, LocalId, Pat, PatKind, StmtKind as HirStmt};
use crate::ty::TyKind;
use loom_syntax::ast::FieldName;

/// What may have been moved at a point in the code.
#[derive(Clone, Debug, Default)]
struct Flow {
    /// Each moved place: its variable, its field path, and where it moved.
    moved: Vec<(LocalId, Vec<String>, Span)>,
    /// No code gets here (after a `return` or a `break`).
    dead: bool,
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
        Flow { moved, dead: false }
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
    reported: HashSet<(&'static str, Span)>,
}

impl Checker<'_> {
    /// Checks the moves in the body of function `f`, whose types are known.
    pub(super) fn check_moves(&mut self, f: &FnDef, ret_unit: bool) {
        let Some(body) = &f.body else { return };
        let mut cx = MoveCx::default();
        for param in &f.params {
            cx.depth_of.insert(param.local, 0);
            if param.mode != ParamMode::Take {
                cx.borrowed.insert(param.local, Borrow::Param);
            }
        }
        self.mv(&mut cx, body, !ret_unit);
    }

    /// Goes through `e`. `owning`: its value goes to an owning slot.
    fn mv(&mut self, cx: &mut MoveCx, e: &Expr, owning: bool) {
        match &e.kind {
            ExprKind::Local(_) | ExprKind::LeadingDot(_) => self.mv_place(cx, e, owning),
            ExprKind::Field { base, .. } => match self.place_of(cx, e) {
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
                        self.mv(cx, guard, false);
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
                // from it; a stream's items are its own.
                if is_seq {
                    self.mv(cx, iter, false);
                } else {
                    let ranged = iter_ty.is_some_and(|t| matches!(self.tcx.kind(t), TyKind::Range(_)));
                    self.mv(cx, iter, !ranged);
                }
                let borrowed = is_seq && self.place_of(cx, iter).is_some();
                for local in pat_locals(pat) {
                    cx.depth_of.insert(local, cx.depth);
                    if borrowed {
                        cx.borrowed.insert(local, Borrow::LoopItem);
                    }
                }
                self.mv_loop(cx, |this, cx| this.mv_block(cx, body, false));
            }
            ExprKind::While { cond, body } => self.mv_loop(cx, |this, cx| {
                this.mv(cx, cond, false);
                this.mv_block(cx, body, false);
            }),
            ExprKind::Loop { body } => self.mv_loop(cx, |this, cx| this.mv_block(cx, body, false)),
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.mv(cx, v, true);
                }
                cx.flow.dead = true;
            }
            ExprKind::Continue { .. } => cx.flow.dead = true,
            ExprKind::Return(value) => {
                if let Some(v) = value {
                    self.mv(cx, v, true);
                }
                cx.flow.dead = true;
            }
            ExprKind::Fail(v) => {
                self.mv(cx, v, true);
                cx.flow.dead = true;
            }
            ExprKind::Lambda { params, body } => {
                let saved = cx.flow.clone();
                cx.depth += 1;
                for p in params {
                    cx.depth_of.insert(p.local, cx.depth);
                    cx.borrowed.insert(p.local, Borrow::LambdaParam);
                }
                self.mv(cx, body, true);
                cx.depth -= 1;
                cx.flow = saved;
            }
            ExprKind::Try(inner) => self.mv(cx, inner, owning),
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
                match (op, self.place_of(cx, target)) {
                    // A whole new value: what was moved out of it is back.
                    (AssignOp::Assign, Some((Root::Local(l), path))) => {
                        let parent = &path[..path.len().saturating_sub(1)];
                        if !path.is_empty() {
                            self.check_use(cx, l, parent, target.span);
                        }
                        cx.flow.moved.retain(|(m, p, _)| !(*m == l && p.starts_with(&path)));
                    }
                    _ => self.mv(cx, target, false),
                }
            }
            ExprKind::Lit(_)
            | ExprKind::Def { .. }
            | ExprKind::Type(_)
            | ExprKind::Assoc { .. }
            | ExprKind::BuiltinCase(_)
            | ExprKind::Intrinsic(_)
            | ExprKind::Map(_)
            | ExprKind::Loom { .. }
            | ExprKind::Thread(_)
            | ExprKind::Yield { .. }
            | ExprKind::Pull
            | ExprKind::Const(_)
            | ExprKind::ConstField { .. }
            | ExprKind::Deref(_)
            | ExprKind::Error => {}
        }
    }

    fn mv_block(&mut self, cx: &mut MoveCx, block: &Block, owning: bool) {
        let Some((last, rest)) = block.stmts.split_last() else { return };
        for stmt in rest {
            self.mv_stmt(cx, stmt, false);
        }
        self.mv_stmt(cx, last, owning);
    }

    fn mv_stmt(&mut self, cx: &mut MoveCx, stmt: &crate::hir::Stmt, owning: bool) {
        match &stmt.kind {
            HirStmt::Let { pat, init, .. } => {
                self.mv(cx, init, true);
                for local in pat_locals(pat) {
                    cx.depth_of.insert(local, cx.depth);
                }
            }
            HirStmt::Expr(e) => self.mv(cx, e, owning),
            HirStmt::Given { value, .. } => self.mv(cx, value, false),
            HirStmt::Spool { init, .. } => self.mv(cx, init, false),
            HirStmt::Defer { body, .. } => self.mv(cx, body, false),
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
        let modes: Vec<ParamMode> =
            self.r.sigs.get(&info.callee).map(|s| s.params.iter().map(|p| p.mode).collect()).unwrap_or_default();
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
            let reads =
                matches!(self.b.streams.get(&call.id), Some(super::IterSource::Seq(_) | super::IterSource::Range(_)));
            let through = !info.through.is_empty();
            self.mv(cx, base, recv_mode == ParamMode::Take && !reads && !through);
        } else if recv_mode == ParamMode::Take
            && let Some(Receiver::Local(l)) = info.receiver
        {
            // `.consume()` in a method: takes `self`.
            self.move_local(cx, l, &[], callee.span, callee.span);
        }
        for (i, arg) in args.iter().enumerate() {
            let take = mode_of(super::ArgSource::Arg(i)) == ParamMode::Take;
            self.mv(cx, &arg.value, take);
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
        self.move_local(cx, local, &path, e.span, e.span);
    }

    /// Moves `local`'s place at `path`, or reports why it can't be.
    fn move_local(&mut self, cx: &mut MoveCx, local: LocalId, path: &[String], span: Span, at: Span) {
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
            self.report(
                cx,
                Diagnostic::error(codes::M0102, format!("can't move `{name}` out of the lambda that uses it"))
                    .primary(span, "moved here")
                    .note("a lambda borrows the variables it uses")
                    .help("copy it (`x.copy()`) instead"),
            );
            return;
        }
        if owner != local {
            cx.flow.moved.push((owner, Vec::new(), at));
        }
        cx.flow.moved.push((local, path.to_vec(), at));
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
        // The same spot, gone through again: a loop.
        let diag = if at == span {
            Diagnostic::error(codes::M0101, format!("`{name}` is moved again in the next iteration of a loop"))
                .primary(span, "moved here on the first iteration")
                .help("move it only once (then `break`), or give it a new value before the next iteration")
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

    /// Records the locals a pattern binds from the place starting at `root`.
    fn bind_parts(&mut self, cx: &mut MoveCx, pat: &Pat, root: Option<&Root>) {
        for local in pat_locals(pat) {
            cx.depth_of.insert(local, cx.depth);
            if let Some(Root::Local(owner)) = root {
                let owner = cx.parts_of.get(owner).copied().unwrap_or(*owner);
                cx.parts_of.insert(local, owner);
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
            _ => None,
        }
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
