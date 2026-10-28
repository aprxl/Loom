//! Differential test: Treadle's integer and float operations against the
//! compiler's integer oracle (`ty::int`) and Rust's floats, for every width
//! class, signedness, operator, overflow policy and cast, on boundary and
//! random values (design/vm.md V0).
//!
//! Each operation is a Heddle function of its own, compiled by `codegen` and
//! run on Treadle, so instruction selection is tested too, not only the
//! interpreter's arithmetic.

use loom_compiler::codegen;
use loom_compiler::heddle::{Block, Cmp, FOp, Func, FuncId, Inst, InstKind, IntOp, Module, Policy, Term, VTy, Value};
use loom_compiler::ty::{FloatTy, IntTy};
use loom_syntax::SourceDb;
use loom_sys::{Host, Outcome};
use loom_vm::{Config, Vm};

/// What an operation produced: a value, or an unravel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Out {
    Value(i128),
    Trap,
}

/// A small deterministic generator, so failures reproduce.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 29)
    }

    fn value(&mut self, ty: IntTy) -> i128 {
        let span = (ty.max() - ty.min()) as u128 + 1;
        let r = ((self.next() as u128) << 64 | self.next() as u128) % span;
        ty.min() + r as i128
    }
}

fn samples(ty: IntTy, rng: &mut Lcg) -> Vec<i128> {
    let mut vs = vec![ty.min(), ty.min() + 1, ty.max() - 1, ty.max(), 0, 1, 2, 3, 7];
    if ty.signed {
        vs.extend([-1, -2, -3, -7]);
    }
    vs.extend((0..12).map(|_| rng.value(ty)));
    vs.retain(|&v| ty.contains(v));
    vs.sort();
    vs.dedup();
    vs
}

const WIDTHS: [u8; 10] = [1, 7, 8, 12, 31, 32, 33, 62, 63, 64];

fn types() -> Vec<IntTy> {
    let mut out = Vec::new();
    for bits in WIDTHS {
        for signed in [false, true] {
            out.push(IntTy { signed, bits, size: false });
        }
    }
    out
}

fn name(ty: IntTy) -> String {
    format!("{}{}", if ty.signed { 'i' } else { 'u' }, ty.bits)
}

/// A function of `params` values giving `kind`'s one result.
fn func(name: String, params: &[VTy], result: VTy, kind: impl FnOnce(&[Value]) -> InstKind) -> Func {
    let args: Vec<Value> = (0..params.len() as u32).map(Value).collect();
    let r = Value(params.len() as u32);
    let mut types = params.to_vec();
    types.push(result);
    let block = Block {
        params: args.clone(),
        insts: vec![Inst { kind: kind(&args), results: vec![r], span: None, unwind: None, site: None }],
        term: Some(Term::Ret(vec![r])),
        term_span: None,
        term_unwind: None,
        term_site: None,
    };
    Func { name, blocks: vec![block], results: vec![result], types, slots: Vec::new(), sites: Vec::new() }
}

/// Compiles `funcs` and gives a machine to call them on.
fn compile(funcs: Vec<Func>) -> loom_isa::Image {
    let module = Module {
        changed: vec![true; funcs.len()],
        funcs,
        strings: Vec::new(),
        fmts: Vec::new(),
        data: Vec::new(),
        entry: FuncId(0),
        roots: Vec::new(),
        relocs: Vec::new(),
        plain: Vec::new(),
    };
    loom_compiler::heddle::verify(&module).expect("valid Heddle");
    codegen::generate(&SourceDb::new(), &module).expect("compiles")
}

fn out(result: Result<Vec<u64>, Outcome>, ty: IntTy, traps: &[&str]) -> Out {
    match result {
        Ok(rs) => Out::Value(if ty.signed { rs[0] as i64 as i128 } else { rs[0] as i128 }),
        Err(Outcome::Unravel { message, .. }) => {
            assert!(traps.iter().any(|t| message.starts_with(t)), "unexpected unravel: {message}");
            Out::Trap
        }
        Err(other) => panic!("{other:?}"),
    }
}

