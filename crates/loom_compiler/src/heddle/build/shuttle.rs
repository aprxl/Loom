//! Shuttles (design/vm.md §10.14). A shuttle function's body is built as
//! its *step function* (see `heddle::frame`), and calling the function
//! makes the shuttle: a box on the heap holding a header, the input being
//! pushed, and the frame, with its step. A `Shuttle[T, U]` value is the
//! box's address and the step; a `Burst[T]` is the box's address and the
//! number of the push that made it.
//!
//! The box:
//!
//! | offset | what |
//! |---|---|
//! | 0 | how many values (the shuttle and its bursts) hold it, `u32` |
//! | 4 | the number of the latest burst, `u32` |
//! | 8 | whether that burst is going (`u8`), whether the shuttle is waiting at a `pull` (`u8`), whether the burst has handed its input over (`u8`), whether it's closing (`u8`) |
//! | 12 | where the frame starts, `u32` |
//! | 16 | the box's size, `u32` |
//! | 24 | the step function |
//! | 32 | the drop glue for the input, or 0 |
//! | 40 | the input a push hands over, a `U` |
//!
//! Going through a burst runs the shuttle, handing its input to its first
//! `pull` and stopping at the next; pushing again first runs what's left
//! of the burst before. Closing hands `none` to every `pull` from then on.
//! Dropping the last value that holds a shuttle closes it (its pending
//! `defer`s run) and frees the box.

use loom_syntax::ast::ParamMode;
use loom_syntax::source::Span;

use super::cleanup::{CleanupKind, Stable};
use super::stream::Step;
use super::text::Msg;
use super::value::{Place, Storage, Val};
use super::{Builder, FnKey};
use crate::heddle::frame::{self, FrameInput, FrameLayout};
use crate::heddle::{BlockId, Cmp, Edge, InstKind, Mem, Term, VTy, Value};
use crate::hir::{self, DefKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::BodyId;

const H_REFS: i64 = 0;
const H_GEN: i64 = 4;
const H_ACTIVE: i64 = 8;
const H_AT_PULL: i64 = 9;
const H_DELIVERED: i64 = 10;
const H_CLOSING: i64 = 11;
const H_FRAME: i64 = 12;
const H_SIZE: i64 = 16;
const H_STEP: i64 = 24;
const H_DROP_INPUT: i64 = 32;
const H_INPUT: u64 = 40;

/// A shuttle's body being built as its step.
#[derive(Debug, Default)]
pub(super) struct ShuttleBuild {
    pub out: Option<Value>,
    pub input: Option<Value>,
    pub entry: Vec<Value>,
    pub yields: Option<Ty>,
    pub pulls: Option<Ty>,
    pub params: Vec<u32>,
    /// How each local's parts are kept in memory.
    pub var_mem: std::collections::HashMap<super::Var, Mem>,
    /// What the frame pass needs to know about values in registers.
    pub widths: std::collections::HashMap<Value, Mem>,
    /// Each slot's `life.end` cleanup, while it's to come.
    pub lives: std::collections::HashMap<u32, u32>,
    /// The addresses of temporaries, while they hold a value.
    pub temps: std::collections::HashSet<Value>,
}

/// A generator being built in place, in the loop over it.
#[derive(Debug)]
pub(super) struct InlineGen<'a> {
    pub callee: BodyId,
    /// Whether the code being built is the generator's (not the loop's
    /// body, which a `yield` runs).
    pub active: bool,
    pub pat: &'a hir::Pat,
    pub body: &'a hir::Block,
    pub loop_id: crate::hir::HirId,
    /// The tables the loop is built with.
    pub loop_tables: BodyId,
    /// Where the loop ends.
    pub exit: BlockId,
    /// How many scopes are open outside the generator's.
    pub outer: usize,
    /// What it yields.
    pub item: Ty,
}

/// The `Shuttle` and `Burst` methods whose bodies the builder writes.
pub(super) fn writes_body(path: &str) -> bool {
    matches!(path, "std.iter.Shuttle.push" | "std.iter.Shuttle.close" | "std.iter.Burst.drain")
}

impl<'a> Builder<'a> {
    /// `T` and `U` of `Shuttle[T, U]`.
    pub(super) fn shuttle_parts(&self, ty: Ty) -> Option<(Ty, Ty)> {
        match self.tcx().kind(self.tcx().degauge(ty)) {
            TyKind::Adt(def, args) if self.p.def_path(*def) == "std.iter.Shuttle" && args.len() == 2 => {
                Some((self.tcx().degauge(args[0]), self.tcx().degauge(args[1])))
            }
            _ => None,
        }
    }

    /// Notes that `v` is (part of) a local's value: how many bytes it
    /// takes in memory, if it's ever kept in a shuttle's frame.
    pub(super) fn note_width(&mut self, var: super::Var, v: Value) {
        let Some(sb) = self.f.shuttle.as_mut() else { return };
        let Some(&m) = sb.var_mem.get(&var) else { return };
        sb.widths
            .entry(v)
            .and_modify(|w| {
                if *w != m {
                    *w = Mem::W64
                }
            })
            .or_insert(m);
    }

    // ------------------------------------------------------------ the step

