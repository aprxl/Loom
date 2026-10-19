//! The `#builtin` methods of text, characters, integers, floats and
//! optionals (`std.string`, `std.num`, `std.option`): bodies the builder
//! writes, calling `loom_vm`'s natives for the leaves (design/vm.md D9).
//!
//! These warps stand for a type that isn't one: `Int[i32]` is `i32`,
//! `Str` is `str`, `Optional[T]` is `?T`. Their methods' `self` is passed
//! as that type ([`plain_ty`](Builder::plain_ty)).

use loom_syntax::source::Span;

use super::value::Val;
use super::{Builder, ParamAbi};
use crate::heddle::{Cmp, InstKind, IntOp, Mem, Policy, Term, VTy, Value};
use crate::ty::{Ty, TyKind};
use crate::typeck::BodyId;

/// The methods this module writes.
const BODIES: &[&str] = &[
    "std.string.String.with_capacity",
    "std.string.String.of",
    "std.string.String.push",
    "std.string.String.push_str",
    "std.string.String.pop",
    "std.string.String.clear",
    "std.string.Str.len",
    "std.string.Str.is_empty",
    "std.string.Str.char_count",
    "std.string.Str.contains",
    "std.string.Str.starts_with",
    "std.string.Str.ends_with",
    "std.string.Str.find",
    "std.string.Str.trim",
    "std.string.Str.trim_start",
    "std.string.Str.trim_end",
    "std.string.Str.to_upper",
    "std.string.Str.to_lower",
    "std.string.Str.replace",
    "std.string.Str.repeat",
    "std.string.Str.split",
    "std.string.Str.to_string",
    "std.string.Str.copy",
    "std.string.Str.parse",
    "std.string.Char.is_digit",
    "std.string.Char.is_alphabetic",
    "std.string.Char.is_alphanumeric",
    "std.string.Char.is_whitespace",
    "std.string.Char.is_upper",
    "std.string.Char.is_lower",
    "std.string.Char.to_upper",
    "std.string.Char.to_lower",
    "std.string.Char.to_digit",
    "std.string.Char.digit",
    "std.string.Char.len_utf8",
    "std.num.Int.abs",
    "std.num.Int.min",
    "std.num.Int.max",
    "std.num.Int.pow",
    "std.num.Int.trailing_zeros",
    "std.num.Int.leading_zeros",
    "std.num.Int.count_ones",
    "std.num.Int.is_power_of_two",
    "std.num.Int.log2",
    "std.num.Int.signum",
    "std.num.Int.rem_euclid",
    "std.num.Int.div_euclid",
    "std.num.Int.div_floor",
    "std.num.Int.div_ceil",
    "std.num.Int.gcd",
    "std.num.Int.lcm",
    "std.num.Int.isqrt",
    "std.num.Int.ilog10",
    "std.num.Int.next_power_of_two",
    "std.num.Int.count_zeros",
    "std.num.Float.sin",
    "std.num.Float.cos",
    "std.num.Float.tan",
    "std.num.Float.asin",
    "std.num.Float.acos",
    "std.num.Float.atan",
    "std.num.Float.atan2",
    "std.num.Float.sinh",
    "std.num.Float.cosh",
    "std.num.Float.tanh",
    "std.num.Float.exp",
    "std.num.Float.exp2",
    "std.num.Float.ln",
    "std.num.Float.log2",
    "std.num.Float.log10",
    "std.num.Float.log",
    "std.num.Float.hypot",
    "std.num.Float.fract",
    "std.num.Float.signum",
    "std.num.Float.copysign",
    "std.num.Float.to_degrees",
    "std.num.Float.to_radians",
    "std.num.Float.is_finite",
    "std.num.Float.is_infinite",
    "std.num.Float.is_sign_negative",
    "std.num.Int.rotate_left",
    "std.num.Int.rotate_right",
    "std.num.Int.to_string",
    "std.num.Float.abs",
    "std.num.Float.min",
    "std.num.Float.max",
    "std.num.Float.sqrt",
    "std.num.Float.floor",
    "std.num.Float.ceil",
    "std.num.Float.trunc",
    "std.num.Float.round",
    "std.num.Float.pow",
    "std.num.Float.is_nan",
    "std.num.Float.to_string",
    "std.option.Optional.take",
];

pub(super) fn writes_body(path: &str) -> bool {
    BODIES.contains(&path)
}

