//! The methods std leaves to the runtime (`#builtin`), written by the
//! builder as Heddle, one function per instance (design/vm.md D9): the
//! methods of lists and slices.
//!
//! They have no locations, so an unravel in one is reported where it was
//! called from, as `error(message, 3)` does in the Lua runtime.

use loom_syntax::source::Span;

use super::text::Msg;
use super::value::Val;
use super::{Builder, ParamAbi};
use crate::heddle::{Cmp, InstKind, IntOp, Mem, Term, VTy, Value};
use crate::ty::Ty;
use crate::typeck::BodyId;

/// The `#builtin` methods whose bodies the builder writes.
const BODIES: &[&str] = &[
    "std.list.List.filled",
    "std.list.List.push",
    "std.list.List.pop",
    "std.list.List.insert",
    "std.list.List.remove",
    "std.list.List.clear",
    "std.list.List.truncate",
    "std.list.List.extend",
    "std.list.List.copy",
    "std.slice.Slice.len",
    "std.slice.Slice.is_empty",
    "std.slice.Slice.get",
    "std.slice.Slice.first",
    "std.slice.Slice.last",
    "std.slice.Slice.contains",
    "std.slice.Slice.index_of",
    "std.slice.Slice.copy_from",
    "std.slice.Slice.fill",
    "std.slice.Slice.swap",
    "std.slice.Slice.reverse",
    "std.slice.Slice.sort",
    "std.slice.Slice.to_list",
    "std.slice.Slice.map",
    "std.slice.Slice.filter",
];

/// Whether the builder writes the body of the `#builtin` method `path`.
pub(super) fn writes_body(path: &str) -> bool {
    BODIES.contains(&path)
        || super::methods::writes_body(path)
        || super::stream::writes_body(path)
        || super::shuttle::writes_body(path)
        || super::thread::writes_body(path)
        || path.starts_with("std.map.Map.")
        || path.starts_with("std.set.Set.")
        || matches!(path, "std.fs.File.append" | "std.fs.File.write_line")
}