    /// The step function of shuttle function instance `id` (see
    /// `heddle::frame`): `step(op, frame, out, input) -> status`.
    pub(super) fn shuttle_step(&mut self, id: BodyId) -> Option<Vec<VTy>> {
        let p = self.p;
        let inst = self.typed.results.instance(id);
        let def = p.def(inst.def);
        let DefKind::Fn(f) = &def.kind else { return None };
        let span = def.span;
        let sig = self.typed.results.sigs.get(&id)?.clone();
        let (t, u) = self.shuttle_parts(sig.ret)?;
        let body = f.body.as_ref()?;
        let lends = Self::is_projection(f);
        let abi = self.body_abi(id, span)?;
        self.f.scan = self.scan(id, body);
        let entry: Vec<Value> = [VTy::Int, VTy::Ptr, VTy::Ptr, VTy::Ptr].iter().map(|&t| self.new_value(t)).collect();
        self.f.blocks[0].params.extend(&entry);
        self.f.shuttle = Some(ShuttleBuild {
            out: Some(entry[2]),
            input: Some(entry[3]),
            entry: entry.clone(),
            yields: Some(t),
            pulls: Some(u),
            ..ShuttleBuild::default()
        });
        self.f.ret = Some(self.tcx().unit);
        let depth = self.open_scope();
        // The parameters are in the frame, where making the shuttle put
        // them; what it took is dropped when it ends. A projection's
        // borrowed ones are there as their addresses.
        let mut borrowed = Vec::new();
        for ((param, sp), pabi) in f.params.iter().zip(&sig.params).zip(&abi.params) {
            let ty = self.local_ty(param.local);
            if lends && Self::by_address(sp.mode, pabi) {
                let slot = self.raw_slot(8, 8);
                borrowed.push((param.local, slot));
                self.f.shuttle.as_mut()?.params.push(slot);
                continue;
            }
            let slot = self.new_slot(ty, param.span)?;
            self.f.locals.insert(param.local, Storage::Slot(slot));
            if self.needs_drop(ty) {
                self.add_cleanup(CleanupKind::Drop { at: Stable::Slot(slot), ty });
            }
            self.f.shuttle.as_mut()?.params.push(slot);
        }
        self.suspend(frame::START);
        for (local, slot) in borrowed {
            let at = self.slot_addr(slot);
            let ptr = self.load(Mem::W64, at, 0, VTy::Ptr);
            let var = self.new_var(VTy::Ptr);
            self.write(var, ptr);
            self.f.locals.insert(local, Storage::Ref(var));
        }
        self.f.span = Some(span);
        let val = self.expr(body);
        if self.live() && val.is_some() {
            self.ret_val(Val::Regs(Vec::new()), span)?;
        }
        self.close_scope(depth);
        Some(vec![VTy::Int])
    }

    /// What the frame pass needs, once the step is built.
    pub(super) fn frame_input(&self) -> Option<FrameInput> {
        let sb = self.f.shuttle.as_ref()?;
        Some(FrameInput {
            entry: [sb.entry[0], sb.entry[1], sb.entry[2], sb.entry[3]],
            widths: sb.widths.clone(),
            owned: self.f.owned_slots.iter().map(|&(s, _)| s).collect(),
            params: sb.params.clone(),
        })
    }

    /// Suspends the shuttle, giving its driver `status`; building goes on
    /// where it's resumed. Closed there instead, it runs every cleanup live
    /// here, as a `return` would.
    fn suspend(&mut self, status: u8) {
        let (resume, close) = (self.new_block(), self.new_block());
        self.terminate(Term::Suspend {
            status,
            resume: Edge { block: resume, args: Vec::new() },
            close: Edge { block: close, args: Vec::new() },
        });
        self.enter(close);
        self.run_cleanups(0);
        if self.live() {
            let k = self.iconst(frame::FINISHED);
            self.terminate(Term::Ret(vec![k]));
        }
        self.enter(resume);
    }

    /// Whether `f` is a projection (`~> mut T`, `~> ref T`).
    pub(super) fn is_projection(f: &hir::FnDef) -> bool {
        matches!(f.ret, hir::FnRet::Shuttle { yields: Some((Some(_), _)), .. })
    }

    /// Whether a projection keeps a parameter passed this way as its
    /// address: a borrowed one, given in memory.
    fn by_address(mode: ParamMode, pabi: &super::ParamAbi) -> bool {
        mode != ParamMode::Take && matches!(pabi, super::ParamAbi::Addr)
    }

    /// A projection's `yield mut place`: the place's address, for the
    /// driver.
    pub(super) fn yield_place(&mut self, value: &'a hir::Expr, span: Span) -> Option<Val> {
        self.f.span = Some(span);
        self.cancel_point();
        let out = self.f.shuttle.as_ref()?.out?;
        let (place, _) = self.place_or_temp(value)?;
        let ptr = self.place_addr(&place, span)?;
        self.store(Mem::W64, out, 0, ptr);
        self.suspend(frame::YIELDED as u8);
        Some(Val::Regs(Vec::new()))
    }

