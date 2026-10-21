//! Glue: code for the values of a type, made once per type the program
//! needs it for (design/vm.md §5.4).
//!
//! Drop glue frees what a value owns: a list's elements and buffer, a
//! string's bytes, each field of a record or a tuple that owns something,
//! the payload of a sum's case, the value in a `some`. A value of all zero
//! bytes owns nothing (a zero capacity, a `none`, case 0 with a zero
//! payload), so dropping a moved-from place does nothing.

use loom_syntax::source::Span;

use super::text::Sink;
use super::value::Val;
use super::{Builder, Glue};
use crate::heddle::{Cmp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
use crate::ty::layout::Shape;
use crate::ty::{IntTy, Ty, TyKind};
use crate::typeck::BodyId;

const U64: IntTy = IntTy { signed: false, bits: 64, size: false };

impl<'a> Builder<'a> {
    /// Builds glue `g` for `ty`; its result types.
    pub(super) fn glue_func(&mut self, g: Glue, ty: Ty) -> Option<Vec<VTy>> {
        let span = self.here();
        match g {
            Glue::Drop => {
                let ptr = self.new_value(VTy::Ptr);
                self.f.blocks[0].params.push(ptr);
                self.drop_glue_body(ptr, ty, span)?;
                if self.live() {
                    self.terminate(Term::Ret(Vec::new()));
                }
                Some(Vec::new())
            }
            Glue::Copy => {
                let dst = self.new_value(VTy::Ptr);
                let src = self.new_value(VTy::Ptr);
                self.f.blocks[0].params.extend([dst, src]);
                self.copy_glue_body(dst, src, ty, span)?;
                if self.live() {
                    self.terminate(Term::Ret(Vec::new()));
                }
                Some(Vec::new())
            }
            Glue::Hash => self.hash_glue(ty, span),
            Glue::Eq | Glue::Cmp => {
                let a = self.glue_value_param(ty, span)?;
                let b = self.glue_value_param(ty, span)?;
                let r = if g == Glue::Eq {
                    self.eq_structure(a, b, ty, span)?
                } else {
                    self.cmp_structure(a, b, ty, span)?
                };
                self.terminate(Term::Ret(vec![r]));
                Some(vec![VTy::Int])
            }
            Glue::Show { debug } => {
                let v = self.glue_value_param(ty, span)?;
                let h = self.new_value(VTy::Int);
                self.f.blocks[0].params.push(h);
                self.show_structure(Sink(h), v, ty, debug, span)?;
                if self.live() {
                    self.terminate(Term::Ret(Vec::new()));
                }
                Some(Vec::new())
            }
        }
    }

    /// Drops the value of `ty` at `ptr`, in the glue being built.
    fn drop_glue_body(&mut self, ptr: Value, ty: Ty, span: Span) -> Option<()> {
        let ty = self.tcx().degauge(ty);
        match self.tcx().kind(ty).clone() {
            TyKind::Tuple(items) => {
                let offsets = self.field_offsets(ty, span)?;
                for (t, off) in items.into_iter().zip(offsets) {
                    self.drop_field(ptr, off, t);
                }
            }
            TyKind::Array(elem, n) => {
                let (size, _) = self.size_align(elem)?;
                let count = self.iconst(n);
                self.drop_elems(ptr, count, elem, size);
            }
            // A result or a union: the payload its tag says it holds.
            TyKind::Result(..) | TyKind::Union(_) => {
                let members = match self.tcx().kind(ty).clone() {
                    TyKind::Result(t, e) => vec![t, e],
                    TyKind::Union(ms) => ms,
                    _ => Vec::new(),
                };
                let Shape::Tagged { tag, payload } = self.layout(ty)?.shape else { return None };
                let t = self.load(super::warp::tag_mem(tag.bytes), ptr, tag.offset as i64, VTy::Int);
                let done = self.new_block();
                for (i, m) in members.into_iter().enumerate() {
                    if !self.needs_drop(m) {
                        continue;
                    }
                    let (yes, next) = (self.new_block(), self.new_block());
                    let k = self.iconst(i as u64);
                    let c = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: t, b: k }, VTy::Int);
                    self.br(c, yes, next);
                    self.enter(yes);
                    self.drop_field(ptr, payload, m);
                    self.jump(done);
                    self.enter(next);
                }
                self.jump(done);
                self.enter(done);
            }
            TyKind::Any(_) => self.drop_any(ptr),
            TyKind::Ptr(..) => self.drop_ptr(ptr, ty, span)?,
            TyKind::Optional(inner) => {
                let v = super::value::Val::Mem(ptr);
                let present = self.is_some(&v, ty, span)?;
                let (yes, done) = (self.new_block(), self.new_block());
                self.br(present, yes, done);
                self.enter(yes);
                self.drop_at(ptr, inner);
                self.jump(done);
                self.enter(done);
            }
            TyKind::Adt(def, args) => match self.p.def_path(def).as_str() {
                "std.string.String" | "std.fs.File" => self.free_buffer(ptr, 1, 1),
                "std.iter.Stream" => self.drop_stream(ptr),
                "std.iter.Shuttle" | "std.iter.Burst" => self.drop_shuttle(ptr),
                "std.sync.Pipe" => self.drop_pipe(ptr),
                // The value is after the lock word.
                "std.sync.Mutex" => self.drop_field(ptr, 8, args[0]),
                "std.map.Map" | "std.set.Set" => {
                    let c = self.coll_of(ty)?;
                    self.drop_coll(&c, ptr);
                }
                "std.list.List" => {
                    let elem = args[0];
                    let (size, align) = self.size_align(elem)?;
                    if self.needs_drop(elem) {
                        let buf = self.load(Mem::W64, ptr, 0, VTy::Ptr);
                        let len = self.load(Mem::W64, ptr, 8, VTy::Int);
                        self.drop_elems(buf, len, elem, size);
                    }
                    self.free_buffer(ptr, size, align);
                }
                path if self.is_builtin(def) => {
                    return self.unsupported(span, &format!("dropping a `{path}`"));
                }
                _ => {
                    let info = self.warp_info(ty)?;
                    let layout = self.layout(ty)?;
                    match layout.shape {
                        Shape::Fields(offsets) => {
                            for (f, off) in info.fields.iter().zip(offsets) {
                                self.drop_field(ptr, off, f.ty);
                            }
                        }
                        Shape::Sum { tag, shared, cases, .. } => {
                            for (f, off) in info.fields.iter().zip(shared) {
                                self.drop_field(ptr, off, f.ty);
                            }
                            let t = self.load(super::warp::tag_mem(tag.bytes), ptr, tag.offset as i64, VTy::Int);
                            let done = self.new_block();
                            for (ci, case) in info.cases.iter().enumerate() {
                                let tys: Vec<Ty> = case.fields.iter().flatten().map(|f| f.ty).collect();
                                if !tys.iter().any(|&t| self.needs_drop(t)) {
                                    continue;
                                }
                                let (yes, next) = (self.new_block(), self.new_block());
                                let k = self.iconst(ci as u64);
                                let c =
                                    self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: t, b: k }, VTy::Int);
                                self.br(c, yes, next);
                                self.enter(yes);
                                for (fty, &off) in tys.iter().zip(&cases[ci]) {
                                    self.drop_field(ptr, off, *fty);
                                }
                                self.jump(done);
                                self.enter(next);
                            }
                            self.jump(done);
                            self.enter(done);
                        }
                        _ => return self.internal(span, "dropping a warp laid out as something else"),
                    }
                }
            },
            _ => {
                let shown = self.tcx().display(self.p, ty);
                return self.unsupported(span, &format!("dropping a `{shown}`"));
            }
        }
        Some(())
    }

    /// Drops the field of `ty` at `ptr + off`, if it owns anything.
    fn drop_field(&mut self, ptr: Value, off: u64, ty: Ty) {
        if !self.needs_drop(ty) {
            return;
        }
        let at = self.offset(ptr, off as i64);
        self.drop_at(at, ty);
    }

    /// Drops `count` elements of `elem`, `size` bytes apart, from `buf`.
    pub(super) fn drop_elems(&mut self, buf: Value, count: Value, elem: Ty, size: u64) {
        if !self.needs_drop(elem) {
            return;
        }
        let i = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(i, zero);
        let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let more = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: cur, b: count }, VTy::Int);
        self.br(more, body, done);
        self.enter(body);
        let cur = self.read(i);
        let at = self.emit1(InstKind::Index { base: buf, index: cur, scale: size }, VTy::Ptr);
        self.drop_at(at, elem);
        let one = self.iconst(1);
        let next =
            self.emit1(InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: U64, a: cur, b: one }, VTy::Int);
        self.write(i, next);
        self.jump(head);
        self.seal(head);
        self.enter(done);
    }

    /// Frees the buffer of the list (or string) whose header is at `ptr`,
    /// of elements of `size` bytes aligned to `align`, unless it has no
    /// capacity (empty, or moved away).
    pub(super) fn free_buffer(&mut self, ptr: Value, size: u64, align: u64) {
        if size > 0 {
            self.emit(InstKind::FreeBuf { header: ptr, off: 0, size, align }, &[]);
        }
    }

    // ------------------------------------------------------------ copy glue

    /// A value of its own that's a copy of `v`, a value of `ty`: a deep
    /// copy, for a type that owns memory.
    pub(super) fn copy_val(&mut self, v: &Val, ty: Ty, span: Span) -> Option<Val> {
        match v {
            Val::Regs(_) => Some(v.clone()),
            Val::Mem(_) => {
                let dst = self.temp(ty, span)?;
                self.copy_into(dst, v, ty, span)?;
                self.own_temp(dst, ty);
                Some(Val::Mem(dst))
            }
        }
    }

    /// Writes a copy of `v`, a value of `ty`, at `dst`, which holds
    /// nothing.
    pub(super) fn copy_into(&mut self, dst: Value, v: &Val, ty: Ty, span: Span) -> Option<()> {
        if !self.needs_drop(ty) {
            return self.store_val(dst, 0, ty, v, span);
        }
        let src = self.addr_of(v.clone(), ty, span)?;
        let func = self.glue(Glue::Copy, ty);
        self.emit(InstKind::Call { func, args: vec![dst, src] }, &[]);
        Some(())
    }

    /// Copies the value of `ty` at `src` to `dst`, in the glue being
    /// built: its bytes, and then a copy of what each part owns.
    fn copy_glue_body(&mut self, dst: Value, src: Value, ty: Ty, span: Span) -> Option<()> {
        let ty = self.tcx().degauge(ty);
        let (size, _) = self.size_align(ty)?;
        if size > 0 {
            self.emit(InstKind::Copy { dst, src, size }, &[]);
        }
        match self.tcx().kind(ty).clone() {
            TyKind::Tuple(items) => {
                let offsets = self.field_offsets(ty, span)?;
                for (t, off) in items.into_iter().zip(offsets) {
                    self.copy_field(dst, src, off, t);
                }
            }
            TyKind::Array(elem, n) => {
                let zero = self.iconst(0);
                let n = self.iconst(n);
                self.count_loop(zero, n, |this, i| {
                    let (d, s) = (this.elem_at(dst, i, elem)?, this.elem_at(src, i, elem)?);
                    this.copy_field(d, s, 0, elem);
                    Some(())
                })?;
            }
            TyKind::Any(_) => self.copy_any(dst, src),
            TyKind::Ptr(..) => self.copy_ptr(dst, src, ty, span)?,
            TyKind::Optional(inner) => {
                let present = self.is_some(&Val::Mem(src), ty, span)?;
                let (yes, done) = (self.new_block(), self.new_block());
                self.br(present, yes, done);
                self.enter(yes);
                self.copy_field(dst, src, 0, inner);
                self.jump(done);
                self.enter(done);
            }
            TyKind::Result(..) | TyKind::Union(_) => {
                let members = match self.tcx().kind(ty).clone() {
                    TyKind::Result(t, e) => vec![t, e],
                    TyKind::Union(ms) => ms,
                    _ => Vec::new(),
                };
                let Shape::Tagged { tag, payload } = self.layout(ty)?.shape else { return None };
                let t = self.load(super::warp::tag_mem(tag.bytes), src, tag.offset as i64, VTy::Int);
                self.by_case(t, members.len(), |this, i| {
                    this.copy_field(dst, src, payload, members[i]);
                    Some(())
                })?;
            }
            TyKind::Adt(def, args) => match self.p.def_path(def).as_str() {
                "std.string.String" => {
                    let (p, n) = self.list_view(src);
                    self.emit(InstKind::Native { name: "str.new", imm: 0, args: vec![p, n, dst] }, &[]);
                }
                "std.list.List" => {
                    let elem = args[0];
                    let (buf, len) = self.list_view(src);
                    let new = self.alloc_elems(len, elem)?;
                    if self.needs_drop(elem) {
                        let zero = self.iconst(0);
                        self.count_loop(zero, len, |this, i| {
                            let (d, s) = (this.elem_at(new, i, elem)?, this.elem_at(buf, i, elem)?);
                            this.copy_field(d, s, 0, elem);
                            Some(())
                        })?;
                    } else {
                        let (size, _) = self.size_align(elem)?;
                        let scale = self.iconst(size);
                        let bytes = self.uarith(IntOp::Mul, len, scale);
                        self.emit(InstKind::Native { name: "mem.move", imm: 0, args: vec![new, buf, bytes] }, &[]);
                    }
                    let cap = self.cap_for(len, elem)?;
                    self.set_header(dst, new, len, cap);
                }
                "std.map.Map" | "std.set.Set" => {
                    let c = self.coll_of(ty)?;
                    self.copy_coll(&c, dst, src, span)?;
                }
                path if self.is_builtin(def) => {
                    return self.unsupported(span, &format!("copying a `{path}`"));
                }
                _ => {
                    let info = self.warp_info(ty)?;
                    match self.layout(ty)?.shape {
                        Shape::Fields(offsets) => {
                            for (f, off) in info.fields.iter().zip(offsets) {
                                self.copy_field(dst, src, off, f.ty);
                            }
                        }
                        Shape::Sum { tag, shared, cases, .. } => {
                            for (f, off) in info.fields.iter().zip(shared) {
                                self.copy_field(dst, src, off, f.ty);
                            }
                            let t = self.load(super::warp::tag_mem(tag.bytes), src, tag.offset as i64, VTy::Int);
                            self.by_case(t, info.cases.len(), |this, ci| {
                                let tys: Vec<Ty> = info.cases[ci].fields.iter().flatten().map(|f| f.ty).collect();
                                for (fty, &off) in tys.iter().zip(&cases[ci]) {
                                    this.copy_field(dst, src, off, *fty);
                                }
                                Some(())
                            })?;
                        }
                        _ => return self.internal(span, "copying a warp laid out as something else"),
                    }
                }
            },
            _ => {
                let shown = self.tcx().display(self.p, ty);
                return self.unsupported(span, &format!("copying a `{shown}`"));
            }
        }
        Some(())
    }

    /// Copies what the field of `ty` at `off` owns, if anything: its bytes
    /// are copied already, and its glue copies them again and then the rest.
    fn copy_field(&mut self, dst: Value, src: Value, off: u64, ty: Ty) {
        if !self.needs_drop(ty) {
            return;
        }
        let (d, s) = (self.offset(dst, off as i64), self.offset(src, off as i64));
        let func = self.glue(Glue::Copy, ty);
        self.emit(InstKind::Call { func, args: vec![d, s] }, &[]);
    }

    /// Builds `each(i)` for the case `tag` says, of `count`.
    fn by_case(
        &mut self,
        tag: Value,
        count: usize,
        mut each: impl FnMut(&mut Self, usize) -> Option<()>,
    ) -> Option<()> {
        let done = self.new_block();
        for i in 0..count {
            let (yes, next) = (self.new_block(), self.new_block());
            let k = self.iconst(i as u64);
            let c = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: tag, b: k }, VTy::Int);
            self.br(c, yes, next);
            self.enter(yes);
            each(self, i)?;
            self.jump(done);
            self.enter(next);
        }
        self.jump(done);
        self.enter(done);
        Some(())
    }

    // ------------------------------------------------------------ show glue

    /// The value parameter of glue for `ty`: its parts, or its address, as
    /// a read parameter is passed.
    pub(super) fn glue_value_param(&mut self, ty: Ty, span: Span) -> Option<Val> {
        Some(match self.repr_of(ty, span)? {
            super::Repr::Regs(parts) => {
                let vals: Vec<Value> = parts.iter().map(|p| self.new_value(p.vty)).collect();
                self.f.blocks[0].params.extend(vals.iter().copied());
                Val::Regs(vals)
            }
            super::Repr::Mem { .. } => {
                let ptr = self.new_value(VTy::Ptr);
                self.f.blocks[0].params.push(ptr);
                Val::Mem(ptr)
            }
        })
    }

    /// Writes the structure of `v`, a value of `ty`, as `{x}` (or `{x:?}`)
    /// writes it (`rt.show` in the Lua runtime).
    fn show_structure(&mut self, sink: Sink, v: Val, ty: Ty, debug: bool, span: Span) -> Option<()> {
        let ty = self.tcx().degauge(ty);
        match self.tcx().kind(ty).clone() {
            TyKind::Unit => self.write_text(sink, "()"),
            TyKind::Tuple(items) => {
                let parts = self.tuple_items(&v, ty, span)?;
                self.write_text(sink, "(");
                for (i, (item, t)) in parts.into_iter().enumerate() {
                    if i > 0 {
                        self.write_text(sink, ", ");
                    }
                    self.write_value(sink, item, t, None, debug, span)?;
                }
                self.write_text(sink, if items.len() == 1 { ",)" } else { ")" });
            }
            TyKind::Optional(inner) => {
                let present = self.is_some(&v, ty, span)?;
                let (yes, no, done) = (self.new_block(), self.new_block(), self.new_block());
                self.br(present, yes, no);
                self.enter(yes);
                let inside = self.unwrap_some(&v, ty, span)?;
                if debug {
                    self.write_text(sink, "some(");
                    self.write_value(sink, inside, inner, None, true, span)?;
                    self.write_text(sink, ")");
                } else {
                    self.write_value(sink, inside, inner, None, false, span)?;
                }
                self.jump(done);
                self.enter(no);
                self.write_text(sink, "none");
                self.jump(done);
                self.enter(done);
            }
            TyKind::Result(ok, err) => {
                let Val::Mem(ptr) = v else { return self.internal(span, "a result in registers") };
                let Shape::Tagged { tag, payload } = self.layout(ty)?.shape else { return None };
                let t = self.load(super::warp::tag_mem(tag.bytes), ptr, tag.offset as i64, VTy::Int);
                let zero = self.iconst(0);
                let is_ok = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: t, b: zero }, VTy::Int);
                let (yes, no, done) = (self.new_block(), self.new_block(), self.new_block());
                self.br(is_ok, yes, no);
                for (block, word, inner) in [(yes, "ok(", ok), (no, "err(", err)] {
                    self.enter(block);
                    self.write_text(sink, word);
                    let x = self.load_val(ptr, payload as i64, inner, span)?;
                    self.write_value(sink, x, inner, None, true, span)?;
                    self.write_text(sink, ")");
                    self.jump(done);
                }
                self.enter(done);
            }
            TyKind::Any(_) => {
                let Val::Mem(ptr) = v else { return self.internal(span, "an `any` in registers") };
                self.show_any(sink, ptr, debug);
            }
            TyKind::Ptr(..) => {
                let (x, target) = self.pointee(&v, ty, span)?;
                self.write_value(sink, x, target, None, debug, span)?;
            }
            // The member it holds, as that member shows.
            TyKind::Union(members) => {
                let tag = self.tag_value(&v, ty, span)?;
                let done = self.new_block();
                for (i, &m) in members.iter().enumerate() {
                    let (yes, no) = (self.new_block(), self.new_block());
                    let k = self.iconst(i as u64);
                    let hit = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: tag, b: k }, VTy::Int);
                    self.br(hit, yes, no);
                    self.enter(yes);
                    let x = self.payload(&v, ty, m, span)?;
                    self.write_value(sink, x, m, None, debug, span)?;
                    self.jump(done);
                    self.enter(no);
                }
                self.jump(done);
                self.enter(done);
            }
            TyKind::Array(elem, n) => {
                let Val::Mem(ptr) = v else { return self.internal(span, "an array in registers") };
                let len = self.iconst(n);
                self.show_elems(sink, ptr, len, elem, debug, span)?;
            }
            TyKind::Adt(def, args) => match self.p.def_path(def).as_str() {
                "std.list.List" => {
                    let Val::Mem(ptr) = v else { return self.internal(span, "a list in registers") };
                    let buf = self.load(Mem::W64, ptr, 0, VTy::Ptr);
                    let len = self.load(Mem::W64, ptr, 8, VTy::Int);
                    self.show_elems(sink, buf, len, args[0], debug, span)?;
                }
                "std.slice.Slice" => {
                    let vals = self.regs(v, ty, span)?;
                    self.show_elems(sink, vals[0], vals[1], args[0], debug, span)?;
                }
                "std.map.Map" | "std.set.Set" => {
                    let Val::Mem(ptr) = v else { return self.internal(span, "a map in registers") };
                    let c = self.coll_of(ty)?;
                    self.show_coll(sink, &c, ptr, debug, span)?;
                }
                path if self.is_builtin(def) => return self.unsupported(span, &format!("showing a `{path}`")),
                _ => self.show_warp(sink, v, ty, debug, span)?,
            },
            _ => {
                let shown = self.tcx().display(self.p, ty);
                return self.unsupported(span, &format!("showing a `{shown}`"));
            }
        }
        Some(())
    }

    /// `[a, b, c]`: `len` elements of `elem` from `buf`.
    fn show_elems(&mut self, sink: Sink, buf: Value, len: Value, elem: Ty, debug: bool, span: Span) -> Option<()> {
        let (size, _) = self.size_align(elem)?;
        self.write_text(sink, "[");
        let i = self.new_var(VTy::Int);
        let zero = self.iconst(0);
        self.write(i, zero);
        let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        let more = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: true, a: cur, b: len }, VTy::Int);
        self.br(more, body, done);
        self.enter(body);
        let cur = self.read(i);
        let zero = self.iconst(0);
        let first = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: cur, b: zero }, VTy::Int);
        let (sep, item) = (self.new_block(), self.new_block());
        self.br(first, item, sep);
        self.enter(sep);
        self.write_text(sink, ", ");
        self.jump(item);
        self.enter(item);
        let cur = self.read(i);
        let at = self.emit1(InstKind::Index { base: buf, index: cur, scale: size }, VTy::Ptr);
        let x = self.load_val(at, 0, elem, span)?;
        self.write_value(sink, x, elem, None, debug, span)?;
        let cur = self.read(i);
        let one = self.iconst(1);
        let next =
            self.emit1(InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: U64, a: cur, b: one }, VTy::Int);
        self.write(i, next);
        self.jump(head);
        self.seal(head);
        self.enter(done);
        self.write_text(sink, "]");
        Some(())
    }

    /// A warp: its own `show` when it has one (and this isn't the debug
    /// form), or `Name(field: value, …)`, a case's name and payload, or a
    /// unit case's name.
    fn show_warp(&mut self, sink: Sink, v: Val, ty: Ty, debug: bool, span: Span) -> Option<()> {
        if !debug && let Some(show) = self.typed.results.op_fns.get(&ty).and_then(|f| f.show) {
            return self.call_show(show, sink, v, ty, span);
        }
        let info = self.warp_info(ty)?;
        if !info.is_sum() {
            self.write_text(sink, &format!("{}(", info.name));
            for (i, f) in info.fields.iter().enumerate() {
                if i > 0 {
                    self.write_text(sink, ", ");
                }
                if !info.tuple {
                    self.write_text(sink, &format!("{}: ", f.name));
                }
                let pos = self.field_pos(ty, None, &f.key, span)?;
                let x = self.field_of(&v, &pos, span)?;
                self.write_value(sink, x, f.ty, None, true, span)?;
            }
            self.write_text(sink, ")");
            return Some(());
        }
        let tag = self.tag_of(&v, ty, span)?;
        let done = self.new_block();
        for (ci, case) in info.cases.iter().enumerate() {
            let (yes, next) = (self.new_block(), self.new_block());
            let k = self.iconst(ci as u64);
            let c = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: tag, b: k }, VTy::Int);
            self.br(c, yes, next);
            self.enter(yes);
            // The payload's fields, then (in the debug form) the shared ones.
            let mut parts: Vec<(Option<String>, crate::typeck::Key, Ty, bool)> =
                case.fields.iter().flatten().map(|f| (f.name.clone(), f.key.clone(), f.ty, true)).collect();
            if debug {
                parts.extend(info.fields.iter().map(|f| (Some(f.name.clone()), f.key.clone(), f.ty, false)));
            }
            if case.fields.is_none() && parts.is_empty() {
                self.write_text(sink, &case.name);
            } else {
                self.write_text(sink, &format!("{}(", case.name));
                for (i, (name, key, fty, in_case)) in parts.into_iter().enumerate() {
                    if i > 0 {
                        self.write_text(sink, ", ");
                    }
                    if let Some(name) = name {
                        self.write_text(sink, &format!("{name}: "));
                    }
                    let pos = self.field_pos(ty, in_case.then_some(case.def), &key, span)?;
                    let x = self.field_of(&v, &pos, span)?;
                    self.write_value(sink, x, fty, None, true, span)?;
                }
                self.write_text(sink, ")");
            }
            self.jump(done);
            self.enter(next);
        }
        self.terminate(Term::Crash(0));
        self.enter(done);
        Some(())
    }

    /// Calls a hand-written `show(self, mut f: Fmt)` (instance `show`) with
    /// a `Fmt` that writes to `sink`.
    pub(super) fn call_show(&mut self, show: BodyId, sink: Sink, v: Val, ty: Ty, span: Span) -> Option<()> {
        let fmt = self.raw_slot(8, 8);
        let f = self.slot_addr(fmt);
        self.store(Mem::W64, f, 0, sink.0);
        let func = self.func_id(super::FnKey::Body(show));
        let mut args = match self.repr_of(ty, span)? {
            super::Repr::Regs(_) => self.regs(v, ty, span)?,
            super::Repr::Mem { .. } => vec![self.addr_of(v, ty, span)?],
        };
        args.push(f);
        self.emit(InstKind::Call { func, args }, &[]);
        Some(())
    }
}