impl<'a> Builder<'a> {
    /// The type a value of `ty` is, for the warps that stand for one.
    pub(super) fn plain_ty(&self, ty: Ty) -> Ty {
        let tcx = self.tcx();
        let ty = tcx.degauge(ty);
        let TyKind::Adt(def, args) = tcx.kind(ty) else { return ty };
        match (self.p.def_path(*def).as_str(), args.as_slice()) {
            ("std.num.Int" | "std.num.Float", [t]) => tcx.degauge(*t),
            ("std.string.Str", []) => tcx.str,
            ("std.string.Char", []) => tcx.char,
            ("std.option.Optional", [t]) => tcx.find(&TyKind::Optional(*t)).unwrap_or(ty),
            ("std.iter.Range", [t]) => tcx.find(&TyKind::Range(*t)).unwrap_or(ty),
            _ => ty,
        }
    }

    /// The body of method instance `id`, of text, a number or an optional.
    pub(super) fn method_body(&mut self, id: BodyId, path: &str, span: Span) -> Option<Vec<VTy>> {
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
        // The first register of argument `i`, and both of a text one.
        let reg = |i: usize| match &args[i] {
            Val::Regs(v) => v[0],
            &Val::Mem(p) => p,
        };
        let text = |this: &mut Self, i: usize| this.text_of(args[i].clone(), tys[i], span);
        let native = |this: &mut Self, name: &'static str, imm: u16, args: Vec<Value>, n: usize| {
            let tys = vec![VTy::Int; n];
            this.emit(InstKind::Native { name, imm, args }, &tys)
        };
        let (warp, name) = path.rsplit_once('.').unwrap_or_default();
        let out: Option<Vec<Value>> = match (warp, name) {
            // ---------------------------------------------------------- String
            ("std.string.String", "with_capacity") => {
                self.empty_header(sret?, ret)?;
                Some(Vec::new())
            }
            ("std.string.String", "of") => {
                let (p, n) = self.view_of(&args[0], tys[0], span)?;
                native(self, "string.of", 0, vec![p, n, sret?], 0);
                Some(Vec::new())
            }
            ("std.string.String", "push") => {
                native(self, "string.push_char", 0, vec![reg(0), reg(1)], 0);
                Some(Vec::new())
            }
            ("std.string.String", "push_str") => {
                let (p, n) = text(self, 1)?;
                native(self, "string.push_str", 0, vec![reg(0), p, n], 0);
                Some(Vec::new())
            }
            ("std.string.String", "pop") => {
                let r = native(self, "string.pop", 0, vec![reg(0)], 2);
                Some(self.opt_from(r[0], r[1], ret, self.tcx().char, span)?)
            }
            ("std.string.String", "clear") => {
                let zero = self.iconst(0);
                self.store(Mem::W64, reg(0), 8, zero);
                Some(Vec::new())
            }
            // ---------------------------------------------------------- str
            ("std.string.Str", "len") => Some(vec![text(self, 0)?.1]),
            ("std.string.Str", "is_empty") => {
                let (_, n) = text(self, 0)?;
                let zero = self.iconst(0);
                Some(vec![self.ucmp(Cmp::Eq, n, zero)])
            }
            ("std.string.Str", "char_count") => {
                let (p, n) = text(self, 0)?;
                Some(native(self, "str.char_count", 0, vec![p, n], 1))
            }
            ("std.string.Str", "contains" | "find" | "starts_with" | "ends_with") => {
                let (p, n) = text(self, 0)?;
                let (q, m) = text(self, 1)?;
                match name {
                    "contains" => Some(vec![native(self, "str.find", 0, vec![p, n, q, m], 2)[1]]),
                    "find" => {
                        let r = native(self, "str.find", 0, vec![p, n, q, m], 2);
                        Some(self.opt_from(r[0], r[1], ret, self.tcx().usize, span)?)
                    }
                    "starts_with" => Some(native(self, "str.starts_with", 0, vec![p, n, q, m], 1)),
                    _ => Some(native(self, "str.ends_with", 0, vec![p, n, q, m], 1)),
                }
            }
            ("std.string.Str", "trim" | "trim_start" | "trim_end") => {
                let (p, n) = text(self, 0)?;
                let imm = match name {
                    "trim" => 0,
                    "trim_start" => 1,
                    _ => 2,
                };
                Some(native(self, "str.trim", imm, vec![p, n], 2))
            }
            ("std.string.Str", "to_upper" | "to_lower") => {
                let (p, n) = text(self, 0)?;
                native(self, "str.case", u16::from(name == "to_lower"), vec![p, n, sret?], 0);
                Some(Vec::new())
            }
            ("std.string.Str", "replace") => {
                let (p, n) = text(self, 0)?;
                let (q, m) = text(self, 1)?;
                let (r, k) = text(self, 2)?;
                native(self, "str.replace", 0, vec![p, n, q, m, r, k, sret?], 0);
                Some(Vec::new())
            }
            ("std.string.Str", "repeat") => {
                let (p, n) = text(self, 0)?;
                native(self, "str.repeat", 0, vec![p, n, reg(1), sret?], 0);
                Some(Vec::new())
            }
            ("std.string.Str", "split") => {
                let (p, n) = text(self, 0)?;
                let (q, m) = text(self, 1)?;
                native(self, "str.split", 0, vec![p, n, q, m, sret?], 0);
                Some(Vec::new())
            }
            ("std.string.Str", "to_string" | "copy") => {
                let (p, n) = text(self, 0)?;
                native(self, "str.new", 0, vec![p, n, sret?], 0);
                Some(Vec::new())
            }
            ("std.string.Str", "parse") => {
                let (p, n) = text(self, 0)?;
                return self.parse_body(p, n, ret, span).map(|()| abi.results);
            }
            // ---------------------------------------------------------- char
            ("std.string.Char", _) => self.char_method(name, reg(0), args.get(1).map(|_| reg(1)), ret, span),
            // ---------------------------------------------------------- integers
            ("std.num.Int", _) => {
                let Some(int) = self.int_of(tys[0]) else { return self.internal(span, "an integer method") };
                let spec = self.iconst(super::int_spec(int).pack());
                let v = reg(0);
                match name {
                    "abs" if int.signed => {
                        let zero = self.iconst(0);
                        let neg = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: false, a: v, b: zero }, VTy::Int);
                        let r = self.new_var(VTy::Int);
                        self.write(r, v);
                        let (yes, done) = (self.new_block(), self.new_block());
                        self.br(neg, yes, done);
                        self.enter(yes);
                        let n = self.emit1(InstKind::Neg { ty: int, a: v }, VTy::Int);
                        self.write(r, n);
                        self.jump(done);
                        self.enter(done);
                        Some(vec![self.read(r)])
                    }
                    "abs" => Some(vec![v]),
                    "min" | "max" => {
                        let other = reg(1);
                        let (a, b) = if name == "min" { (other, v) } else { (v, other) };
                        // min: `other` when it's smaller; max: `other` when `v` is.
                        let less = self.emit1(InstKind::ICmp { cmp: Cmp::Lt, unsigned: !int.signed, a, b }, VTy::Int);
                        let r = self.new_var(VTy::Int);
                        self.write(r, v);
                        let (yes, done) = (self.new_block(), self.new_block());
                        self.br(less, yes, done);
                        self.enter(yes);
                        self.write(r, other);
                        self.jump(done);
                        self.enter(done);
                        Some(vec![self.read(r)])
                    }
                    "pow" => {
                        let r = self.new_var(VTy::Int);
                        let one = self.iconst(1);
                        self.write(r, one);
                        let zero = self.iconst(0);
                        self.count_loop(zero, reg(1), |this, _| {
                            let cur = this.read(r);
                            let next = this.emit1(
                                InstKind::Arith { op: IntOp::Mul, policy: Policy::Trap, ty: int, a: cur, b: v },
                                VTy::Int,
                            );
                            this.write(r, next);
                            Some(())
                        })?;
                        Some(vec![self.read(r)])
                    }
                    "trailing_zeros" | "leading_zeros" | "count_ones" | "is_power_of_two" | "log2" => {
                        let imm = match name {
                            "trailing_zeros" => 0,
                            "leading_zeros" => 1,
                            "count_ones" => 2,
                            "is_power_of_two" => 3,
                            _ => 4,
                        };
                        Some(native(self, "int.bits", imm, vec![v, spec], 1))
                    }
                    "signum" | "rem_euclid" | "div_euclid" | "div_floor" | "div_ceil" | "gcd" | "lcm" | "isqrt"
                    | "ilog10" | "next_power_of_two" | "count_zeros" => {
                        let imm = [
                            "signum",
                            "rem_euclid",
                            "div_euclid",
                            "div_floor",
                            "div_ceil",
                            "gcd",
                            "lcm",
                            "isqrt",
                            "ilog10",
                            "next_power_of_two",
                            "count_zeros",
                        ]
                        .iter()
                        .position(|n| *n == name)
                        .unwrap_or(0) as u16;
                        let other = if args.len() > 1 { reg(1) } else { v };
                        Some(native(self, "int.math", imm, vec![v, other, spec], 1))
                    }
                    "rotate_left" | "rotate_right" => {
                        Some(native(self, "int.rotate", u16::from(name == "rotate_right"), vec![v, reg(1), spec], 1))
                    }
                    _ => self.shown_string(args[0].clone(), tys[0], span),
                }
            }
            // ---------------------------------------------------------- floats
            ("std.num.Float", "to_string") => self.shown_string(args[0].clone(), tys[0], span),
            ("std.num.Float", _) => {
                let f32_ = self.float_of(tys[0]) == Some(crate::ty::FloatTy::F32);
                let op: u16 = match name {
                    "abs" => 0,
                    "min" => 1,
                    "max" => 2,
                    "sqrt" => 3,
                    "floor" => 4,
                    "ceil" => 5,
                    "trunc" => 6,
                    "round" => 7,
                    "pow" => 8,
                    "sin" => 10,
                    "cos" => 11,
                    "tan" => 12,
                    "asin" => 13,
                    "acos" => 14,
                    "atan" => 15,
                    "sinh" => 16,
                    "cosh" => 17,
                    "tanh" => 18,
                    "exp" => 19,
                    "exp2" => 20,
                    "ln" => 21,
                    "log2" => 22,
                    "log10" => 23,
                    "atan2" => 24,
                    "hypot" => 25,
                    "log" => 26,
                    "fract" => 27,
                    "signum" => 28,
                    "copysign" => 29,
                    "to_degrees" => 30,
                    "to_radians" => 31,
                    "is_finite" => 32,
                    "is_infinite" => 33,
                    "is_sign_negative" => 34,
                    _ => 9,
                };
                let b = if args.len() > 1 { reg(1) } else { reg(0) };
                let r = native(self, "float.math", op * 2 + u16::from(f32_), vec![reg(0), b], 1)[0];
                // The bits come back in an integer register: as a float.
                let is_bool = matches!(name, "is_nan" | "is_finite" | "is_infinite" | "is_sign_negative");
                Some(vec![if is_bool { r } else { self.bits_to_float(r, f32_) }])
            }
            // ---------------------------------------------------------- optionals
            ("std.option.Optional", "take") => {
                let opt = tys[0];
                let place = reg(0);
                let v = self.load_val(place, 0, opt, span)?;
                let taken = match v {
                    Val::Mem(src) => {
                        let (size, _) = self.size_align(opt)?;
                        let dst = self.temp(opt, span)?;
                        if size > 0 {
                            self.emit(InstKind::Copy { dst, src, size }, &[]);
                        }
                        self.own_temp(dst, opt);
                        Val::Mem(dst)
                    }
                    v => v,
                };
                let none = self.none(opt, span)?;
                self.store_val(place, 0, opt, &none, span)?;
                if let Val::Mem(n) = none {
                    self.moved_from(&Val::Mem(n), opt);
                }
                self.ret_val(taken, span)?;
                return Some(abi.results);
            }
            _ => return self.internal(span, &format!("no body for `{path}`")),
        };
        let out = out?;
        if self.live() {
            self.terminate(Term::Ret(out));
        }
        Some(abi.results)
    }

    /// A float of the bits in integer register `r`.
    fn bits_to_float(&mut self, r: Value, f32_: bool) -> Value {
        // Through memory: a register's bits, read back as a float.
        let slot = self.raw_slot(8, 8);
        let at = self.slot_addr(slot);
        self.store(Mem::W64, at, 0, r);
        if f32_ { self.load(Mem::U32, at, 0, VTy::F32) } else { self.load(Mem::W64, at, 0, VTy::F64) }
    }

    /// `x.to_string()`: what `{x}` writes, as a `String`.
    fn shown_string(&mut self, v: Val, ty: Ty, span: Span) -> Option<Vec<Value>> {
        let dst = self.f.sret?;
        let buf = self.new_sink();
        self.write_value(buf, v, ty, None, false, span)?;
        self.emit(InstKind::Native { name: "sink.string", imm: 0, args: vec![buf.0, dst] }, &[]);
        Some(Vec::new())
    }

    /// `s.parse[T]()`: a `T ! NumError`.
    fn parse_body(&mut self, p: Value, n: Value, ret: Ty, span: Span) -> Option<()> {
        let TyKind::Result(t, _) = *self.tcx().kind(ret) else { return self.internal(span, "`parse`'s result") };
        let t = self.tcx().degauge(t);
        let r = match (self.int_of(t), self.float_of(t)) {
            (Some(int), _) => {
                let spec = self.iconst(super::int_spec(int).pack());
                self.emit(
                    InstKind::Native { name: "str.parse_int", imm: 0, args: vec![p, n, spec] },
                    &[VTy::Int, VTy::Int],
                )
            }
            (_, Some(f)) => {
                let is_f32 = self.iconst(u64::from(f == crate::ty::FloatTy::F32));
                self.emit(
                    InstKind::Native { name: "str.parse_float", imm: 0, args: vec![p, n, is_f32] },
                    &[VTy::Int, VTy::Int],
                )
            }
            _ => return self.unsupported(span, "parsing this type"),
        };
        let fine = self.iconst(3);
        let ok = self.ucmp(Cmp::Eq, r[1], fine);
        let (yes, no) = (self.new_block(), self.new_block());
        self.br(ok, yes, no);
        self.enter(yes);
        let value = match self.float_of(t) {
            Some(f) => self.bits_to_float(r[0], f == crate::ty::FloatTy::F32),
            None => r[0],
        };
        let v = self.result(ret, true, Val::Regs(vec![value]), span)?;
        self.ret_val(v, span)?;
        self.enter(no);
        let e = self.result(ret, false, Val::Regs(vec![r[1]]), span)?;
        self.ret_val(e, span)
    }

    /// A method of `char`, on `c` (and `arg`, `to_digit`'s radix).
    fn char_method(&mut self, name: &str, c: Value, arg: Option<Value>, ret: Ty, span: Span) -> Option<Vec<Value>> {
        let within = |this: &mut Self, lo: u32, hi: u32| {
            let l = this.iconst(u64::from(lo));
            let h = this.iconst(u64::from(hi));
            let a = this.ucmp(Cmp::Ge, c, l);
            let b = this.ucmp(Cmp::Le, c, h);
            this.emit1(InstKind::Bit { op: crate::heddle::BitOp::And, a, b }, VTy::Int)
        };
        let or = |this: &mut Self, a: Value, b: Value| {
            this.emit1(InstKind::Bit { op: crate::heddle::BitOp::Or, a, b }, VTy::Int)
        };
        let shift = |this: &mut Self, cond: Value, delta: i64| {
            let r = this.new_var(VTy::Int);
            this.write(r, c);
            let (yes, done) = (this.new_block(), this.new_block());
            this.br(cond, yes, done);
            this.enter(yes);
            let d = this.iconst(delta as u64);
            let moved = this.uarith(IntOp::Add, c, d);
            this.write(r, moved);
            this.jump(done);
            this.enter(done);
            this.read(r)
        };
        Some(vec![match name {
            "is_digit" => within(self, 48, 57),
            "is_upper" => within(self, 65, 90),
            "is_lower" => within(self, 97, 122),
            "is_alphabetic" => {
                let (u, l) = (within(self, 65, 90), within(self, 97, 122));
                or(self, u, l)
            }
            "is_alphanumeric" => {
                let (u, l, d) = (within(self, 65, 90), within(self, 97, 122), within(self, 48, 57));
                let a = or(self, u, l);
                or(self, a, d)
            }
            "is_whitespace" => {
                let sp = self.iconst(32);
                let a = self.ucmp(Cmp::Eq, c, sp);
                let b = within(self, 9, 13);
                or(self, a, b)
            }
            "to_upper" => {
                let l = within(self, 97, 122);
                shift(self, l, -32)
            }
            "to_lower" => {
                let u = within(self, 65, 90);
                shift(self, u, 32)
            }
            "digit" => self.emit(InstKind::Native { name: "char.digit", imm: 0, args: vec![c] }, &[VTy::Int])[0],
            "len_utf8" => {
                // 1 below 0x80, 2 below 0x800, 3 below 0x10000, else 4.
                let mut n = self.iconst(1);
                for limit in [0x80u64, 0x800, 0x10000] {
                    let k = self.iconst(limit);
                    let past = self.ucmp(Cmp::Ge, c, k);
                    n = self.uarith(IntOp::Add, n, past);
                }
                n
            }
            "to_digit" => {
                // The digit's value: 0-9, then letters from 10, below `radix`.
                let radix = arg?;
                let r = self.new_var(VTy::Int);
                let big = self.iconst(99);
                self.write(r, big);
                for (lo, hi, base) in [(48u32, 57u32, 48u64), (97, 122, 87), (65, 90, 55)] {
                    let hit = within(self, lo, hi);
                    let (yes, next) = (self.new_block(), self.new_block());
                    self.br(hit, yes, next);
                    self.enter(yes);
                    let b = self.iconst(base);
                    let v = self.uarith(IntOp::Sub, c, b);
                    self.write(r, v);
                    self.jump(next);
                    self.enter(next);
                }
                let v = self.read(r);
                let ok = self.ucmp(Cmp::Lt, v, radix);
                let TyKind::Optional(inner) = *self.tcx().kind(ret) else { return None };
                return self.opt_from(v, ok, ret, inner, span);
            }
            _ => return self.internal(span, "a method of `char`"),
        }])
    }
}