    /// A call of a projection (`g.at(1, 2)`): makes it and runs it to its
    /// first `yield`, which lends the place. It's finished when the
    /// statement is (see `CleanupKind::Lend`).
    pub(super) fn lend_place(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        let span = e.span;
        let crate::hir::ExprKind::Call { callee, args } = &e.kind else { return None };
        let info = self.tables().calls.get(&e.id)?.clone();
        // A mutex's `lock` is no shuttle: the lock, and a cleanup.
        if self.p.def_path(self.typed.results.instance(info.callee).def) == "std.sync.Mutex.lock" {
            let _ = (callee, args);
            return self.mutex_lock(e);
        }
        // A map's `entry` is no shuttle either: the place is found (or made)
        // where it's evaluated.
        if self.p.def_path(self.typed.results.instance(info.callee).def) == "std.map.Map.entry" {
            return self.map_entry(e);
        }
        let sig = self.typed.results.sigs.get(&info.callee)?.clone();
        let abi = self.body_abi(info.callee, span)?;
        let modes: Vec<ParamMode> = sig.params.iter().map(|p| p.mode).collect();
        let recv_ty = info
            .args
            .iter()
            .position(|&s| s == crate::typeck::ArgSource::Receiver)
            .map(|k| self.plain_ty(sig.params[k].ty));
        let worked = self.call_args(callee, args, &info, &modes, recv_ty)?;
        let mut worked: Vec<Option<super::call::Arg>> = worked.into_iter().map(Some).collect();
        let def = self.typed.results.instance(info.callee).def;
        let DefKind::Fn(f) = &self.p.def(def).kind else { return None };
        let mut by_param = Vec::with_capacity(info.args.len());
        for (k, source) in info.args.iter().enumerate() {
            by_param.push(match source {
                crate::typeck::ArgSource::Receiver => worked[0].take(),
                crate::typeck::ArgSource::Ambient(_) | crate::typeck::ArgSource::Heap => {
                    return self.unsupported(span, "`given` parameters of a generator");
                }
                crate::typeck::ArgSource::Arg(i) => worked[i + usize::from(info.receiver.is_some())].take(),
                crate::typeck::ArgSource::Default => {
                    let default = f.params.get(k).and_then(|p| p.default.as_ref())?;
                    let saved = std::mem::replace(&mut self.f.body, info.callee);
                    let v = self.expr(default);
                    let dty = self.coerced_ty(default);
                    self.f.body = saved;
                    Some(super::call::Arg::Val(v?, dty))
                }
            });
        }
        let func = self.func_id(FnKey::Body(info.callee));
        let sh_ty = self.plain_ty(sig.ret);
        let Some(Val::Mem(sh)) = self.emit_call(func, &abi, by_param, sh_ty, &modes, span, sh_ty) else {
            return self.internal(span, "a projection made in registers");
        };
        // It's finished, or closed, when the statement ends.
        if let Some(id) = self.f.temps.remove(&sh) {
            self.kill_cleanup(id);
        }
        let slot = self.slot_of(sh)?;
        self.add_cleanup(CleanupKind::Lend { slot, ty: sh_ty });
        let (t, _) = self.shuttle_parts(sh_ty)?;
        let out = self.raw_slot(8, 8);
        let out = self.slot_addr(out);
        let b = self.load(Mem::W64, sh, 0, VTy::Ptr);
        let frame = self.frame_of(b);
        let input = self.offset(b, H_INPUT as i64);
        let st = self.step_box(b, frame::RESUME, frame, out, input);
        let yielded = self.iconst(frame::YIELDED);
        let lent = self.ucmp(Cmp::Eq, st, yielded);
        let (ok, none) = (self.new_block(), self.new_block());
        self.br(lent, ok, none);
        self.enter(none);
        self.unravel_msg(&[Msg::Text("a projection ended without lending a place")], span)?;
        self.enter(ok);
        let ptr = self.load(Mem::W64, out, 0, VTy::Ptr);
        Some((Place::At { ptr, off: 0 }, t))
    }

    /// Finishes the projection in slot `slot` (a shuttle of type `ty`):
    /// resumes it to its end, unless unwinding, then drops it. One that
    /// lends a second place unravels.
    pub(super) fn finish_lend(&mut self, slot: u32, ty: Ty) {
        let at = self.slot_addr(slot);
        let mut again = None;
        if self.f.in_pad == 0 {
            let b = self.load(Mem::W64, at, 0, VTy::Ptr);
            let frame = self.frame_of(b);
            let out = self.raw_slot(8, 8);
            let out = self.slot_addr(out);
            let input = self.offset(b, H_INPUT as i64);
            let st = self.step_box(b, frame::RESUME, frame, out, input);
            let yielded = self.iconst(frame::YIELDED);
            again = Some(self.ucmp(Cmp::Eq, st, yielded));
        }
        self.drop_at(at, ty);
        if let Some((size, _)) = self.size_align(ty) {
            self.emit(InstKind::Zero { dst: at, size }, &[]);
        }
        if let Some(again) = again {
            let (bad, ok) = (self.new_block(), self.new_block());
            self.br(again, bad, ok);
            self.enter(bad);
            let span = self.here();
            let _ = self.unravel_msg(&[Msg::Text("a projection lent more than one place")], span);
            self.enter(ok);
        }
    }

    /// `yield v`: moved to where the driver wants it, then suspended.
    pub(super) fn yield_expr(&mut self, value: &'a hir::Expr, span: Span) -> Option<Val> {
        self.f.span = Some(span);
        self.cancel_point();
        if self.inline_gen().is_some() {
            return self.inline_yield(value, span);
        }
        let sb = self.f.shuttle.as_ref()?;
        let (out, t) = (sb.out?, sb.yields?);
        let from = self.f.cleanups.len();
        let v = self.expr(value)?;
        self.moved_away(value);
        self.move_to(out, 0, v, t, span)?;
        // What computing the item made would be dropped at the end of the
        // statement, after the shuttle is resumed: an item that can't
        // borrow from it can't tell when, and the frame needn't keep it.
        if !self.holds_views(t, &mut Vec::new()) {
            self.run_cleanups(from);
            let ids: Vec<u32> = self.f.cleanups[from..].iter().map(|c| c.id).collect();
            for id in ids {
                self.kill_cleanup(id);
            }
        }
        self.suspend(frame::YIELDED as u8);
        Some(Val::Regs(Vec::new()))
    }

    /// Whether a value of `ty` may be a view of memory something else owns:
    /// it holds text or a slice, or something that may.
    fn holds_views(&mut self, ty: Ty, seen: &mut Vec<Ty>) -> bool {
        let ty = self.tcx().degauge(ty);
        if seen.contains(&ty) {
            return false;
        }
        seen.push(ty);
        match self.tcx().kind(ty).clone() {
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Unit => false,
            TyKind::Tuple(items) => items.into_iter().any(|t| self.holds_views(t, seen)),
            TyKind::Optional(t) | TyKind::Array(t, _) => self.holds_views(t, seen),
            TyKind::Result(t, e) => self.holds_views(t, seen) || self.holds_views(e, seen),
            TyKind::Adt(def, args) => match self.p.def_path(def).as_str() {
                "std.string.String" => false,
                "std.list.List" | "std.map.Map" | "std.set.Set" => args.into_iter().any(|t| self.holds_views(t, seen)),
                _ if self.is_builtin(def) => true,
                _ => match self.warp_info(ty) {
                    Some(info) => info.field_tys().into_iter().any(|t| self.holds_views(t, seen)),
                    None => true,
                },
            },
            _ => true,
        }
    }

