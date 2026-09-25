//! Lowering lambdas, function values and streams.
//!
//! A lambda is a Lua closure: it sees the enclosing locals as upvalues, so
//! `xs.each(x => total += x)` changes `total` itself. A function value is
//! the function in `F`. A stream is a Lua function that gives `true, item`
//! (see runtime/rt/core.lua), so a `for` loop over one is Lua's generic
//! `for`.

use super::lir::{Expr, Stmt, StmtKind};
use super::{Dest, Lowerer};
use crate::hir::{self, Block, DefKind, LambdaParam, LocalId};
use crate::ty::TyKind;
use crate::typeck::{BodyId, IterSource};
use loom_syntax::ast::ParamMode;
use loom_syntax::source::Span;

impl Lowerer<'_> {
    pub(super) fn lambda(&mut self, e: &hir::Expr, params: &[LambdaParam], body: &hir::Expr) -> Expr {
        let names = params.iter().map(|p| self.local_name(p.local)).collect();
        let valueless = match self.tcx().kind(self.ty_of(e)) {
            TyKind::Fn(_, ret) => matches!(self.tcx().kind(*ret), TyKind::Unit | TyKind::Never),
            _ => true,
        };
        // A `?` inside can't jump to a `mend` outside: labels don't cross
        // into a Lua function.
        let saved_mends = std::mem::take(&mut self.mend_labels);
        let stmts =
            self.body(body.span, |this| this.lower_into(body, if valueless { &Dest::Discard } else { &Dest::Return }));
        self.mend_labels = saved_mends;
        Expr::Function(names, stmts)
    }

    /// A function used as a value: `F.name`, or the runtime's function.
    pub(super) fn fn_value(&mut self, id: BodyId, span: Span) -> Expr {
        let def = self.typed.results.instance(id).def;
        let p = self.p;
        let runtime = match &p.def(def).kind {
            DefKind::Fn(f) => {
                f.abi.is_some()
                    || (f.body.is_none()
                        && p.def(def).parent.is_some_and(|w| p.def(w).attrs.iter().any(|a| a.name == "builtin")))
            }
            _ => false,
        };
        if runtime {
            return self.extern_fn(def, span).unwrap_or(Expr::Nil);
        }
        Expr::field(Expr::name("F"), &self.fn_name(id))
    }

    /// `defer body`: code for the end of the block, in a function a
    /// `<close>` local calls. `defer x => body` also keeps a flag that each
    /// move of `x` clears, and runs `body` only while it's set.
    pub(super) fn defer(&mut self, binding: Option<LocalId>, body: &hir::Expr, span: Span) {
        let flag = binding.map(|_| self.new_temp());
        if let Some(flag) = &flag {
            self.push(StmtKind::Local(vec![flag.clone()], vec![Expr::Bool(true)]));
        }
        let saved_mends = std::mem::take(&mut self.mend_labels);
        let mut stmts = self.body(body.span, |this| this.lower_into(body, &Dest::Discard));
        self.mend_labels = saved_mends;
        if let (Some(flag), Some(local)) = (flag, binding) {
            stmts = vec![Stmt { kind: StmtKind::If(vec![(Expr::name(flag.clone()), stmts)], None), span: Some(span) }];
            self.live_flags.entry(local).or_default().push(flag);
        }
        let d = self.new_temp();
        self.push(StmtKind::LocalClose(d, Expr::rt("defer", vec![Expr::Function(Vec::new(), stmts)])));
    }

    /// If `e` moves a local's value away, clears that local's fallback
    /// defer flags: there's nothing left to clean up.
    pub(super) fn moved_away(&mut self, id: hir::HirId) {
        let Some(local) = self.tables().moves.get(&id).copied() else { return };
        for flag in self.live_flags.get(&local).cloned().unwrap_or_default() {
            self.push(StmtKind::Assign(vec![Expr::name(flag)], vec![Expr::Bool(false)]));
        }
    }

    /// `f(a, b)` where `f` is a function value.
    pub(super) fn call_value(&mut self, callee: &hir::Expr, args: &[hir::Arg]) -> Expr {
        let mut operands: Vec<&hir::Expr> = vec![callee];
        operands.extend(args.iter().map(|a| &a.value));
        let mut lowered = self.lower_operands(&operands);
        let f = lowered.remove(0);
        Expr::call(f, lowered)
    }

    /// A stream of the items of `v`, which come from `source`.
    pub(super) fn stream_from(&mut self, source: &IterSource, v: Expr, span: Span) -> Expr {
        match source {
            IterSource::Seq(elem) => {
                let d = self.desc(*elem, span);
                Expr::rt("seq_stream", vec![d, v])
            }
            IterSource::Range(elem) => {
                let d = self.desc(*elem, span);
                Expr::rt("range_stream", vec![d, v])
            }
            IterSource::MapPairs(kt, vt) => {
                let (dk, dv) = (self.desc(*kt, span), self.desc(*vt, span));
                Expr::rt("map_stream", vec![dk, dv, v])
            }
            IterSource::SetItems(t) => {
                let d = self.desc(*t, span);
                Expr::rt("set_stream", vec![d, v])
            }
            IterSource::Shuttle(_) => Expr::rt("shuttle_stream", vec![v]),
            IterSource::Burst(_) => Expr::rt("burst_stream", vec![v]),
            IterSource::Woven { next, item } => {
                let boxed = self.opt_boxed(*item);
                // A `mut self` that isn't a table comes back after the item.
                let copy_out = self
                    .typed
                    .results
                    .sigs
                    .get(next)
                    .and_then(|s| s.params.first())
                    .is_some_and(|p| p.mode == ParamMode::Mut && !self.is_table(p.ty));
                let f = Expr::field(Expr::name("F"), &self.fn_name(*next));
                Expr::rt("iter_stream", vec![f, v, Expr::Bool(boxed), Expr::Bool(copy_out)])
            }
        }
    }

    /// `for x in s`, over a stream or a type that weaves `Iter`: Lua's
    /// generic `for`.
    pub(super) fn for_stream(&mut self, e: &hir::Expr, pat: &hir::Pat, iter: &hir::Expr, body: &Block) {
        let s = match self.tables().streams.get(&e.id).cloned() {
            // Leaving a loop over a generator early closes it, which runs
            // its `defer`s: `rt.shuttle_loop` gives the stream and a closer.
            Some(IterSource::Shuttle(_)) => {
                let v = self.lower_owned(iter);
                Expr::rt("shuttle_loop", vec![v])
            }
            // The iterator is changed as it goes, so it's a copy of a
            // `Copy` place.
            Some(source) => {
                let v = self.lower_owned(iter);
                self.stream_from(&source, v, iter.span)
            }
            None => self.lower_expr(iter),
        };
        self.for_stream_of(e, pat, s, body);
    }

    /// `for pat in s`, where `s` is a stream.
    pub(super) fn for_stream_of(&mut self, e: &hir::Expr, pat: &hir::Pat, s: Expr, body: &Block) {
        let item = self.new_temp();
        let mut body_stmts = self.nested(|this| this.bind_irrefutable(pat, Expr::name(&item)));
        body_stmts.extend(self.loop_body(e.id, Dest::Discard, body));
        self.push(StmtKind::GenFor(vec!["_".into(), item], s, body_stmts));
        self.after_loop();
    }
}
