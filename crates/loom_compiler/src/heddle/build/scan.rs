//! A look over a function's body before it's built: which locals must live
//! in memory, and what each lambda captures.
//!
//! A local lives in registers (as SSA values) unless something needs its
//! address: a `mut` argument (the callee writes through it), a lambda or a
//! `defer` that uses it (a lambda runs over the frame's copy; a landing pad
//! reads memory, never registers), or a dot-block that edits it in place.

use std::collections::{HashMap, HashSet};

use loom_syntax::ast::ParamMode;

use super::Builder;
use crate::hir::{self, Block, ExprKind, HirId, LocalId, PatKind, StmtKind};
use crate::typeck::{ArgSource, BodyId, Receiver};

#[derive(Debug, Default)]
pub(super) struct Scan {
    /// Locals whose address is needed.
    pub in_memory: HashSet<LocalId>,
    /// The locals each lambda uses from outside it, in order of first use.
    pub captures: HashMap<HirId, Vec<LocalId>>,
    /// The same for each `defer` statement's body, by the body's id.
    pub deferred: HashMap<HirId, Vec<LocalId>>,
}

/// Something directly inside an expression.
#[derive(Clone, Copy)]
pub(super) enum Node<'e> {
    Expr(&'e hir::Expr),
    Stmt(&'e hir::Stmt),
    Pat(&'e hir::Pat),
}

/// What's directly inside `e`, in source order.
pub(super) fn children(e: &hir::Expr) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    fn stmts<'e>(b: &'e Block, out: &mut Vec<Node<'e>>) {
        out.extend(b.stmts.iter().map(Node::Stmt));
    }
    let x = Node::Expr;
    match &e.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Def { .. }
        | ExprKind::Type(_)
        | ExprKind::Assoc { .. }
        | ExprKind::BuiltinCase(_)
        | ExprKind::Intrinsic(_)
        | ExprKind::LeadingDot(_)
        | ExprKind::Continue { .. }
        | ExprKind::Pull
        | ExprKind::Error => {}
        ExprKind::Template(parts) => {
            for part in parts {
                if let hir::TemplatePart::Interp { expr, .. } = part {
                    out.push(x(expr));
                }
            }
        }
        ExprKind::Tuple(items) | ExprKind::Array(items) => out.extend(items.iter().map(x)),
        ExprKind::ArrayRepeat { value, count } => out.extend([x(value), x(count)]),
        ExprKind::Map(pairs) => {
            for (k, v) in pairs {
                out.extend([x(k), x(v)]);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop { body: b } | ExprKind::Unsafe { body: b, .. } => stmts(b, &mut out),
        ExprKind::If { cond, then, else_, .. } => {
            out.push(x(cond));
            stmts(then, &mut out);
            if let Some(e) = else_ {
                out.push(x(e));
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            out.push(x(scrutinee));
            for arm in arms {
                out.push(Node::Pat(&arm.pat));
                if let Some(g) = &arm.guard {
                    out.push(x(g));
                }
                out.push(x(&arm.body));
            }
        }
        ExprKind::For { pat, iter, body, .. } => {
            out.push(x(iter));
            out.push(Node::Pat(pat));
            stmts(body, &mut out);
        }
        ExprKind::While { cond, body } => {
            out.push(x(cond));
            stmts(body, &mut out);
        }
        ExprKind::Loom { args, body, .. } => {
            out.extend(args.iter().map(|a| x(&a.value)));
            stmts(body, &mut out);
        }
        ExprKind::Thread { body: v, .. }
        | ExprKind::Fail(v)
        | ExprKind::Const(v)
        | ExprKind::Try(v)
        | ExprKind::Deref(v)
        | ExprKind::Yield { value: v, .. } => out.push(x(v)),
        ExprKind::Break { value, .. } | ExprKind::Return(value) => out.extend(value.iter().map(|v| x(v))),
        ExprKind::Lambda { body, .. } => out.push(x(body)),
        ExprKind::Call { callee, args } => {
            out.push(x(callee));
            out.extend(args.iter().map(|a| x(&a.value)));
        }
        ExprKind::Index { base, args } => {
            out.push(x(base));
            for a in args {
                if let hir::GenericArg::Expr(i) = a {
                    out.push(x(i));
                }
            }
        }
        ExprKind::Field { base, .. } => out.push(x(base)),
        ExprKind::ConstField { base, field } => out.extend([x(base), x(field)]),
        ExprKind::DotBlock { subject, body } => {
            out.push(x(subject));
            stmts(body, &mut out);
        }
        ExprKind::Unary { operand, .. } => out.push(x(operand)),
        ExprKind::Binary { lhs, rhs, .. } => out.extend([x(lhs), x(rhs)]),
        ExprKind::Chain { first, rest } => {
            out.push(x(first));
            out.extend(rest.iter().map(|(_, r)| x(r)));
        }
        ExprKind::Is { expr, pat } => {
            out.push(x(expr));
            out.push(Node::Pat(pat));
        }
        ExprKind::Cast { expr, .. } => out.push(x(expr)),
        ExprKind::Range { lo, hi, .. } => out.extend(lo.iter().chain(hi.iter()).map(|b| x(b))),
        ExprKind::Mend { expr, handler, .. } => out.extend([x(expr), x(handler)]),
        ExprKind::Assign { target, value, .. } => out.extend([x(value), x(target)]),
    }
    out
}

/// The locals a pattern binds.
pub(super) fn pat_locals(p: &hir::Pat, out: &mut Vec<LocalId>) {
    match &p.kind {
        PatKind::Bind(l) | PatKind::TypeTest { local: l, .. } | PatKind::Rest(Some(l)) => out.push(*l),
        PatKind::Twill { captures, .. } => out.extend(captures),
        PatKind::Tuple(ps) | PatKind::Slice(ps) | PatKind::Or(ps) => ps.iter().for_each(|p| pat_locals(p, out)),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => pat_locals(p, out),
        PatKind::Case { fields, .. } => fields.iter().flatten().for_each(|f| pat_locals(&f.pat, out)),
        PatKind::Range { lo, hi, .. } => {
            lo.iter().chain(hi.iter()).for_each(|p| pat_locals(p, out));
        }
        _ => {}
    }
}

struct Scanner<'b, 'a> {
    b: &'b Builder<'a>,
    /// Whose tables are being read.
    body: BodyId,
    scan: Scan,
    /// Lambdas and `defer`s being gone through, innermost last: what's
    /// declared inside each, and what it uses from outside.
    open: Vec<(HashSet<LocalId>, Vec<LocalId>)>,
}

impl<'a> Builder<'a> {
    /// Looks over `body`, the body of function instance `id`.
    pub(super) fn scan(&self, id: BodyId, body: &hir::Expr) -> Scan {
        let mut s = Scanner { b: self, body: id, scan: Scan::default(), open: Vec::new() };
        s.expr(body);
        s.scan
    }
}

impl Scanner<'_, '_> {
    fn tables(&self) -> &crate::typeck::Body {
        self.b.typed.results.body(self.body)
    }

    /// A use of local `l`: a capture, for each lambda or `defer` it's
    /// declared outside of.
    fn use_local(&mut self, l: LocalId) {
        let mut captured = false;
        for (declared, uses) in self.open.iter_mut().rev() {
            if declared.contains(&l) {
                break;
            }
            captured = true;
            if !uses.contains(&l) {
                uses.push(l);
            }
        }
        if captured {
            self.scan.in_memory.insert(l);
        }
    }

    fn declare(&mut self, l: LocalId) {
        if let Some((declared, _)) = self.open.last_mut() {
            declared.insert(l);
        }
    }

    fn pat(&mut self, p: &hir::Pat) {
        let mut locals = Vec::new();
        pat_locals(p, &mut locals);
        for l in locals {
            self.declare(l);
        }
        if let PatKind::ConstCase(e) = &p.kind {
            self.expr(e);
        }
    }

    /// The local a place starts from: `p` in `p.pos.x` or `xs[i]`.
    fn root(&self, e: &hir::Expr) -> Option<LocalId> {
        match &e.kind {
            ExprKind::Local(l) => Some(*l),
            ExprKind::Field { base, .. } | ExprKind::ConstField { base, .. } | ExprKind::Index { base, .. } => {
                match self.tables().fields.get(&e.id).map(|f| f.base) {
                    Some(Receiver::Local(l)) => Some(l),
                    _ => self.root(base),
                }
            }
            ExprKind::LeadingDot(_) => match self.tables().fields.get(&e.id).map(|f| f.base) {
                Some(Receiver::Local(l)) => Some(l),
                _ => None,
            },
            _ => None,
        }
    }

    fn needs_address(&mut self, e: &hir::Expr) {
        if let Some(l) = self.root(e) {
            self.scan.in_memory.insert(l);
        }
    }

    fn stmt(&mut self, s: &hir::Stmt) {
        match &s.kind {
            StmtKind::Let { pat, init, .. } => {
                self.expr(init);
                self.pat(pat);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Defer { binding, body } => {
                self.open.push((HashSet::new(), Vec::new()));
                // A fallback `defer x =>` uses `x` too.
                if let Some(b) = binding {
                    self.use_local(*b);
                }
                self.expr(body);
                let (_, uses) = self.open.pop().unwrap_or_default();
                for &l in &uses {
                    self.scan.in_memory.insert(l);
                }
                self.scan.deferred.insert(body.id, uses.clone());
                // What it uses is used by what's around it, too.
                for l in uses {
                    self.use_local(l);
                }
            }
            StmtKind::Given { value, .. } => self.expr(value),
            StmtKind::Spool { init, .. } => self.expr(init),
            StmtKind::Item(_) => {}
        }
    }

    fn expr(&mut self, e: &hir::Expr) {
        match &e.kind {
            ExprKind::Local(l) => self.use_local(*l),
            ExprKind::LeadingDot(_) => {
                if let Some(Receiver::Local(l)) = self.tables().fields.get(&e.id).map(|f| f.base) {
                    self.use_local(l);
                }
            }
            ExprKind::Lambda { params, body } => {
                self.open.push((params.iter().map(|p| p.local).collect(), Vec::new()));
                self.expr(body);
                let (_, uses) = self.open.pop().unwrap_or_default();
                self.scan.captures.insert(e.id, uses.clone());
                for l in uses {
                    self.use_local(l);
                }
                return;
            }
            // A thread is a function of its own, like a lambda: it captures
            // what it uses, and its loom's group.
            ExprKind::Thread { group, body, .. } => {
                self.open.push((HashSet::new(), Vec::new()));
                if let Some(g) = group {
                    self.use_local(*g);
                }
                self.expr(body);
                let (_, uses) = self.open.pop().unwrap_or_default();
                self.scan.captures.insert(e.id, uses.clone());
                for l in uses {
                    self.use_local(l);
                }
                return;
            }
            ExprKind::Loom { group, .. } => self.declare(*group),
            ExprKind::Call { callee, args } => self.call(e, callee, args),
            ExprKind::Yield { place: Some(_), value } => self.needs_address(value),
            ExprKind::DotBlock { subject, .. } if self.tables().dot_blocks.get(&e.id) == Some(&true) => {
                self.needs_address(subject);
            }
            ExprKind::For { is_const: true, body, .. } => {
                // Each item's copy of the body has tables of its own.
                for id in self.tables().unrolled.get(&e.id).cloned().unwrap_or_default() {
                    let saved = std::mem::replace(&mut self.body, id);
                    body.stmts.iter().for_each(|s| self.stmt(s));
                    self.body = saved;
                }
                return;
            }
            ExprKind::Mend { binding: Some(b), .. } => self.declare(*b),
            _ => {}
        }
        for node in children(e) {
            match node {
                Node::Expr(x) => self.expr(x),
                Node::Stmt(st) => self.stmt(st),
                Node::Pat(p) => self.pat(p),
            }
        }
    }

    /// A call: its `mut` arguments need their places' addresses, including
    /// the receiver of a `mut self` method.
    fn call(&mut self, e: &hir::Expr, callee: &hir::Expr, args: &[hir::Arg]) {
        let tables = self.tables();
        if let Some(info) = tables.calls.get(&e.id).cloned()
            && let Some(sig) = self.b.typed.results.sigs.get(&info.callee)
        {
            let method = match &callee.kind {
                ExprKind::Index { base, .. } => base.as_ref(),
                _ => callee,
            };
            for (k, source) in info.args.iter().enumerate() {
                if sig.params.get(k).map(|p| p.mode) != Some(ParamMode::Mut) {
                    continue;
                }
                match source {
                    ArgSource::Arg(i) => self.needs_address(&args[*i].value),
                    ArgSource::Receiver => match (info.receiver, &method.kind) {
                        (Some(Receiver::Expr), ExprKind::Field { base, .. }) => self.needs_address(base),
                        (Some(Receiver::Local(l)), _) => {
                            self.scan.in_memory.insert(l);
                        }
                        _ => {}
                    },
                    ArgSource::Default => {}
                }
            }
        }
    }
}
