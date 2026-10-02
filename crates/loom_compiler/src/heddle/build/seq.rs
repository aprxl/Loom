//! Arrays, lists and slices (design/vm.md §5.2, §10.8).
//!
//! An array `[T; N]` is its N elements, inline. A list is a header (the
//! buffer's address, the length, the capacity) whose buffer is on the heap;
//! a slice `[T]` is a view, an address and a length. Reading `xs[i]` checks
//! `i` against the length (`bounds`), unless the index's type can't reach
//! past an array's end.
//!
//! A `for` loop reads the address and the length once. Over a list whose
//! loop body calls anything, it checks at the top of each iteration that
//! the list still has them: a lambda the body calls could change it, which
//! the type checker can't see (`rt.same_len` on Lua).

use loom_syntax::source::Span;

use super::text::Msg;
use super::value::{Place, Val};
use super::{Builder, LoopCtx, LoopResult};
use crate::heddle::{BitOp, Cmp, InstKind, IntOp, Mem, Policy, VTy, Value};
use crate::hir::{self, Block, ExprKind, GenericArg, PatKind};
use crate::ty::{IntTy, Ty, TyKind};
use crate::typeck::Seq;

pub(super) const U64: IntTy = IntTy { signed: false, bits: 64, size: false };

impl<'a> Builder<'a> {
    /// The sequence `ty` is, if it's one.
    pub(super) fn seq_of(&self, ty: Ty) -> Option<Seq> {
        let ty = self.tcx().degauge(ty);
        match self.tcx().kind(ty) {
            TyKind::Array(t, n) => Some(Seq::Array(*t, *n)),
            TyKind::Adt(def, args) if args.len() == 1 => match self.p.def_path(*def).as_str() {
                "std.list.List" => Some(Seq::List(args[0])),
                "std.slice.Slice" => Some(Seq::Slice(args[0])),
                _ => None,
            },
            _ => None,
        }
    }

    /// The elements of `v`, a sequence of type `ty`: their address and how
    /// many there are.
    pub(super) fn view_of(&mut self, v: &Val, ty: Ty, span: Span) -> Option<(Value, Value)> {
        match (self.seq_of(ty), v) {
            (Some(Seq::Array(_, n)), &Val::Mem(ptr)) => Some((ptr, self.iconst(n))),
            (Some(Seq::List(_)), &Val::Mem(header)) => Some(self.list_view(header)),
            (Some(Seq::Slice(_)), Val::Regs(vals)) => Some((vals[0], vals[1])),
            (Some(Seq::Slice(_)), &Val::Mem(ptr)) => Some(self.list_view(ptr)),
            _ => self.internal(span, "the elements of something that isn't a sequence"),
        }
    }

    /// The buffer and the length of the list whose header is at `header`
    /// (or of the slice at it).
    pub(super) fn list_view(&mut self, header: Value) -> (Value, Value) {
        (self.load(Mem::W64, header, 0, VTy::Ptr), self.load(Mem::W64, header, 8, VTy::Int))
    }

    /// The address of element `i` of the `elem`s from `buf`.
    pub(super) fn elem_at(&mut self, buf: Value, i: Value, elem: Ty) -> Option<Value> {
        let (size, _) = self.size_align(elem)?;
        Some(self.emit1(InstKind::Index { base: buf, index: i, scale: size }, VTy::Ptr))
    }

    /// `a op b` on `u64`s, wrapping (the operands are lengths and indices,
    /// which don't).
    pub(super) fn uarith(&mut self, op: IntOp, a: Value, b: Value) -> Value {
        self.emit1(InstKind::Arith { op, policy: Policy::Wrap, ty: U64, a, b }, VTy::Int)
    }

    pub(super) fn ucmp(&mut self, cmp: Cmp, a: Value, b: Value) -> Value {
        self.emit1(InstKind::ICmp { cmp, unsigned: true, a, b }, VTy::Int)
    }