impl<'a> Builder<'a> {
    /// The body of `#builtin` method instance `id`.
    pub(super) fn builtin_body(&mut self, id: BodyId, span: Span) -> Option<Vec<VTy>> {
        self.f.span = None;
        let path = self.p.def_path(self.typed.results.instance(id).def);
        if super::methods::writes_body(&path) {
            return self.method_body(id, &path, span);
        }
        if let Some(name) = path.strip_prefix("std.fs.File.") {
            return self.file_method(id, name, span);
        }
        let sig = self.typed.results.sigs.get(&id)?.clone();
        let tys: Vec<Ty> = sig.params.iter().map(|p| self.plain_ty(p.ty)).collect();
        let ret = self.plain_ty(sig.ret);
        let abi = self.body_abi(id, span)?;
        let (sret, _, params) = self.entry_params(&abi);
        self.f.ret = Some(ret);
        self.f.sret = sret;
        let args: Vec<Val> = abi
            .params
            .iter()
            .zip(params)
            .map(|(p, vals)| match p {
                ParamAbi::Regs(_) => Val::Regs(vals),
                ParamAbi::Addr => Val::Mem(vals[0]),
            })
            .collect();
        if super::stream::writes_body(&path) {
            self.stream_method(&path, &args, &tys, ret, sret, span)?;
            return Some(abi.results);
        }
        if super::shuttle::writes_body(&path) {
            self.shuttle_method(&path, &args, &tys, sret, span)?;
            return Some(abi.results);
        }
        if super::thread::writes_body(&path) {
            self.spun_join(&args[0], tys[0], ret, sret, span)?;
            return Some(abi.results);
        }
        if path.starts_with("std.map.Map.") || path.starts_with("std.set.Set.") {
            let name = path.rsplit('.').next().unwrap_or_default();
            self.coll_method(name, &args, &tys, ret, span)?;
            return Some(abi.results);
        }
        // The element type: of `self`, or of the list `filled` makes.
        let seq_ty = if path == "std.list.List.filled" { ret } else { tys[0] };
        let Some(seq) = self.seq_of(seq_ty) else {
            return self.internal(span, "a method of something that isn't a sequence");
        };
        let elem = self.tcx().degauge(seq.elem());
        let (size, align) = self.size_align(elem)?;
        let m = Method { elem, size, align, ret, span };
        let name = path.rsplit('.').next().unwrap_or_default();
        if name == "filled" {
            let n = self.regs(args[1].clone(), self.tcx().usize, span)?[0];
            return self.list_filled(&m, &args[0], n).map(|()| abi.results);
        }
        let (buf, len) = self.view_of(&args[0], tys[0], span)?;
        let header = match args[0] {
            Val::Mem(h) => h,
            Val::Regs(_) => buf,
        };
        let int = |this: &mut Self, i: usize| -> Option<Value> {
            let v = this.regs(args[i].clone(), this.tcx().usize, span)?;
            Some(v[0])
        };
        match name {
            "len" => self.ret_regs(vec![len]),
            "is_empty" => {
                let zero = self.iconst(0);
                let empty = self.ucmp(Cmp::Eq, len, zero);
                self.ret_regs(vec![empty])
            }
            "get" => {
                let i = int(self, 1)?;
                self.copy_of_elem(&m, buf, len, i)?
            }
            "first" => {
                let zero = self.iconst(0);
                self.copy_of_elem(&m, buf, len, zero)?
            }
            "last" => {
                // Past the end when it's empty (`len - 1` wraps).
                let one = self.iconst(1);
                let i = self.uarith(IntOp::Sub, len, one);
                self.copy_of_elem(&m, buf, len, i)?
            }
            "index_of" | "contains" => self.find(&m, buf, len, &args[1], name == "contains")?,
            "copy_from" => self.copy_from(&m, buf, len, &args[1], tys[1])?,
            "fill" => {
                let zero = self.iconst(0);
                self.count_loop(zero, len, |this, i| {
                    let at = this.elem_at(buf, i, elem)?;
                    this.drop_at(at, elem);
                    this.copy_into(at, &args[1], elem, span)
                })?;
                self.ret_regs(Vec::new())
            }
            "swap" => {
                let (i, j) = (int(self, 1)?, int(self, 2)?);
                self.emit(InstKind::Bounds { index: i, len }, &[]);
                self.emit(InstKind::Bounds { index: j, len }, &[]);
                let (a, b) = (self.elem_at(buf, i, elem)?, self.elem_at(buf, j, elem)?);
                self.swap_bytes(&m, a, b);
                self.ret_regs(Vec::new())
            }
            "reverse" => {
                let zero = self.iconst(0);
                let one = self.iconst(1);
                let two = self.iconst(2);
                let half = self.emit1(InstKind::Div { ty: super::seq::U64, a: len, b: two }, VTy::Int);
                let last = self.uarith(IntOp::Sub, len, one);
                self.count_loop(zero, half, |this, i| {
                    let j = this.uarith(IntOp::Sub, last, i);
                    let (a, b) = (this.elem_at(buf, i, elem)?, this.elem_at(buf, j, elem)?);
                    this.swap_bytes(&m, a, b);
                    Some(())
                })?;
                self.ret_regs(Vec::new())
            }
            "sort" => {
                self.merge_sort(&m, buf, len)?;
                self.ret_regs(Vec::new())
            }
            "to_list" | "copy" => {
                let dst = sret?;
                let new = self.alloc_elems(len, elem)?;
                self.copy_elems(&m, new, buf, len)?;
                let cap = self.cap_for(len, elem)?;
                self.set_header(dst, new, len, cap);
                self.ret_regs(Vec::new())
            }
            "map" => {
                let dst = sret?;
                let fty = tys[1];
                let f = self.regs(args[1].clone(), fty, span)?;
                let Some(crate::typeck::Seq::List(out)) = self.seq_of(ret) else { return None };
                let out = self.tcx().degauge(out);
                let new = self.alloc_elems(len, out)?;
                // The new list owns what's made so far, should `f` unravel.
                let zero = self.iconst(0);
                let cap = self.cap_for(len, out)?;
                self.set_header(dst, new, zero, cap);
                self.count_loop(zero, len, |this, i| {
                    let at = this.elem_at(buf, i, elem)?;
                    let x = this.load_val(at, 0, elem, span)?;
                    let y = this.call_fn(&f, fty, vec![x], span)?;
                    let to = this.elem_at(new, i, out)?;
                    this.move_to(to, 0, y, out, span)?;
                    let one = this.iconst(1);
                    let n = this.uarith(IntOp::Add, i, one);
                    this.store(Mem::W64, dst, 8, n);
                    Some(())
                })?;
                self.ret_regs(Vec::new())
            }
            "filter" => {
                let dst = sret?;
                let fty = tys[1];
                let f = self.regs(args[1].clone(), fty, span)?;
                self.empty_header(dst, ret)?;
                let zero = self.iconst(0);
                self.count_loop(zero, len, |this, i| {
                    let at = this.elem_at(buf, i, elem)?;
                    let x = this.load_val(at, 0, elem, span)?;
                    let keep = this.call_fn(&f, fty, vec![x.clone()], span)?;
                    let keep = this.regs(keep, this.tcx().bool, span)?[0];
                    let (yes, no) = (this.new_block(), this.new_block());
                    this.br(keep, yes, no);
                    this.enter(yes);
                    let (_, n) = this.list_view(dst);
                    let one = this.iconst(1);
                    let want = this.uarith(IntOp::Add, n, one);
                    this.grow(&m, dst, want);
                    let (nbuf, _) = this.list_view(dst);
                    let to = this.elem_at(nbuf, n, elem)?;
                    this.copy_into(to, &x, elem, span)?;
                    this.store(Mem::W64, dst, 8, want);
                    this.jump(no);
                    this.enter(no);
                    Some(())
                })?;
                self.ret_regs(Vec::new())
            }
            "push" => {
                let one = self.iconst(1);
                let want = self.uarith(IntOp::Add, len, one);
                self.grow(&m, header, want);
                let (buf, _) = self.list_view(header);
                let at = self.elem_at(buf, len, elem)?;
                self.store_val(at, 0, elem, &args[1], span)?;
                self.store(Mem::W64, header, 8, want);
                self.ret_regs(Vec::new())
            }
            "pop" => {
                let zero = self.iconst(0);
                let empty = self.ucmp(Cmp::Eq, len, zero);
                let (none, some) = (self.new_block(), self.new_block());
                self.br(empty, none, some);
                self.enter(none);
                let v = self.none(ret, span)?;
                self.ret_val(v, span)?;
                self.enter(some);
                let one = self.iconst(1);
                let last = self.uarith(IntOp::Sub, len, one);
                self.store(Mem::W64, header, 8, last);
                let at = self.elem_at(buf, last, elem)?;
                let x = self.take_elem(&m, at)?;
                let v = self.some(ret, elem, x, span)?;
                self.ret_val(v, span)?;
            }
            "insert" => {
                let i = int(self, 1)?;
                let past = self.ucmp(Cmp::Gt, i, len);
                self.unravel_if(
                    past,
                    &[
                        Msg::Text("index "),
                        Msg::Int(i),
                        Msg::Text(" is out of bounds (len "),
                        Msg::Int(len),
                        Msg::Text(")"),
                    ],
                )?;
                let one = self.iconst(1);
                let want = self.uarith(IntOp::Add, len, one);
                self.grow(&m, header, want);
                let (buf, _) = self.list_view(header);
                let at = self.elem_at(buf, i, elem)?;
                let next = self.uarith(IntOp::Add, i, one);
                let to = self.elem_at(buf, next, elem)?;
                let rest = self.uarith(IntOp::Sub, len, i);
                self.move_elems(&m, to, at, rest);
                self.store_val(at, 0, elem, &args[2], span)?;
                self.store(Mem::W64, header, 8, want);
                self.ret_regs(Vec::new())
            }
            "remove" => {
                let i = int(self, 1)?;
                self.emit(InstKind::Bounds { index: i, len }, &[]);
                let at = self.elem_at(buf, i, elem)?;
                let x = self.take_elem(&m, at)?;
                let one = self.iconst(1);
                let next = self.uarith(IntOp::Add, i, one);
                let from = self.elem_at(buf, next, elem)?;
                let last = self.uarith(IntOp::Sub, len, one);
                let rest = self.uarith(IntOp::Sub, last, i);
                self.move_elems(&m, at, from, rest);
                self.store(Mem::W64, header, 8, last);
                self.ret_val(x, span)?;
            }
            "clear" => {
                self.drop_elems(buf, len, elem, size);
                let zero = self.iconst(0);
                self.store(Mem::W64, header, 8, zero);
                self.ret_regs(Vec::new())
            }
            "truncate" => {
                let k = int(self, 1)?;
                let shorter = self.ucmp(Cmp::Lt, k, len);
                let (yes, done) = (self.new_block(), self.new_block());
                self.br(shorter, yes, done);
                self.enter(yes);
                let from = self.elem_at(buf, k, elem)?;
                let n = self.uarith(IntOp::Sub, len, k);
                self.drop_elems(from, n, elem, size);
                self.store(Mem::W64, header, 8, k);
                self.jump(done);
                self.enter(done);
                self.ret_regs(Vec::new())
            }
            "extend" => {
                // Copies first, then room: the slice may be of this list.
                let (sbuf, n) = self.view_of(&args[1], tys[1], span)?;
                let copies = self.alloc_elems(n, elem)?;
                self.copy_elems(&m, copies, sbuf, n)?;
                let want = self.uarith(IntOp::Add, len, n);
                self.grow(&m, header, want);
                let (buf, _) = self.list_view(header);
                let at = self.elem_at(buf, len, elem)?;
                self.move_elems(&m, at, copies, n);
                self.free_elems(&m, copies, n);
                self.store(Mem::W64, header, 8, want);
                self.ret_regs(Vec::new())
            }
            _ => return self.internal(span, &format!("no body for `{path}`")),
        }
        Some(abi.results)
    }