/// Makes an instruction of a type from its operands.
type MakeInst = dyn Fn(IntTy, &[Value]) -> InstKind;

const TRAPS: [&str; 5] = [
    "arithmetic overflow",
    "division by zero",
    "shift amount out of range",
    "value out of range",
    "float out of range",
];

#[test]
fn arithmetic_matches_the_oracle() {
    let int = [VTy::Int, VTy::Int];
    let mut funcs = Vec::new();
    // For each type: its operations, in this order.
    let ops: Vec<(&str, Box<MakeInst>)> = vec![
        ("add", Box::new(|t, v| InstKind::Arith { op: IntOp::Add, policy: Policy::Trap, ty: t, a: v[0], b: v[1] })),
        ("add%", Box::new(|t, v| InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: t, a: v[0], b: v[1] })),
        ("add|", Box::new(|t, v| InstKind::Arith { op: IntOp::Add, policy: Policy::Sat, ty: t, a: v[0], b: v[1] })),
        ("sub", Box::new(|t, v| InstKind::Arith { op: IntOp::Sub, policy: Policy::Trap, ty: t, a: v[0], b: v[1] })),
        ("sub%", Box::new(|t, v| InstKind::Arith { op: IntOp::Sub, policy: Policy::Wrap, ty: t, a: v[0], b: v[1] })),
        ("sub|", Box::new(|t, v| InstKind::Arith { op: IntOp::Sub, policy: Policy::Sat, ty: t, a: v[0], b: v[1] })),
        ("mul", Box::new(|t, v| InstKind::Arith { op: IntOp::Mul, policy: Policy::Trap, ty: t, a: v[0], b: v[1] })),
        ("mul%", Box::new(|t, v| InstKind::Arith { op: IntOp::Mul, policy: Policy::Wrap, ty: t, a: v[0], b: v[1] })),
        ("mul|", Box::new(|t, v| InstKind::Arith { op: IntOp::Mul, policy: Policy::Sat, ty: t, a: v[0], b: v[1] })),
        ("div", Box::new(|t, v| InstKind::Div { ty: t, a: v[0], b: v[1] })),
        ("rem", Box::new(|t, v| InstKind::Rem { ty: t, a: v[0], b: v[1] })),
        ("shl", Box::new(|t, v| InstKind::Shl { ty: t, a: v[0], b: v[1] })),
        ("shr", Box::new(|t, v| InstKind::Shr { ty: t, a: v[0], b: v[1] })),
    ];
    for ty in types() {
        for (op, kind) in &ops {
            funcs.push(func(format!("{op}.{}", name(ty)), &int, VTy::Int, |v| kind(ty, v)));
        }
        funcs.push(func(format!("bnot.{}", name(ty)), &[VTy::Int], VTy::Int, |v| InstKind::BitNot { ty, a: v[0] }));
        if ty.signed {
            funcs.push(func(format!("neg.{}", name(ty)), &[VTy::Int], VTy::Int, |v| InstKind::Neg { ty, a: v[0] }));
        }
    }
    let image = compile(funcs);
    let mut vm = Vm::new(&image, Host::captured(Default::default()), &Config::default()).expect("loads");
    let mut rng = Lcg(0x10AD);
    let mut cases = 0;
    let mut f = 0u32;
    for ty in types() {
        let values = samples(ty, &mut rng);
        let first = f;
        for &a in &values {
            for &b in &values {
                // Two big u64s multiply past even i128: then the product is
                // just "far out of range", with the right sign.
                let product = a.checked_mul(b).unwrap_or(if (a < 0) != (b < 0) { i128::MIN } else { i128::MAX });
                let want = [
                    ty.checked(a + b).map_or(Out::Trap, Out::Value),
                    Out::Value(ty.wrap(a + b)),
                    Out::Value(ty.saturate(a + b)),
                    ty.checked(a - b).map_or(Out::Trap, Out::Value),
                    Out::Value(ty.wrap(a - b)),
                    Out::Value(ty.saturate(a - b)),
                    ty.checked(product).map_or(Out::Trap, Out::Value),
                    Out::Value(ty.wrap(a.wrapping_mul(b))),
                    Out::Value(ty.saturate(product)),
                    if b == 0 { Out::Trap } else { ty.checked(a / b).map_or(Out::Trap, Out::Value) },
                    if b == 0 { Out::Trap } else { Out::Value(a % b) },
                ];
                for (k, want) in want.iter().enumerate() {
                    let got = out(vm.call(first + k as u32, &[a as u64, b as u64]), ty, &TRAPS);
                    assert_eq!(got, *want, "{} {} {a} {b}", ops[k].0, name(ty));
                    cases += 1;
                }
            }
            for n in [0i128, 1, 3, i128::from(ty.bits) - 1, i128::from(ty.bits), 64, -1] {
                let in_range = n >= 0 && n < i128::from(ty.bits);
                let shl = if in_range { Out::Value(ty.wrap(a << n)) } else { Out::Trap };
                let shr = if in_range { Out::Value(a >> n) } else { Out::Trap };
                assert_eq!(
                    out(vm.call(first + 11, &[a as u64, n as u64]), ty, &TRAPS),
                    shl,
                    "shl {} {a} {n}",
                    name(ty)
                );
                assert_eq!(
                    out(vm.call(first + 12, &[a as u64, n as u64]), ty, &TRAPS),
                    shr,
                    "shr {} {a} {n}",
                    name(ty)
                );
                cases += 2;
            }
            let bnot = out(vm.call(first + 13, &[a as u64]), ty, &TRAPS);
            assert_eq!(bnot, Out::Value(ty.wrap(!a)), "bnot {} {a}", name(ty));
            if ty.signed {
                let neg = out(vm.call(first + 14, &[a as u64]), ty, &TRAPS);
                assert_eq!(neg, ty.checked(-a).map_or(Out::Trap, Out::Value), "neg {} {a}", name(ty));
            }
            cases += 2;
        }
        f += ops.len() as u32 + 1 + u32::from(ty.signed);
    }
    assert!(cases > 100_000, "only {cases} cases");
}