    /// Builds `body(i)` for each `i` from `from` up to (not including) `to`.
    pub(super) fn count_loop(
        &mut self,
        from: Value,
        to: Value,
        mut body: impl FnMut(&mut Self, Value) -> Option<()>,
    ) -> Option<()> {
        let i = self.new_var(VTy::Int);
        self.write(i, from);
        let (head, inside, exit) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let more = self.ucmp(Cmp::Lt, cur, to);
        self.br(more, inside, exit);
        self.enter(inside);
        let cur = self.read(i);
        body(self, cur)?;
        if self.live() {
            let cur = self.read(i);
            let one = self.iconst(1);
            let next = self.uarith(IntOp::Add, cur, one);
            self.write(i, next);
            self.jump(head);
        }
        self.seal(head);
        self.enter(exit);
        Some(())
    }

    // ------------------------------------------------------------ making them

    /// A new buffer for `n` elements of `elem`, or 0 for none (or none of
    /// size 0): its address.
    pub(super) fn alloc_elems(&mut self, n: Value, elem: Ty) -> Option<Value> {
        let (size, align) = self.size_align(elem)?;
        let ptr = self.new_var(VTy::Ptr);
        let null = self.emit1(InstKind::Iconst(0), VTy::Ptr);
        self.write(ptr, null);
        let zero = self.iconst(0);
        let (yes, done) = (self.new_block(), self.new_block());
        let some = self.ucmp(Cmp::Ne, n, zero);
        let wants = if size == 0 { zero } else { some };
        self.br(wants, yes, done);
        self.enter(yes);
        let scale = self.iconst(size);
        let bytes =
            self.emit1(InstKind::Arith { op: IntOp::Mul, policy: Policy::Trap, ty: U64, a: n, b: scale }, VTy::Int);
        let a = self.iconst(align);
        let p = self.emit1(InstKind::Native { name: "mem.alloc", imm: 0, args: vec![bytes, a] }, VTy::Ptr);
        self.write(ptr, p);
        self.jump(done);
        self.enter(done);
        Some(self.read(ptr))
    }

    /// Writes a list header at `header`: its buffer, length and capacity.
    pub(super) fn set_header(&mut self, header: Value, buf: Value, len: Value, cap: Value) {
        self.store(Mem::W64, header, 0, buf);
        self.store(Mem::W64, header, 8, len);
        self.store(Mem::W64, header, 16, cap);
    }

    /// The capacity a list of `elem` made with room for `n` has: `n`, or
    /// everything for elements of size 0 (as `mem.grow` gives them).
    pub(super) fn cap_for(&mut self, n: Value, elem: Ty) -> Option<Value> {
        let (size, _) = self.size_align(elem)?;
        Some(if size == 0 { self.iconst(u64::MAX) } else { n })
    }

    /// `[a, b, c]`: an array, or a list where one is expected. Every
    /// element is worked out before any is put in place.
    pub(super) fn array_lit(&mut self, e: &'a hir::Expr, items: &'a [hir::Expr]) -> Option<Val> {
        let span = e.span;
        let ty = self.ty_of(e);
        let Some(seq) = self.seq_of(ty) else { return self.unsupported(span, "this kind of `[…]`") };
        let elem = self.tcx().degauge(seq.elem());
        let mut vals = Vec::with_capacity(items.len());
        for item in items {
            let v = self.expr(item)?;
            self.moved_away(item);
            vals.push(v);
        }
        let (size, _) = self.size_align(elem)?;
        match seq {
            Seq::Array(..) => {
                let ptr = self.temp(ty, span)?;
                for (i, v) in vals.into_iter().enumerate() {
                    self.move_to(ptr, (i as u64 * size) as i64, v, elem, span)?;
                }
                self.own_temp(ptr, ty);
                Some(Val::Mem(ptr))
            }
            Seq::List(_) => {
                let n = self.iconst(vals.len() as u64);
                let buf = self.alloc_elems(n, elem)?;
                for (i, v) in vals.into_iter().enumerate() {
                    self.move_to(buf, (i as u64 * size) as i64, v, elem, span)?;
                }
                let header = self.temp(ty, span)?;
                let cap = self.cap_for(n, elem)?;
                self.set_header(header, buf, n, cap);
                self.own_temp(header, ty);
                Some(Val::Mem(header))
            }
            Seq::Slice(_) => self.unsupported(span, "a slice literal"),
        }
    }

