//! Differential test of constant folding (the `sccp` pass): each operation,
//! on constant operands, as built (Treadle works it out) and as folded (the
//! compiler did), must give the same value, or trap the same way, for every
//! width class, signedness, operator, policy and conversion.

use loom_compiler::codegen;
use loom_compiler::heddle::{
    Block, Cmp, FOp, Func, FuncId, Inst, InstKind, IntOp, Module, Policy, Term, VTy, Value, opt,
};
use loom_compiler::ty::{FloatTy, IntTy};
use loom_syntax::SourceDb;
use loom_sys::{Host, Outcome};
use loom_vm::{Config, Vm};

fn inst(kind: InstKind, r: u32) -> Inst {
    Inst { kind, results: vec![Value(r)], span: None, unwind: None, site: None }
}

/// `f() = kind(a, b)`: two constants (of `operand`'s type), and the result.
fn func(operand: VTy, result: VTy, a: u64, b: u64, kind: InstKind) -> Func {
    let constant =
        |bits| if matches!(operand, VTy::F32 | VTy::F64) { InstKind::Fconst(bits) } else { InstKind::Iconst(bits) };
    let block = Block {
        params: Vec::new(),
        insts: vec![inst(constant(a), 0), inst(constant(b), 1), inst(kind, 2)],
        term: Some(Term::Ret(vec![Value(2)])),
        ..Block::default()
    };
    Func {
        name: "f".into(),
        blocks: vec![block],
        results: vec![result],
        types: vec![operand, operand, result],
        ..Func::default()
    }
}

/// What each function of `funcs` gives (`None`: it unravelled), compiled as
/// it is, or after the `sccp` pass.
fn run(funcs: &[Func], folded: bool) -> (Vec<Option<u64>>, usize) {
    let mut module =
        Module { funcs: funcs.to_vec(), strings: Vec::new(), fmts: Vec::new(), data: Vec::new(), entry: FuncId(0) };
    if folded {
        module = opt::run(module, &["sccp"]).expect("valid after sccp");
    }
    // How many were worked out: one constant, returned.
    let constants = module.funcs.iter().filter(|f| f.blocks[0].insts.len() == 1).count();
    let image = codegen::generate(&SourceDb::new(), &module).expect("compiles");
    let mut vm = Vm::new(&image, Host::captured(Default::default()), &Config::default()).expect("loads");
    let out = (0..funcs.len() as u32)
        .map(|f| match vm.call(f, &[]) {
            Ok(rs) => Some(rs[0]),
            Err(Outcome::Unravel { .. }) => None,
            Err(other) => panic!("{other:?}"),
        })
        .collect();
    (out, constants)
}

fn compare(funcs: Vec<Func>, what: &[String]) {
    let (built, _) = run(&funcs, false);
    let (folded, constants) = run(&funcs, true);
    for (k, (b, f)) in built.iter().zip(&folded).enumerate() {
        let same = b == f
            || matches!((b, f), (Some(x), Some(y)) if f64::from_bits(*x).is_nan() && f64::from_bits(*y).is_nan());
        assert!(same, "{}: built {b:?}, folded {f:?}", what[k]);
    }
    // Most of them fold (the rest trap).
    assert!(constants * 2 > funcs.len(), "only {constants} of {} folded", funcs.len());
}

const WIDTHS: [u8; 8] = [1, 7, 8, 12, 32, 33, 63, 64];

fn samples(ty: IntTy) -> Vec<u64> {
    let mut vs: Vec<i128> = vec![ty.min(), ty.min() + 1, ty.max() - 1, ty.max(), 0, 1, 2, 3];
    if ty.signed {
        vs.extend([-1, -2, -3]);
    }
    vs.retain(|&v| ty.contains(v));
    vs.sort();
    vs.dedup();
    vs.into_iter().map(|v| v as u64).collect()
}

