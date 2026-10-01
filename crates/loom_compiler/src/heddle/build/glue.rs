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
use crate::heddle::{BlockId, Cmp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
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
                "std.string.String" => self.free_buffer(ptr, 1, 1),
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
    fn drop_elems(&mut self, buf: Value, count: Value, elem: Ty, size: u64) {
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
        let cap = self.load(Mem::W64, ptr, 16, VTy::Int);
        let zero = self.iconst(0);
        let has = self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: cap, b: zero }, VTy::Int);
        let (yes, done): (BlockId, BlockId) = (self.new_block(), self.new_block());
        self.br(has, yes, done);
        self.enter(yes);
        if size > 0 {
            let buf = self.load(Mem::W64, ptr, 0, VTy::Ptr);
            let scale = self.iconst(size);
            let bytes = self
                .emit1(InstKind::Arith { op: IntOp::Mul, policy: Policy::Wrap, ty: U64, a: cap, b: scale }, VTy::Int);
            let a = self.iconst(align);
            self.emit(InstKind::Native { name: "mem.free", imm: 0, args: vec![buf, bytes, a] }, &[]);
        }
        self.jump(done);
        self.enter(done);
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
