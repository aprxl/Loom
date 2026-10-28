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
//! source, which is when the stream would have asked it. The adaptors are
//! `map`, `filter`, `filter_map`, `inspect`, `step_by`, `take_while`,
//! `skip_while`, `take` and `skip`.
//!
//! Only for items, and adaptors' arguments, that own nothing (numbers,
//! `bool`s, `char`s, `str` views): what owns memory goes through boxes as
//! before, where the steps drop what they drop.
//!
//! A source that's a list, a slice or an array (not a temporary) is read in
//! place, with no stream box; a leading `step_by` then jumps the index.
//!
//! A source that's a range written out (`(0..n).filter(p)`) is no stream
//! at all: the loop counts, as `for i in 0..n` does, so what's known about
//! the count (it's below `n`) is known about the items.

use loom_syntax::source::Span;

use super::cleanup::{CleanupKind, Stable};
use super::text::Msg;
use super::value::Val;
use super::{Builder, LoopCtx, LoopResult, Var};
use crate::heddle::{BlockId, Cmp, FOp, InstKind, IntOp, Policy, VTy, Value};
use crate::hir::{self, ExprKind};
use crate::ty::IntTy;
use crate::ty::{Ty, TyKind};
use crate::typeck::{ArgSource, IterSource, Receiver, Seq};

/// What an adaptor does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Map,
    Filter,
    FilterMap,
    Inspect,
    StepBy,
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

/// A pipeline started: its source (a stream at `Source::Stream`, owned by
/// the scope at `depth`), its adaptors' arguments, and a variable for each
/// `take` and `skip` (how many are left) and `skip_while` (whether it's
/// still skipping).
struct Started {
    source: Source,
    depth: usize,
    args: Vec<Vec<Value>>,
    vars: Vec<Option<Var>>,
}

/// Where a started pipeline's items come from.
enum Source {
    Stream(Value),
    /// A range written out: the next item (for an open range, the last one
    /// given), the end (if there is one), whether that's included, and
    /// whether there are items left (an inclusive range) or none has been
    /// given yet (an open one).
    Count {
        next: Var,
        end: Option<Value>,
        inclusive: bool,
        left: Option<Var>,
        int: IntTy,
    },
    /// A list, slice or array read in place: its elements, how many there
    /// are, the next index, and the list's header (0 when it isn't a list),
    /// to see that it doesn't change under the loop.
    Seq {
        buf: Value,
        len: Value,
        next: Var,
        header: Value,
        elem: Ty,
    },
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
            "filter_map" => Kind::FilterMap,
            "inspect" => Kind::Inspect,
            "step_by" => Kind::StepBy,
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
        let (source, depth) = if let Some(IterSource::Range(elem)) = p.of
            && let ExprKind::Range { lo: Some(lo), hi, inclusive } = &p.source.kind
            && let Some(int) = self.int_of(self.tcx().degauge(elem))
        {
            // The bounds, evaluated once, as `for` does.
            let lo = self.value(lo)?[0];
            let end = match hi {
                Some(hi) => Some(self.value(hi)?[0]),
                None => None,
            };
            let next = self.new_var(VTy::Int);
            self.write(next, lo);
            // An inclusive range: whether there are items left. An open
            // one: whether none has been given yet.
            let left = match (end, inclusive) {
                (Some(_), false) => None,
                _ => {
                    let left = self.new_var(VTy::Int);
                    let one = self.iconst(1);
                    self.write(left, one);
                    Some(left)
                }
            };
            let depth = self.open_scope();
            (Source::Count { next, end, inclusive: *inclusive, left, int }, depth)
        } else {
            let v = self.expr(p.source)?;
            self.moved_away(p.source);
            let from = self.coerced_ty(p.source);
            let owned_temp = matches!(&v, Val::Mem(m) if self.f.temps.contains_key(m))
                && !matches!(self.seq_of(from), Some(Seq::Slice(_)));
            if let Some(IterSource::Seq(elem)) = &p.of
                && !owned_temp
            {
                let (buf, len) = self.view_of(&v, from, span)?;
                let header = match (self.seq_of(from), &v) {
                    (Some(Seq::List(_)), &Val::Mem(h)) => h,
                    _ => self.emit1(InstKind::Iconst(0), VTy::Ptr),
                };
                let next = self.new_var(VTy::Int);
                let zero = self.iconst(0);
                self.write(next, zero);
                let depth = self.open_scope();
                (Source::Seq { buf, len, next, header, elem: self.tcx().degauge(*elem) }, depth)
            } else {
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
                (Source::Stream(s), depth)
            }
        };
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
                Kind::StepBy => {
                    let zero = self.iconst(0);
                    let bad = self.ucmp(Cmp::Eq, r[0], zero);
                    let (fail, ok) = (self.new_block(), self.new_block());
                    self.br(bad, fail, ok);
                    self.enter(fail);
                    self.unravel_msg(&[Msg::Text("step_by of 0 items")], span)?;
                    self.enter(ok);
                    let skip = self.new_var(VTy::Int);
                    let zero = self.iconst(0);
                    self.write(skip, zero);
                    Some(skip)
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
        Some(Started { source, depth, args, vars })
    }

