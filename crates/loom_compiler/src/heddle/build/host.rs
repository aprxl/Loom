//! What std asks of the machine: `std.env`, `std.fs`, `std.process`,
//! `std.time` and `std.term`, through `loom_vm`'s host natives.
//!
//! A `File` is its path (a `String`): each write opens the file, adds to
//! it and closes it, as on Lua, so nothing is left open however the
//! program ends.

use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, ParamAbi};
use crate::heddle::{Cmp, InstKind, Mem, VTy, Value};
use crate::hir::{self, DefId, DefKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::{ArgSource, BodyId, Call};
use loom_syntax::ast::ParamMode;

/// What the host natives give when a file operation works.
const FINE: u64 = 3;

impl<'a> Builder<'a> {
    /// A call of an `extern "host"` function the program declares: a
    /// `hostcall` of the import named by its path. Only integers, `bool`
    /// and `char` cross, one register each.
    fn user_host_call(
        &mut self,
        path: &str,
        f: &hir::FnDef,
        info: &Call,
        args: &'a [hir::Arg],
        span: Span,
    ) -> Option<Val> {
        let sig = self.typed.results.sigs.get(&info.callee)?.clone();
        let tcx = self.tcx();
        let scalar = |ty: Ty| matches!(tcx.kind(ty), TyKind::Int(_) | TyKind::Bool | TyKind::Char);
        if !f.generics.is_empty() {
            return self.unsupported(span, &format!("`host function {path}` with generic parameters on Treadle"));
        }
        for param in &sig.params {
            let ty = self.plain_ty(param.ty);
            if !scalar(ty) || param.mode != ParamMode::Read || param.has_default {
                let shown = self.tcx().display(self.p, ty);
                return self.unsupported(span, &format!("`host function with a parameter of type {shown}` on Treadle"));
            }
        }
        let ret = self.plain_ty(sig.ret);
        let unit = matches!(self.tcx().kind(ret), TyKind::Unit);
        if !unit && !scalar(ret) {
            let shown = self.tcx().display(self.p, ret);
            return self.unsupported(span, &format!("`host function with a result of type {shown}` on Treadle"));
        }
        let mut regs = Vec::with_capacity(sig.params.len());
        for k in 0..sig.params.len() {
            let Some(ArgSource::Arg(i)) = info.args.get(k) else { return self.unsupported(span, "this call") };
            let e = &args[*i].value;
            let v = self.expr(e)?;
            let ty = self.coerced_ty(e);
            regs.push(self.regs(v, ty, span)?[0]);
        }
        let results = usize::from(!unit);
        let index = match self.host_imports.iter().position(|h| h.name == path) {
            Some(index) => index,
            None => {
                let import =
                    loom_isa::HostImport { name: path.to_owned(), args: regs.len() as u16, results: results as u16 };
                self.host_imports.push(import);
                self.host_imports.len() - 1
            }
        };
        let out =
            self.emit(InstKind::Native { name: "host.call", imm: index as u16, args: regs }, &vec![VTy::Int; results]);
        Some(Val::Regs(out))
    }

    /// A call of a host extern of std, if `path` is one.
    pub(super) fn host_call(
        &mut self,
        path: &str,
        def: DefId,
        info: &Call,
        args: &'a [hir::Arg],
        span: Span,
    ) -> Option<Option<Val>> {
        if !path.starts_with("std.")
            && let DefKind::Fn(f) = &self.p.def(def).kind
            && f.abi.as_deref() == Some("host")
        {
            return Some(self.user_host_call(path, f, info, args, span));
        }
        let ret = self.plain_ty(self.typed.results.sigs.get(&info.callee)?.ret);
        let arg = |this: &mut Self, k: usize| -> Option<(Val, Ty)> {
            let Some(ArgSource::Arg(i)) = info.args.get(k) else { return None };
            let e = &args[*i].value;
            let v = this.expr(e)?;
            Some((v, this.coerced_ty(e)))
        };
        Some(match path {
            "std.env.get" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let dst = self.temp(ret, span)?;
                self.native("env.get", vec![p, n, dst], 0);
                self.own_temp(dst, ret);
                Some(Val::Mem(dst))
            }
            "std.io.read_line" | "std.io.read_all" => {
                let dst = self.temp(ret, span)?;
                let name = if path == "std.io.read_line" { "io.read_line" } else { "io.read_all" };
                self.native(name, vec![dst], 0);
                self.own_temp(dst, ret);
                Some(Val::Mem(dst))
            }
            "std.io.write_err" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                self.native("io.write_err", vec![p, n], 0);
                Some(Val::Regs(Vec::new()))
            }
            "std.io.write_out" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                self.native("out.str", vec![p, n], 0);
                Some(Val::Regs(Vec::new()))
            }
            "std.env.args" => {
                let dst = self.temp(ret, span)?;
                self.native("env.args", vec![dst], 0);
                self.own_temp(dst, ret);
                Some(Val::Mem(dst))
            }
            "std.process.exit" => {
                let (v, ty) = arg(self, 0)?;
                let code = self.regs(v, ty, span)?[0];
                self.native("process.exit", vec![code], 0);
                self.terminate(crate::heddle::Term::Crash(1));
                None
            }
            "std.sync.sched_yield" => {
                self.native("task.yield", Vec::new(), 0);
                Some(Val::Regs(Vec::new()))
            }
            "std.time.sleep" => {
                // A `Duration` is its nanoseconds.
                let (v, ty) = arg(self, 0)?;
                let nanos = self.regs(v, ty, span)?[0];
                self.native("time.sleep", vec![nanos], 0);
                Some(Val::Regs(Vec::new()))
            }
            "std.time.unix_nanos" => Some(Val::Regs(self.native("time.unix", Vec::new(), 1))),
            "std.process.id" => Some(Val::Regs(self.native("process.id", Vec::new(), 1))),
            "std.env.current_dir" => {
                let dst = self.temp(ret, span)?;
                self.native("env.cwd", vec![dst], 0);
                self.own_temp(dst, ret);
                Some(Val::Mem(dst))
            }
            "std.term.columns" | "std.term.rows" | "std.term.is_tty" => {
                let imm = match path {
                    "std.term.columns" => 0,
                    "std.term.rows" => 1,
                    _ => 2,
                };
                Some(Val::Regs(self.emit(InstKind::Native { name: "term.info", imm, args: Vec::new() }, &[VTy::Int])))
            }
            "std.fs.file_kind" | "std.fs.file_size" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let name = if path == "std.fs.file_kind" { "fs.kind" } else { "fs.size" };
                Some(Val::Regs(self.native(name, vec![p, n], 1)))
            }
            "std.time.monotonic_nanos" => Some(Val::Regs(self.native("time.now", Vec::new(), 1))),
            "std.term.raw_mode" => {
                let (v, ty) = arg(self, 0)?;
                let on = self.regs(v, ty, span)?[0];
                self.native("term.raw_mode", vec![on], 0);
                Some(Val::Regs(Vec::new()))
            }
            "std.term.poll_keys" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                Some(Val::Regs(self.native("term.poll_keys", vec![p, n], 1)))
            }
            "std.fs.write_text" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let (t, tty) = arg(self, 1)?;
                let (tp, tn) = self.text_of(t, tty, span)?;
                let msg = self.temp_string(span)?;
                let code = self.native("fs.write", vec![p, n, tp, tn, msg], 1)[0];
                self.io_result(ret, code, (p, n), None, msg, span)
            }
            "std.fs.create_dir_all" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let msg = self.temp_string(span)?;
                let code = self.native("fs.dir", vec![p, n, msg], 1)[0];
                self.io_result(ret, code, (p, n), None, msg, span)
            }
            "std.fs.rename" | "std.fs.copy" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let (w, wty) = arg(self, 1)?;
                let (q, m) = self.text_of(w, wty, span)?;
                let msg = self.temp_string(span)?;
                let imm = u16::from(path == "std.fs.copy");
                let code =
                    self.emit(InstKind::Native { name: "fs.move", imm, args: vec![p, n, q, m, msg] }, &[VTy::Int])[0];
                self.io_result(ret, code, (p, n), None, msg, span)
            }
            "std.fs.read_dir" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let TyKind::Result(ok, _) = *self.tcx().kind(ret) else { return None };
                let got = self.temp(ok, span)?;
                let code = self.native("fs.list", vec![p, n, got], 1)[0];
                self.io_result(ret, code, (p, n), Some(got), got, span)
            }
            "std.fs.exists" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                Some(Val::Regs(self.native("fs.exists", vec![p, n], 1)))
            }
            "std.fs.remove_file" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let msg = self.temp_string(span)?;
                let code = self.native("fs.remove", vec![p, n, msg], 1)[0];
                self.io_result(ret, code, (p, n), None, msg, span)
            }
            "std.fs.read" | "std.fs.read_text" => {
                let (v, ty) = arg(self, 0)?;
                let (p, n) = self.text_of(v, ty, span)?;
                let TyKind::Result(ok, _) = *self.tcx().kind(ret) else { return None };
                let got = self.temp(ok, span)?;
                let code = self.native("fs.read", vec![p, n, got], 1)[0];
                self.io_result(ret, code, (p, n), Some(got), got, span)
            }
            _ => return None,
        })
    }

    /// The bodies of `File`'s methods.
    pub(super) fn file_method(&mut self, id: BodyId, name: &str, span: Span) -> Option<Vec<VTy>> {
        self.f.span = None;
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
        match name {
            // `File.append(path)`: a file to add lines to, made if it isn't
            // there.
            "append" => {
                let (p, n) = self.text_of(args[0].clone(), tys[0], span)?;
                let msg = self.temp_string(span)?;
                let code = self.native("fs.append", vec![p, n, msg], 1)[0];
                let r = self.io_result(ret, code, (p, n), None, msg, span)?;
                self.ret_val(r, span)?;
            }
            "write_line" => {
                let Val::Mem(file) = args[0] else { return self.internal(span, "a file in registers") };
                let (p, n) = (self.load(Mem::W64, file, 0, VTy::Ptr), self.load(Mem::W64, file, 8, VTy::Int));
                let (q, m) = self.text_of(args[1].clone(), tys[1], span)?;
                let msg = self.temp_string(span)?;
                let code = self.native("fs.write_line", vec![p, n, q, m, msg], 1)[0];
                let r = self.io_result(ret, code, (p, n), None, msg, span)?;
                self.ret_val(r, span)?;
            }
            _ => return self.internal(span, &format!("no body for `File.{name}`")),
        }
        Some(abi.results)
    }

    fn native(&mut self, name: &'static str, args: Vec<Value>, results: usize) -> Vec<Value> {
        self.emit(InstKind::Native { name, imm: 0, args }, &vec![VTy::Int; results])
    }

    /// A temporary `String` slot the host may write a message into.
    fn temp_string(&mut self, span: Span) -> Option<Value> {
        let string = self.string_ty()?;
        let dst = self.temp(string, span)?;
        self.emit(InstKind::Zero { dst, size: 24 }, &[]);
        Some(dst)
    }

    /// `T ! IoError` (`ret`) from a file native's `code`: `ok` of the value
    /// at `got` (or of a `File` of `path`, or of `()`), or the `IoError`
    /// it says, with `path`, or the message at `msg`.
    fn io_result(
        &mut self,
        ret: Ty,
        code: Value,
        path: (Value, Value),
        got: Option<Value>,
        msg: Value,
        span: Span,
    ) -> Option<Val> {
        let TyKind::Result(ok_ty, err_ty) = *self.tcx().kind(ret) else { return None };
        let join = self.join(ret, span)?;
        let fine = self.iconst(FINE);
        let ok = self.ucmp(Cmp::Eq, code, fine);
        let (yes, no, done) = (self.new_block(), self.new_block(), self.new_block());
        self.br(ok, yes, no);
        self.enter(yes);
        let value = match got {
            Some(g) => Val::Mem(g),
            None if matches!(self.tcx().kind(ok_ty), TyKind::Unit) => Val::Regs(Vec::new()),
            // A `File`: a `String` of its path.
            None => {
                let f = self.temp(ok_ty, span)?;
                self.emit(InstKind::Native { name: "str.new", imm: 0, args: vec![path.0, path.1, f] }, &[]);
                Val::Mem(f)
            }
        };
        let r = self.result(ret, true, value, span)?;
        self.join_put(&join, r, 0, span)?;
        self.jump(done);
        self.enter(no);
        // The error: `NotFound(path)`, `Denied(path)` or `Other(message)`.
        let e = self.temp(err_ty, span)?;
        let info = self.warp_info(err_ty)?;
        let layout = self.layout(err_ty)?;
        let crate::ty::layout::Shape::Sum { tag, cases, .. } = layout.shape else {
            return self.internal(span, "an `IoError` laid out as something else");
        };
        let two = self.iconst(2);
        let other = self.ucmp(Cmp::Eq, code, two);
        let (from_path, from_msg, made) = (self.new_block(), self.new_block(), self.new_block());
        self.br(other, from_msg, from_path);
        self.enter(from_path);
        let at = self.offset(e, cases[0][0] as i64);
        self.emit(InstKind::Native { name: "str.new", imm: 0, args: vec![path.0, path.1, at] }, &[]);
        self.jump(made);
        self.enter(from_msg);
        let at = self.offset(e, cases[2][0] as i64);
        self.emit(InstKind::Copy { dst: at, src: msg, size: 24 }, &[]);
        self.jump(made);
        self.enter(made);
        self.store(super::warp::tag_mem(tag.bytes), e, tag.offset as i64, code);
        let _ = info;
        let r = self.result(ret, false, Val::Mem(e), span)?;
        self.join_put(&join, r, 0, span)?;
        self.jump(done);
        self.enter(done);
        Some(self.join_get(&join))
    }
}