    /// `pull`: suspended until the driver hands over an input, which is
    /// `some` of it, or `none` (no input: the shuttle is closed).
    pub(super) fn pull_expr(&mut self, e: &'a hir::Expr, span: Span) -> Option<Val> {
        self.f.span = Some(span);
        self.cancel_point();
        let opt = self.ty_of(e);
        let sb = self.f.shuttle.as_ref()?;
        let u = sb.pulls?;
        self.suspend(frame::PULLED as u8);
        let input = self.f.shuttle.as_ref()?.input?;
        let join = self.join(opt, span)?;
        let from = self.f.cleanups.len();
        let zero = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        let has = self.ucmp(Cmp::Ne, input, zero);
        let (yes, no, after) = (self.new_block(), self.new_block(), self.new_block());
        self.br(has, yes, no);
        self.enter(yes);
        let v = self.take_input(input, u, span)?;
        let v = self.some(opt, u, v, span)?;
        self.join_put(&join, v, from, span)?;
        self.jump(after);
        self.enter(no);
        let v = self.none(opt, span)?;
        self.join_put(&join, v, from, span)?;
        self.jump(after);
        self.enter(after);
        Some(self.join_get(&join))
    }

    /// The `U` at `input`, which the shuttle now owns (zeroed there).
    fn take_input(&mut self, input: Value, u: Ty, span: Span) -> Option<Val> {
        let drops = self.needs_drop(u);
        let (size, _) = self.size_align(u)?;
        let v = match self.repr_of(u, span)? {
            super::Repr::Regs(parts) => Val::Regs(self.load_parts(input, 0, &parts)),
            super::Repr::Mem { .. } => {
                let t = self.temp(u, span)?;
                if size > 0 {
                    self.emit(InstKind::Copy { dst: t, src: input, size }, &[]);
                }
                self.own_temp(t, u);
                Val::Mem(t)
            }
        };
        if drops && size > 0 {
            self.emit(InstKind::Zero { dst: input, size }, &[]);
        }
        Some(v)
    }

    // ------------------------------------------------------------ in place

    /// The generator whose body is being built in place, when the code
    /// being built is its own (not the body of the loop over it).
    fn inline_gen(&self) -> Option<usize> {
        self.f.inline_gens.iter().rposition(|g| g.active)
    }

    /// For `for x in g(…)`: the instance of `g`, when it's a generator the
    /// program writes, called right there, so its body can be built in
    /// place (see [`for_generator`](Self::for_generator)).
    pub(super) fn inlinable_generator(&self, e: &'a hir::Expr, iter: &'a hir::Expr) -> Option<BodyId> {
        let crate::hir::ExprKind::Call { .. } = &iter.kind else { return None };
        let tables = self.tables();
        if !matches!(tables.streams.get(&e.id), Some(crate::typeck::IterSource::Shuttle(_)))
            || tables.closure_calls.contains(&iter.id)
        {
            return None;
        }
        let info = tables.calls.get(&iter.id)?;
        if info.dynamic.is_some() || info.receiver.is_some() {
            return None;
        }
        let def = self.typed.results.instance(info.callee).def;
        let DefKind::Fn(f) = &self.p.def(def).kind else { return None };
        let simple = f.body.is_some()
            && f.abi.is_none()
            && matches!(f.ret, hir::FnRet::Shuttle { yields: Some((None, _)), pulls: None })
            && f.params.iter().all(|p| p.default.as_ref().is_none_or(|d| !super::expr::mentions_locals(d)));
        // Each in place only once on the way in: one that loops over
        // another doesn't nest without end, and code doesn't grow without
        // bound.
        let nested = self.f.inline_gens.iter().any(|g| g.callee == info.callee) || self.f.inline_gens.len() >= 4;
        (simple && !nested).then_some(info.callee)
    }

    /// `for pat in g(…) { body }`, where `g` is generator instance `callee`
    /// (see [`inlinable_generator`](Self::inlinable_generator)): `g`'s body is
    /// built right here, each `yield` running the loop's body with the
    /// item, so an item costs what an iteration of a `while` loop does.
    /// Leaving the loop early (`break`, `return`, an unravel) runs the
    /// generator's pending `defer`s on the way out, as closing it would;
    /// `continue` goes on after the `yield`.
    pub(super) fn for_generator(
        &mut self,
        e: &'a hir::Expr,
        pat: &'a hir::Pat,
        iter: &'a hir::Expr,
        body: &'a hir::Block,
        callee: BodyId,
    ) -> Option<()> {
        let span = iter.span;
        let crate::hir::ExprKind::Call { callee: callee_e, args } = &iter.kind else { return None };
        let info = self.tables().calls.get(&iter.id)?.clone();
        let def = self.typed.results.instance(callee).def;
        let DefKind::Fn(f) = &self.p.def(def).kind else { return None };
        let gbody = f.body.as_ref()?;
        let sig = self.typed.results.sigs.get(&callee)?.clone();
        let (t, _) = self.shuttle_parts(sig.ret)?;
        // The arguments, as for a call, then the defaults, with the
        // generator's tables.
        let modes: Vec<ParamMode> = sig.params.iter().map(|p| p.mode).collect();
        let worked = self.call_args(callee_e, args, &info, &modes, None)?;
        let mut worked: Vec<Option<super::call::Arg>> = worked.into_iter().map(Some).collect();
        let mut by_param = Vec::with_capacity(info.args.len());
        for (k, source) in info.args.iter().enumerate() {
            let v = match source {
                crate::typeck::ArgSource::Arg(i) => match worked[*i].take()? {
                    super::call::Arg::Val(v, _) => v,
                    super::call::Arg::Place(_) => return self.internal(span, "a generator's `mut` parameter"),
                },
                crate::typeck::ArgSource::Default => {
                    let default = f.params.get(k).and_then(|p| p.default.as_ref())?;
                    let saved = std::mem::replace(&mut self.f.body, callee);
                    let v = self.expr(default);
                    self.f.body = saved;
                    v?
                }
                crate::typeck::ArgSource::Receiver => return self.internal(span, "a generator with a receiver"),
                crate::typeck::ArgSource::Ambient(_) | crate::typeck::ArgSource::Heap => {
                    return self.unsupported(span, "`given` parameters of a generator");
                }
            };
            by_param.push(v);
        }
        // Its body: with its own tables and its own scope, which its
        // parameters live in.
        let outer = self.f.scopes.len();
        let depth = self.open_scope();
        let loop_tables = std::mem::replace(&mut self.f.body, callee);
        let scan = self.scan(callee, gbody);
        self.f.scan.in_memory.extend(scan.in_memory);
        self.f.scan.captures.extend(scan.captures);
        self.f.scan.deferred.extend(scan.deferred);
        let saved_stmt = self.f.stmt_start.take();
        for (param, v) in f.params.iter().zip(by_param) {
            let ty = self.local_ty(param.local);
            let v = self.owned(v, ty, param.span)?;
            self.bind_owned(param.local, v, param.span)?;
        }
        let exit = self.new_block();
        self.f.inline_gens.push(InlineGen {
            callee,
            active: true,
            pat,
            body,
            loop_id: e.id,
            loop_tables,
            exit,
            outer,
            item: t,
        });
        let done = self.expr(gbody);
        let g = self.f.inline_gens.pop()?;
        self.f.body = g.loop_tables;
        self.f.stmt_start = saved_stmt;
        let _ = done;
        // It ran to its end: its scope's cleanups, and the loop is over.
        self.close_scope(depth);
        if self.live() {
            self.jump(exit);
        }
        self.enter(exit);
        Some(())
    }