    /// `[x; n]`: `n` copies of `x` (the last one `x` itself).
    pub(super) fn array_repeat(&mut self, e: &'a hir::Expr, value: &'a hir::Expr, count: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        let ty = self.ty_of(e);
        let Some(seq) = self.seq_of(ty) else { return self.unsupported(span, "this kind of `[x; n]`") };
        let elem = self.tcx().degauge(seq.elem());
        let v = self.expr(value)?;
        self.moved_away(value);
        match seq {
            // An array's length is its type's; the count isn't worked out.
            Seq::Array(_, n) => {
                let ptr = self.temp(ty, span)?;
                let n = self.iconst(n);
                self.fill_elems(ptr, n, v, elem, span)?;
                self.own_temp(ptr, ty);
                Some(Val::Mem(ptr))
            }
            Seq::List(_) => {
                let n = self.value(count)?[0];
                let header = self.temp(ty, span)?;
                self.new_list_of(header, n, v, elem, span)?;
                self.own_temp(header, ty);
                Some(Val::Mem(header))
            }
            Seq::Slice(_) => self.unsupported(span, "a slice literal"),
        }
    }

    /// Makes a list of `n` copies of `v` at `header` (`List.filled`).
    pub(super) fn new_list_of(&mut self, header: Value, n: Value, v: Val, elem: Ty, span: Span) -> Option<()> {
        let buf = self.alloc_elems(n, elem)?;
        self.fill_elems(buf, n, v, elem, span)?;
        let cap = self.cap_for(n, elem)?;
        self.set_header(header, buf, n, cap);
        Some(())
    }

    /// Puts `n` copies of `v` in the empty elements from `buf`: copies of
    /// it, and then `v` itself, moved.
    pub(super) fn fill_elems(&mut self, buf: Value, n: Value, v: Val, elem: Ty, span: Span) -> Option<()> {
        let zero = self.iconst(0);
        if !self.needs_drop(elem) {
            return self.count_loop(zero, n, |this, i| {
                let at = this.elem_at(buf, i, elem)?;
                this.store_val(at, 0, elem, &v, span)
            });
        }
        let none = self.ucmp(Cmp::Eq, n, zero);
        let (some, done) = (self.new_block(), self.new_block());
        self.br(none, done, some);
        self.enter(some);
        let one = self.iconst(1);
        let last = self.uarith(IntOp::Sub, n, one);
        self.count_loop(zero, last, |this, i| {
            let at = this.elem_at(buf, i, elem)?;
            this.copy_into(at, &v, elem, span)
        })?;
        let at = self.elem_at(buf, last, elem)?;
        self.move_to(at, 0, v, elem, span)?;
        self.jump(done);
        self.enter(done);
        Some(())
    }

    /// `List()` and `String()`: empty, with no buffer.
    pub(super) fn new_empty(&mut self, e: &'a hir::Expr) -> Option<Val> {
        let span = e.span;
        let ty = self.ty_of(e);
        let is_list = matches!(self.seq_of(ty), Some(Seq::List(_)));
        let is_coll = self.coll_of_ty(ty);
        if !is_list && !is_coll && !self.is_string(ty) {
            return self.unsupported(span, "this kind of `T()`");
        }
        let header = self.temp(ty, span)?;
        let size = if is_coll { 40 } else { 24 };
        self.emit(InstKind::Zero { dst: header, size }, &[]);
        self.own_temp(header, ty);
        Some(Val::Mem(header))
    }