#[test]
fn casts_match_the_oracle() {
    let mut funcs = Vec::new();
    for to in types() {
        for from in types() {
            for policy in [Policy::Trap, Policy::Wrap, Policy::Sat] {
                let f = func(format!("cast.{}.{}", name(from), name(to)), &[VTy::Int], VTy::Int, |v| {
                    InstKind::IntCast { policy, to, from, a: v[0] }
                });
                funcs.push(f);
            }
        }
    }
    let image = compile(funcs);
    let mut vm = Vm::new(&image, Host::captured(Default::default()), &Config::default()).expect("loads");
    let mut rng = Lcg(0xCA57);
    let mut f = 0;
    for to in types() {
        for from in types() {
            for v in samples(from, &mut rng) {
                let fits = to.contains(v);
                let label = format!("{} -> {} {v}", name(from), name(to));
                let call = |vm: &mut Vm, k: u32| out(vm.call(f + k, &[v as u64]), to, &TRAPS);
                assert_eq!(call(&mut vm, 0), if fits { Out::Value(v) } else { Out::Trap }, "as {label}");
                assert_eq!(call(&mut vm, 1), Out::Value(to.wrap(v)), "as% {label}");
                assert_eq!(call(&mut vm, 2), Out::Value(to.saturate(v)), "as| {label}");
            }
            f += 3;
        }
    }
}

const F64S: [f64; 16] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.1,
    2.5,
    -7.75,
    1e300,
    -1e300,
    f64::MIN_POSITIVE,
    f64::MAX,
    3.4e38,
    9.223372036854776e18,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
];