    /// `yield v` in a generator built in place: the loop's body, with `v`.
    fn inline_yield(&mut self, value: &'a hir::Expr, span: Span) -> Option<Val> {
        let k = self.inline_gen()?;
        let (t, pat, body, loop_id, loop_tables, exit, outer) = {
            let g = &self.f.inline_gens[k];
            (g.item, g.pat, g.body, g.loop_id, g.loop_tables, g.exit, g.outer)
        };
        let v = self.expr(value)?;
        self.moved_away(value);
        let v = self.owned(v, t, span)?;
        // The loop's body is the loop's code: its tables, its `return`.
        self.f.inline_gens[k].active = false;
        let gen_tables = std::mem::replace(&mut self.f.body, loop_tables);
        let cont = self.new_block();
        let inner = self.open_scope();
        if let Val::Mem(p) = v {
            self.own_temp(p, t);
        }
        self.f.loops.push(super::LoopCtx {
            id: loop_id,
            brk: exit,
            cont,
            result: super::LoopResult::None,
            scopes: inner - 1,
            brk_scopes: outer,
        });
        let saved_stmt = self.f.stmt_start.take();
        let mut done = self.bind_irrefutable(pat, v, t);
        self.f.stmt_start = saved_stmt;
        if done.is_some() {
            let join = self.join(self.tcx().unit, span)?;
            done = self.block_into(body, &join);
        }
        self.f.loops.pop();
        self.close_scope(inner);
        if self.live() && done.is_some() {
            self.jump(cont);
        }
        self.enter(cont);
        self.f.body = gen_tables;
        self.f.inline_gens[k].active = true;
        Some(Val::Regs(Vec::new()))
    }

    /// `return` (or the end of the body) in a generator built in place: its
    /// cleanups, and the loop is over. `false` when the code being built
    /// isn't one.
    pub(super) fn inline_return(&mut self) -> Option<bool> {
        let Some(k) = self.inline_gen() else { return Some(false) };
        let (outer, exit) = (self.f.inline_gens[k].outer, self.f.inline_gens[k].exit);
        self.leave_scopes(outer);
        if self.live() {
            self.jump(exit);
        }
        Some(true)
    }

    // ------------------------------------------------------------ making one

    /// Where shuttle function instance `id` keeps things in its frame:
    /// its step is built first, if it isn't yet.
    pub(super) fn frame_layout(&mut self, id: BodyId) -> Option<FrameLayout> {
        if let Some(l) = self.frames.get(&id) {
            return Some(l.clone());
        }
        let key = FnKey::ShuttleStep(id);
        let fid = self.func_id(key);
        if self.funcs[fid.0 as usize].is_none() {
            let saved = std::mem::take(&mut self.f);
            self.build_func(key);
            self.f = saved;
        }
        self.frames.get(&id).cloned()
    }

    /// Where the frame starts in a box whose shuttle pulls `U`s, and the
    /// box's size.
    fn box_layout(&mut self, u: Ty, layout: &FrameLayout) -> Option<(u64, u64)> {
        let (usize_, _) = self.size_align(u)?;
        let align = layout.align.max(8);
        let frame_off = (H_INPUT + usize_).div_ceil(align) * align;
        Some((frame_off, frame_off + layout.size.div_ceil(8) * 8))
    }