    /// A view of all of `v`, an array or a list of type `from`.
    pub(super) fn slice_of(&mut self, v: Val, from: Ty, span: Span) -> Option<Val> {
        let (buf, len) = self.view_of(&v, from, span)?;
        Some(Val::Regs(vec![buf, len]))
    }

    // ------------------------------------------------------------ indexing

    /// `xs[i]`, or `xs[a..b]`: an element (a view of it, for one in
    /// memory), or a slice.
    pub(super) fn index(&mut self, e: &'a hir::Expr, base: &'a hir::Expr, args: &'a [GenericArg]) -> Option<Val> {
        let span = e.span;
        let [GenericArg::Expr(index)] = args else { return self.unsupported(span, "this kind of indexing") };
        let bty = self.coerced_ty(base);
        if self.coll_of(bty).is_some() {
            let (place, vt) = self.map_index_place(base, index, span)?;
            return self.read_place(&place, vt, span);
        }
        if let ExprKind::Range { lo, hi, inclusive } = &index.kind
            && (matches!(self.tcx().kind(bty), TyKind::Str) || self.is_string(bty))
        {
            return self.substr(base, lo.as_deref(), hi.as_deref(), *inclusive, span);
        }
        let Some(seq) = self.seq_of(bty) else {
            return self.unsupported(span, "indexing this");
        };
        let (place, bty) = self.place_or_temp(base)?;
        if let ExprKind::Range { lo, hi, inclusive } = &index.kind {
            return self.subslice(&place, bty, lo.as_deref(), hi.as_deref(), *inclusive, span);
        }
        let i = self.value(index)?[0];
        let ity = self.coerced_ty(index);
        let at = self.checked_elem(&place, bty, seq, i, ity, span)?;
        self.load_val(at, 0, seq.elem(), span)
    }

    /// `xs[i]` as a place: the element's address.
    pub(super) fn index_place(&mut self, e: &'a hir::Expr) -> Option<(Place, Ty)> {
        let span = e.span;
        let ExprKind::Index { base, args } = &e.kind else { return self.internal(span, "an index that isn't one") };
        let [GenericArg::Expr(index)] = args.as_slice() else {
            return self.unsupported(span, "this kind of indexing");
        };
        if self.coll_of(self.ty_of(base)).is_some() {
            return self.map_index_place(base, index, span);
        }
        let (place, bty) = self.place_or_temp(base)?;
        let Some(seq) = self.seq_of(bty) else { return self.unsupported(span, "assigning to this") };
        let i = self.value(index)?[0];
        let ity = self.coerced_ty(index);
        let at = self.checked_elem(&place, bty, seq, i, ity, span)?;
        Some((Place::At { ptr: at, off: 0 }, self.tcx().degauge(seq.elem())))
    }

    /// Whether `e` is `xs[i]` of a sequence that's a place.
    pub(super) fn is_index_place(&self, e: &hir::Expr) -> bool {
        let ExprKind::Index { base, args } = &e.kind else { return false };
        let [GenericArg::Expr(index)] = args.as_slice() else { return false };
        !matches!(index.kind, ExprKind::Range { .. })
            && (self.seq_of(self.ty_of(base)).is_some() || self.coll_of_ty(self.ty_of(base)))
            && self.is_place(base)
    }

    /// The address of element `i` of the sequence in `place`, checked
    /// against its length (unless `i`'s type `ity` can't reach past an
    /// array's end).
    fn checked_elem(&mut self, place: &Place, ty: Ty, seq: Seq, i: Value, ity: Ty, span: Span) -> Option<Value> {
        let v = self.read_place(place, ty, span)?;
        let (buf, len) = self.view_of(&v, ty, span)?;
        let fits = match seq {
            Seq::Array(_, n) => self.int_of(ity).is_some_and(|t| !t.signed && (t.max() as u128) < n as u128),
            _ => false,
        };
        if !fits {
            self.emit(InstKind::Bounds { index: i, len }, &[]);
        }
        self.elem_at(buf, i, seq.elem())
    }