#[test]
fn integer_folds_match_treadle() {
    type Make = fn(IntTy, Value, Value) -> InstKind;
    type Op = Box<dyn Fn(IntTy, Value, Value) -> InstKind>;
    let arith = |op, policy| move |ty, a, b| InstKind::Arith { op, policy, ty, a, b };
    let ops: Vec<(&str, Op)> = vec![
        ("add", Box::new(arith(IntOp::Add, Policy::Trap))),
        ("add%", Box::new(arith(IntOp::Add, Policy::Wrap))),
        ("add|", Box::new(arith(IntOp::Add, Policy::Sat))),
        ("sub", Box::new(arith(IntOp::Sub, Policy::Trap))),
        ("sub%", Box::new(arith(IntOp::Sub, Policy::Wrap))),
        ("sub|", Box::new(arith(IntOp::Sub, Policy::Sat))),
        ("mul", Box::new(arith(IntOp::Mul, Policy::Trap))),
        ("mul%", Box::new(arith(IntOp::Mul, Policy::Wrap))),
        ("mul|", Box::new(arith(IntOp::Mul, Policy::Sat))),
        ("div", Box::new((|ty, a, b| InstKind::Div { ty, a, b }) as Make)),
        ("rem", Box::new((|ty, a, b| InstKind::Rem { ty, a, b }) as Make)),
        ("bnot", Box::new((|ty, a, _| InstKind::BitNot { ty, a }) as Make)),
        ("lt", Box::new((|ty, a, b| InstKind::ICmp { cmp: Cmp::Lt, unsigned: !ty.signed, a, b }) as Make)),
        ("ge", Box::new((|ty, a, b| InstKind::ICmp { cmp: Cmp::Ge, unsigned: !ty.signed, a, b }) as Make)),
    ];
    let mut funcs = Vec::new();
    let mut what = Vec::new();
    for bits in WIDTHS {
        for signed in [false, true] {
            let ty = IntTy { signed, bits, size: false };
            let values = samples(ty);
            for &a in &values {
                for &b in &values {
                    for (name, make) in &ops {
                        funcs.push(func(VTy::Int, VTy::Int, a, b, make(ty, Value(0), Value(1))));
                        what.push(format!("{name} {ty:?} {a:#x} {b:#x}"));
                    }
                }
                if signed {
                    funcs.push(func(VTy::Int, VTy::Int, a, 0, InstKind::Neg { ty, a: Value(0) }));
                    what.push(format!("neg {ty:?} {a:#x}"));
                }
                for n in [0u64, 1, u64::from(bits) - 1, u64::from(bits), u64::MAX] {
                    funcs.push(func(VTy::Int, VTy::Int, a, n, InstKind::Shl { ty, a: Value(0), b: Value(1) }));
                    what.push(format!("shl {ty:?} {a:#x} {n}"));
                    funcs.push(func(VTy::Int, VTy::Int, a, n, InstKind::Shr { ty, a: Value(0), b: Value(1) }));
                    what.push(format!("shr {ty:?} {a:#x} {n}"));
                }
                // Casts to every other width, each policy.
                for to_bits in WIDTHS {
                    for to_signed in [false, true] {
                        let to = IntTy { signed: to_signed, bits: to_bits, size: false };
                        for policy in [Policy::Trap, Policy::Wrap, Policy::Sat] {
                            funcs.push(func(
                                VTy::Int,
                                VTy::Int,
                                a,
                                0,
                                InstKind::IntCast { policy, to, from: ty, a: Value(0) },
                            ));
                            what.push(format!("cast {policy:?} {ty:?} to {to:?} {a:#x}"));
                        }
                    }
                }
                for to in [FloatTy::F32, FloatTy::F64] {
                    let result = if to == FloatTy::F32 { VTy::F32 } else { VTy::F64 };
                    funcs.push(func(VTy::Int, result, a, 0, InstKind::IntToFloat { from: ty, to, a: Value(0) }));
                    what.push(format!("itof {ty:?} {to:?} {a:#x}"));
                }
            }
        }
    }
    compare(funcs, &what);
}

#[test]
fn float_folds_match_treadle() {
    let f64s = [0.0, -0.0, 1.5, -2.25, 1e300, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, f64::MIN_POSITIVE / 4.0];
    let f32s = [0.0f32, -0.0, 1.5, -2.25, 3e38, f32::INFINITY, f32::NAN, f32::MIN_POSITIVE / 4.0];
    let mut funcs = Vec::new();
    let mut what = Vec::new();
    let ops = [FOp::Add, FOp::Sub, FOp::Mul, FOp::Div, FOp::Rem];
    let cmps = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge];
    for (ty, vty, values) in [
        (FloatTy::F64, VTy::F64, f64s.iter().map(|x| x.to_bits()).collect::<Vec<_>>()),
        (FloatTy::F32, VTy::F32, f32s.iter().map(|x| u64::from(x.to_bits())).collect()),
    ] {
        for &a in &values {
            for &b in &values {
                for op in ops {
                    funcs.push(func(vty, vty, a, b, InstKind::FArith { op, ty, a: Value(0), b: Value(1) }));
                    what.push(format!("{op:?} {ty:?} {a:#x} {b:#x}"));
                }
                for cmp in cmps {
                    funcs.push(func(vty, VTy::Int, a, b, InstKind::FCmp { cmp, ty, a: Value(0), b: Value(1) }));
                    what.push(format!("{cmp:?} {ty:?} {a:#x} {b:#x}"));
                }
            }
            funcs.push(func(vty, vty, a, 0, InstKind::FNeg { ty, a: Value(0) }));
            what.push(format!("fneg {ty:?} {a:#x}"));
            let (to, to_vty) = if ty == FloatTy::F64 { (FloatTy::F32, VTy::F32) } else { (FloatTy::F64, VTy::F64) };
            funcs.push(func(vty, to_vty, a, 0, InstKind::FloatCast { to, a: Value(0) }));
            what.push(format!("fcast {ty:?} {a:#x}"));
        }
    }
    compare(funcs, &what);
}