    /// Calling shuttle function instance `id`: makes the shuttle, without
    /// running any of its body.
    pub(super) fn shuttle_maker(&mut self, id: BodyId, f: &'a hir::FnDef, span: Span) -> Option<Vec<VTy>> {
        let sig = self.typed.results.sigs.get(&id)?.clone();
        let (_, u) = self.shuttle_parts(sig.ret)?;
        let layout = self.frame_layout(id)?;
        let abi = self.body_abi(id, span)?;
        let (sret, _, params) = self.entry_params(&abi);
        let (frame_off, size) = self.box_layout(u, &layout)?;
        let (n, a) = (self.iconst(size), self.iconst(8));
        let b = self.emit1(InstKind::Native { name: "mem.calloc", imm: 0, args: vec![n, a] }, VTy::Ptr);
        let one = self.iconst(1);
        self.store(Mem::U32, b, H_REFS, one);
        let k = self.iconst(frame_off);
        self.store(Mem::U32, b, H_FRAME, k);
        let k = self.iconst(size);
        self.store(Mem::U32, b, H_SIZE, k);
        let step = self.func_id(FnKey::ShuttleStep(id));
        let step = self.emit1(InstKind::FuncRef(step), VTy::Int);
        self.store(Mem::W64, b, H_STEP, step);
        let drop_input = if self.needs_drop(u) {
            let g = self.glue(super::Glue::Drop, u);
            self.emit1(InstKind::FuncRef(g), VTy::Int)
        } else {
            self.iconst(0)
        };
        self.store(Mem::W64, b, H_DROP_INPUT, drop_input);
        let frame = self.offset(b, frame_off as i64);
        let lends = Self::is_projection(f);
        for (i, ((param, sp), vals)) in f.params.iter().zip(&sig.params).zip(params).enumerate() {
            let ty = self.plain_ty(sp.ty);
            if lends && Self::by_address(sp.mode, &abi.params[i]) {
                if let Some(off) = layout.params.get(i).copied().flatten() {
                    self.store(Mem::W64, frame, off as i64, vals[0]);
                }
                continue;
            }
            let v = match abi.params[i] {
                super::ParamAbi::Regs(_) => Val::Regs(vals),
                super::ParamAbi::Addr => Val::Mem(vals[0]),
            };
            match layout.params.get(i).copied().flatten() {
                Some(off) => self.store_val(frame, off as i64, ty, &v, param.span)?,
                // The body never reads it: what it took goes now.
                None if sp.mode == loom_syntax::ast::ParamMode::Take => {
                    if let Val::Mem(ptr) = v {
                        self.drop_at(ptr, ty);
                    }
                }
                None => {}
            }
        }
        let dst = sret?;
        self.store(Mem::W64, dst, 0, b);
        self.store(Mem::W64, dst, 8, step);
        self.terminate(Term::Ret(Vec::new()));
        Some(abi.results)
    }

    // ------------------------------------------------------------ driving one

    /// The frame of the shuttle in box `b`.
    fn frame_of(&mut self, b: Value) -> Value {
        let off = self.load(Mem::U32, b, H_FRAME, VTy::Int);
        self.emit1(InstKind::Index { base: b, index: off, scale: 1 }, VTy::Ptr)
    }