    /// Sorts the `n` elements of `elem` from `buf`, keeping equal ones in
    /// order.
    pub(super) fn sort_list(&mut self, buf: Value, n: Value, elem: Ty, span: Span) -> Option<()> {
        let (size, align) = self.size_align(elem)?;
        let m = Method { elem, size, align, ret: self.tcx().unit, span };
        self.merge_sort(&m, buf, n)
    }

    /// Returns `vals`.
    fn ret_regs(&mut self, vals: Vec<Value>) {
        self.terminate(Term::Ret(vals));
    }

    /// Unravels with `msg` if `cond`.
    fn unravel_if(&mut self, cond: Value, msg: &[Msg]) -> Option<()> {
        let (fail, ok) = (self.new_block(), self.new_block());
        self.br(cond, fail, ok);
        self.enter(fail);
        let span = self.here();
        self.unravel_msg(msg, span)?;
        self.enter(ok);
        Some(())
    }

    /// `List.filled(x, n)`: `n` copies of `x`.
    fn list_filled(&mut self, m: &Method, x: &Val, n: Value) -> Option<()> {
        let dst = self.f.sret?;
        let buf = self.alloc_elems(n, m.elem)?;
        let zero = self.iconst(0);
        self.count_loop(zero, n, |this, i| {
            let at = this.elem_at(buf, i, m.elem)?;
            this.copy_into(at, x, m.elem, m.span)
        })?;
        let cap = self.cap_for(n, m.elem)?;
        self.set_header(dst, buf, n, cap);
        self.ret_regs(Vec::new());
        Some(())
    }

