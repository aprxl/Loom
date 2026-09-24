//! Lowering lambdas, function values and streams.
//!
//! A lambda is a Lua closure: it sees the enclosing locals as upvalues, so
//! `xs.each(x => total += x)` changes `total` itself. A function value is
//! the function in `F`. A stream is a Lua function that gives `true, item`
//! (see runtime/rt/core.lua), so a `for` loop over one is Lua's generic
//! `for`.

use super::lir::{Expr, StmtKind};
use super::{Dest, Lowerer};
use crate::hir::{self, Block, DefKind, LambdaParam};
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
            IterSource::Seq => Expr::rt("seq_stream", vec![v]),
            IterSource::Range(elem) => {
                let d = self.desc(*elem, span);
                Expr::rt("range_stream", vec![d, v])
            }
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