    /// Whether the shuttle whose frame is at `frame` has finished.
    fn finished(&mut self, frame: Value) -> Value {
        let st = self.load(Mem::U32, frame, 0, VTy::Int);
        let mask = self.iconst(frame::DONE);
        let pt = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a: st, b: mask }, VTy::Int);
        self.ucmp(Cmp::Eq, pt, mask)
    }

    /// Steps the shuttle in box `b`: its status.
    fn step_box(&mut self, b: Value, op: u64, frame: Value, out: Value, input: Value) -> Value {
        let step = self.load(Mem::W64, b, H_STEP, VTy::Int);
        let op = self.iconst(op);
        self.emit(InstKind::CallInd { callee: step, args: vec![op, frame, out, input] }, &[VTy::Int])[0]
    }

    /// `drive(box, burst, out)`: the next item of burst number `burst` of
    /// the shuttle in `box`, written at `out`: whether there was one.
    pub(super) fn shuttle_drive_func(&mut self) -> Option<Vec<VTy>> {
        self.f.span = None;
        let (b, burst_no, out) = (self.new_value(VTy::Ptr), self.new_value(VTy::Int), self.new_value(VTy::Ptr));
        self.f.blocks[0].params.extend([b, burst_no, out]);
        let cur = self.load(Mem::U32, b, H_GEN, VTy::Int);
        let active = self.load(Mem::U8, b, H_ACTIVE, VTy::Int);
        let same = self.ucmp(Cmp::Eq, cur, burst_no);
        let zero = self.iconst(0);
        let going = self.ucmp(Cmp::Ne, active, zero);
        let (check, over) = (self.new_block(), self.new_block());
        self.br(same, check, over);
        self.enter(check);
        let (run, over2) = (self.new_block(), self.new_block());
        self.br(going, run, over2);
        self.enter(over2);
        self.ret_int_shuttle(0);
        self.enter(over);
        self.ret_int_shuttle(0);
        self.enter(run);
        let frame = self.frame_of(b);
        let input = self.offset(b, H_INPUT as i64);
        let null = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        let (head, stop) = (self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let done = self.finished(frame);
        let (go, at_pull_b) = (self.new_block(), self.new_block());
        let resume_b = self.new_block();
        self.br(done, stop, go);
        self.enter(go);
        // Not at a `pull`: resume it.
        let at_pull = self.load(Mem::U8, b, H_AT_PULL, VTy::Int);
        let zero = self.iconst(0);
        let waiting = self.ucmp(Cmp::Ne, at_pull, zero);
        self.br(waiting, at_pull_b, resume_b);
        // At a `pull`: hand over the input, once; `none` while closing, or
        // once closed.
        self.enter(at_pull_b);
        let delivered = self.load(Mem::U8, b, H_DELIVERED, VTy::Int);
        let zero = self.iconst(0);
        let fresh = self.ucmp(Cmp::Eq, delivered, zero);
        let (hand, closed_b, give_none, idle) =
            (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.br(fresh, hand, closed_b);
        self.enter(hand);
        let one = self.iconst(1);
        self.store(Mem::U8, b, H_DELIVERED, one);
        let zero = self.iconst(0);
        self.store(Mem::U8, b, H_AT_PULL, zero);
        let closing = self.load(Mem::U8, b, H_CLOSING, VTy::Int);
        let zero = self.iconst(0);
        let closing = self.ucmp(Cmp::Ne, closing, zero);
        let (with_input, _) = (self.new_block(), ());
        self.br(closing, give_none, with_input);
        self.enter(with_input);
        let st = self.step_box(b, frame::RESUME, frame, out, input);
        let after = self.new_block();
        let status = self.new_value(VTy::Int);
        self.f.blocks[after.0 as usize].params.push(status);
        self.terminate(Term::Jump(Edge { block: after, args: vec![st] }));
        self.enter(closed_b);
        let st_word = self.load(Mem::U32, frame, 0, VTy::Int);
        let cbit = self.iconst(frame::CLOSED);
        let c = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a: st_word, b: cbit }, VTy::Int);
        let zero = self.iconst(0);
        let is_closed = self.ucmp(Cmp::Ne, c, zero);
        self.br(is_closed, give_none, idle);
        self.enter(give_none);
        let zero = self.iconst(0);
        self.store(Mem::U8, b, H_AT_PULL, zero);
        let st = self.step_box(b, frame::RESUME, frame, out, null);
        self.terminate(Term::Jump(Edge { block: after, args: vec![st] }));
        self.enter(idle);
        self.jump(stop);
        self.enter(resume_b);
        let st = self.step_box(b, frame::RESUME, frame, out, input);
        self.terminate(Term::Jump(Edge { block: after, args: vec![st] }));
        self.enter(after);
        let yielded = self.iconst(frame::YIELDED);
        let is_item = self.ucmp(Cmp::Eq, status, yielded);
        let (item, not_item) = (self.new_block(), self.new_block());
        self.br(is_item, item, not_item);
        self.enter(item);
        self.ret_int_shuttle(1);
        self.enter(not_item);
        let pulled = self.iconst(frame::PULLED);
        let is_pull = self.ucmp(Cmp::Eq, status, pulled);
        let (mark, back) = (self.new_block(), self.new_block());
        self.br(is_pull, mark, back);
        self.enter(mark);
        let one = self.iconst(1);
        self.store(Mem::U8, b, H_AT_PULL, one);
        self.jump(back);
        self.enter(back);
        self.jump(head);
        self.seal(head);
        self.enter(stop);
        let zero = self.iconst(0);
        self.store(Mem::U8, b, H_ACTIVE, zero);
        self.ret_int_shuttle(0);
        Some(vec![VTy::Int])
    }

    fn ret_int_shuttle(&mut self, v: u64) {
        let k = self.iconst(v);
        self.terminate(Term::Ret(vec![k]));
    }

    /// `release(box)`: one value fewer holds the box; the last one closes
    /// the shuttle (it runs its pending `defer`s), drops the input it
    /// wasn't handed and frees the box.
    pub(super) fn shuttle_release_func(&mut self) -> Option<Vec<VTy>> {
        self.f.span = None;
        let b = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.push(b);
        let refs = self.load(Mem::U32, b, H_REFS, VTy::Int);
        let one = self.iconst(1);
        let left = self.uarith(crate::heddle::IntOp::Sub, refs, one);
        self.store(Mem::U32, b, H_REFS, left);
        let zero = self.iconst(0);
        let last = self.ucmp(Cmp::Eq, left, zero);
        let (free, keep) = (self.new_block(), self.new_block());
        self.br(last, free, keep);
        self.enter(keep);
        self.terminate(Term::Ret(Vec::new()));
        self.enter(free);
        // The box goes however closing ends.
        self.add_cleanup(CleanupKind::FreeShuttle(b));
        let input = self.offset(b, H_INPUT as i64);
        let drop_input = self.load(Mem::W64, b, H_DROP_INPUT, VTy::Int);
        let zero = self.iconst(0);
        let has = self.ucmp(Cmp::Ne, drop_input, zero);
        let (drop_b, go) = (self.new_block(), self.new_block());
        self.br(has, drop_b, go);
        self.enter(drop_b);
        self.emit(InstKind::CallInd { callee: drop_input, args: vec![input] }, &[]);
        self.jump(go);
        self.enter(go);
        let frame = self.frame_of(b);
        let null = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        self.step_box(b, frame::CLOSE, frame, null, null);
        self.run_cleanups(0);
        self.terminate(Term::Ret(Vec::new()));
        Some(Vec::new())
    }

    /// Frees the box of a shuttle at `b`.
    pub(super) fn free_shuttle_box(&mut self, b: Value) {
        let size = self.load(Mem::U32, b, H_SIZE, VTy::Int);
        let eight = self.iconst(8);
        self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![b, size, eight] }, &[]);
    }

    /// Drops the shuttle or burst at `ptr` (drop glue).
    pub(super) fn drop_shuttle(&mut self, ptr: Value) {
        let b = self.load(Mem::W64, ptr, 0, VTy::Ptr);
        let zero = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        let some = self.ucmp(Cmp::Ne, b, zero);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(some, yes, done);
        self.enter(yes);
        let release = self.func_id(FnKey::ShuttleRelease);
        let pad = self.pad();
        let span = self.f.span;
        let cur = self.cur_block();
        self.f.blocks[cur.0 as usize].insts.push(crate::heddle::Inst {
            kind: InstKind::Call { func: release, args: vec![b] },
            results: Vec::new(),
            span,
            unwind: pad,
            site: None,
        });
        self.jump(done);
        self.enter(done);
    }

    /// Runs what's left of the shuttle in box `b`'s current burst,
    /// dropping the items of type `t` it yields.
    fn drain_box(&mut self, b: Value, t: Ty, span: Span) -> Option<()> {
        let burst_no = self.load(Mem::U32, b, H_GEN, VTy::Int);
        self.drain_burst(b, burst_no, t, span)
    }

    fn drain_burst(&mut self, b: Value, burst_no: Value, t: Ty, span: Span) -> Option<()> {
        let x = self.new_slot(t, span)?;
        let x = self.slot_addr(x);
        let drive = self.func_id(FnKey::ShuttleDrive);
        let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let got = self.emit(InstKind::Call { func: drive, args: vec![b, burst_no, x] }, &[VTy::Int])[0];
        self.br(got, body, done);
        self.enter(body);
        self.drop_at(x, t);
        self.jump(head);
        self.seal(head);
        self.enter(done);
        Some(())
    }

    /// A new burst of the shuttle in box `b`, written at `dst`: it hands
    /// `input` over (or `none`, closing).
    fn new_burst(&mut self, b: Value, dst: Value, closing: bool) {
        let burst_no = self.load(Mem::U32, b, H_GEN, VTy::Int);
        let one = self.iconst(1);
        let burst_no = self.uarith(crate::heddle::IntOp::Add, burst_no, one);
        self.store(Mem::U32, b, H_GEN, burst_no);
        self.store(Mem::U8, b, H_ACTIVE, one);
        let zero = self.iconst(0);
        self.store(Mem::U8, b, H_DELIVERED, zero);
        let c = self.iconst(u64::from(closing));
        self.store(Mem::U8, b, H_CLOSING, c);
        let refs = self.load(Mem::U32, b, H_REFS, VTy::Int);
        let refs = self.uarith(crate::heddle::IntOp::Add, refs, one);
        self.store(Mem::U32, b, H_REFS, refs);
        self.store(Mem::W64, dst, 0, b);
        self.store(Mem::W64, dst, 8, burst_no);
    }

    /// The bodies of `push`, `close` and `drain`.
    pub(super) fn shuttle_method(
        &mut self,
        path: &str,
        args: &[Val],
        tys: &[Ty],
        sret: Option<Value>,
        span: Span,
    ) -> Option<()> {
        let Val::Mem(s) = args[0] else { return self.internal(span, "a shuttle in registers") };
        match path {
            "std.iter.Shuttle.push" | "std.iter.Shuttle.close" => {
                let (t, u) = self.shuttle_parts(tys[0])?;
                let b = self.load(Mem::W64, s, 0, VTy::Ptr);
                let frame = self.frame_of(b);
                let st = self.load(Mem::U32, frame, 0, VTy::Int);
                let cbit = self.iconst(frame::CLOSED);
                let c = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a: st, b: cbit }, VTy::Int);
                let input = self.offset(b, H_INPUT as i64);
                if path.ends_with("push") {
                    let zero = self.iconst(0);
                    let closed = self.ucmp(Cmp::Ne, c, zero);
                    let (bad, ok) = (self.new_block(), self.new_block());
                    self.br(closed, bad, ok);
                    self.enter(bad);
                    if let Val::Mem(p) = args[1] {
                        self.drop_at(p, u);
                    }
                    self.unravel_msg(&[Msg::Text("can't push into a closed shuttle")], span)?;
                    self.enter(ok);
                }
                self.drain_box(b, t, span)?;
                // What the last push handed over and the shuttle never took.
                self.drop_at(input, u);
                if path.ends_with("push") {
                    self.move_to(input, 0, args[1].clone(), u, span)?;
                } else {
                    let st = self.load(Mem::U32, frame, 0, VTy::Int);
                    let cbit = self.iconst(frame::CLOSED);
                    let st = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::Or, a: st, b: cbit }, VTy::Int);
                    self.store(Mem::U32, frame, 0, st);
                    if let Some((size, _)) = self.size_align(u)
                        && size > 0
                    {
                        self.emit(InstKind::Zero { dst: input, size }, &[]);
                    }
                }
                self.new_burst(b, sret?, path.ends_with("close"));
                self.ret_written()
            }
            "std.iter.Burst.drain" => {
                let t = match self.tcx().kind(tys[0]) {
                    TyKind::Adt(_, a) => self.tcx().degauge(a[0]),
                    _ => return None,
                };
                let b = self.load(Mem::W64, s, 0, VTy::Ptr);
                let burst_no = self.load(Mem::W64, s, 8, VTy::Int);
                self.drain_burst(b, burst_no, t, span)?;
                self.drop_at(s, tys[0]);
                self.ret_written()
            }
            _ => self.internal(span, "a shuttle method"),
        }
    }

    // ------------------------------------------------------------ as streams

    /// Builds `op` 0 of a generator's or a burst's stream step: the box
    /// holds the shuttle (its box and step) or the burst (its box and
    /// number).
    pub(super) fn shuttle_next(&mut self, step: Step, sb: Value, out: Value) -> Option<()> {
        let b = self.load(Mem::W64, sb, 0, VTy::Ptr);
        match step {
            Step::Gen { .. } => {
                let frame = self.frame_of(b);
                let done = self.finished(frame);
                let (stop, go) = (self.new_block(), self.new_block());
                self.br(done, stop, go);
                self.enter(stop);
                self.ret_int_shuttle(0);
                self.enter(go);
                let input = self.offset(b, H_INPUT as i64);
                let st = self.step_box(b, frame::RESUME, frame, out, input);
                let yielded = self.iconst(frame::YIELDED);
                let item = self.ucmp(Cmp::Eq, st, yielded);
                self.terminate(Term::Ret(vec![item]));
            }
            Step::Burst { .. } => {
                let burst_no = self.load(Mem::W64, sb, 8, VTy::Int);
                let drive = self.func_id(FnKey::ShuttleDrive);
                let got = self.emit(InstKind::Call { func: drive, args: vec![b, burst_no, out] }, &[VTy::Int])[0];
                self.terminate(Term::Ret(vec![got]));
            }
            _ => return None,
        }
        Some(())
    }

    /// The step a `Shuttle` or `Burst` value of type `from` is a stream
    /// with, of items `item`.
    pub(super) fn shuttle_stream_step(&self, from: Ty, item: Ty) -> Step {
        match self.shuttle_parts(from) {
            Some(_) => Step::Gen { item, sh: self.tcx().degauge(from) },
            None => Step::Burst { item, burst: self.tcx().degauge(from) },
        }
    }
}