    /// Returns a copy of element `i`, as `some`, or `none` past the end.
    fn copy_of_elem(&mut self, m: &Method, buf: Value, len: Value, i: Value) -> Option<()> {
        let inside = self.ucmp(Cmp::Lt, i, len);
        let (yes, no) = (self.new_block(), self.new_block());
        self.br(inside, yes, no);
        self.enter(yes);
        let at = self.elem_at(buf, i, m.elem)?;
        let x = self.load_val(at, 0, m.elem, m.span)?;
        let x = self.copy_val(&x, m.elem, m.span)?;
        let v = self.some(m.ret, m.elem, x, m.span)?;
        self.ret_val(v, m.span)?;
        self.enter(no);
        let v = self.none(m.ret, m.span)?;
        self.ret_val(v, m.span)
    }

    /// `index_of(x)` (or `contains(x)`): the first element equal to `x`.
    fn find(&mut self, m: &Method, buf: Value, len: Value, x: &Val, contains: bool) -> Option<()> {
        let zero = self.iconst(0);
        self.count_loop(zero, len, |this, i| {
            let at = this.elem_at(buf, i, m.elem)?;
            let y = this.load_val(at, 0, m.elem, m.span)?;
            let eq = this.eq_vals(y, x.clone(), m.elem, m.span)?;
            let (found, next) = (this.new_block(), this.new_block());
            this.br(eq, found, next);
            this.enter(found);
            if contains {
                let yes = this.iconst(1);
                this.ret_regs(vec![yes]);
            } else {
                let usize = this.tcx().usize;
                let v = this.some(m.ret, usize, Val::Regs(vec![i]), m.span)?;
                this.ret_val(v, m.span)?;
            }
            this.enter(next);
            Some(())
        })?;
        if contains {
            let no = self.iconst(0);
            self.ret_regs(vec![no]);
            return Some(());
        }
        let v = self.none(m.ret, m.span)?;
        self.ret_val(v, m.span)
    }