fn same_f64(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

#[test]
fn floats_match_rust() {
    let f64s = [VTy::F64, VTy::F64];
    let f32s = [VTy::F32, VTy::F32];
    let fops = [FOp::Add, FOp::Sub, FOp::Mul, FOp::Div, FOp::Rem];
    let cmps = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge];
    let mut funcs = Vec::new();
    for (ty, params, vty) in [(FloatTy::F64, f64s, VTy::F64), (FloatTy::F32, f32s, VTy::F32)] {
        for op in fops {
            funcs.push(func(format!("{op:?}"), &params, vty, |v| InstKind::FArith { op, ty, a: v[0], b: v[1] }));
        }
        for cmp in cmps {
            funcs.push(func(format!("{cmp:?}"), &params, VTy::Int, |v| InstKind::FCmp { cmp, ty, a: v[0], b: v[1] }));
        }
    }
    let image = compile(funcs);
    let mut vm = Vm::new(&image, Host::captured(Default::default()), &Config::default()).expect("loads");
    let arith64: [fn(f64, f64) -> f64; 5] = [|a, b| a + b, |a, b| a - b, |a, b| a * b, |a, b| a / b, |a, b| a % b];
    let arith32: [fn(f32, f32) -> f32; 5] = [|a, b| a + b, |a, b| a - b, |a, b| a * b, |a, b| a / b, |a, b| a % b];
    let cmp64: [fn(f64, f64) -> bool; 6] =
        [|a, b| a == b, |a, b| a != b, |a, b| a < b, |a, b| a <= b, |a, b| a > b, |a, b| a >= b];
    let cmp32: [fn(f32, f32) -> bool; 6] =
        [|a, b| a == b, |a, b| a != b, |a, b| a < b, |a, b| a <= b, |a, b| a > b, |a, b| a >= b];
    for &a in &F64S {
        for &b in &F64S {
            for (k, op) in arith64.iter().enumerate() {
                let got = f64::from_bits(vm.call(k as u32, &[a.to_bits(), b.to_bits()]).expect("returns")[0]);
                assert!(same_f64(got, op(a, b)), "{:?} {a} {b}: {got}", fops[k]);
            }
            for (k, cmp) in cmp64.iter().enumerate() {
                let got = vm.call(5 + k as u32, &[a.to_bits(), b.to_bits()]).expect("returns")[0];
                assert_eq!(got, u64::from(cmp(a, b)), "{:?} {a} {b}", cmps[k]);
            }
            let (a32, b32) = (a as f32, b as f32);
            let args = [u64::from(a32.to_bits()), u64::from(b32.to_bits())];
            for (k, op) in arith32.iter().enumerate() {
                let got = f32::from_bits(vm.call(11 + k as u32, &args).expect("returns")[0] as u32);
                let want = op(a32, b32);
                assert!(
                    (got.is_nan() && want.is_nan()) || got.to_bits() == want.to_bits(),
                    "f32 {:?} {a32} {b32}",
                    fops[k]
                );
            }
            for (k, cmp) in cmp32.iter().enumerate() {
                let got = vm.call(16 + k as u32, &args).expect("returns")[0];
                assert_eq!(got, u64::from(cmp(a32, b32)), "f32 {:?} {a32} {b32}", cmps[k]);
            }
        }
    }
}

