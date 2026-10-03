//! Streams (design/vm.md §10.13), not fused yet: a `Stream[T]` is a box on
//! the heap, holding what the stream needs, and the number of its *step*
//! function: `step(op, box, out)`. With `op` 0 it writes the next item (a
//! value of its own) at `out` and gives 1, or gives 0 when there are no
//! more; with `op` 1 it drops what the box holds and frees it. Every source
//! (an array, a range, an `Iter`, text) and every adaptor (`map`, `zip`,
//! …) is a step function of its own, made per instance; the consumers
//! (`collect`, `fold`, a `for` loop) call the step until it gives 0.

use loom_syntax::source::Span;

use super::cleanup::{CleanupKind, Stable};
use super::text::Msg;
use super::value::Val;
use super::{Builder, FnKey};
use crate::heddle::{BitOp, BlockId, Cmp, FOp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
use crate::ty::{Ty, TyKind};
use crate::typeck::{BodyId, IterSource, Seq};

/// What a stream is made of: which step function it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Step {
    /// Copies of a run of elements: `[0]` buffer, `[8]` length, `[16]`
    /// index, `[24]` the list's header (0 for none) to check it doesn't
    /// change, then the array or list the stream owns, if it does.
    Seq {
        elem: Ty,
        owned: Option<Ty>,
    },
    /// A range's values: `[0]` the next, `[8]` the high end, `[16]` the
    /// mode: `RANGE_TO`, `RANGE_THROUGH`, `RANGE_FROM` (`RANGE_FROM_ON`
    /// once started), or `RANGE_DONE`.
    Range(Ty),
    /// A type that weaves `Iter`, at `[0]`: its `next` gives the items.
    Woven {
        next: BodyId,
        iter: Ty,
        item: Ty,
    },
    /// Text's characters, bytes, lines or words: `[0]` address, `[8]`
    /// length, `[16]` where the next item starts.
    Text(TextItems),
    /// Views of `n` elements at a time: `[0]` buffer, `[8]` length, `[16]`
    /// `n`, `[24]` where the next starts; `windows` when they overlap.
    Chunks {
        elem: Ty,
        windows: bool,
    },
    /// The adaptors: `[0]` the stream they read, then `[16]` a function or
    /// a count, `[32]` a state word, `[40]` another stream (`zip`, `chain`).
    Map {
        from: Ty,
        to: Ty,
        f: Ty,
    },
    Filter {
        item: Ty,
        f: Ty,
    },
    TakeWhile {
        item: Ty,
        f: Ty,
    },
    SkipWhile {
        item: Ty,
        f: Ty,
    },
    Take(Ty),
    Skip(Ty),
    Indexed {
        item: Ty,
        pair: Ty,
    },
    Zip {
        a: Ty,
        b: Ty,
        pair: Ty,
    },
    Chain(Ty),
    /// Copies of a map's keys, values or pairs, or a set's items: `[0]`
    /// the map's header, `[8]` its length and `[16]` its entries' buffer
    /// when the stream started, `[24]` the next entry, then the map, if
    /// the stream owns it.
    Coll {
        coll: Ty,
        what: super::map::CollItems,
        item: Ty,
        owned: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum TextItems {
    Chars,
    Bytes,
    Lines,
    Words,
}

/// The `Stream` methods whose bodies the builder writes.
const METHODS: &[&str] = &[
    "next",
    "map",
    "filter",
    "take",
    "skip",
    "take_while",
    "skip_while",
    "indexed",
    "zip",
    "chain",
    "collect",
    "count",
    "last",
    "each",
    "fold",
    "reduce",
    "all",
    "exists",
    "find",
    "position",
    "sum",
    "product",
    "min",
    "max",
    "join",
];

pub(super) fn writes_body(path: &str) -> bool {
    path.strip_prefix("std.iter.Stream.").is_some_and(|m| METHODS.contains(&m))
        || matches!(
            path,
            "std.string.Str.chars"
                | "std.string.Str.bytes"
                | "std.string.Str.lines"
                | "std.string.Str.words"
                | "std.slice.Slice.chunks"
                | "std.slice.Slice.windows"
        )
}

/// A range stream's modes.
const RANGE_TO: u64 = 0;
const RANGE_FROM: u64 = 1;
const RANGE_THROUGH: u64 = 2;
const RANGE_DONE: u64 = 3;
const RANGE_FROM_ON: u64 = 4;

impl<'a> Builder<'a> {
    // ------------------------------------------------------------ boxes and steps

    /// How many bytes a box of `step` takes.
    fn box_size(&mut self, step: Step) -> Option<u64> {
        Some(match step {
            Step::Seq { owned, .. } => 32 + owned.map_or(Some(0), |t| self.size_align(t).map(|s| s.0))?,
            Step::Woven { iter, .. } => self.size_align(iter)?.0.max(8),
            Step::Range(_) | Step::Text(_) => 24,
            Step::Coll { owned, .. } => 32 + if owned { 40 } else { 0 },
            Step::Chunks { .. } => 32,
            _ => 56,
        })
    }

    /// A new box of `step`'s size: its address.
    fn new_box(&mut self, step: Step) -> Option<Value> {
        let size = self.box_size(step)?.div_ceil(8) * 8;
        let n = self.iconst(size);
        let a = self.iconst(8);
        Some(self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![n, a] }, VTy::Ptr))
    }

    fn word(&mut self, b: Value, off: i64) -> Value {
        self.load(Mem::W64, b, off, VTy::Int)
    }

    fn set_word(&mut self, b: Value, off: i64, v: Value) {
        self.store(Mem::W64, b, off, v);
    }

    /// Writes a stream with `step` and box `b` at `dst`.
    fn stream_at(&mut self, dst: Value, step: Step, b: Value) {
        let func = self.func_id(FnKey::Step(step));
        let f = self.emit1(InstKind::FuncRef(func), VTy::Int);
        self.store(Mem::W64, dst, 0, b);
        self.store(Mem::W64, dst, 8, f);
    }

    /// A new stream (a temporary of type `ty`) with `step` and box `b`.
    fn stream_val(&mut self, step: Step, b: Value, ty: Ty, span: Span) -> Option<Val> {
        let dst = self.temp(ty, span)?;
        self.stream_at(dst, step, b);
        self.own_temp(dst, ty);
        Some(Val::Mem(dst))
    }

    /// Steps the stream whose header is at `s`, writing its item at `out`:
    /// whether there was one.
    pub(super) fn step(&mut self, s: Value, out: Value) -> Value {
        let b = self.load(Mem::W64, s, 0, VTy::Ptr);
        let f = self.load(Mem::W64, s, 8, VTy::Int);
        let zero = self.iconst(0);
        self.emit(InstKind::CallInd { callee: f, args: vec![zero, b, out] }, &[VTy::Int])[0]
    }

    /// Drops the stream whose header is at `s` (drop glue): its step frees
    /// its box, unless it has none (it was moved away).
    pub(super) fn drop_stream(&mut self, s: Value) {
        let b = self.load(Mem::W64, s, 0, VTy::Ptr);
        let zero = self.iconst(0);
        let some = self.ucmp(Cmp::Ne, b, zero);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(some, yes, done);
        self.enter(yes);
        let f = self.load(Mem::W64, s, 8, VTy::Int);
        let one = self.iconst(1);
        let null = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        self.emit(InstKind::CallInd { callee: f, args: vec![one, b, null] }, &[VTy::Int]);
        self.jump(done);
        self.enter(done);
    }

    /// A slot for an item of `ty`, which holds nothing.
    fn item_slot(&mut self, ty: Ty, span: Span) -> Option<Value> {
        let slot = self.new_slot(ty, span)?;
        Some(self.slot_addr(slot))
    }

    /// Returns, once the result is written (or there's none), after the
    /// function's cleanups.
    pub(super) fn ret_written(&mut self) -> Option<()> {
        self.run_cleanups(0);
        if self.live() {
            self.terminate(Term::Ret(Vec::new()));
        }
        Some(())
    }

    fn ret_int(&mut self, v: u64) {
        let k = self.iconst(v);
        self.terminate(Term::Ret(vec![k]));
    }

    /// If `cond`, gives 0 from the step being built.
    fn none_if(&mut self, cond: Value) {
        let (stop, go) = (self.new_block(), self.new_block());
        self.br(cond, stop, go);
        self.enter(stop);
        self.ret_int(0);
        self.enter(go);
    }

    // ------------------------------------------------------------ step functions

    /// Builds the step function of `step`.
    pub(super) fn step_func(&mut self, step: Step) -> Option<Vec<VTy>> {
        self.f.span = None;
        let span = self.here();
        let op = self.new_value(VTy::Int);
        let b = self.new_value(VTy::Ptr);
        let out = self.new_value(VTy::Ptr);
        self.f.blocks[0].params.extend([op, b, out]);
        let one = self.iconst(1);
        let dropping = self.ucmp(Cmp::Eq, op, one);
        let (drop_b, next_b) = (self.new_block(), self.new_block());
        self.br(dropping, drop_b, next_b);
        self.enter(drop_b);
        self.step_drop(step, b);
        let size = self.box_size(step)?.div_ceil(8) * 8;
        let n = self.iconst(size);
        let a = self.iconst(8);
        self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![b, n, a] }, &[]);
        self.ret_int(0);
        self.enter(next_b);
        self.step_next(step, b, out, span)?;
        Some(vec![VTy::Int])
    }

    /// Drops what a box of `step` owns.
    fn step_drop(&mut self, step: Step, b: Value) {
        match step {
            Step::Seq { owned: Some(t), .. } => {
                let at = self.offset(b, 32);
                self.drop_at(at, t);
            }
            Step::Woven { iter, .. } => self.drop_at(b, iter),
            Step::Coll { coll, owned: true, .. } => {
                let at = self.offset(b, 32);
                self.drop_at(at, coll);
            }
            Step::Map { .. }
            | Step::Filter { .. }
            | Step::TakeWhile { .. }
            | Step::SkipWhile { .. }
            | Step::Take(_)
            | Step::Skip(_)
            | Step::Indexed { .. } => self.drop_stream(b),
            Step::Zip { .. } | Step::Chain(_) => {
                self.drop_stream(b);
                let other = self.offset(b, 40);
                self.drop_stream(other);
            }
            _ => {}
        }
    }

    /// Builds `op` 0 of `step`'s step function: the next item.
    fn step_next(&mut self, step: Step, b: Value, out: Value, span: Span) -> Option<()> {
        match step {
            Step::Seq { elem, .. } => {
                let (buf, len, i) = (self.word(b, 0), self.word(b, 8), self.word(b, 16));
                let done = self.ucmp(Cmp::Ge, i, len);
                self.none_if(done);
                // A list a lambda changed under the stream.
                let header = self.word(b, 24);
                let zero = self.iconst(0);
                let has = self.ucmp(Cmp::Ne, header, zero);
                let (check, go) = (self.new_block(), self.new_block());
                self.br(has, check, go);
                self.enter(check);
                let hdr = self.load(Mem::W64, b, 24, VTy::Ptr);
                let (now_buf, now_len) = self.list_view(hdr);
                let moved = self.ucmp(Cmp::Ne, now_buf, buf);
                let resized = self.ucmp(Cmp::Ne, now_len, len);
                let changed = self.emit1(InstKind::Bit { op: BitOp::Or, a: moved, b: resized }, VTy::Int);
                let (fail, ok) = (self.new_block(), self.new_block());
                self.br(changed, fail, ok);
                self.enter(fail);
                self.unravel_msg(
                    &[
                        Msg::Text("the list changed while a loop went over it (len "),
                        Msg::Int(len),
                        Msg::Text(", now "),
                        Msg::Int(now_len),
                        Msg::Text(")"),
                    ],
                    span,
                )?;
                self.enter(ok);
                self.jump(go);
                self.enter(go);
                let buf = self.load(Mem::W64, b, 0, VTy::Ptr);
                let at = self.elem_at(buf, i, elem)?;
                let x = self.load_val(at, 0, elem, span)?;
                self.copy_into(out, &x, elem, span)?;
                let one = self.iconst(1);
                let next = self.uarith(IntOp::Add, i, one);
                self.set_word(b, 16, next);
                self.ret_int(1);
            }
            Step::Range(elem) => self.range_next(elem, b, out, span)?,
            Step::Coll { coll, what, item, .. } => self.coll_next(coll, what, item, b, out, span)?,
            Step::Woven { next, iter, item } => {
                let opt = self.typed.results.sigs.get(&next)?.ret;
                let opt = self.plain_ty(opt);
                let abi = self.body_abi(next, span)?;
                let func = self.func_id(FnKey::Body(next));
                let r = match abi.sret {
                    Some(_) => {
                        let dst = self.item_slot(opt, span)?;
                        self.emit(InstKind::Call { func, args: vec![dst, b] }, &[]);
                        Val::Mem(dst)
                    }
                    None => Val::Regs(self.emit(InstKind::Call { func, args: vec![b] }, &abi.results)),
                };
                let _ = iter;
                let some = self.is_some(&r, opt, span)?;
                let not = self.emit1(InstKind::Not(some), VTy::Int);
                self.none_if(not);
                let x = self.unwrap_some(&r, opt, span)?;
                self.store_val(out, 0, item, &x, span)?;
                self.ret_int(1);
            }
            Step::Text(what) => self.text_next(what, b, out)?,
            Step::Chunks { elem, windows } => {
                let (len, n, at) = (self.word(b, 8), self.word(b, 16), self.word(b, 24));
                let (size, next) = if windows {
                    let end = self.uarith(IntOp::Add, at, n);
                    let past = self.ucmp(Cmp::Gt, end, len);
                    self.none_if(past);
                    let one = self.iconst(1);
                    (n, self.uarith(IntOp::Add, at, one))
                } else {
                    let done = self.ucmp(Cmp::Ge, at, len);
                    self.none_if(done);
                    let left = self.uarith(IntOp::Sub, len, at);
                    let size = self.umin(left, n);
                    (size, self.uarith(IntOp::Add, at, size))
                };
                let buf = self.load(Mem::W64, b, 0, VTy::Ptr);
                let start = self.elem_at(buf, at, elem)?;
                self.store(Mem::W64, out, 0, start);
                self.store(Mem::W64, out, 8, size);
                self.set_word(b, 24, next);
                self.ret_int(1);
            }
            Step::Map { from, to, f } => {
                let tmp = self.item_slot(from, span)?;
                let got = self.step(b, tmp);
                let not = self.emit1(InstKind::Not(got), VTy::Int);
                self.none_if(not);
                let fv = self.fn_in_box(b, 16);
                let x = self.load_val(tmp, 0, from, span)?;
                let y = self.call_fn(&fv, f, vec![x], span)?;
                self.drop_at(tmp, from);
                self.move_to(out, 0, y, to, span)?;
                self.ret_int(1);
            }
            Step::Filter { item, f } | Step::TakeWhile { item, f } | Step::SkipWhile { item, f } => {
                let flag = self.word(b, 32);
                let zero = self.iconst(0);
                match step {
                    // Done once something didn't pass.
                    Step::TakeWhile { .. } => {
                        let done = self.ucmp(Cmp::Ne, flag, zero);
                        self.none_if(done);
                    }
                    // Past the first that didn't pass: what's left.
                    Step::SkipWhile { .. } => {
                        let past = self.ucmp(Cmp::Ne, flag, zero);
                        let (rest, skip) = (self.new_block(), self.new_block());
                        self.br(past, rest, skip);
                        self.enter(rest);
                        let got = self.step(b, out);
                        self.terminate(Term::Ret(vec![got]));
                        self.enter(skip);
                    }
                    _ => {}
                }
                let head = self.new_block();
                self.jump(head);
                self.switch_to(head);
                let got = self.step(b, out);
                let not = self.emit1(InstKind::Not(got), VTy::Int);
                self.none_if(not);
                let fv = self.fn_in_box(b, 16);
                let x = self.load_val(out, 0, item, span)?;
                let pass = self.call_fn(&fv, f, vec![x], span)?;
                let pass = self.regs(pass, self.tcx().bool, span)?[0];
                let (yes, no) = (self.new_block(), self.new_block());
                self.br(pass, yes, no);
                match step {
                    Step::Filter { .. } => {
                        self.enter(yes);
                        self.ret_int(1);
                        self.enter(no);
                        self.drop_at(out, item);
                        self.jump(head);
                    }
                    Step::TakeWhile { .. } => {
                        self.enter(yes);
                        self.ret_int(1);
                        self.enter(no);
                        self.drop_at(out, item);
                        let one = self.iconst(1);
                        self.set_word(b, 32, one);
                        self.ret_int(0);
                    }
                    _ => {
                        self.enter(yes);
                        self.drop_at(out, item);
                        self.jump(head);
                        self.enter(no);
                        let one = self.iconst(1);
                        self.set_word(b, 32, one);
                        self.ret_int(1);
                    }
                }
                self.seal(head);
            }
            Step::Take(_) => {
                let n = self.word(b, 16);
                let zero = self.iconst(0);
                let done = self.ucmp(Cmp::Eq, n, zero);
                self.none_if(done);
                let one = self.iconst(1);
                let left = self.uarith(IntOp::Sub, n, one);
                self.set_word(b, 16, left);
                let got = self.step(b, out);
                self.terminate(Term::Ret(vec![got]));
            }
            Step::Skip(item) => {
                let tmp = self.item_slot(item, span)?;
                let (head, skip, rest) = (self.new_block(), self.new_block(), self.new_block());
                self.jump(head);
                self.switch_to(head);
                let n = self.word(b, 16);
                let zero = self.iconst(0);
                let more = self.ucmp(Cmp::Ne, n, zero);
                self.br(more, skip, rest);
                self.enter(skip);
                let one = self.iconst(1);
                let left = self.uarith(IntOp::Sub, n, one);
                self.set_word(b, 16, left);
                let got = self.step(b, tmp);
                let not = self.emit1(InstKind::Not(got), VTy::Int);
                self.none_if(not);
                self.drop_at(tmp, item);
                self.jump(head);
                self.seal(head);
                self.enter(rest);
                let got = self.step(b, out);
                self.terminate(Term::Ret(vec![got]));
            }
            Step::Indexed { item, pair } => {
                let offs = self.field_offsets(pair, span)?;
                let at = self.offset(out, offs[1] as i64);
                let got = self.step(b, at);
                let not = self.emit1(InstKind::Not(got), VTy::Int);
                self.none_if(not);
                let i = self.word(b, 32);
                self.store(Mem::W64, out, offs[0] as i64, i);
                let one = self.iconst(1);
                let next = self.uarith(IntOp::Add, i, one);
                self.set_word(b, 32, next);
                self.ret_int(1);
                let _ = item;
            }
            Step::Zip { a, b: bt, pair } => {
                let done = self.word(b, 32);
                let zero = self.iconst(0);
                let over = self.ucmp(Cmp::Ne, done, zero);
                self.none_if(over);
                let offs = self.field_offsets(pair, span)?;
                let (at_a, at_b) = (self.offset(out, offs[0] as i64), self.offset(out, offs[1] as i64));
                let got = self.step(b, at_a);
                let (first, end) = (self.new_block(), self.new_block());
                self.br(got, first, end);
                self.enter(first);
                let other = self.offset(b, 40);
                let got = self.step(other, at_b);
                let (both, only) = (self.new_block(), self.new_block());
                self.br(got, both, only);
                self.enter(both);
                self.ret_int(1);
                self.enter(only);
                self.drop_at(at_a, a);
                self.jump(end);
                self.enter(end);
                let one = self.iconst(1);
                self.set_word(b, 32, one);
                self.ret_int(0);
                let _ = bt;
            }
            Step::Chain(_) => {
                let second = self.word(b, 32);
                let zero = self.iconst(0);
                let on_first = self.ucmp(Cmp::Eq, second, zero);
                let (first, rest) = (self.new_block(), self.new_block());
                self.br(on_first, first, rest);
                self.enter(first);
                let got = self.step(b, out);
                let (yes, no) = (self.new_block(), self.new_block());
                self.br(got, yes, no);
                self.enter(yes);
                self.ret_int(1);
                self.enter(no);
                let one = self.iconst(1);
                self.set_word(b, 32, one);
                self.jump(rest);
                self.enter(rest);
                let other = self.offset(b, 40);
                let got = self.step(other, out);
                self.terminate(Term::Ret(vec![got]));
            }
        }
        Some(())
    }

    /// What a step function is called: its kind, and its types.
    pub(super) fn step_name(&self, step: Step) -> String {
        let show = |t: Ty| self.tcx().display(self.p, t);
        match step {
            Step::Seq { elem, .. } => format!("seq[{}]", show(elem)),
            Step::Range(t) => format!("range[{}]", show(t)),
            Step::Woven { iter, .. } => format!("iter[{}]", show(iter)),
            Step::Text(what) => format!("{what:?}").to_lowercase(),
            Step::Chunks { elem, windows } => format!("{}[{}]", if windows { "windows" } else { "chunks" }, show(elem)),
            Step::Map { from, to, .. } => format!("map[{}, {}]", show(from), show(to)),
            Step::Filter { item, .. } => format!("filter[{}]", show(item)),
            Step::TakeWhile { item, .. } => format!("take_while[{}]", show(item)),
            Step::SkipWhile { item, .. } => format!("skip_while[{}]", show(item)),
            Step::Take(t) => format!("take[{}]", show(t)),
            Step::Skip(t) => format!("skip[{}]", show(t)),
            Step::Indexed { item, .. } => format!("indexed[{}]", show(item)),
            Step::Zip { pair, .. } => format!("zip[{}]", show(pair)),
            Step::Chain(t) => format!("chain[{}]", show(t)),
            Step::Coll { coll, what, .. } => format!("{what:?}[{}]", show(coll)).to_lowercase(),
        }
    }

    /// The item type of the stream type `ty`.
    fn stream_item_ty(&self, ty: Ty) -> Option<Ty> {
        match self.tcx().kind(ty) {
            TyKind::Adt(_, args) => args.first().map(|&t| self.tcx().degauge(t)),
            _ => None,
        }
    }

    /// Fills a box of `Step::Coll` for the map at `header`.
    fn coll_box(&mut self, b: Value, header: Value) {
        self.set_word(b, 0, header);
        let len = self.word(header, 8);
        self.set_word(b, 8, len);
        let buf = self.word(header, 0);
        self.set_word(b, 16, buf);
        let zero = self.iconst(0);
        self.set_word(b, 24, zero);
    }

    /// A stream of the map (or set) at `header`, of type `coll`, written at
    /// the function's result: it borrows the map.
    pub(super) fn coll_stream_at(
        &mut self,
        header: Value,
        coll: Ty,
        what: super::map::CollItems,
        sty: Ty,
        span: Span,
    ) -> Option<()> {
        let item = self.stream_item_ty(sty)?;
        let step = Step::Coll { coll, what, item, owned: false };
        let b = self.new_box(step)?;
        self.coll_box(b, header);
        let _ = span;
        self.stream_at(self.f.sret?, step, b);
        Some(())
    }

    /// The next of a map's keys, values or pairs, or a set's items.
    fn coll_next(
        &mut self,
        coll: Ty,
        what: super::map::CollItems,
        item: Ty,
        b: Value,
        out: Value,
        span: Span,
    ) -> Option<()> {
        let c = self.coll_of(coll)?;
        let header = self.load(Mem::W64, b, 0, VTy::Ptr);
        let (len, buf, i) = (self.word(b, 8), self.word(b, 16), self.word(b, 24));
        let (now_len, now_buf) = (self.word(header, 8), self.word(header, 0));
        let resized = self.ucmp(Cmp::Ne, now_len, len);
        let moved = self.ucmp(Cmp::Ne, now_buf, buf);
        let changed = self.emit1(InstKind::Bit { op: BitOp::Or, a: resized, b: moved }, VTy::Int);
        let (fail, ok) = (self.new_block(), self.new_block());
        self.br(changed, fail, ok);
        self.enter(fail);
        let what_text: &'static str = if c.val.is_some() {
            "the map changed while a loop went over it (len "
        } else {
            "the set changed while a loop went over it (len "
        };
        self.unravel_msg(
            &[Msg::Text(what_text), Msg::Int(len), Msg::Text(", now "), Msg::Int(now_len), Msg::Text(")")],
            span,
        )?;
        self.enter(ok);
        let done = self.ucmp(Cmp::Ge, i, len);
        self.none_if(done);
        let ebuf = self.load(Mem::W64, header, 0, VTy::Ptr);
        let ent = self.emit1(InstKind::Index { base: ebuf, index: i, scale: c.esize }, VTy::Ptr);
        let k = self.load_val(ent, c.koff as i64, c.key, span)?;
        match (what, c.val) {
            (super::map::CollItems::Keys, _) => self.copy_into(out, &k, c.key, span)?,
            (super::map::CollItems::Values, Some(vt)) => {
                let v = self.load_val(ent, c.voff as i64, vt, span)?;
                self.copy_into(out, &v, vt, span)?;
            }
            (_, Some(vt)) => {
                let offs = self.field_offsets(item, span)?;
                let at = self.offset(out, offs[0] as i64);
                self.copy_into(at, &k, c.key, span)?;
                let v = self.load_val(ent, c.voff as i64, vt, span)?;
                let at = self.offset(out, offs[1] as i64);
                self.copy_into(at, &v, vt, span)?;
            }
            _ => return self.internal(span, "values of a set"),
        }
        let one = self.iconst(1);
        let next = self.uarith(IntOp::Add, i, one);
        self.set_word(b, 24, next);
        self.ret_int(1);
        Some(())
    }

    /// The function value at `b + off`.
    fn fn_in_box(&mut self, b: Value, off: i64) -> Vec<Value> {
        vec![self.load(Mem::W64, b, off, VTy::Int), self.load(Mem::W64, b, off + 8, VTy::Ptr)]
    }

    /// The next value of a range (`rt.range_stream`), by its mode (see
    /// [`Step::Range`]).
    fn range_next(&mut self, elem: Ty, b: Value, out: Value, span: Span) -> Option<()> {
        // A `char` steps as its code point.
        let int = match (self.int_of(elem), self.tcx().kind(elem)) {
            (Some(int), _) => int,
            (None, TyKind::Char) => crate::ty::IntTy { signed: false, bits: 32, size: false },
            _ => return self.unsupported(span, "streams of this range"),
        };
        let mode = self.word(b, 16);
        let v = self.word(b, 0);
        let x = self.new_var(VTy::Int);
        self.write(x, v);
        let one = self.iconst(1);
        let emit = self.new_block();
        let add = |this: &mut Self, policy: Policy| {
            this.emit1(InstKind::Arith { op: IntOp::Add, policy, ty: int, a: v, b: one }, VTy::Int)
        };
        let below = |this: &mut Self, cmp: Cmp, hi: Value| {
            this.emit1(InstKind::ICmp { cmp, unsigned: !int.signed, a: v, b: hi }, VTy::Int)
        };
        let is = |this: &mut Self, k: u64| {
            let k = this.iconst(k);
            this.ucmp(Cmp::Eq, mode, k)
        };
        let set_mode = |this: &mut Self, k: u64| {
            let k = this.iconst(k);
            this.set_word(b, 16, k);
        };

        // `lo..hi`: below the end, the next is one more (which can't wrap).
        let (excl, other) = (self.new_block(), self.new_block());
        let c = is(self, RANGE_TO);
        self.br(c, excl, other);
        self.enter(excl);
        let hi = self.word(b, 8);
        let inside = below(self, Cmp::Lt, hi);
        let (go, stop) = (self.new_block(), self.new_block());
        self.br(inside, go, stop);
        self.enter(go);
        let next = add(self, Policy::Wrap);
        self.set_word(b, 0, next);
        self.jump(emit);
        self.enter(stop);
        self.ret_int(0);

        // `lo..=hi`: at the end, done after it.
        self.enter(other);
        let (incl, other) = (self.new_block(), self.new_block());
        let c = is(self, RANGE_THROUGH);
        self.br(c, incl, other);
        self.enter(incl);
        let hi = self.word(b, 8);
        let inside = below(self, Cmp::Le, hi);
        let (go, stop) = (self.new_block(), self.new_block());
        self.br(inside, go, stop);
        self.enter(stop);
        self.ret_int(0);
        self.enter(go);
        let last = self.ucmp(Cmp::Eq, v, hi);
        let (mark, step) = (self.new_block(), self.new_block());
        self.br(last, mark, step);
        self.enter(mark);
        set_mode(self, RANGE_DONE);
        self.jump(emit);
        self.enter(step);
        let next = add(self, Policy::Wrap);
        self.set_word(b, 0, next);
        self.jump(emit);

        // `lo..`: the first value as it is, then each one more, which
        // unravels past the type's maximum (only when it's asked for).
        self.enter(other);
        let (fresh, other) = (self.new_block(), self.new_block());
        let c = is(self, RANGE_FROM);
        self.br(c, fresh, other);
        self.enter(fresh);
        set_mode(self, RANGE_FROM_ON);
        self.jump(emit);
        self.enter(other);
        let (on, done) = (self.new_block(), self.new_block());
        let c = is(self, RANGE_FROM_ON);
        self.br(c, on, done);
        self.enter(done);
        self.ret_int(0);
        self.enter(on);
        let next = add(self, Policy::Trap);
        self.set_word(b, 0, next);
        self.write(x, next);
        self.jump(emit);

        self.enter(emit);
        let v = self.read(x);
        self.store_val(out, 0, elem, &Val::Regs(vec![v]), span)?;
        self.ret_int(1);
        Some(())
    }

    /// The next character, byte, line or word of text.
    fn text_next(&mut self, what: TextItems, b: Value, out: Value) -> Option<()> {
        let (p, n, at) = (self.load(Mem::W64, b, 0, VTy::Ptr), self.word(b, 8), self.word(b, 16));
        match what {
            TextItems::Words => {
                let r = self.emit(
                    InstKind::Native { name: "str.next_word", imm: 0, args: vec![p, n, at] },
                    &[VTy::Ptr, VTy::Int, VTy::Int, VTy::Int],
                );
                let zero = self.iconst(0);
                let none = self.ucmp(Cmp::Eq, r[3], zero);
                self.none_if(none);
                self.store(Mem::W64, out, 0, r[0]);
                self.store(Mem::W64, out, 8, r[1]);
                self.set_word(b, 16, r[2]);
            }
            _ => {
                let done = self.ucmp(Cmp::Ge, at, n);
                self.none_if(done);
                match what {
                    TextItems::Chars => {
                        let r = self.emit(
                            InstKind::Native { name: "str.next_char", imm: 0, args: vec![p, n, at] },
                            &[VTy::Int, VTy::Int],
                        );
                        self.store(Mem::U32, out, 0, r[0]);
                        self.set_word(b, 16, r[1]);
                    }
                    TextItems::Bytes => {
                        let at_p = self.emit1(InstKind::Index { base: p, index: at, scale: 1 }, VTy::Ptr);
                        let byte = self.load(Mem::U8, at_p, 0, VTy::Int);
                        self.store(Mem::U8, out, 0, byte);
                        let one = self.iconst(1);
                        let next = self.uarith(IntOp::Add, at, one);
                        self.set_word(b, 16, next);
                    }
                    _ => {
                        let r = self.emit(
                            InstKind::Native { name: "str.next_line", imm: 0, args: vec![p, n, at] },
                            &[VTy::Ptr, VTy::Int, VTy::Int],
                        );
                        self.store(Mem::W64, out, 0, r[0]);
                        self.store(Mem::W64, out, 8, r[1]);
                        self.set_word(b, 16, r[2]);
                    }
                }
            }
        }
        self.ret_int(1);
        Some(())
    }

    // ------------------------------------------------------------ making streams

    /// A stream of the items of `v`, a value of `from`, which come from
    /// `source`; `ty` is the stream's type.
    pub(super) fn stream_of(&mut self, source: &IterSource, v: Val, from: Ty, ty: Ty, span: Span) -> Option<Val> {
        match *source {
            IterSource::Seq(elem) => {
                let elem = self.tcx().degauge(elem);
                // A temporary list or array: the stream owns it.
                let owned = match &v {
                    Val::Mem(p)
                        if self.f.temps.contains_key(p) && !matches!(self.seq_of(from), Some(Seq::Slice(_))) =>
                    {
                        Some(from)
                    }
                    _ => None,
                };
                let step = Step::Seq { elem, owned };
                let b = self.new_box(step)?;
                let header = match (owned, self.seq_of(from), &v) {
                    (None, Some(Seq::List(_)), &Val::Mem(h)) => h,
                    _ => self.emit1(InstKind::Iconst(0), VTy::Ptr),
                };
                let (buf, len) = match owned {
                    Some(t) => {
                        let inside = self.offset(b, 32);
                        self.move_to(inside, 0, v, t, span)?;
                        self.view_of(&Val::Mem(inside), t, span)?
                    }
                    None => self.view_of(&v, from, span)?,
                };
                self.set_word(b, 0, buf);
                self.set_word(b, 8, len);
                let zero = self.iconst(0);
                self.set_word(b, 16, zero);
                self.set_word(b, 24, header);
                self.stream_val(step, b, ty, span)
            }
            IterSource::Range(elem) => {
                let elem = self.tcx().degauge(elem);
                let r = self.regs(v, from, span)?;
                let step = Step::Range(elem);
                let b = self.new_box(step)?;
                self.set_word(b, 0, r[0]);
                self.set_word(b, 8, r[1]);
                // The range's flags (1: there's a high end, 2: it's
                // included) as a mode: `flags & 2`, or `RANGE_FROM` without
                // a high end.
                let flags = r[2];
                let (one, two) = (self.iconst(1), self.iconst(2));
                let has_hi = self.emit1(InstKind::Bit { op: BitOp::And, a: flags, b: one }, VTy::Int);
                let open = self.emit1(InstKind::Bit { op: BitOp::Xor, a: has_hi, b: one }, VTy::Int);
                let incl = self.emit1(InstKind::Bit { op: BitOp::And, a: flags, b: two }, VTy::Int);
                let mode = self.emit1(InstKind::Bit { op: BitOp::Or, a: open, b: incl }, VTy::Int);
                self.set_word(b, 16, mode);
                self.stream_val(step, b, ty, span)
            }
            IterSource::Woven { next, item } => {
                let iter = self.tcx().degauge(from);
                let item = self.plain_ty(item);
                let step = Step::Woven { next, iter, item };
                let b = self.new_box(step)?;
                // Its own copy: the stream changes it as it goes.
                let v = self.owned(v, iter, span)?;
                self.move_to(b, 0, v, iter, span)?;
                self.stream_val(step, b, ty, span)
            }
            IterSource::MapPairs(..) | IterSource::SetItems(_) => {
                let what = match source {
                    IterSource::MapPairs(..) => super::map::CollItems::Pairs,
                    _ => super::map::CollItems::Keys,
                };
                let Val::Mem(p) = v else { return self.internal(span, "a map in registers") };
                let item = match *source {
                    IterSource::MapPairs(k, v) => {
                        let (k, v) = (self.tcx().degauge(k), self.tcx().degauge(v));
                        self.tcx().find(&TyKind::Tuple(vec![k, v]))?
                    }
                    IterSource::SetItems(t) => self.tcx().degauge(t),
                    _ => return None,
                };
                // A temporary map: the stream owns it.
                if self.f.temps.contains_key(&p) {
                    let step = Step::Coll { coll: from, what, item, owned: true };
                    let b = self.new_box(step)?;
                    let at = self.offset(b, 32);
                    self.move_to(at, 0, Val::Mem(p), from, span)?;
                    self.coll_box(b, at);
                    return self.stream_val(step, b, ty, span);
                }
                let step = Step::Coll { coll: from, what, item, owned: false };
                let b = self.new_box(step)?;
                self.coll_box(b, p);
                self.stream_val(step, b, ty, span)
            }
            IterSource::Shuttle(_) | IterSource::Burst(_) => self.unsupported(span, "shuttles"),
        }
    }

    /// The `for` loop `e` over a stream: `iter` gives one (or something
    /// `source` makes one of).
    pub(super) fn for_stream(
        &mut self,
        e: &'a crate::hir::Expr,
        pat: &'a crate::hir::Pat,
        iter: &'a crate::hir::Expr,
        body: &'a crate::hir::Block,
        source: Option<IterSource>,
    ) -> Option<()> {
        let span = iter.span;
        let v = self.expr(iter)?;
        self.moved_away(iter);
        let from = self.coerced_ty(iter);
        let (v, sty, item) = match &source {
            Some(src) => {
                let item = match src {
                    IterSource::Seq(t) | IterSource::Range(t) | IterSource::SetItems(t) => self.plain_ty(*t),
                    IterSource::Woven { item, .. } => self.plain_ty(*item),
                    IterSource::MapPairs(k, v) => {
                        let (k, v) = (self.tcx().degauge(*k), self.tcx().degauge(*v));
                        match self.tcx().find(&TyKind::Tuple(vec![k, v])) {
                            Some(t) => t,
                            None => return self.internal(span, "the pairs of a map with no type"),
                        }
                    }
                    _ => return self.unsupported(span, "this kind of `for` loop"),
                };
                let sty = self.stream_ty(item).or_else(|| self.internal(span, "a stream with no type"))?;
                (self.stream_of(src, v, from, sty, span)?, sty, item)
            }
            None => {
                let TyKind::Adt(_, args) = self.tcx().kind(from).clone() else { return None };
                (v, from, self.tcx().degauge(args[0]))
            }
        };
        // The loop owns the stream: dropped however it ends.
        let Val::Mem(s) = self.owned(v, sty, span)? else { return None };
        let depth = self.open_scope();
        if let Some(slot) = self.f.slot_of.get(&s).copied() {
            self.add_cleanup(CleanupKind::Drop { at: Stable::Slot(slot), ty: sty });
        }
        let (head, body_b, exit) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let inner = self.open_scope();
        let item_at = self.temp(item, span)?;
        let got = self.step(s, item_at);
        let (yes, no) = (self.new_block(), self.new_block());
        self.br(got, yes, no);
        // No item: nothing in the item's scope is owned yet.
        self.enter(no);
        self.jump(exit);
        self.enter(yes);
        self.own_temp(item_at, item);
        let x = self.load_val(item_at, 0, item, span)?;
        let scopes = self.f.scopes.len();
        self.f.loops.push(super::LoopCtx {
            id: e.id,
            brk: exit,
            cont: head,
            result: super::LoopResult::None,
            scopes: scopes - 1,
        });
        let mut done = self.bind_irrefutable(pat, x, item);
        if done.is_some() {
            let join = self.join(self.tcx().unit, e.span)?;
            done = self.block_into(body, &join);
        }
        self.f.loops.pop();
        self.close_scope(inner);
        if self.live() && done.is_some() {
            self.jump(head);
        }
        let _ = body_b;
        self.seal(head);
        self.enter(exit);
        self.close_scope(depth);
        Some(())
    }

    /// `Stream[item]`; or, if the program has no such type, another
    /// stream's type: every stream is held and dropped the same way.
    fn stream_ty(&self, item: Ty) -> Option<Ty> {
        let m = self.p.modules.iter().find(|m| m.path == "std.iter")?;
        let def = *m.items.get("Stream")?;
        self.tcx().find(&TyKind::Adt(def, vec![item])).or_else(|| self.tcx().any_of(def))
    }

    // ------------------------------------------------------------ the methods

    /// The body of a `Stream` method, or a method that makes a stream.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stream_method(
        &mut self,
        path: &str,
        args: &[Val],
        tys: &[Ty],
        ret: Ty,
        sret: Option<Value>,
        span: Span,
    ) -> Option<()> {
        let name = path.rsplit('.').next().unwrap_or_default();
        // Makers of streams of text and of views.
        if let Some(what) = match path {
            "std.string.Str.chars" => Some(TextItems::Chars),
            "std.string.Str.bytes" => Some(TextItems::Bytes),
            "std.string.Str.lines" => Some(TextItems::Lines),
            "std.string.Str.words" => Some(TextItems::Words),
            _ => None,
        } {
            let (p, n) = self.text_of(args[0].clone(), tys[0], span)?;
            let step = Step::Text(what);
            let b = self.new_box(step)?;
            self.set_word(b, 0, p);
            self.set_word(b, 8, n);
            let zero = self.iconst(0);
            self.set_word(b, 16, zero);
            self.stream_at(sret?, step, b);
            return self.ret_written();
        }
        if let "chunks" | "windows" = name
            && path.starts_with("std.slice.")
        {
            let (buf, len) = self.view_of(&args[0], tys[0], span)?;
            let n = self.regs(args[1].clone(), self.tcx().usize, span)?[0];
            let zero = self.iconst(0);
            let bad = self.ucmp(Cmp::Eq, n, zero);
            let (fail, ok) = (self.new_block(), self.new_block());
            self.br(bad, fail, ok);
            self.enter(fail);
            let msg: &'static str = if name == "chunks" { "chunks of 0 elements" } else { "windows of 0 elements" };
            self.unravel_msg(&[Msg::Text(msg)], span)?;
            self.enter(ok);
            let elem = self.seq_of(tys[0])?.elem();
            let step = Step::Chunks { elem: self.tcx().degauge(elem), windows: name == "windows" };
            let b = self.new_box(step)?;
            self.set_word(b, 0, buf);
            self.set_word(b, 8, len);
            self.set_word(b, 16, n);
            self.set_word(b, 24, zero);
            self.stream_at(sret?, step, b);
            return self.ret_written();
        }
        // A `Stream` method: `self` is the stream, at an address, its own.
        let Val::Mem(s) = args[0] else { return self.internal(span, "a stream in registers") };
        let sty = tys[0];
        let TyKind::Adt(_, sargs) = self.tcx().kind(sty).clone() else { return None };
        let item = self.tcx().degauge(sargs[0]);
        let adaptor = |this: &mut Self, step: Step, f: Option<(Val, Ty)>, word: Option<Value>, other: Option<Val>| {
            let b = this.new_box(step)?;
            // The stream moves into the box (the box drops it).
            let (size, _) = (16u64, 8u64);
            this.emit(InstKind::Copy { dst: b, src: s, size }, &[]);
            if let Some((fv, fty)) = f {
                let r = this.regs(fv, fty, span)?;
                this.store(Mem::W64, b, 16, r[0]);
                this.store(Mem::W64, b, 24, r[1]);
            }
            let zero = this.iconst(0);
            this.set_word(b, 32, zero);
            if let Some(w) = word {
                this.set_word(b, 16, w);
            }
            // `other` is a read parameter, but a stream can't be used
            // again once it's passed (M0101): the box takes it, and the
            // caller is left one with no box, which drops as nothing.
            if let Some(Val::Mem(o)) = other {
                let at = this.offset(b, 40);
                this.emit(InstKind::Copy { dst: at, src: o, size: 16 }, &[]);
                this.emit(InstKind::Zero { dst: o, size: 16 }, &[]);
            }
            this.stream_at(this.f.sret?, step, b);
            this.ret_written()
        };
        match name {
            "map" => {
                let TyKind::Adt(_, out) = self.tcx().kind(ret).clone() else { return None };
                let to = self.tcx().degauge(out[0]);
                adaptor(self, Step::Map { from: item, to, f: tys[1] }, Some((args[1].clone(), tys[1])), None, None)
            }
            "filter" => adaptor(self, Step::Filter { item, f: tys[1] }, Some((args[1].clone(), tys[1])), None, None),
            "take_while" => {
                adaptor(self, Step::TakeWhile { item, f: tys[1] }, Some((args[1].clone(), tys[1])), None, None)
            }
            "skip_while" => {
                adaptor(self, Step::SkipWhile { item, f: tys[1] }, Some((args[1].clone(), tys[1])), None, None)
            }
            "take" | "skip" => {
                let n = self.regs(args[1].clone(), self.tcx().usize, span)?[0];
                let step = if name == "take" { Step::Take(item) } else { Step::Skip(item) };
                adaptor(self, step, None, Some(n), None)
            }
            "indexed" => {
                let TyKind::Adt(_, out) = self.tcx().kind(ret).clone() else { return None };
                adaptor(self, Step::Indexed { item, pair: self.tcx().degauge(out[0]) }, None, None, None)
            }
            "zip" => {
                let TyKind::Adt(_, out) = self.tcx().kind(ret).clone() else { return None };
                let pair = self.tcx().degauge(out[0]);
                let TyKind::Tuple(parts) = self.tcx().kind(pair).clone() else { return None };
                adaptor(self, Step::Zip { a: item, b: parts[1], pair }, None, None, Some(args[1].clone()))
            }
            "chain" => adaptor(self, Step::Chain(item), None, None, Some(args[1].clone())),
            _ => self.consume(name, s, sty, item, args, tys, ret, span),
        }
    }

    /// The `Stream` methods that run it: each drops the stream when done.
    #[allow(clippy::too_many_arguments)]
    fn consume(
        &mut self,
        name: &str,
        s: Value,
        sty: Ty,
        item: Ty,
        args: &[Val],
        tys: &[Ty],
        ret: Ty,
        span: Span,
    ) -> Option<()> {
        self.add_cleanup(CleanupKind::Drop { at: Stable::Param(s), ty: sty });
        let x = self.item_slot(item, span)?;
        // Builds a loop over the items: `each(x)` with the item at `x`,
        // which it moves or drops; it may leave the function.
        let run = |this: &mut Self, each: &mut dyn FnMut(&mut Self, BlockId) -> Option<()>| -> Option<()> {
            let (head, body, done) = (this.new_block(), this.new_block(), this.new_block());
            this.jump(head);
            this.switch_to(head);
            let got = this.step(s, x);
            this.br(got, body, done);
            this.enter(body);
            each(this, head)?;
            if this.live() {
                this.jump(head);
            }
            this.seal(head);
            this.enter(done);
            Some(())
        };
        let usize = self.tcx().usize;
        match name {
            "next" => {
                // `mut self`: one item, and the stream stays.
                self.f.cleanups.clear();
                let got = self.step(s, x);
                let (yes, no) = (self.new_block(), self.new_block());
                self.br(got, yes, no);
                self.enter(yes);
                let v = self.load_val(x, 0, item, span)?;
                let v = self.some(ret, item, v, span)?;
                self.ret_val(v, span)?;
                self.enter(no);
                let v = self.none(ret, span)?;
                self.ret_val(v, span)
            }
            "count" => {
                let n = self.new_var(VTy::Int);
                let zero = self.iconst(0);
                self.write(n, zero);
                run(self, &mut |this, _| {
                    this.drop_at(x, item);
                    let cur = this.read(n);
                    let one = this.iconst(1);
                    let next = this.uarith(IntOp::Add, cur, one);
                    this.write(n, next);
                    Some(())
                })?;
                let n = self.read(n);
                self.ret_val(Val::Regs(vec![n]), span)
            }
            "collect" => {
                let dst = self.f.sret?;
                self.empty_header(dst, ret)?;
                let (size, align) = self.size_align(item)?;
                // The list so far is dropped if the stream unravels while
                // it's being collected.
                let partial = self.add_cleanup(CleanupKind::Drop { at: Stable::Param(dst), ty: ret });
                run(self, &mut |this, _| {
                    let (_, n) = this.list_view(dst);
                    let one = this.iconst(1);
                    let want = this.uarith(IntOp::Add, n, one);
                    let (sz, al) = (this.iconst(size), this.iconst(align));
                    this.emit(InstKind::Native { name: "mem.grow", imm: 0, args: vec![dst, sz, al, want] }, &[]);
                    let (buf, _) = this.list_view(dst);
                    let at = this.elem_at(buf, n, item)?;
                    if size > 0 {
                        this.emit(InstKind::Copy { dst: at, src: x, size }, &[]);
                    }
                    this.store(Mem::W64, dst, 8, want);
                    Some(())
                })?;
                self.kill_cleanup(partial);
                self.ret_written()
            }
            "each" => {
                let f = self.regs(args[1].clone(), tys[1], span)?;
                run(self, &mut |this, _| {
                    let v = this.load_val(x, 0, item, span)?;
                    this.call_fn(&f, tys[1], vec![v], span)?;
                    this.drop_at(x, item);
                    Some(())
                })?;
                self.ret_written()
            }
            "all" | "exists" | "find" | "position" => {
                let f = self.regs(args[1].clone(), tys[1], span)?;
                let i = self.new_var(VTy::Int);
                let zero = self.iconst(0);
                self.write(i, zero);
                run(self, &mut |this, _| {
                    let v = this.load_val(x, 0, item, span)?;
                    let pass = this.call_fn(&f, tys[1], vec![v], span)?;
                    let pass = this.regs(pass, this.tcx().bool, span)?[0];
                    let (hit, miss) = (this.new_block(), this.new_block());
                    // `all` stops at the first that doesn't pass.
                    if name == "all" {
                        this.br(pass, miss, hit);
                    } else {
                        this.br(pass, hit, miss);
                    }
                    this.enter(hit);
                    match name {
                        "find" => {
                            let v = this.load_val(x, 0, item, span)?;
                            let v = this.some(ret, item, v, span)?;
                            this.ret_val(v, span)?;
                        }
                        "position" => {
                            this.drop_at(x, item);
                            let cur = this.read(i);
                            let v = this.some(ret, usize, Val::Regs(vec![cur]), span)?;
                            this.ret_val(v, span)?;
                        }
                        _ => {
                            this.drop_at(x, item);
                            let r = this.iconst(u64::from(name == "exists"));
                            this.ret_val(Val::Regs(vec![r]), span)?;
                        }
                    }
                    this.enter(miss);
                    this.drop_at(x, item);
                    let cur = this.read(i);
                    let one = this.iconst(1);
                    let next = this.uarith(IntOp::Add, cur, one);
                    this.write(i, next);
                    Some(())
                })?;
                match name {
                    "all" | "exists" => {
                        let r = self.iconst(u64::from(name == "all"));
                        self.ret_val(Val::Regs(vec![r]), span)
                    }
                    _ => {
                        let v = self.none(ret, span)?;
                        self.ret_val(v, span)
                    }
                }
            }
            "last" | "min" | "max" | "reduce" => {
                // The one kept so far, in a slot of its own.
                let best = self.item_slot(item, span)?;
                let have = self.new_var(VTy::Int);
                let zero = self.iconst(0);
                self.write(have, zero);
                let (size, _) = self.size_align(item)?;
                let float = self.float_of(item).is_some();
                let f = if name == "reduce" { Some(self.regs(args[1].clone(), tys[1], span)?) } else { None };
                run(self, &mut |this, head| {
                    let keep_new = |this: &mut Self| {
                        if size > 0 {
                            this.emit(InstKind::Copy { dst: best, src: x, size }, &[]);
                        }
                    };
                    let had = this.read(have);
                    let one = this.iconst(1);
                    // A NaN is neither smaller nor larger than anything.
                    if float && name != "last" && name != "reduce" {
                        let v = this.load_val(x, 0, item, span)?;
                        let v = this.regs(v, item, span)?[0];
                        let fl = this.float_of(item)?;
                        let nan = this.emit1(InstKind::FCmp { cmp: Cmp::Ne, ty: fl, a: v, b: v }, VTy::Int);
                        let (skip, go) = (this.new_block(), this.new_block());
                        this.br(nan, skip, go);
                        this.enter(skip);
                        this.jump(head);
                        this.enter(go);
                    }
                    let (first, later, next) = (this.new_block(), this.new_block(), this.new_block());
                    this.br(had, later, first);
                    this.enter(first);
                    keep_new(this);
                    this.write(have, one);
                    this.jump(next);
                    this.enter(later);
                    match name {
                        "last" => {
                            this.drop_at(best, item);
                            keep_new(this);
                        }
                        "reduce" => {
                            let (a, v) = (this.load_val(best, 0, item, span)?, this.load_val(x, 0, item, span)?);
                            let r = this.call_fn(f.as_ref()?, tys[1], vec![a, v], span)?;
                            this.drop_at(best, item);
                            this.drop_at(x, item);
                            this.store_val(best, 0, item, &r, span)?;
                            this.moved_from(&r, item);
                        }
                        _ => {
                            let (v, b) = (this.load_val(x, 0, item, span)?, this.load_val(best, 0, item, span)?);
                            let c = this.cmp_vals(v, b, item, span)?;
                            let zero = this.iconst(0);
                            let cmp = if name == "min" { Cmp::Lt } else { Cmp::Ge };
                            let better = this.emit1(InstKind::ICmp { cmp, unsigned: false, a: c, b: zero }, VTy::Int);
                            let (take, drop_new) = (this.new_block(), this.new_block());
                            this.br(better, take, drop_new);
                            this.enter(take);
                            this.drop_at(best, item);
                            keep_new(this);
                            this.jump(next);
                            this.enter(drop_new);
                            this.drop_at(x, item);
                        }
                    }
                    this.jump(next);
                    this.enter(next);
                    Some(())
                })?;
                let had = self.read(have);
                let (yes, no) = (self.new_block(), self.new_block());
                self.br(had, yes, no);
                self.enter(yes);
                let v = self.load_val(best, 0, item, span)?;
                let v = self.some(ret, item, v, span)?;
                self.ret_val(v, span)?;
                self.enter(no);
                let v = self.none(ret, span)?;
                self.ret_val(v, span)
            }
            "fold" => {
                let acc_ty = tys[1];
                let f = self.regs(args[2].clone(), tys[2], span)?;
                let acc = self.item_slot(acc_ty, span)?;
                let init = args[1].clone();
                self.store_val(acc, 0, acc_ty, &init, span)?;
                run(self, &mut |this, _| {
                    let (a, v) = (this.load_val(acc, 0, acc_ty, span)?, this.load_val(x, 0, item, span)?);
                    let r = this.call_fn(&f, tys[2], vec![a, v], span)?;
                    this.drop_at(acc, acc_ty);
                    this.drop_at(x, item);
                    this.store_val(acc, 0, acc_ty, &r, span)?;
                    this.moved_from(&r, acc_ty);
                    Some(())
                })?;
                let v = self.load_val(acc, 0, acc_ty, span)?;
                let v = self.owned(v, acc_ty, span)?;
                self.ret_val(v, span)
            }
            "sum" | "product" => {
                let acc = self.new_var(match self.float_of(item) {
                    Some(crate::ty::FloatTy::F32) => VTy::F32,
                    Some(_) => VTy::F64,
                    None => VTy::Int,
                });
                let start: u64 = u64::from(name == "product");
                let init = match self.float_of(item) {
                    Some(fl) => {
                        let bits = if fl == crate::ty::FloatTy::F32 {
                            u64::from((start as f32).to_bits())
                        } else {
                            (start as f64).to_bits()
                        };
                        let vty = if fl == crate::ty::FloatTy::F32 { VTy::F32 } else { VTy::F64 };
                        self.emit1(InstKind::Fconst(bits), vty)
                    }
                    None => self.iconst(start),
                };
                self.write(acc, init);
                run(self, &mut |this, _| {
                    let v = this.load_val(x, 0, item, span)?;
                    let v = this.regs(v, item, span)?[0];
                    let cur = this.read(acc);
                    let next = match (this.float_of(item), this.int_of(item)) {
                        (Some(fl), _) => {
                            let op = if name == "sum" { FOp::Add } else { FOp::Mul };
                            let vty = if fl == crate::ty::FloatTy::F32 { VTy::F32 } else { VTy::F64 };
                            this.emit1(InstKind::FArith { op, ty: fl, a: cur, b: v }, vty)
                        }
                        (_, Some(int)) => {
                            let op = if name == "sum" { IntOp::Add } else { IntOp::Mul };
                            this.emit1(InstKind::Arith { op, policy: Policy::Trap, ty: int, a: cur, b: v }, VTy::Int)
                        }
                        _ => return this.unsupported(span, "summing these"),
                    };
                    this.write(acc, next);
                    Some(())
                })?;
                let v = self.read(acc);
                self.ret_val(Val::Regs(vec![v]), span)
            }
            "join" => {
                let dst = self.f.sret?;
                let (sp, sn) = self.text_of(args[1].clone(), tys[1], span)?;
                let buf = self.new_sink();
                let first = self.new_var(VTy::Int);
                let one = self.iconst(1);
                self.write(first, one);
                run(self, &mut |this, _| {
                    let is_first = this.read(first);
                    let (sep, put) = (this.new_block(), this.new_block());
                    this.br(is_first, put, sep);
                    this.enter(sep);
                    this.emit(InstKind::Native { name: "sink.str", imm: 0, args: vec![buf.0, sp, sn] }, &[]);
                    this.jump(put);
                    this.enter(put);
                    let zero = this.iconst(0);
                    this.write(first, zero);
                    let v = this.load_val(x, 0, item, span)?;
                    this.write_value(buf, v, item, None, false, span)?;
                    this.drop_at(x, item);
                    Some(())
                })?;
                self.emit(InstKind::Native { name: "sink.string", imm: 0, args: vec![buf.0, dst] }, &[]);
                self.ret_written()
            }
            _ => self.internal(span, &format!("no body for `Stream.{name}`")),
        }
    }
}
