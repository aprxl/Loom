//! Streams fused (pass 8 of design/vm.md §7): a stream made and used up in
//! one place, `xs.filter(p).map(f).sum()` or `for x in (1..n).filter(p)`,
//! is one loop. Its source still gives its items as a stream does (its step
//! function, which inlining copies in), but the adaptors are no boxes of
//! their own, each calling the one before through a function value: each
//! item goes through them in turn, in the loop, where the lambdas are
//! called directly (and inlined).
//!
//! It's the same as the stream, call for call: an adaptor asks for an item
//! only when the one after it does. `filter` and `skip` go back for another;
//! `take_while` ends the loop once one fails. `take(n)` stops asking after
//! `n`: the loop checks how many each `take` has left before it asks the
//! source, which is when the stream would have asked it.
//!
//! Only for items, and adaptors' arguments, that own nothing (numbers,
//! `bool`s, `char`s, `str` views): what owns memory goes through boxes as
//! before, where the steps drop what they drop.

use loom_syntax::source::Span;

use super::cleanup::{CleanupKind, Stable};
use super::value::Val;
use super::{Builder, LoopCtx, LoopResult, Var};
use crate::heddle::{BlockId, Cmp, FOp, InstKind, IntOp, Policy, VTy, Value};
use crate::hir::{self, ExprKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::{ArgSource, IterSource, Receiver};

/// What an adaptor does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Map,
    Filter,
    TakeWhile,
    SkipWhile,
    Take,
    Skip,
}

/// An adaptor in a pipeline: what it does, its argument (a function or a
/// count) and its type, and the items it gives.
struct Stage<'a> {
    kind: Kind,
    arg: &'a hir::Expr,
    arg_ty: Ty,
    out: Ty,
}

/// A stream made by adaptors and used up where it's made.
pub(super) struct Pipeline<'a> {
    /// What the first adaptor is called on.
    source: &'a hir::Expr,
    /// When that isn't a stream: how it has items.
    of: Option<IterSource>,
    /// The type of the stream it is, or becomes.
    sty: Ty,
    /// The type of its items.
    item: Ty,
    /// Innermost first.
    stages: Vec<Stage<'a>>,
}

impl Pipeline<'_> {
    /// The type of the items out of the last adaptor.
    fn out(&self) -> Ty {
        self.stages.last().map_or(self.item, |s| s.out)
    }
}

/// A pipeline started: its source stream (at `s`, owned by the scope at
/// `depth`), its adaptors' arguments, and a variable for each `take` and
/// `skip` (how many are left) and `skip_while` (whether it's still
/// skipping).
struct Started {
    s: Value,
    depth: usize,
    args: Vec<Vec<Value>>,
    vars: Vec<Option<Var>>,
}

impl<'a> Builder<'a> {
    /// The adaptor call `e` is, when a pipeline can take it: a `Stream`
    /// method of std on the expression before the dot, with one argument;
    /// and that expression.
    fn stage_call(&self, e: &'a hir::Expr) -> Option<(&'a hir::Expr, Kind, &'a hir::Expr)> {
        let ExprKind::Call { callee, args } = &e.kind else { return None };
        let info = self.tables().calls.get(&e.id)?;
        if info.dynamic.is_some()
            || !info.through.is_empty()
            || info.receiver != Some(Receiver::Expr)
            || info.args != [ArgSource::Receiver, ArgSource::Arg(0)]
        {
            return None;
        }
        let ExprKind::Field { base, .. } = &callee.kind else { return None };
        let def = self.typed.results.instance(info.callee).def;
        let kind = match self.p.def_path(def).strip_prefix("std.iter.Stream.")? {
            "map" => Kind::Map,
            "filter" => Kind::Filter,
            "take_while" => Kind::TakeWhile,
            "skip_while" => Kind::SkipWhile,
            "take" => Kind::Take,
            "skip" => Kind::Skip,
            _ => return None,
        };
        let [arg] = args.as_slice() else { return None };
        Some((base, kind, &arg.value))
    }

    /// The item type of `Stream[T]`.
    fn stream_item(&self, sty: Ty) -> Option<Ty> {
        match self.tcx().kind(self.tcx().degauge(sty)) {
            TyKind::Adt(_, args) if args.len() == 1 => Some(self.tcx().degauge(args[0])),
            _ => None,
        }
    }