#[test]
fn conversions_match_rust() {
    let mut funcs = Vec::new();
    let ints = types();
    for &t in &ints {
        funcs.push(func(format!("itof64.{}", name(t)), &[VTy::Int], VTy::F64, |v| InstKind::IntToFloat {
            from: t,
            to: FloatTy::F64,
            a: v[0],
        }));
        funcs.push(func(format!("itof32.{}", name(t)), &[VTy::Int], VTy::F32, |v| InstKind::IntToFloat {
            from: t,
            to: FloatTy::F32,
            a: v[0],
        }));
        funcs.push(func(format!("ftoi.{}", name(t)), &[VTy::F64], VTy::Int, |v| InstKind::FloatToInt {
            sat: false,
            from: FloatTy::F64,
            to: t,
            a: v[0],
        }));
        funcs.push(func(format!("ftoi|.{}", name(t)), &[VTy::F64], VTy::Int, |v| InstKind::FloatToInt {
            sat: true,
            from: FloatTy::F64,
            to: t,
            a: v[0],
        }));
    }
    funcs.push(func("f64to32".into(), &[VTy::F64], VTy::F32, |v| InstKind::FloatCast { to: FloatTy::F32, a: v[0] }));
    let image = compile(funcs);
    let mut vm = Vm::new(&image, Host::captured(Default::default()), &Config::default()).expect("loads");
    let mut rng = Lcg(0xF10A);
    for (i, &t) in ints.iter().enumerate() {
        let f = 4 * i as u32;
        for v in samples(t, &mut rng) {
            let got = f64::from_bits(vm.call(f, &[v as u64]).expect("returns")[0]);
            assert_eq!(got.to_bits(), (v as f64).to_bits(), "{v} as f64 from {}", name(t));
            let got = f32::from_bits(vm.call(f + 1, &[v as u64]).expect("returns")[0] as u32);
            assert_eq!(got.to_bits(), (v as f32).to_bits(), "{v} as f32 from {}", name(t));
        }
        for x in F64S.iter().copied().chain([0.5, -0.5, 255.9, -128.9, 4096.0, 1e19, -1e19]) {
            let t_range = if x.is_nan() {
                None
            } else {
                let tr = x.trunc();
                (tr >= t.min() as f64 && tr <= t.max() as f64 && t.contains(tr as i128)).then_some(tr as i128)
            };
            let want = t_range.map_or(Out::Trap, Out::Value);
            assert_eq!(out(vm.call(f + 2, &[x.to_bits()]), t, &TRAPS), want, "{x} as {}", name(t));
            let sat = if x.is_nan() { 0 } else { t_range.unwrap_or(if x > 0.0 { t.max() } else { t.min() }) };
            assert_eq!(out(vm.call(f + 3, &[x.to_bits()]), t, &TRAPS), Out::Value(sat), "{x} as| {}", name(t));
        }
    }
    let last = 4 * ints.len() as u32;
    for x in F64S {
        let got = f32::from_bits(vm.call(last, &[x.to_bits()]).expect("returns")[0] as u32);
        let want = x as f32;
        assert!((got.is_nan() && want.is_nan()) || got.to_bits() == want.to_bits(), "{x} as f32");
    }
}

