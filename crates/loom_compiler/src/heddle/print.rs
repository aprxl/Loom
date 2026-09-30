//! Heddle as text, for `loom build --emit heddle` and tests.
//!
//! ```text
//! fn fib(v0: i64) -> i64 {
//! b0(v0: i64):
//!     v1 = iconst.i64 2
//!     v2 = icmp.lt.s v0, v1
//!     br v2, b1, b2
//! …
//! }
//! ```

use std::fmt::Write as _;

use super::{BitOp, Cmp, Edge, FOp, Func, InstKind, IntOp, Mem, Module, Policy, Term, VTy, Value};
use crate::ty::{FloatTy, IntTy};

pub fn print(module: &Module) -> String {
    let mut out = String::new();
    for (i, f) in module.funcs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        func(&mut out, module, f);
    }
    out
}

fn int(t: IntTy) -> String {
    match (t.size, t.signed) {
        (true, true) => "isize".into(),
        (true, false) => "usize".into(),
        (false, true) => format!("i{}", t.bits),
        (false, false) => format!("u{}", t.bits),
    }
}

fn float(t: FloatTy) -> &'static str {
    match t {
        FloatTy::F32 => "f32",
        FloatTy::F64 => "f64",
    }
}

fn vty(t: VTy) -> &'static str {
    match t {
        VTy::Int => "int",
        VTy::F32 => "f32",
        VTy::F64 => "f64",
        VTy::Ptr => "ptr",
    }
}

fn v(x: Value) -> String {
    format!("v{}", x.0)
}

fn list(xs: &[Value]) -> String {
    xs.iter().map(|&x| v(x)).collect::<Vec<_>>().join(", ")
}

fn policy(p: Policy) -> &'static str {
    match p {
        Policy::Trap => "",
        Policy::Wrap => "%",
        Policy::Sat => "|",
    }
}

fn cmp(c: Cmp) -> &'static str {
    match c {
        Cmp::Eq => "eq",
        Cmp::Ne => "ne",
        Cmp::Lt => "lt",
        Cmp::Le => "le",
        Cmp::Gt => "gt",
        Cmp::Ge => "ge",
    }
}

fn edge(e: &Edge) -> String {
    if e.args.is_empty() { format!("b{}", e.block.0) } else { format!("b{}({})", e.block.0, list(&e.args)) }
}

fn func(out: &mut String, module: &Module, f: &Func) {
    let params: Vec<String> = f.params().iter().map(|&p| format!("{}: {}", v(p), vty(f.ty(p)))).collect();
    let results: Vec<&str> = f.results.iter().map(|&t| vty(t)).collect();
    let _ = writeln!(out, "fn {}({}) -> ({}) {{", f.name, params.join(", "), results.join(", "));
    for (i, s) in f.slots.iter().enumerate() {
        let _ = writeln!(out, "  slot{i}: {} bytes, align {}", s.size, s.align);
    }
    for (i, b) in f.blocks.iter().enumerate() {
        if b.params.is_empty() {
            let _ = writeln!(out, "b{i}:");
        } else {
            let ps: Vec<String> = b.params.iter().map(|&p| format!("{}: {}", v(p), vty(f.ty(p)))).collect();
            let _ = writeln!(out, "b{i}({}):", ps.join(", "));
        }
        for inst in &b.insts {
            let lhs = if inst.results.is_empty() { String::new() } else { format!("{} = ", list(&inst.results)) };
            let pad = inst.unwind.map(|p| format!(" unwind b{}", p.0)).unwrap_or_default();
            let _ = writeln!(out, "    {lhs}{}{pad}", kind(module, f, &inst.kind, &inst.results));
        }
        let term = match &b.term {
            None => "(no terminator)".to_string(),
            Some(Term::Jump(e)) => format!("jump {}", edge(e)),
            Some(Term::Br { cond, then, else_ }) => format!("br {}, {}, {}", v(*cond), edge(then), edge(else_)),
            Some(Term::Ret(vs)) => format!("ret {}", list(vs)).trim_end().to_string(),
            Some(Term::Unravel { ptr, len }) => format!("unravel {}, {}", v(*ptr), v(*len)),
            Some(Term::UnravelMsg) => "unravel msg".into(),
            Some(Term::Crash(k)) => format!("crash {k}"),
            Some(Term::Resume) => "resume".into(),
        };
        let pad = b.term_unwind.map(|p| format!(" unwind b{}", p.0)).unwrap_or_default();
        let _ = writeln!(out, "    {term}{pad}");
    }
    out.push_str("}\n");
}