    /// The next item of a counted source, in a block that goes on only when
    /// there is one (to `done` otherwise); the count is stepped past it.
    fn count_next(&mut self, source: &Source, done: BlockId) -> Value {
        let Source::Count { next, end, inclusive, left, int } = *source else { unreachable!("a counted source") };
        if end.is_none() {
            return self.open_next(next, left.expect("a flag"), int);
        }
        let cur = self.read(next);
        if let Some(end) = end {
            if let Some(left) = left {
                let more = self.read(left);
                let go = self.new_block();
                self.br(more, go, done);
                self.enter(go);
            }
            let cmp = if inclusive { Cmp::Le } else { Cmp::Lt };
            let c = self.emit1(InstKind::ICmp { cmp, unsigned: !int.signed, a: cur, b: end }, VTy::Int);
            let go = self.new_block();
            self.br(c, go, done);
            self.enter(go);
        }
        let one = self.iconst(1);
        let step = |this: &mut Self, policy| {
            let n = this.emit1(InstKind::Arith { op: IntOp::Add, policy, ty: int, a: cur, b: one }, VTy::Int);
            this.write(next, n);
        };
        match (end, left) {
            // Stepping past the last item of an inclusive range at the
            // type's maximum mustn't overflow: there are none left instead.
            (Some(end), Some(left)) => {
                let last = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: cur, b: end }, VTy::Int);
                let (stop, inc, go) = (self.new_block(), self.new_block(), self.new_block());
                self.br(last, stop, inc);
                self.enter(stop);
                let zero = self.iconst(0);
                self.write(left, zero);
                self.jump(go);
                self.enter(inc);
                step(self, Policy::Wrap);
                self.jump(go);
                self.enter(go);
            }
            _ => step(self, Policy::Wrap),
        }
        cur
    }

    /// The next item of an open range (`lo..`): `next` is the last one
    /// given, past the first. It steps when the next item is asked for, as
    /// the stream does, so it traps only when asked for one past the type's
    /// maximum.
    fn open_next(&mut self, next: Var, first: Var, int: IntTy) -> Value {
        let item = self.new_var(VTy::Int);
        let fresh = self.read(first);
        let (start, step, go) = (self.new_block(), self.new_block(), self.new_block());
        self.br(fresh, start, step);
        self.enter(start);
        let zero = self.iconst(0);
        self.write(first, zero);
        let cur = self.read(next);
        self.write(item, cur);
        self.jump(go);
        self.enter(step);
        let cur = self.read(next);
        let one = self.iconst(1);
        let n = self.emit1(InstKind::Arith { op: IntOp::Add, policy: Policy::Trap, ty: int, a: cur, b: one }, VTy::Int);
        self.write(next, n);
        self.write(item, n);
        self.jump(go);
        self.enter(go);
        self.read(item)
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
        let Started { source, depth, args, vars } = st;
        let x = match source {
            Source::Stream(_) => Some(self.temp(p.item, span)?),
            Source::Count { .. } | Source::Seq { .. } => None,
        };
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
        let mut cur = match (&source, x) {
            (&Source::Stream(s), Some(x)) => {
                let got = self.step(s, x);
                self.br(got, body, done);
                self.enter(body);
                self.load_val(x, 0, p.item, span)?
            }
            (&Source::Seq { buf, len, next, header, elem }, None) => {
                let i = self.read(next);
                let over = self.ucmp(Cmp::Ge, i, len);
                let go = self.new_block();
                self.br(over, done, go);
                self.enter(go);
                self.check_seq_unchanged(buf, len, header, span)?;
                let at = self.elem_at(buf, i, elem)?;
                let cur = self.load_val(at, 0, elem, span)?;
                let one = self.iconst(1);
                let n = self.uarith(IntOp::Add, i, one);
                self.write(next, n);
                self.jump(body);
                self.enter(body);
                cur
            }
            _ => {
                let cur = self.count_next(&source, done);
                self.jump(body);
                self.enter(body);
                Val::Regs(vec![cur])
            }
        };
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
                Kind::FilterMap => {
                    let r = self.call_fn(&args[k], stage.arg_ty, vec![cur], span)?;
                    let TyKind::Fn(_, opt) = self.tcx().kind(self.tcx().degauge(stage.arg_ty)).clone() else {
                        return self.internal(span, "a function");
                    };
                    let opt = self.tcx().degauge(opt);
                    let some = self.is_some(&r, opt, span)?;
                    let next = self.new_block();
                    self.br(some, next, head);
                    self.enter(next);
                    cur = self.unwrap_some(&r, opt, span)?;
                }
                Kind::StepBy if k == 0 && matches!(source, Source::Seq { .. }) => {
                    let Source::Seq { len, next, .. } = source else { unreachable!("a sequence source") };
                    let at = self.read(next);
                    let rest = self.uarith(IntOp::Sub, len, at);
                    let one = self.iconst(1);
                    let skips = self.uarith(IntOp::Sub, args[k][0], one);
                    let fits = self.ucmp(Cmp::Lt, skips, rest);
                    let (jump, end, joined) = (self.new_block(), self.new_block(), self.new_block());
                    self.br(fits, jump, end);
                    self.enter(jump);
                    let to = self.uarith(IntOp::Add, at, skips);
                    self.write(next, to);
                    self.jump(joined);
                    self.enter(end);
                    self.write(next, len);
                    self.jump(joined);
                    self.enter(joined);
                }
                Kind::StepBy => {
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
                    let one = self.iconst(1);
                    let skips = self.uarith(IntOp::Sub, args[k][0], one);
                    self.write(var, skips);
                }
                Kind::Inspect => {
                    self.call_fn(&args[k], stage.arg_ty, vec![cur.clone()], span)?;
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