/// `x op k` with `k` a constant (on either side), which codegen folds into
/// an immediate when it fits 16 bits; and branches on `x cmp k`.
#[test]
fn immediates_match_the_oracle() {
    let ks: [i128; 9] = [0, 1, 2, 7, 255, 32767, -1, -2, -32768];
    let mut funcs = Vec::new();
    let mut cases = Vec::new();
    for ty in types().into_iter().filter(|t| matches!(t.bits, 8 | 32 | 64 | 7)) {
        for &k in ks.iter().filter(|&&k| ty.contains(k)) {
            for (op, policy, flip) in [
                (IntOp::Add, Policy::Trap, false),
                (IntOp::Add, Policy::Trap, true),
                (IntOp::Sub, Policy::Trap, false),
                (IntOp::Add, Policy::Wrap, false),
                (IntOp::Sub, Policy::Wrap, false),
            ] {
                // v0: x, v1: k, v2: the result.
                let (a, b) = if flip { (Value(1), Value(0)) } else { (Value(0), Value(1)) };
                let block = Block {
                    params: vec![Value(0)],
                    insts: vec![
                        Inst {
                            kind: InstKind::Iconst(k as u64),
                            results: vec![Value(1)],
                            span: None,
                            unwind: None,
                            site: None,
                        },
                        Inst {
                            kind: InstKind::Arith { op, policy, ty, a, b },
                            results: vec![Value(2)],
                            span: None,
                            unwind: None,
                            site: None,
                        },
                    ],
                    term: Some(Term::Ret(vec![Value(2)])),
                    term_span: None,
                    term_unwind: None,
                    term_site: None,
                };
                let types = vec![VTy::Int; 3];
                funcs.push(Func {
                    name: format!("{op:?}{policy:?}"),
                    blocks: vec![block],
                    results: vec![VTy::Int],
                    types,
                    slots: Vec::new(),
                    sites: Vec::new(),
                });
                cases.push((ty, k, op, policy, flip, None));
            }
            for cmp in [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge] {
                for flip in [false, true] {
                    // b0(v0): v1 = k; v2 = cmp; br v2, b1, b2. b1: ret 1. b2: ret 0.
                    let (a, b) = if flip { (Value(1), Value(0)) } else { (Value(0), Value(1)) };
                    let entry = Block {
                        params: vec![Value(0)],
                        insts: vec![
                            Inst {
                                kind: InstKind::Iconst(k as u64),
                                results: vec![Value(1)],
                                span: None,
                                unwind: None,
                                site: None,
                            },
                            Inst {
                                kind: InstKind::ICmp { cmp, unsigned: !ty.signed, a, b },
                                results: vec![Value(2)],
                                span: None,
                                unwind: None,
                                site: None,
                            },
                        ],
                        term: Some(Term::Br {
                            cond: Value(2),
                            then: loom_compiler::heddle::Edge {
                                block: loom_compiler::heddle::BlockId(1),
                                args: vec![],
                            },
                            else_: loom_compiler::heddle::Edge {
                                block: loom_compiler::heddle::BlockId(2),
                                args: vec![],
                            },
                        }),
                        term_span: None,
                        term_unwind: None,
                        term_site: None,
                    };
                    let ret = |v: u32, bits: u64| Block {
                        params: vec![],
                        insts: vec![Inst {
                            kind: InstKind::Iconst(bits),
                            results: vec![Value(v)],
                            span: None,
                            unwind: None,
                            site: None,
                        }],
                        term: Some(Term::Ret(vec![Value(v)])),
                        term_span: None,
                        term_unwind: None,
                        term_site: None,
                    };
                    let types = vec![VTy::Int; 5];
                    let blocks = vec![entry, ret(3, 1), ret(4, 0)];
                    funcs.push(Func {
                        name: format!("{cmp:?}"),
                        blocks,
                        results: vec![VTy::Int],
                        types,
                        slots: Vec::new(),
                        sites: Vec::new(),
                    });
                    cases.push((ty, k, IntOp::Add, Policy::Trap, flip, Some(cmp)));
                }
            }
        }
    }
    let image = compile(funcs);
    let text = loom_isa::disassemble(&image);
    assert!(text.contains("addk.") && text.contains("jltk.") && text.contains("jgek."), "immediates are used");
    let mut vm = Vm::new(&image, Host::captured(Default::default()), &Config::default()).expect("loads");
    let mut rng = Lcg(0x1AA);
    for (f, &(ty, k, op, policy, flip, cmp)) in cases.iter().enumerate() {
        for x in samples(ty, &mut rng) {
            let (a, b) = if flip { (k, x) } else { (x, k) };
            let got = out(vm.call(f as u32, &[x as u64]), ty, &TRAPS);
            let want = match (cmp, op, policy) {
                (Some(cmp), _, _) => Out::Value(i128::from(match cmp {
                    Cmp::Eq => a == b,
                    Cmp::Ne => a != b,
                    Cmp::Lt => a < b,
                    Cmp::Le => a <= b,
                    Cmp::Gt => a > b,
                    Cmp::Ge => a >= b,
                })),
                (None, IntOp::Add, Policy::Trap) => ty.checked(a + b).map_or(Out::Trap, Out::Value),
                (None, IntOp::Sub, Policy::Trap) => ty.checked(a - b).map_or(Out::Trap, Out::Value),
                (None, IntOp::Add, _) => Out::Value(ty.wrap(a + b)),
                (None, _, _) => Out::Value(ty.wrap(a - b)),
            };
            assert_eq!(got, want, "{} {op:?} {policy:?} {cmp:?} flip={flip}: {a}, {b}", name(ty));
        }
    }
}
