//! Region checking (design/v6-spools.md): which spool each value points
//! into, and that nothing outlives it or is used after it was reset.
//!
//! Types say whether a pointer is into the heap or into *a* spool, not
//! which. This pass finds that out for the values of one function, as far
//! as the code says it: a value is rooted in the spools it was allocated in
//! (`^e`, `arena.put(e)`, or a call that returns into the spool it was
//! given), and keeps the roots of what it is made of or taken from. Then:
//!
//! - R0101: a value rooted in a spool of this function leaves the block the
//!   spool ends with: returned, failed with, or stored in an outer variable.
//! - R0102: a value is used after the spool it is rooted in was reset.
//! - R0104: an argument is rooted in another spool than the parameter says
//!   (`fun f(e: ^Expr@sp) given sp: Spool` called with `given: other`).
//!
//! It runs only for functions that use spools at all.

use std::collections::HashMap;

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{ArgSource, Checker};
use crate::hir::{Block, DefKind, Expr, ExprKind, FnDef, LocalId, LocalKind, Pat, Region, StmtKind, TyKind as HirTy};
use crate::ty::{Ty, TyKind};

/// The spools a value is in: a `spool` binding or a `given`, and how many
/// times it had been reset when the value was made. A value made after a
/// reset is fine.
type Roots = Vec<(LocalId, u32)>;

fn join(a: &mut Roots, b: &[(LocalId, u32)]) {
    for &x in b {
        if !a.contains(&x) {
            a.push(x);
        }
    }
}

#[derive(Clone, Default)]
struct Flow {
    roots: HashMap<LocalId, Roots>,
    /// How many times each spool was reset so far, and where last.
    resets: HashMap<LocalId, (u32, Span)>,
}

impl Flow {
    fn merge(&mut self, other: &Flow) {
        for (l, rs) in &other.roots {
            join(self.roots.entry(*l).or_default(), rs);
        }
        for (&spool, &(n, at)) in &other.resets {
            let mine = self.resets.entry(spool).or_insert((0, at));
            if n > mine.0 {
                *mine = (n, at);
            }
        }
    }

    /// A root in `spool` as it is now.
    fn root(&self, spool: LocalId) -> (LocalId, u32) {
        (spool, self.resets.get(&spool).map_or(0, |r| r.0))
    }
}

#[derive(Default)]
struct Cx {
    flow: Flow,
    /// How deep in blocks each local was declared.
    depth_of: HashMap<LocalId, u32>,
    depth: u32,
    /// `given name = spool`: the spool.
    alias: HashMap<LocalId, LocalId>,
    /// Inside a `thread`: how deep the blocks were where it started.
    thread_base: Option<u32>,
}

