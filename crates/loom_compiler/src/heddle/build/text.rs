//! Text: `print`, templates, the messages of `assert` and `unravel`, and
//! showing values (design/vm.md §10.3).
//!
//! Text goes to a *sink*, named by a handle: 0 is the program's output, 1
//! the message of an unravel being built, and others are buffers a
//! template is building a `String` in (see `loom_isa::natives`). A
//! template's operands are all worked out first, left to right, and then
//! its pieces are written one after another, so an operand that prints does
//! so before any of the template is written, as on Lua.
//!
//! Numbers, `bool`, `char` and text are written by natives; anything else
//! by its type's show glue (see `glue.rs`), which writes its pieces the
//! same way. A spec that pads (`{p:>12}`) shows the value into a buffer
//! first, and pads that.

use loom_isa::{FmtItem, ValueKind};
use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, Glue, parse_spec, value_kind};
use crate::heddle::{InstKind, Mem, Term, VTy, Value};
use crate::hir::{self, DefId, ExprKind, TemplatePart};
use crate::ty::{Ty, TyKind};
use crate::typeck::{ArgSource, Call};

/// The program's output.
pub(super) const OUT: u64 = 0;
/// The message of an unravel being built.
pub(super) const MSG: u64 = 1;

/// Where text goes: a sink's handle.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sink(pub Value);