    /// `copy_from(src)`: copies of `src`'s elements over the first ones.
    fn copy_from(&mut self, m: &Method, buf: Value, len: Value, src: &Val, src_ty: Ty) -> Option<()> {
        let (sbuf, n) = self.view_of(src, src_ty, m.span)?;
        let longer = self.ucmp(Cmp::Gt, n, len);
        self.unravel_if(
            longer,
            &[Msg::Text("can't copy "), Msg::Int(n), Msg::Text(" elements into a slice of "), Msg::Int(len)],
        )?;
        if !self.needs_drop(m.elem) {
            self.move_elems(m, buf, sbuf, n);
        } else {
            // Through a buffer of copies, in case the two overlap.
            let copies = self.alloc_elems(n, m.elem)?;
            self.copy_elems(m, copies, sbuf, n)?;
            self.drop_elems(buf, n, m.elem, m.size);
            self.move_elems(m, buf, copies, n);
            self.free_elems(m, copies, n);
        }
        self.ret_regs(Vec::new());
        Some(())
    }

    /// Writes copies of the `n` elements from `src` to the empty ones
    /// from `dst`.
    fn copy_elems(&mut self, m: &Method, dst: Value, src: Value, n: Value) -> Option<()> {
        if !self.needs_drop(m.elem) {
            self.move_elems(m, dst, src, n);
            return Some(());
        }
        let zero = self.iconst(0);
        self.count_loop(zero, n, |this, i| {
            let (d, s) = (this.elem_at(dst, i, m.elem)?, this.elem_at(src, i, m.elem)?);
            this.copy_into(d, &Val::Mem(s), m.elem, m.span)
        })
    }