impl Checker<'_> {
    /// Checks the regions of function `f`'s body, if it uses spools.
    pub(super) fn check_regions(&mut self, f: &FnDef) {
        let Some(body) = &f.body else { return };
        if !self.b.spools && f.givens.is_empty() {
            return;
        }
        let mut cx = Cx::default();
        for g in &f.givens {
            cx.depth_of.insert(g.local, 0);
        }
        let roots = self.rv(&mut cx, body);
        let tail = match &body.kind {
            ExprKind::Block(b) => match b.stmts.last().map(|s| &s.kind) {
                Some(StmtKind::Expr(e)) => e.span,
                _ => body.span,
            },
            _ => body.span,
        };
        self.leaves(&roots, tail, "returned");
    }

    /// Whether `ty` has a pointer, or a warp holding pointers, into a spool.
    fn in_spool(&self, ty: Ty) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty) {
            TyKind::Ptr(t, r) => *r == self.tcx.spool || self.in_spool(*t),
            TyKind::Adt(_, args) | TyKind::Tuple(args) | TyKind::Union(args) => {
                args.iter().any(|&a| a == self.tcx.spool || self.in_spool(a))
            }
            TyKind::Optional(t) | TyKind::Array(t, _) | TyKind::Range(t) => self.in_spool(*t),
            TyKind::Result(t, e) => self.in_spool(*t) || self.in_spool(*e),
            _ => false,
        }
    }

    /// The spool local an expression names (`raw` in `given: raw`).
    fn spool_of(&self, cx: &Cx, e: &Expr) -> Option<LocalId> {
        let ExprKind::Local(l) = e.kind else { return None };
        self.resolve_spool(cx, l)
    }

    fn resolve_spool(&self, cx: &Cx, l: LocalId) -> Option<LocalId> {
        let l = cx.alias.get(&l).copied().unwrap_or(l);
        matches!(self.p.local(l).kind, LocalKind::Spool | LocalKind::Given).then_some(l)
    }

    /// Reports each spool in `roots` that ends in this function, for a value
    /// that leaves it.
    fn leaves(&mut self, roots: &Roots, span: Span, how: &str) {
        for &(r, _) in roots {
            if self.p.local(r).kind != LocalKind::Spool {
                continue;
            }
            let name = self.p.local(r).name.clone();
            let diag =
                Diagnostic::error(codes::R0101, format!("{how} value points into spool `{name}`, which ends here"))
                    .primary(span, format!("this points into `{name}`"))
                    .secondary(self.p.local(r).span, "spool declared here")
                    .help(
                        "take `given out: Spool` and return `^T@out`, or `.copy()` it into the spool the caller gave",
                    );
            self.report_once(diag);
        }
    }

    fn report_once(&mut self, diag: Diagnostic) {
        let span = diag.labels.iter().find(|l| l.primary).map(|l| l.span);
        let seen = self
            .diags
            .iter()
            .any(|d| d.code == diag.code && d.labels.iter().find(|l| l.primary).map(|l| l.span) == span);
        if !seen {
            self.diags.push(diag);
        }
    }

    /// Whether the spool local `spool` is a `SharedArena`, which threads may use.
    fn is_shared(&self, spool: LocalId) -> bool {
        let ty = self.b.local_ty.get(&spool).map(|&t| self.resolve(t));
        matches!(ty.map(|t| self.tcx.kind(t)), Some(TyKind::Adt(def, _)) if Some(*def) == self.std.shared_arena)
    }

    /// A thread uses `l`, declared outside it: it can't be a spool, or point
    /// into one, that threads can't share.
    fn thread_use(&mut self, cx: &Cx, l: LocalId, span: Span) {
        let Some(base) = cx.thread_base else { return };
        if cx.depth_of.get(&l).is_some_and(|&d| d > base) {
            return;
        }
        let spools: Vec<LocalId> = match self.resolve_spool(cx, l) {
            Some(s) => vec![s],
            None => cx.flow.roots.get(&l).map(|r| r.iter().map(|&(s, _)| s).collect()).unwrap_or_default(),
        };
        for s in spools {
            if self.is_shared(s) {
                continue;
            }
            let (name, spool) = (self.p.local(l).name.clone(), self.p.local(s).name.clone());
            let diag =
                Diagnostic::error(codes::R0103, format!("`{name}` reaches the spool `{spool}` from another thread"))
                    .primary(span, "used by a thread here")
                    .secondary(self.p.local(s).span, "spool declared here")
                    .help("allocate in a `SharedArena`, which threads may use");
            self.report_once(diag);
        }
    }

    /// A read of local `l`: it must not point into a spool that was reset.
    fn use_local(&mut self, cx: &Cx, l: LocalId, span: Span) {
        self.thread_use(cx, l, span);
        let Some(roots) = cx.flow.roots.get(&l) else { return };
        for &(r, made) in roots {
            let Some(&(n, at)) = cx.flow.resets.get(&r) else { continue };
            if n <= made {
                continue;
            }
            let (name, spool) = (self.p.local(l).name.clone(), self.p.local(r).name.clone());
            let diag = Diagnostic::error(codes::R0102, format!("`{name}` points into `{spool}`, which was reset"))
                .primary(span, "used here")
                .secondary(at, format!("`{spool}` reset here"))
                .help("move the reset after the last use, or `.copy()` the value into another spool first");
            self.report_once(diag);
        }
    }

    fn bind_roots(&mut self, cx: &mut Cx, pat: &Pat, roots: &Roots) {
        let mut locals = Vec::new();
        super::moves::pat_locals(pat).into_iter().for_each(|l| locals.push(l));
        for l in locals {
            cx.depth_of.insert(l, cx.depth);
            if roots.is_empty() {
                cx.flow.roots.remove(&l);
            } else {
                cx.flow.roots.insert(l, roots.clone());
            }
        }
    }

    /// The roots of the value of block `b`.
    fn rblock(&mut self, cx: &mut Cx, b: &Block) -> Roots {
        cx.depth += 1;
        let mut value = Roots::new();
        let last = b.stmts.len().saturating_sub(1);
        for (i, stmt) in b.stmts.iter().enumerate() {
            let r = match &stmt.kind {
                StmtKind::Let { pat, init, .. } => {
                    let r = self.rv(cx, init);
                    self.bind_roots(cx, pat, &r);
                    Roots::new()
                }
                StmtKind::Spool { local, init } => {
                    self.rv(cx, init);
                    cx.depth_of.insert(*local, cx.depth);
                    Roots::new()
                }
                StmtKind::Given { local, value } => {
                    self.rv(cx, value);
                    if let (Some(local), Some(spool)) = (local, self.spool_of(cx, value)) {
                        cx.alias.insert(*local, spool);
                    }
                    Roots::new()
                }
                StmtKind::Expr(e) => self.rv(cx, e),
                StmtKind::Defer { body, .. } => {
                    self.rv(cx, body);
                    Roots::new()
                }
                StmtKind::Item(_) => Roots::new(),
            };
            if i == last {
                value = r;
            }
        }
        cx.depth -= 1;
        value
    }

    /// Goes through `e`, in the order it runs; gives the spools its value
    /// points into.
    fn rv(&mut self, cx: &mut Cx, e: &Expr) -> Roots {
        let roots = self.rv_inner(cx, e);
        // A number read through a pointer, a length, a flag: nothing in a
        // spool, whatever it was taken from.
        let holds = self.b.expr_ty.get(&e.id).is_none_or(|&t| self.in_spool(t));
        if holds { roots } else { Roots::new() }
    }

    fn rv_inner(&mut self, cx: &mut Cx, e: &Expr) -> Roots {
        match &e.kind {
            ExprKind::Local(l) => {
                self.use_local(cx, *l, e.span);
                cx.flow.roots.get(l).cloned().unwrap_or_default()
            }
            ExprKind::Block(b) | ExprKind::Unsafe { body: b, .. } => self.rblock(cx, b),
            ExprKind::Call { callee, args } => self.rcall(cx, e, callee, args),
            ExprKind::Unary { op: loom_syntax::ast::UnaryOp::Alloc, operand } => {
                let mut roots = self.rv(cx, operand);
                if let Some(&spool) = self.b.allocs.get(&e.id)
                    && let Some(spool) = self.resolve_spool(cx, spool)
                {
                    join(&mut roots, &[cx.flow.root(spool)]);
                }
                roots
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    let r = self.rv(cx, v);
                    self.leaves(&r, v.span, "returned");
                }
                Roots::new()
            }
            ExprKind::Fail(v) => {
                let r = self.rv(cx, v);
                self.leaves(&r, v.span, "failed-with");
                Roots::new()
            }
            ExprKind::Assign { target, value, .. } => {
                let r = self.rv(cx, value);
                self.assigned(cx, target, &r);
                Roots::new()
            }
            ExprKind::If { cond, then, else_, .. } => {
                self.rv(cx, cond);
                let before = cx.flow.clone();
                let mut out = self.rblock(cx, then);
                let after = std::mem::replace(&mut cx.flow, before);
                if let Some(else_) = else_ {
                    let r = self.rv(cx, else_);
                    join(&mut out, &r);
                }
                cx.flow.merge(&after);
                out
            }
            ExprKind::Match { scrutinee, arms } => {
                let s = self.rv(cx, scrutinee);
                let before = cx.flow.clone();
                let mut merged: Option<Flow> = None;
                let mut out = Roots::new();
                for arm in arms {
                    cx.flow = before.clone();
                    self.bind_roots(cx, &arm.pat, &s);
                    if let Some(g) = &arm.guard {
                        self.rv(cx, g);
                    }
                    let r = self.rv(cx, &arm.body);
                    join(&mut out, &r);
                    match &mut merged {
                        Some(m) => m.merge(&cx.flow),
                        None => merged = Some(cx.flow.clone()),
                    }
                }
                cx.flow = merged.unwrap_or(before);
                out
            }
            ExprKind::Is { expr, pat } => {
                let s = self.rv(cx, expr);
                self.bind_roots(cx, pat, &s);
                Roots::new()
            }
            ExprKind::Loop { body } => {
                self.twice(cx, |this, cx| {
                    this.rblock(cx, body);
                });
                Roots::new()
            }
            ExprKind::While { cond, body } => {
                self.twice(cx, |this, cx| {
                    this.rv(cx, cond);
                    this.rblock(cx, body);
                });
                Roots::new()
            }
            ExprKind::For { pat, iter, body, .. } => {
                let s = self.rv(cx, iter);
                self.twice(cx, |this, cx| {
                    this.bind_roots(cx, pat, &s);
                    this.rblock(cx, body);
                });
                Roots::new()
            }
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.rv(cx, v);
                }
                Roots::new()
            }
            ExprKind::Field { base, .. }
            | ExprKind::Deref(base)
            | ExprKind::Try(base)
            | ExprKind::Cast { expr: base, .. }
            | ExprKind::Unary { operand: base, .. }
            | ExprKind::Const(base) => self.rv(cx, base),
            ExprKind::Index { base, args } => {
                let r = self.rv(cx, base);
                for a in args {
                    if let crate::hir::GenericArg::Expr(i) = a {
                        self.rv(cx, i);
                    }
                }
                r
            }
            ExprKind::Mend { expr, handler, binding } => {
                let mut r = self.rv(cx, expr);
                if let Some(b) = binding {
                    cx.depth_of.insert(*b, cx.depth);
                }
                let h = self.rv(cx, handler);
                join(&mut r, &h);
                r
            }
            ExprKind::DotBlock { subject, body } => {
                let r = self.rv(cx, subject);
                self.rblock(cx, body);
                r
            }
            ExprKind::Tuple(items) | ExprKind::Array(items) => self.union(cx, items.iter()),
            ExprKind::ArrayRepeat { value, count } => {
                let r = self.rv(cx, value);
                self.rv(cx, count);
                r
            }
            ExprKind::Map(pairs) => self.union(cx, pairs.iter().flat_map(|(k, v)| [k, v])),
            ExprKind::Binary { lhs, rhs, .. } => self.union(cx, [lhs.as_ref(), rhs.as_ref()].into_iter()),
            ExprKind::Chain { first, rest } => {
                self.union(cx, std::iter::once(first.as_ref()).chain(rest.iter().map(|(_, e)| e)))
            }
            ExprKind::Range { lo, hi, .. } => self.union(cx, lo.iter().chain(hi.iter()).map(|b| b.as_ref())),
            ExprKind::Template(parts) => {
                for part in parts {
                    if let crate::hir::TemplatePart::Interp { expr, .. } = part {
                        self.rv(cx, expr);
                    }
                }
                Roots::new()
            }
            ExprKind::Lambda { body, .. } => {
                self.rv(cx, body);
                Roots::new()
            }
            ExprKind::Thread { body, .. } => {
                let saved = cx.thread_base.replace(cx.depth);
                let r = self.rv(cx, body);
                cx.thread_base = saved;
                r
            }
            ExprKind::Loom { body, .. } => {
                self.rblock(cx, body);
                Roots::new()
            }
            ExprKind::Yield { value, .. } => {
                self.rv(cx, value);
                Roots::new()
            }
            _ => Roots::new(),
        }
    }

    fn union<'e>(&mut self, cx: &mut Cx, es: impl Iterator<Item = &'e Expr>) -> Roots {
        let mut out = Roots::new();
        for e in es {
            let r = self.rv(cx, e);
            join(&mut out, &r);
        }
        out
    }

    /// Goes through a loop body twice, so a reset in one round meets a use in
    /// the next.
    fn twice(&mut self, cx: &mut Cx, mut body: impl FnMut(&mut Self, &mut Cx)) {
        let before = cx.flow.clone();
        body(self, cx);
        let first = cx.flow.clone();
        cx.flow.merge(&before);
        body(self, cx);
        cx.flow.merge(&first);
    }

    /// `target = value`: the variable at the root of `target` now holds what
    /// `roots` are in, and must not outlive them.
    fn assigned(&mut self, cx: &mut Cx, target: &Expr, roots: &Roots) {
        let mut root = target;
        while let ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::Deref(base) = &root.kind {
            root = base;
        }
        let ExprKind::Local(l) = root.kind else {
            self.rv(cx, target);
            return;
        };
        let whole = std::ptr::eq(root, target);
        for &(r, _) in roots {
            let (here, there) = (cx.depth_of.get(&l).copied(), cx.depth_of.get(&r).copied());
            if let (Some(here), Some(there)) = (here, there)
                && there > here
                && self.p.local(r).kind == LocalKind::Spool
            {
                let (name, spool) = (self.p.local(l).name.clone(), self.p.local(r).name.clone());
                let diag = Diagnostic::error(
                    codes::R0101,
                    format!("`{name}` outlives the spool `{spool}` it would point into"),
                )
                .primary(target.span, format!("`{name}` is declared outside `{spool}`'s block"))
                .secondary(self.p.local(r).span, "spool declared here");
                self.report_once(diag);
            }
        }
        if whole {
            // A new value: what the variable pointed into is no longer so.
            if roots.is_empty() {
                cx.flow.roots.remove(&l);
            } else {
                cx.flow.roots.insert(l, roots.clone());
            }
        } else {
            join(cx.flow.roots.entry(l).or_default(), roots);
        }
    }

    /// A call: its arguments, then what it returns and does to spools.
    fn rcall(&mut self, cx: &mut Cx, call: &Expr, callee: &Expr, args: &[crate::hir::Arg]) -> Roots {
        let receiver = match (&callee.kind, self.b.calls.get(&call.id)) {
            (ExprKind::Field { base, .. }, Some(info)) if info.receiver.is_some() => Some(base.as_ref()),
            _ => None,
        };
        let recv_roots = receiver.map(|r| self.rv(cx, r)).unwrap_or_default();
        let arg_roots: Vec<Roots> = args.iter().map(|a| self.rv(cx, &a.value)).collect();
        let Some(info) = self.b.calls.get(&call.id).cloned().filter(|i| i.callee != super::generic::UNKNOWN) else {
            // A construction: its fields are in whatever its arguments are.
            let mut out = Roots::new();
            arg_roots.iter().for_each(|r| join(&mut out, r));
            return out;
        };
        let def = self.r.instance(info.callee).def;
        let p = self.p;
        let attrs = &p.def(def).attrs;
        let spool_of_receiver = receiver.and_then(|r| self.spool_of(cx, r));
        if attrs.iter().any(|a| a.name == "spool_reset") {
            if let Some(spool) = spool_of_receiver {
                let n = cx.flow.resets.get(&spool).map_or(0, |r| r.0);
                cx.flow.resets.insert(spool, (n + 1, call.span));
            }
            return Roots::new();
        }
        if attrs.iter().any(|a| a.name == "spool_put") {
            let mut out = arg_roots.iter().flatten().copied().collect::<Roots>();
            if let Some(spool) = spool_of_receiver {
                join(&mut out, &[cx.flow.root(spool)]);
            }
            return out;
        }
        // The spools the call's `given`s are, in order.
        let mut spools: Vec<Option<LocalId>> = Vec::new();
        if let DefKind::Fn(f) = &self.p.def(def).kind {
            let declared = f.params.len();
            for source in info.args.iter().skip(declared) {
                spools.push(match source {
                    ArgSource::Arg(i) => self.spool_of(cx, &args[*i].value),
                    ArgSource::Ambient(l) => self.resolve_spool(cx, *l),
                    _ => None,
                });
            }
            // An argument must be in the spool its parameter's type names.
            for (i, source) in info.args.iter().enumerate().take(declared) {
                let ArgSource::Arg(a) = source else { continue };
                let Some(ty) = &f.params[i].ty else { continue };
                let Some(j) = f.givens.iter().position(|g| names_region(ty, g.local)) else { continue };
                let Some(want) = spools.get(j).copied().flatten() else { continue };
                if arg_roots[*a].iter().any(|&(r, _)| r != want) {
                    let (param, spool) =
                        (self.p.local(f.params[i].local).name.clone(), self.p.local(want).name.clone());
                    let diag = Diagnostic::error(codes::R0104, format!("`{param}` has to be in the spool `{spool}`"))
                        .primary(args[*a].value.span, format!("this points into another spool than `{spool}`"))
                        .secondary(f.params[i].span, "the parameter's type names the spool the call gives");
                    self.report_once(diag);
                }
            }
        }
        let ty = self.b.expr_ty.get(&call.id).copied().unwrap_or(self.tcx.error);
        let mut out = Roots::new();
        if self.in_spool(ty) {
            spools.iter().flatten().for_each(|&s| join(&mut out, &[cx.flow.root(s)]));
            // A method that returns what it holds, or a field of its receiver.
            if spools.is_empty() {
                join(&mut out, &recv_roots);
            }
        }
        out
    }
}

/// Whether the type names the region of spool local `l` (`^Expr@sp`).
fn names_region(t: &crate::hir::Ty, l: LocalId) -> bool {
    match &t.kind {
        HirTy::Region { region: Region::Local(r), ty } => *r == l || names_region(ty, l),
        HirTy::Region { ty, .. }
        | HirTy::Pointer(ty)
        | HirTy::Optional(ty)
        | HirTy::Slice(ty)
        | HirTy::RawPointer(ty) => names_region(ty, l),
        HirTy::Tuple(items) | HirTy::Union(items) => items.iter().any(|t| names_region(t, l)),
        HirTy::Def { args, .. } => args.iter().any(|a| match a {
            crate::hir::GenericArg::Type(t) => names_region(t, l),
            crate::hir::GenericArg::Expr(_) => false,
        }),
        _ => false,
    }
}