    /// The pipeline whose last adaptor is the call `e`, if it is one that
    /// can be fused.
    pub(super) fn pipeline(&mut self, e: &'a hir::Expr) -> Option<Pipeline<'a>> {
        let mut found = Vec::new();
        let mut cur = e;
        while let Some((base, kind, arg)) = self.stage_call(cur) {
            found.push((cur, kind, arg));
            cur = base;
        }
        let &(first, _, _) = found.last()?;
        found.reverse();
        let of = self.tables().streams.get(&first.id).cloned();
        let sty = match of {
            // What the adaptor takes: a stream of the source's items.
            Some(_) => {
                let info = self.tables().calls.get(&first.id)?;
                let sig = self.typed.results.sigs.get(&info.callee)?;
                self.plain_ty(sig.params[0].ty)
            }
            None => self.plain_ty(self.ty_of(cur)),
        };
        let item = self.stream_item(sty)?;
        let mut stages = Vec::with_capacity(found.len());
        let mut owns = self.needs_drop(item);
        for (call, kind, arg) in found {
            let out = self.stream_item(self.plain_ty(self.ty_of(call)))?;
            let arg_ty = self.coerced_ty(arg);
            owns |= self.needs_drop(out) || self.needs_drop(arg_ty);
            stages.push(Stage { kind, arg, arg_ty, out });
        }
        if owns {
            return None;
        }
        Some(Pipeline { source: cur, of, sty, item, stages })
    }

    /// Starts pipeline `p`: makes its source and works out its adaptors'
    /// arguments, in the order the calls would. The source is owned by a
    /// scope that [`run_pipeline`](Self::run_pipeline) closes.
    fn start_pipeline(&mut self, p: &Pipeline<'a>, span: Span) -> Option<Started> {
        let v = self.expr(p.source)?;
        self.moved_away(p.source);
        let from = self.coerced_ty(p.source);
        let v = match &p.of {
            Some(src) => self.stream_of(src, v, from, p.sty, span)?,
            None => v,
        };
        // The loop owns the source: dropped however it ends.
        let Val::Mem(s) = self.owned(v, p.sty, span)? else { return None };
        let depth = self.open_scope();
        if let Some(slot) = self.f.slot_of.get(&s).copied() {
            self.add_cleanup(CleanupKind::Drop { at: Stable::Slot(slot), ty: p.sty });
        }
        let mut args: Vec<Vec<Value>> = Vec::with_capacity(p.stages.len());
        let mut vars = Vec::with_capacity(p.stages.len());
        for st in &p.stages {
            let v = self.expr(st.arg)?;
            self.moved_away(st.arg);
            let r = self.regs(v, st.arg_ty, span)?;
            vars.push(match st.kind {
                Kind::Take | Kind::Skip => {
                    let n = self.new_var(VTy::Int);
                    self.write(n, r[0]);
                    Some(n)
                }
                Kind::SkipWhile => {
                    let skipping = self.new_var(VTy::Int);
                    let one = self.iconst(1);
                    self.write(skipping, one);
                    Some(skipping)
                }
                _ => None,
            });
            args.push(r);
        }
        Some(Started { s, depth, args, vars })
    }

    /// Loops over started pipeline `p`, giving `each` every item out of its
    /// last adaptor, with where `continue` and `break` go; then drops the
    /// source.
    fn run_pipeline(
        &mut self,
        p: &Pipeline<'a>,
        st: Started,
        span: Span,
        each: &mut dyn FnMut(&mut Self, Val, BlockId, BlockId) -> Option<()>,
    ) -> Option<()> {
        let Started { s, depth, args, vars } = st;
        let x = self.temp(p.item, span)?;
        let (head, pull, body, done) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        // A `take` with none left isn't asked again, nor is what's before it.
        for (stage, var) in p.stages.iter().zip(&vars) {
            if stage.kind == Kind::Take {
                let left = self.read(var.expect("a count"));
                let zero = self.iconst(0);
                let none = self.ucmp(Cmp::Eq, left, zero);
                let go = self.new_block();
                self.br(none, done, go);
                self.enter(go);
            }
        }
        self.jump(pull);
        self.enter(pull);
        let got = self.step(s, x);
        self.br(got, body, done);
        self.enter(body);
        let mut cur = self.load_val(x, 0, p.item, span)?;
        for (k, stage) in p.stages.iter().enumerate() {
            let pass = |this: &mut Self, f: &[Value], x: &Val| -> Option<Value> {
                let r = this.call_fn(f, stage.arg_ty, vec![x.clone()], span)?;
                Some(this.regs(r, this.tcx().bool, span)?[0])
            };
            match stage.kind {
                Kind::Map => cur = self.call_fn(&args[k], stage.arg_ty, vec![cur], span)?,
                Kind::Filter => {
                    let ok = pass(self, &args[k], &cur)?;
                    let next = self.new_block();
                    self.br(ok, next, head);
                    self.enter(next);
                }
                Kind::TakeWhile => {
                    let ok = pass(self, &args[k], &cur)?;
                    let next = self.new_block();
                    self.br(ok, next, done);
                    self.enter(next);
                }
                Kind::SkipWhile => {
                    let var = vars[k].expect("a flag");
                    let skipping = self.read(var);
                    let (test, next) = (self.new_block(), self.new_block());
                    self.br(skipping, test, next);
                    self.enter(test);
                    let ok = pass(self, &args[k], &cur)?;
                    let past = self.new_block();
                    self.br(ok, head, past);
                    self.enter(past);
                    let zero = self.iconst(0);
                    self.write(var, zero);
                    self.jump(next);
                    self.enter(next);
                }
                Kind::Take => {
                    let var = vars[k].expect("a count");
                    let left = self.read(var);
                    let one = self.iconst(1);
                    let less = self.uarith(IntOp::Sub, left, one);
                    self.write(var, less);
                }
                Kind::Skip => {
                    let var = vars[k].expect("a count");
                    let left = self.read(var);
                    let zero = self.iconst(0);
                    let more = self.ucmp(Cmp::Ne, left, zero);
                    let (skip, next) = (self.new_block(), self.new_block());
                    self.br(more, skip, next);
                    self.enter(skip);
                    let one = self.iconst(1);
                    let less = self.uarith(IntOp::Sub, left, one);
                    self.write(var, less);
                    self.jump(head);
                    self.enter(next);
                }
            }
        }
        each(self, cur, head, done)?;
        if self.live() {
            self.jump(head);
        }
        self.seal(head);
        self.enter(done);
        self.close_scope(depth);
        Some(())
    }

    /// `for pat in e { body }` over a pipeline.
    pub(super) fn for_pipeline(
        &mut self,
        e: &'a hir::Expr,
        pat: &'a hir::Pat,
        body: &'a hir::Block,
        p: Pipeline<'a>,
    ) -> Option<()> {
        let item = p.out();
        let span = e.span;
        let st = self.start_pipeline(&p, span)?;
        let mut done = Some(());
        self.run_pipeline(&p, st, span, &mut |this, x, cont, brk| {
            let scopes = this.f.scopes.len();
            this.f.loops.push(LoopCtx { id: e.id, brk, cont, result: LoopResult::None, scopes, brk_scopes: scopes });
            let inner = this.open_scope();
            // What the pattern binds lives for one iteration.
            let saved = this.f.stmt_start.take();
            let mut ok = this.bind_irrefutable(pat, x, item);
            this.f.stmt_start = saved;
            if ok.is_some() {
                let join = this.join(this.tcx().unit, e.span)?;
                ok = this.block_into(body, &join);
            }
            this.close_scope(inner);
            this.f.loops.pop();
            done = ok;
            Some(())
        })?;
        done
    }

    /// The consumer call `e` (`.sum()`, `.product()`, `.count()`,
    /// `.fold(…)`, `.each(…)`, `.all(…)`, `.exists(…)`) on a pipeline,
    /// fused: `None` when it can't be, and the call is built as before.
    pub(super) fn fused_consume(
        &mut self,
        e: &'a hir::Expr,
        callee: &'a hir::Expr,
        args: &'a [hir::Arg],
        name: &str,
    ) -> Option<Option<Val>> {
        let ExprKind::Field { base, .. } = &callee.kind else { return None };
        let info = self.tables().calls.get(&e.id)?;
        if info.dynamic.is_some() || !info.through.is_empty() || info.receiver != Some(Receiver::Expr) {
            return None;
        }
        let wanted: &[ArgSource] = match name {
            "sum" | "product" | "count" => &[ArgSource::Receiver],
            "each" | "all" | "exists" => &[ArgSource::Receiver, ArgSource::Arg(0)],
            "fold" => &[ArgSource::Receiver, ArgSource::Arg(0), ArgSource::Arg(1)],
            _ => return None,
        };
        if info.args != wanted {
            return None;
        }
        let p = self.pipeline(base)?;
        let item = p.out();
        let span = e.span;
        match name {
            "sum" | "product" => {
                if self.int_of(item).is_none() && self.float_of(item).is_none() {
                    return None;
                }
                Some(self.fused_total(&p, name == "sum", item, span))
            }
            "count" => Some(self.fused_count(&p, span)),
            "each" | "all" | "exists" => {
                let fty = self.coerced_ty(&args[0].value);
                if self.needs_drop(fty) {
                    return None;
                }
                Some(self.fused_calls(&p, name, &args[0].value, fty, span))
            }
            "fold" => {
                let acc_ty = self.plain_ty(self.coerced_ty(&args[0].value));
                let fty = self.coerced_ty(&args[1].value);
                if self.needs_drop(acc_ty) || self.needs_drop(fty) {
                    return None;
                }
                let super::Repr::Regs(parts) = self.repr_of(acc_ty, span)? else { return None };
                let vtys: Vec<VTy> = parts.iter().map(|p| p.vty).collect();
                Some(self.fused_fold(&p, &args[0].value, &args[1].value, (acc_ty, fty), &vtys, span))
            }
            _ => None,
        }
    }

    /// `.count()` of a pipeline.
    fn fused_count(&mut self, p: &Pipeline<'a>, span: Span) -> Option<Val> {
        let st = self.start_pipeline(p, span)?;
        let n = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(n, zero);
        self.run_pipeline(p, st, span, &mut |this, _, _, _| {
            let cur = this.read(n);
            let one = this.iconst(1);
            let next = this.uarith(IntOp::Add, cur, one);
            this.write(n, next);
            Some(())
        })?;
        let n = self.read(n);
        Some(Val::Regs(vec![n]))
    }

    /// `.sum()` or `.product()` of a pipeline of numbers.
    fn fused_total(&mut self, p: &Pipeline<'a>, sum: bool, item: Ty, span: Span) -> Option<Val> {
        let st = self.start_pipeline(p, span)?;
        let float = self.float_of(item);
        let int = self.int_of(item);
        let vty = match float {
            Some(crate::ty::FloatTy::F32) => VTy::F32,
            Some(_) => VTy::F64,
            None => VTy::Int,
        };
        let acc = self.new_var(vty);
        let start = u64::from(!sum);
        let init = match float {
            Some(crate::ty::FloatTy::F32) => self.emit1(InstKind::Fconst(u64::from((start as f32).to_bits())), vty),
            Some(_) => self.emit1(InstKind::Fconst((start as f64).to_bits()), vty),
            None => self.iconst(start),
        };
        self.write(acc, init);
        self.run_pipeline(p, st, span, &mut |this, x, _, _| {
            let v = this.regs(x, item, span)?[0];
            let cur = this.read(acc);
            let next = match (float, int) {
                (Some(fl), _) => {
                    let op = if sum { FOp::Add } else { FOp::Mul };
                    this.emit1(InstKind::FArith { op, ty: fl, a: cur, b: v }, vty)
                }
                (_, Some(int)) => {
                    let op = if sum { IntOp::Add } else { IntOp::Mul };
                    this.emit1(InstKind::Arith { op, policy: Policy::Trap, ty: int, a: cur, b: v }, VTy::Int)
                }
                _ => return None,
            };
            this.write(acc, next);
            Some(())
        })?;
        let v = self.read(acc);
        Some(Val::Regs(vec![v]))
    }

    /// `.each(f)`, `.all(f)` or `.exists(f)` of a pipeline.
    fn fused_calls(&mut self, p: &Pipeline<'a>, name: &str, f: &'a hir::Expr, fty: Ty, span: Span) -> Option<Val> {
        let st = self.start_pipeline(p, span)?;
        // `f` after the stream, as the call's argument after its receiver.
        let v = self.expr(f)?;
        self.moved_away(f);
        let fr = self.regs(v, fty, span)?;
        let result = self.new_var(VTy::Int);
        let start = self.iconst(u64::from(name == "all"));
        self.write(result, start);
        self.run_pipeline(p, st, span, &mut |this, x, _, brk| {
            let r = this.call_fn(&fr, fty, vec![x], span)?;
            if name == "each" {
                return Some(());
            }
            let pass = this.regs(r, this.tcx().bool, span)?[0];
            let (stop, go) = (this.new_block(), this.new_block());
            if name == "all" {
                this.br(pass, go, stop);
            } else {
                this.br(pass, stop, go);
            }
            this.enter(stop);
            let v = this.iconst(u64::from(name == "exists"));
            this.write(result, v);
            this.jump(brk);
            this.enter(go);
            Some(())
        })?;
        if name == "each" {
            return Some(Val::Regs(Vec::new()));
        }
        let v = self.read(result);
        Some(Val::Regs(vec![v]))
    }

    /// `.fold(init, f)` of a pipeline, its accumulator (of type `tys.0`, `f`
    /// of type `tys.1`) in registers of types `vtys`.
    fn fused_fold(
        &mut self,
        p: &Pipeline<'a>,
        init: &'a hir::Expr,
        f: &'a hir::Expr,
        (acc_ty, fty): (Ty, Ty),
        vtys: &[VTy],
        span: Span,
    ) -> Option<Val> {
        let st = self.start_pipeline(p, span)?;
        let acc = self.new_vars(vtys);
        let v = self.expr(init)?;
        self.moved_away(init);
        let r = self.regs(v, acc_ty, span)?;
        self.write_all(&acc, &r);
        let fv = self.expr(f)?;
        self.moved_away(f);
        let fr = self.regs(fv, fty, span)?;
        self.run_pipeline(p, st, span, &mut |this, x, _, _| {
            let a = this.read_all(&acc);
            let r = this.call_fn(&fr, fty, vec![Val::Regs(a), x], span)?;
            let r = this.regs(r, acc_ty, span)?;
            this.write_all(&acc, &r);
            Some(())
        })?;
        let a = self.read_all(&acc);
        Some(Val::Regs(a))
    }
}