    /// Moves the bytes of `n` elements from `src` to `dst`, which may
    /// overlap.
    fn move_elems(&mut self, m: &Method, dst: Value, src: Value, n: Value) {
        if m.size == 0 {
            return;
        }
        let scale = self.iconst(m.size);
        let bytes = self.uarith(IntOp::Mul, n, scale);
        self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![dst, src, bytes] }, &[]);
    }

    /// Frees a buffer of `n` elements `alloc_elems` made.
    fn free_elems(&mut self, m: &Method, buf: Value, n: Value) {
        if m.size == 0 {
            return;
        }
        let scale = self.iconst(m.size);
        let bytes = self.uarith(IntOp::Mul, n, scale);
        let zero = self.iconst(0);
        let some = self.ucmp(Cmp::Ne, bytes, zero);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(some, yes, done);
        self.enter(yes);
        let align = self.iconst(m.align);
        self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![buf, bytes, align] }, &[]);
        self.jump(done);
        self.enter(done);
    }

    /// Makes room in the list at `header` for `want` elements.
    fn grow(&mut self, m: &Method, header: Value, want: Value) {
        let size = self.iconst(m.size);
        let align = self.iconst(m.align);
        self.emit(InstKind::Native { name: "mem.grow", imm: 0, args: vec![header, size, align, want] }, &[]);
    }

    /// The element at `at`, taken out: its parts, or its bytes in a
    /// temporary of its own.
    fn take_elem(&mut self, m: &Method, at: Value) -> Option<Val> {
        match self.load_val(at, 0, m.elem, m.span)? {
            Val::Mem(src) => {
                let dst = self.temp(m.elem, m.span)?;
                if m.size > 0 {
                    self.emit(InstKind::Copy { dst, src, size: m.size }, &[]);
                }
                self.own_temp(dst, m.elem);
                Some(Val::Mem(dst))
            }
            v => Some(v),
        }
    }

    /// Swaps the bytes of the elements at `a` and `b`.
    fn swap_bytes(&mut self, m: &Method, a: Value, b: Value) {
        if m.size == 0 {
            return;
        }
        let slot = self.raw_slot(m.size, m.align);
        let tmp = self.slot_addr(slot);
        self.emit(InstKind::Copy { dst: tmp, src: a, size: m.size }, &[]);
        self.emit(InstKind::Copy { dst: a, src: b, size: m.size }, &[]);
        self.emit(InstKind::Copy { dst: b, src: tmp, size: m.size }, &[]);
    }

    /// `min(a, b)` of two `u64`s.
    pub(super) fn umin(&mut self, a: Value, b: Value) -> Value {
        let r = self.new_var(VTy::Int);
        self.write(r, a);
        let less = self.ucmp(Cmp::Lt, b, a);
        let (yes, done) = (self.new_block(), self.new_block());
        self.br(less, yes, done);
        self.enter(yes);
        self.write(r, b);
        self.jump(done);
        self.enter(done);
        self.read(r)
    }

    /// Sorts the `n` elements from `buf`, keeping equal ones in order: a
    /// bottom-up merge sort, over copies of their bytes, so an unravel in a
    /// comparison leaves the slice as it was.
    fn merge_sort(&mut self, m: &Method, buf: Value, n: Value) -> Option<()> {
        let two = self.iconst(2);
        let big = self.ucmp(Cmp::Ge, n, two);
        let (sort, done) = (self.new_block(), self.new_block());
        self.br(big, sort, done);
        self.enter(sort);
        let first = self.alloc_elems(n, m.elem)?;
        let second = self.alloc_elems(n, m.elem)?;
        self.move_elems(m, first, buf, n);
        let (from, to) = (self.new_var(VTy::Ptr), self.new_var(VTy::Ptr));
        self.write(from, first);
        self.write(to, second);
        let width = self.new_var(VTy::Int);
        let one = self.iconst(1);
        self.write(width, one);
        // while width < n
        let (pass_head, pass, sorted) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(pass_head);
        self.switch_to(pass_head);
        let w = self.read(width);
        let more = self.ucmp(Cmp::Lt, w, n);
        self.br(more, pass, sorted);
        self.enter(pass);
        let w = self.read(width);
        let w2 = self.uarith(IntOp::Mul, w, two);
        let (a, b) = (self.read(from), self.read(to));
        // for lo in (0..n).step_by(2 * width)
        let lo = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(lo, zero);
        let (run_head, run, runs_done) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(run_head);
        self.switch_to(run_head);
        let l = self.read(lo);
        let more = self.ucmp(Cmp::Lt, l, n);
        self.br(more, run, runs_done);
        self.enter(run);
        let l = self.read(lo);
        let lw = self.uarith(IntOp::Add, l, w);
        let mid = self.umin(lw, n);
        let lw2 = self.uarith(IntOp::Add, l, w2);
        let hi = self.umin(lw2, n);
        self.merge(m, a, b, l, mid, hi)?;
        let l = self.read(lo);
        let next = self.uarith(IntOp::Add, l, w2);
        self.write(lo, next);
        self.jump(run_head);
        self.seal(run_head);
        self.enter(runs_done);
        self.write(from, b);
        self.write(to, a);
        let w = self.read(width);
        let next = self.uarith(IntOp::Mul, w, two);
        self.write(width, next);
        self.jump(pass_head);
        self.seal(pass_head);
        self.enter(sorted);
        let result = self.read(from);
        self.move_elems(m, buf, result, n);
        self.free_elems(m, first, n);
        self.free_elems(m, second, n);
        self.jump(done);
        self.enter(done);
        Some(())
    }

    /// Merges the sorted runs `a[lo..mid]` and `a[mid..hi]` into `b[lo..hi]`,
    /// taking from the first run when two are equal.
    fn merge(&mut self, m: &Method, a: Value, b: Value, lo: Value, mid: Value, hi: Value) -> Option<()> {
        let (i, j, k) = (self.new_var(VTy::Int), self.new_var(VTy::Int), self.new_var(VTy::Int));
        self.write(i, lo);
        self.write(j, mid);
        self.write(k, lo);
        let (head, both, take_j, take_i, step, rest) = (
            self.new_block(),
            self.new_block(),
            self.new_block(),
            self.new_block(),
            self.new_block(),
            self.new_block(),
        );
        self.jump(head);
        self.switch_to(head);
        let (ci, cj) = (self.read(i), self.read(j));
        let left = self.ucmp(Cmp::Lt, ci, mid);
        let right = self.ucmp(Cmp::Lt, cj, hi);
        let go = self.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a: left, b: right }, VTy::Int);
        self.br(go, both, rest);
        self.enter(both);
        let (ci, cj) = (self.read(i), self.read(j));
        let (xi, xj) = (self.elem_at(a, ci, m.elem)?, self.elem_at(a, cj, m.elem)?);
        let (vi, vj) = (self.load_val(xi, 0, m.elem, m.span)?, self.load_val(xj, 0, m.elem, m.span)?);
        let before = self.compare_vals(loom_syntax::ast::BinaryOp::Lt, vj, vi, m.elem, m.span)?;
        self.br(before, take_j, take_i);
        for (block, var) in [(take_j, j), (take_i, i)] {
            self.enter(block);
            let (c, ck) = (self.read(var), self.read(k));
            let (src, dst) = (self.elem_at(a, c, m.elem)?, self.elem_at(b, ck, m.elem)?);
            if m.size > 0 {
                self.emit(InstKind::Copy { dst, src, size: m.size }, &[]);
            }
            let one = self.iconst(1);
            let next = self.uarith(IntOp::Add, c, one);
            self.write(var, next);
            self.jump(step);
        }
        self.enter(step);
        let ck = self.read(k);
        let one = self.iconst(1);
        let next = self.uarith(IntOp::Add, ck, one);
        self.write(k, next);
        self.jump(head);
        self.seal(head);
        self.enter(rest);
        // What's left of either run, after what's merged.
        let (ci, cj, ck) = (self.read(i), self.read(j), self.read(k));
        let left = self.uarith(IntOp::Sub, mid, ci);
        let (src, dst) = (self.elem_at(a, ci, m.elem)?, self.elem_at(b, ck, m.elem)?);
        self.move_elems(m, dst, src, left);
        let ck = self.uarith(IntOp::Add, ck, left);
        let right = self.uarith(IntOp::Sub, hi, cj);
        let (src, dst) = (self.elem_at(a, cj, m.elem)?, self.elem_at(b, ck, m.elem)?);
        self.move_elems(m, dst, src, right);
        Some(())
    }
}

/// What a method's body works with.
struct Method {
    elem: Ty,
    size: u64,
    align: u64,
    ret: Ty,
    span: Span,
}