fn kind(module: &Module, f: &Func, k: &InstKind, results: &[Value]) -> String {
    let ty = |i: usize| results.get(i).map_or("", |&r| vty(f.ty(r)));
    match k {
        InstKind::Undef => format!("undef.{}", ty(0)),
        InstKind::Iconst(n) => format!("iconst {}", *n as i64),
        InstKind::Fconst(bits) => match results.first().map(|&r| f.ty(r)) {
            Some(VTy::F32) => format!("fconst.f32 {:?}", f32::from_bits(*bits as u32)),
            _ => format!("fconst.f64 {:?}", f64::from_bits(*bits)),
        },
        InstKind::Str(n) => {
            let text = module.strings.get(*n as usize).map(|s| String::from_utf8_lossy(s).into_owned());
            format!("str {:?}", text.unwrap_or_default())
        }
        InstKind::Arith { op, policy: p, ty: t, a, b } => {
            let name = match op {
                IntOp::Add => "add",
                IntOp::Sub => "sub",
                IntOp::Mul => "mul",
            };
            format!("{name}{}.{} {}, {}", policy(*p), int(*t), v(*a), v(*b))
        }
        InstKind::Div { ty: t, a, b } => format!("div.{} {}, {}", int(*t), v(*a), v(*b)),
        InstKind::Rem { ty: t, a, b } => format!("rem.{} {}, {}", int(*t), v(*a), v(*b)),
        InstKind::Shl { ty: t, a, b } => format!("shl.{} {}, {}", int(*t), v(*a), v(*b)),
        InstKind::Shr { ty: t, a, b } => format!("shr.{} {}, {}", int(*t), v(*a), v(*b)),
        InstKind::Bit { op, a, b } => {
            let name = match op {
                BitOp::And => "and",
                BitOp::Or => "or",
                BitOp::Xor => "xor",
            };
            format!("{name} {}, {}", v(*a), v(*b))
        }
        InstKind::Neg { ty: t, a } => format!("neg.{} {}", int(*t), v(*a)),
        InstKind::BitNot { ty: t, a } => format!("bnot.{} {}", int(*t), v(*a)),
        InstKind::Not(a) => format!("not {}", v(*a)),
        InstKind::ICmp { cmp: c, unsigned, a, b } => {
            format!("icmp.{}{} {}, {}", cmp(*c), if *unsigned { ".u" } else { ".s" }, v(*a), v(*b))
        }
        InstKind::FArith { op, ty: t, a, b } => {
            let name = match op {
                FOp::Add => "fadd",
                FOp::Sub => "fsub",
                FOp::Mul => "fmul",
                FOp::Div => "fdiv",
                FOp::Rem => "frem",
            };
            format!("{name}.{} {}, {}", float(*t), v(*a), v(*b))
        }
        InstKind::FNeg { ty: t, a } => format!("fneg.{} {}", float(*t), v(*a)),
        InstKind::FCmp { cmp: c, ty: t, a, b } => format!("fcmp.{}.{} {}, {}", cmp(*c), float(*t), v(*a), v(*b)),
        InstKind::IntCast { policy: p, to, from, a } => {
            format!("cast{} {} to {} {}", policy(*p), int(*from), int(*to), v(*a))
        }
        InstKind::IntToFloat { from, to, a } => format!("itof {} to {} {}", int(*from), float(*to), v(*a)),
        InstKind::FloatToInt { sat, from, to, a } => {
            format!("ftoi{} {} to {} {}", if *sat { "|" } else { "" }, float(*from), int(*to), v(*a))
        }
        InstKind::FloatCast { to, a } => format!("fcast to {} {}", float(*to), v(*a)),
        InstKind::IntToChar { from, a } => format!("itoc {} {}", int(*from), v(*a)),
        InstKind::Call { func, args } => {
            let name = module.funcs.get(func.0 as usize).map_or("?", |f| f.name.as_str());
            format!("call {name}({})", list(args))
        }
        InstKind::Native { name, imm, args } => format!("native {name} #{imm}({})", list(args)),
        InstKind::Slot(n) => format!("slot{n}"),
        InstKind::Global(n) => format!("global+{n}"),
        InstKind::Load { mem: m, ptr, off } => format!("load.{} {}{}", mem(*m), v(*ptr), offset(*off)),
        InstKind::Store { mem: m, ptr, off, val } => {
            format!("store.{} {}{}, {}", mem(*m), v(*ptr), offset(*off), v(*val))
        }
        InstKind::Copy { dst, src, size } => format!("copy {}, {}, {size}", v(*dst), v(*src)),
        InstKind::Zero { dst, size } => format!("zero {}, {size}", v(*dst)),
        InstKind::Index { base, index, scale } => format!("index {}, {} * {scale}", v(*base), v(*index)),
        InstKind::Bounds { index, len } => format!("bounds {}, {}", v(*index), v(*len)),
        InstKind::FuncRef(func) => {
            let name = module.funcs.get(func.0 as usize).map_or("?", |f| f.name.as_str());
            format!("fnref {name}")
        }
        InstKind::CallInd { callee, args } => format!("call_ind {}({})", v(*callee), list(args)),
    }
}

fn mem(m: Mem) -> &'static str {
    match m {
        Mem::I8 => "i8",
        Mem::U8 => "u8",
        Mem::I16 => "i16",
        Mem::U16 => "u16",
        Mem::I32 => "i32",
        Mem::U32 => "u32",
        Mem::W64 => "64",
    }
}

fn offset(off: i32) -> String {
    if off == 0 { String::new() } else { format!("{off:+}") }
}