    /// `xs[lo..hi]`: a view of part of the sequence in `place`.
    fn subslice(
        &mut self,
        place: &Place,
        ty: Ty,
        lo: Option<&'a hir::Expr>,
        hi: Option<&'a hir::Expr>,
        inclusive: bool,
        span: Span,
    ) -> Option<Val> {
        let start = match lo {
            Some(lo) => self.value(lo)?[0],
            None => self.iconst(0),
        };
        let end = match hi {
            Some(hi) => Some(self.value(hi)?[0]),
            None => None,
        };
        let v = self.read_place(place, ty, span)?;
        let (buf, len) = self.view_of(&v, ty, span)?;
        let seq = self.seq_of(ty)?;
        let (start, end) = self.range_in(start, end, inclusive, len, span)?;
        let at = self.elem_at(buf, start, seq.elem())?;
        let n = self.uarith(IntOp::Sub, end, start);
        Some(Val::Regs(vec![at, n]))
    }

    /// `s[lo..hi]`: the text between two byte positions, each of which must
    /// fall between characters.
    fn substr(
        &mut self,
        base: &'a hir::Expr,
        lo: Option<&'a hir::Expr>,
        hi: Option<&'a hir::Expr>,
        inclusive: bool,
        span: Span,
    ) -> Option<Val> {
        let v = self.expr(base)?;
        let ty = self.coerced_ty(base);
        let start = match lo {
            Some(lo) => self.value(lo)?[0],
            None => self.iconst(0),
        };
        let end = match hi {
            Some(hi) => Some(self.value(hi)?[0]),
            None => None,
        };
        let (p, len) = self.text_of(v, ty, span)?;
        let (start, end) = self.range_in(start, end, inclusive, len, span)?;
        for at in [start, end] {
            let inside = self.ucmp(Cmp::Lt, at, len);
            let (check, ok) = (self.new_block(), self.new_block());
            self.br(inside, check, ok);
            self.enter(check);
            let at_p = self.emit1(InstKind::Index { base: p, index: at, scale: 1 }, VTy::Ptr);
            let b = self.load(Mem::U8, at_p, 0, VTy::Int);
            let mask = self.iconst(0xC0);
            let top = self.emit1(InstKind::Bit { op: BitOp::And, a: b, b: mask }, VTy::Int);
            let cont = self.iconst(0x80);
            let mid = self.ucmp(Cmp::Eq, top, cont);
            let fail = self.new_block();
            self.br(mid, fail, ok);
            self.enter(fail);
            self.unravel_msg(&[Msg::Text("byte "), Msg::Int(at), Msg::Text(" is inside a character")], span)?;
            self.enter(ok);
        }
        let from = self.emit1(InstKind::Index { base: p, index: start, scale: 1 }, VTy::Ptr);
        let n = self.uarith(IntOp::Sub, end, start);
        Some(Val::Regs(vec![from, n]))
    }

    /// The bounds `start..end` (or `..=end`, or `start..` for no `end`) of
    /// something `len` long, as a start and an exclusive end; unravels
    /// unless `start <= end <= len`.
    pub(super) fn range_in(
        &mut self,
        start: Value,
        end: Option<Value>,
        inclusive: bool,
        len: Value,
        span: Span,
    ) -> Option<(Value, Value)> {
        let end = match end {
            Some(end) if inclusive => {
                let one = self.iconst(1);
                self.emit1(InstKind::Arith { op: IntOp::Add, policy: Policy::Trap, ty: U64, a: end, b: one }, VTy::Int)
            }
            Some(end) => end,
            None => len,
        };
        let backwards = self.ucmp(Cmp::Gt, start, end);
        let past = self.ucmp(Cmp::Gt, end, len);
        let bad = self.emit1(InstKind::Bit { op: BitOp::Or, a: backwards, b: past }, VTy::Int);
        let (fail, ok) = (self.new_block(), self.new_block());
        self.br(bad, fail, ok);
        self.enter(fail);
        self.unravel_msg(
            &[
                Msg::Text("range "),
                Msg::Int(start),
                Msg::Text(".."),
                Msg::Int(end),
                Msg::Text(" is out of bounds (len "),
                Msg::Int(len),
                Msg::Text(")"),
            ],
            span,
        )?;
        self.enter(ok);
        Some((start, end))
    }