/// A piece of text to write: literal text, or a value with its spec.
enum Piece<'a> {
    Text(&'a str),
    Value { val: Val, ty: Ty, spec: Option<&'a str>, span: Span },
}

impl<'a> Builder<'a> {
    pub(super) fn sink(&mut self, handle: u64) -> Sink {
        Sink(self.iconst(handle))
    }

    /// A call of an `extern "lua"` function of std.
    pub(super) fn extern_call(&mut self, def: DefId, info: &Call, args: &'a [hir::Arg], span: Span) -> Option<Val> {
        let path = self.p.def_path(def);
        match path.as_str() {
            "std.io.print" => {
                let pieces = self.param_pieces(def, info, args, 0, span)?;
                let out = self.sink(OUT);
                self.write_pieces(out, pieces)?;
                self.write_text(out, "\n");
                Some(Val::Regs(Vec::new()))
            }
            "std.error.unravel" => {
                let pieces = self.param_pieces(def, info, args, 0, span)?;
                self.unravel_with(pieces)?;
                None
            }
            "std.error.assert" => {
                let cond = match info.args.first() {
                    Some(ArgSource::Arg(i)) => self.value(&args[*i].value)?[0],
                    _ => return self.unsupported(span, "this `assert`"),
                };
                // The message is worked out whether or not it's needed.
                let pieces = self.param_pieces(def, info, args, 1, span)?;
                let (ok, fail) = (self.new_block(), self.new_block());
                self.br(cond, ok, fail);
                self.enter(fail);
                self.unravel_with(pieces)?;
                self.enter(ok);
                Some(Val::Regs(Vec::new()))
            }
            _ => self.unsupported(span, &format!("`{path}` on Treadle")),
        }
    }

    /// The pieces of the argument for parameter `k` of the extern `def`.
    fn param_pieces(
        &mut self,
        def: DefId,
        info: &Call,
        args: &'a [hir::Arg],
        k: usize,
        span: Span,
    ) -> Option<Vec<Piece<'a>>> {
        match info.args.get(k) {
            Some(ArgSource::Arg(i)) => self.pieces(&args[*i].value),
            Some(ArgSource::Default) => {
                let crate::hir::DefKind::Fn(f) = &self.p.def(def).kind else { return None };
                let default = f.params.get(k)?.default.as_ref()?;
                // The default was checked with the function.
                let saved = std::mem::replace(&mut self.f.body, info.callee);
                let pieces = self.pieces(default);
                self.f.body = saved;
                pieces
            }
            _ => self.unsupported(span, "this argument"),
        }
    }

    /// The pieces of `e`: a template's, or `e` as one value. Every operand
    /// is worked out here, before anything is written.
    fn pieces(&mut self, e: &'a hir::Expr) -> Option<Vec<Piece<'a>>> {
        match &e.kind {
            ExprKind::Template(parts) => self.template_pieces(parts),
            // The value as it is: the conversion to `any Show` (or from
            // `String` to `str`) is what writing it does.
            _ => {
                let ty = self.ty_of(e);
                let val = self.expr_raw(e)?;
                Some(vec![Piece::Value { val, ty, spec: None, span: e.span }])
            }
        }
    }

    fn template_pieces(&mut self, parts: &'a [TemplatePart]) -> Option<Vec<Piece<'a>>> {
        let mut out = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                TemplatePart::Text(text) => out.push(Piece::Text(text)),
                TemplatePart::Interp { expr, spec } => {
                    let ty = self.ty_of(expr);
                    let val = self.expr_raw(expr)?;
                    out.push(Piece::Value { val, ty, spec: spec.as_deref(), span: expr.span });
                }
            }
        }
        Some(out)
    }

    /// A template as a value: a new `String`.
    pub(super) fn template_string(&mut self, e: &'a hir::Expr, parts: &'a [TemplatePart]) -> Option<Val> {
        let pieces = self.template_pieces(parts)?;
        let buf = self.new_sink();
        self.write_pieces(buf, pieces)?;
        let ty = self.ty_of(e);
        let dst = self.temp(ty, e.span)?;
        self.emit(InstKind::Native { name: "sink.string", imm: 0, args: vec![buf.0, dst] }, &[]);
        self.own_temp(dst, ty);
        Some(Val::Mem(dst))
    }

    /// A new buffer to write into.
    fn new_sink(&mut self) -> Sink {
        Sink(self.emit1(InstKind::Native { name: "sink.push", imm: 0, args: Vec::new() }, VTy::Int))
    }

    fn write_pieces(&mut self, sink: Sink, pieces: Vec<Piece<'a>>) -> Option<()> {
        for piece in pieces {
            match piece {
                Piece::Text(text) => self.write_text(sink, text),
                Piece::Value { val, ty, spec, span } => self.write_value(sink, val, ty, spec, false, span)?,
            }
        }
        Some(())
    }

    pub(super) fn write_text(&mut self, sink: Sink, text: &str) {
        if text.is_empty() {
            return;
        }
        let t = self.str_lit(text.as_bytes());
        self.emit(InstKind::Native { name: "sink.str", imm: 0, args: vec![sink.0, t[0], t[1]] }, &[]);
    }

    /// Whether `ty` is `std.string.String`.
    pub(super) fn is_string(&self, ty: Ty) -> bool {
        matches!(self.tcx().kind(ty), TyKind::Adt(def, _) if self.p.def_path(*def) == "std.string.String")
    }

    /// The text of `val`, a `str` or a `String`: its address and length.
    pub(super) fn text_of(&mut self, val: Val, ty: Ty, span: Span) -> Option<(Value, Value)> {
        if self.is_string(ty) {
            let Val::Mem(ptr) = val else { return self.internal(span, "a `String` in registers") };
            let p = self.load(Mem::W64, ptr, 0, VTy::Ptr);
            let n = self.load(Mem::W64, ptr, 8, VTy::Int);
            return Some((p, n));
        }
        let vals = self.regs(val, ty, span)?;
        Some((vals[0], vals[1]))
    }

    /// Writes `val`, a value of type `ty`, as `{x}` (or `{x:?}` with
    /// `debug`) would, with `spec`.
    pub(super) fn write_value(
        &mut self,
        sink: Sink,
        val: Val,
        ty: Ty,
        spec: Option<&str>,
        debug: bool,
        span: Span,
    ) -> Option<()> {
        let ty = self.tcx().degauge(ty);
        let (debug, spec) = match spec {
            Some("?") => (true, None),
            Some(s) => (debug, Some(parse_spec(s)?)),
            None => (debug, None),
        };
        // Text: a `String` writes like a `str`.
        if self.is_string(ty) || matches!(self.tcx().kind(ty), TyKind::Str) {
            let (p, n) = self.text_of(val, ty, span)?;
            if spec.is_none() && !debug {
                self.emit(InstKind::Native { name: "sink.str", imm: 0, args: vec![sink.0, p, n] }, &[]);
            } else {
                let imm = self.fmt_item(FmtItem { kind: ValueKind::Str, debug, spec });
                self.emit(InstKind::Native { name: "sink.fmt2", imm, args: vec![sink.0, p, n] }, &[]);
            }
            return Some(());
        }
        if let Some(kind) = value_kind(self.tcx(), ty)
            && kind != ValueKind::Unit
        {
            let vals = self.regs(val, ty, span)?;
            let imm = self.fmt_item(FmtItem { kind, debug, spec });
            self.emit(InstKind::Native { name: "sink.fmt", imm, args: vec![sink.0, vals[0]] }, &[]);
            return Some(());
        }
        match spec {
            // Shown into a buffer, then padded into the sink.
            Some(spec) => {
                let buf = self.new_sink();
                self.show(buf, val, ty, debug, span)?;
                let imm = self.fmt_item(FmtItem { kind: ValueKind::Str, debug: false, spec: Some(spec) });
                self.emit(InstKind::Native { name: "sink.pad", imm, args: vec![buf.0, sink.0] }, &[]);
                Some(())
            }
            None => self.show(sink, val, ty, debug, span),
        }
    }

    /// Writes `val` by its type's show glue.
    pub(super) fn show(&mut self, sink: Sink, val: Val, ty: Ty, debug: bool, span: Span) -> Option<()> {
        let func = self.glue(Glue::Show { debug }, ty);
        let mut args = match self.repr_of(ty, span)? {
            super::Repr::Regs(_) => self.regs(val, ty, span)?,
            super::Repr::Mem { .. } => vec![self.addr_of(val, ty, span)?],
        };
        args.push(sink.0);
        self.emit(InstKind::Call { func, args }, &[]);
        Some(())
    }

    /// `f.put(x)`: writes `x` to `f`'s sink, as `{x}` would.
    pub(super) fn fmt_put(&mut self, f: Value, x: &'a hir::Expr) -> Option<Val> {
        let h = self.load(Mem::W64, f, 0, VTy::Int);
        let ty = self.ty_of(x);
        let val = self.expr_raw(x)?;
        self.write_value(Sink(h), val, ty, None, false, x.span)?;
        Some(Val::Regs(Vec::new()))
    }

    /// Unravels with `pieces` as the message.
    fn unravel_with(&mut self, pieces: Vec<Piece<'a>>) -> Option<()> {
        // A plain `str` is the message as it is.
        if let [Piece::Value { val, ty, spec: None, span }] = pieces.as_slice()
            && matches!(self.tcx().kind(*ty), TyKind::Str)
        {
            let (ptr, len) = self.text_of(val.clone(), *ty, *span)?;
            self.terminate(Term::Unravel { ptr, len });
            return Some(());
        }
        let msg = self.sink(MSG);
        self.write_pieces(msg, pieces)?;
        self.terminate(Term::UnravelMsg);
        Some(())
    }
}
