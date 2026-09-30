//! Glue: code for the values of a type, made once per type the program
//! needs it for (design/vm.md §5.4).
//!
//! Drop glue frees what a value owns: a list's elements and buffer, a
//! string's bytes, each field of a record or a tuple that owns something,
//! the payload of a sum's case, the value in a `some`. A value of all zero
//! bytes owns nothing (a zero capacity, a `none`, case 0 with a zero
//! payload), so dropping a moved-from place does nothing.

use loom_syntax::source::Span;

use super::{Builder, Glue};
use crate::heddle::{BlockId, Cmp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
use crate::ty::layout::Shape;
use crate::ty::{IntTy, Ty, TyKind};

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
}