    // ------------------------------------------------------------ loops

    /// `for x in xs` over an array, a list or a slice. `for mut x in xs`
    /// makes `x` the element itself.
    pub(super) fn for_seq(
        &mut self,
        e: &'a hir::Expr,
        pat: &'a hir::Pat,
        iter: &'a hir::Expr,
        body: &'a Block,
        seq: Seq,
    ) -> Option<()> {
        let span = iter.span;
        let elem = self.tcx().degauge(seq.elem());
        let (place, ty) = self.place_or_temp(iter)?;
        let v = self.read_place(&place, ty, span)?;
        let (buf, len) = self.view_of(&v, ty, span)?;
        // Only a call can reach the list from inside the body.
        let header = match (seq, &v) {
            (Seq::List(_), &Val::Mem(header)) if block_calls(body) => Some(header),
            _ => None,
        };
        let alias = match &pat.kind {
            PatKind::Bind(l) if self.p.local(*l).mutable => Some(*l),
            _ => None,
        };
        let i = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(i, zero);
        let (head, body_b, step, exit) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let more = self.ucmp(Cmp::Lt, cur, len);
        self.br(more, body_b, exit);
        self.enter(body_b);
        if let Some(header) = header {
            self.same_list(header, buf, len, span)?;
        }
        let scopes = self.f.scopes.len();
        self.f.loops.push(LoopCtx { id: e.id, brk: exit, cont: step, result: LoopResult::None, scopes });
        let cur = self.read(i);
        let at = self.elem_at(buf, cur, elem)?;
        let depth = self.open_scope();
        let mut done = match alias {
            Some(l) => {
                self.bind_ref(l, at);
                Some(())
            }
            None => {
                let item = self.load_val(at, 0, elem, span)?;
                self.bind_with(pat, &item, elem, super::pat::Binding::View)
            }
        };
        if done.is_some() {
            let join = self.join(self.tcx().unit, e.span)?;
            done = self.block_into(body, &join);
        }
        self.close_scope(depth);
        if self.live() && done.is_some() {
            self.jump(step);
        }
        self.f.loops.pop();
        self.enter(step);
        if self.live() {
            let cur = self.read(i);
            let one = self.iconst(1);
            let next = self.uarith(IntOp::Add, cur, one);
            self.write(i, next);
            self.jump(head);
        }
        self.seal(head);
        self.enter(exit);
        Some(())
    }

    /// Unravels unless the list at `header` still has the buffer `buf`
    /// and the length `len` a loop over it started with.
    fn same_list(&mut self, header: Value, buf: Value, len: Value, span: Span) -> Option<()> {
        let (now_buf, now_len) = self.list_view(header);
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
        Some(())
    }
}

/// Whether running `b` can call anything.
fn block_calls(b: &Block) -> bool {
    b.stmts.iter().any(stmt_calls)
}

fn stmt_calls(s: &hir::Stmt) -> bool {
    match &s.kind {
        hir::StmtKind::Let { init: e, .. }
        | hir::StmtKind::Given { value: e, .. }
        | hir::StmtKind::Spool { init: e, .. }
        | hir::StmtKind::Defer { body: e, .. }
        | hir::StmtKind::Expr(e) => calls(e),
        hir::StmtKind::Item(_) => false,
    }
}

pub(super) fn calls(e: &hir::Expr) -> bool {
    matches!(e.kind, ExprKind::Call { .. })
        || super::scan::children(e).into_iter().any(|n| match n {
            super::scan::Node::Expr(x) => calls(x),
            super::scan::Node::Stmt(s) => stmt_calls(s),
            super::scan::Node::Pat(_) => false,
        })
}
