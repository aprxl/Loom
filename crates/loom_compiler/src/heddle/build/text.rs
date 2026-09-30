//! Text: `print`, and the messages of `assert` and `unravel`.
//!
//! A template passed to one of them is never a string of its own. Its
//! operands are all worked out first, left to right, and then its pieces
//! are written one after another: to the output, or to the message an
//! unravel is built in. Working out every operand first keeps effects in
//! the order the Lua backend has them (an operand that prints does so
//! before any of the template is written).

use loom_isa::FmtItem;
use loom_syntax::source::Span;

use super::{Builder, Vals, parse_spec, value_kind};
use crate::heddle::{InstKind, Term};
use crate::hir::{self, DefId, ExprKind, TemplatePart};
use crate::ty::{Ty, TyKind};
use crate::typeck::{ArgSource, Call};

/// Where text goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sink {
    /// The program's output.
    Out,
    /// The message of an unravel being built.
    Msg,
}

impl Sink {
    fn native(self, what: &str) -> &'static str {
        match (self, what) {
            (Sink::Out, "str") => "out.str",
            (Sink::Out, "fmt") => "out.fmt",
            (Sink::Out, _) => "out.fmt2",
            (Sink::Msg, "str") => "msg.str",
            (Sink::Msg, "fmt") => "msg.fmt",
            (Sink::Msg, _) => "msg.fmt2",
        }
    }
}

/// A piece of text to write: literal text, or a value with its spec.
enum Piece<'a> {
    Text(&'a str),
    Value { vals: Vals, ty: Ty, spec: Option<&'a str>, span: Span },
}

impl<'a> Builder<'a> {
    /// A call of an `extern "lua"` function of std.
    pub(super) fn extern_call(&mut self, def: DefId, info: &Call, args: &'a [hir::Arg], span: Span) -> Option<Vals> {
        let path = self.p.def_path(def);
        match path.as_str() {
            "std.io.print" => {
                let pieces = self.param_pieces(def, info, args, 0, span)?;
                self.write_pieces(Sink::Out, pieces)?;
                self.write_text(Sink::Out, "\n");
                Some(Vec::new())
            }
            "std.error.unravel" => {
                let pieces = self.param_pieces(def, info, args, 0, span)?;
                self.unravel_with(pieces)?;
                None
            }
            "std.error.assert" => {
                let cond = match info.args.first() {
                    Some(ArgSource::Arg(i)) => self.expr(&args[*i].value)?[0],
                    _ => return self.unsupported(span, "this `assert`"),
                };
                // The message is worked out whether or not it's needed.
                let pieces = self.param_pieces(def, info, args, 1, span)?;
                let (ok, fail) = (self.new_block(), self.new_block());
                self.br(cond, ok, fail);
                self.enter(fail);
                self.unravel_with(pieces)?;
                self.enter(ok);
                Some(Vec::new())
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
            ExprKind::Template(parts) => {
                let mut out = Vec::with_capacity(parts.len());
                for part in parts {
                    match part {
                        TemplatePart::Text(text) => out.push(Piece::Text(text)),
                        TemplatePart::Interp { expr, spec } => {
                            let vals = self.expr(expr)?;
                            let ty = self.ty_of(expr);
                            out.push(Piece::Value { vals, ty, spec: spec.as_deref(), span: expr.span });
                        }
                    }
                }
                Some(out)
            }
            // The value as it is: the conversion to `any Show` (or from
            // `String` to `str`) is what writing it does.
            _ => {
                let vals = self.expr_raw(e)?;
                Some(vec![Piece::Value { vals, ty: self.ty_of(e), spec: None, span: e.span }])
            }
        }
    }

    fn write_pieces(&mut self, sink: Sink, pieces: Vec<Piece<'a>>) -> Option<()> {
        for piece in pieces {
            match piece {
                Piece::Text(text) => self.write_text(sink, text),
                Piece::Value { vals, ty, spec, span } => self.write_value(sink, &vals, ty, spec, false, span)?,
            }
        }
        Some(())
    }

    fn write_text(&mut self, sink: Sink, text: &str) {
        if text.is_empty() {
            return;
        }
        let args = self.str_lit(text.as_bytes());
        self.emit(InstKind::Native { name: sink.native("str"), imm: 0, args }, &[]);
    }

    /// Writes `vals`, a value of type `ty`, as `{x}` (or `{x:?}` with
    /// `debug`) would, with `spec`.
    fn write_value(
        &mut self,
        sink: Sink,
        vals: &[crate::heddle::Value],
        ty: Ty,
        spec: Option<&str>,
        debug: bool,
        span: Span,
    ) -> Option<()> {
        let tcx = self.tcx();
        if let TyKind::Tuple(items) = tcx.kind(tcx.degauge(ty)) {
            let debug = match spec {
                None => debug,
                Some("?") => true,
                Some(_) => return self.unsupported(span, "format specs on tuples"),
            };
            let items = items.clone();
            let parts = self.tuple_parts(ty, span)?;
            self.write_text(sink, "(");
            let mut at = 0;
            for (i, (item, n)) in items.iter().zip(parts.iter().map(|(_, n)| *n)).enumerate() {
                if i > 0 {
                    self.write_text(sink, ", ");
                }
                self.write_value(sink, &vals[at..at + n], *item, None, debug, span)?;
                at += n;
            }
            self.write_text(sink, if items.len() == 1 { ",)" } else { ")" });
            return Some(());
        }
        let Some(kind) = value_kind(tcx, ty) else {
            let shown = tcx.display(self.p, ty);
            return self.unsupported(span, &format!("printing values of type `{shown}`"));
        };
        let spec = match spec {
            Some(s) => Some(parse_spec(s)?),
            None => None,
        };
        match kind {
            loom_isa::ValueKind::Unit if spec.is_none() => self.write_text(sink, "()"),
            loom_isa::ValueKind::Unit => return self.unsupported(span, "format specs on `()`"),
            loom_isa::ValueKind::Str if spec.is_none() && !debug => {
                self.emit(InstKind::Native { name: sink.native("str"), imm: 0, args: vals.to_vec() }, &[]);
            }
            _ => {
                let imm = self.fmt_item(FmtItem { kind, debug, spec });
                let name = sink.native(if kind == loom_isa::ValueKind::Str { "fmt2" } else { "fmt" });
                self.emit(InstKind::Native { name, imm, args: vals.to_vec() }, &[]);
            }
        }
        Some(())
    }

    /// Unravels with `pieces` as the message.
    fn unravel_with(&mut self, pieces: Vec<Piece<'a>>) -> Option<()> {
        // A plain `str` is the message as it is.
        if let [Piece::Value { vals, ty, spec: None, .. }] = pieces.as_slice()
            && matches!(self.tcx().kind(*ty), TyKind::Str)
        {
            let (ptr, len) = (vals[0], vals[1]);
            self.terminate(Term::Unravel { ptr, len });
            return Some(());
        }
        self.write_pieces(Sink::Msg, pieces)?;
        self.terminate(Term::UnravelMsg);
        Some(())
    }
}
