//! Differential test: the runtime's integer operations (Lua, `runtime/rt`)
//! against the compiler's integer oracle (Rust, `ty::int`), for every width
//! class, operator and overflow policy, on boundary and random values.

use loom_compiler::ty::IntTy;
use loom_host::{RT_CORE, Vm};
use mlua::{Function, Table, Value};

/// What an operation produced: a value, or an unravel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Out {
    Value(i128),
    /// A checked operation's `none`.
    Nil,
    Trap,
}

struct Rt {
    rt: Table,
}

impl Rt {
    fn new(vm: &Vm) -> Self {
        Rt { rt: vm.rt().expect("rt table") }
    }

    fn desc(&self, ty: IntTy) -> Table {
        let int: Function = self.rt.get("int").expect("rt.int");
        int.call((ty.bits, ty.signed, false)).expect("descriptor")
    }

    /// Calls `rt.<name>(args…)`, turning a Loom unravel into `Out::Trap`.
    fn call(&self, name: &str, ty: IntTy, args: impl mlua::IntoLuaMulti) -> Out {
        let f: Function = self.rt.get(name).unwrap_or_else(|_| panic!("rt.{name}"));
        self.outcome(name, ty, f.call::<Value>(args))
    }

    /// Calls the closure `rt.spec(name, d)` makes with the operands only:
    /// what the generated code calls.
    fn call_spec(&self, name: &str, ty: IntTy, d: &Table, args: impl mlua::IntoLuaMulti) -> Out {
        let make: Function = self.rt.get("spec").expect("rt.spec");
        let f: Function = make.call((name, d.clone())).unwrap_or_else(|e| panic!("spec {name}: {e}"));
        self.outcome(name, ty, f.call::<Value>(args))
    }

    /// A conversion's closure, `rt.spec(name, to, from)`, called with the value.
    fn call_cast_spec(&self, name: &str, ty: IntTy, to: &Table, from: &Table, v: i64) -> Out {
        let make: Function = self.rt.get("spec").expect("rt.spec");
        let f: Function = make.call((name, to.clone(), from.clone())).unwrap_or_else(|e| panic!("spec {name}: {e}"));
        self.outcome(name, ty, f.call::<Value>(v))
    }

    fn outcome(&self, name: &str, ty: IntTy, result: mlua::Result<Value>) -> Out {
        match result {
            Ok(Value::Integer(v)) => Out::Value(from_lua(ty, v)),
            Ok(Value::Nil) => Out::Nil,
            Ok(other) => panic!("rt.{name} returned {other:?}"),
            Err(e) => {
                let text = e.to_string();
                assert!(text.contains('\u{1f}'), "rt.{name} raised a non-Loom error: {text}");
                Out::Trap
            }
        }
    }
}

/// A Loom value as the Lua integer that represents it (u64 as a bit pattern).
fn to_lua(v: i128) -> i64 {
    v as u64 as i64
}

fn from_lua(ty: IntTy, v: i64) -> i128 {
    if !ty.signed && ty.bits == 64 { v as u64 as i128 } else { v as i128 }
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

/// The oracle for one policy applied to an exact result.
fn policy(ty: IntTy, exact: i128, which: &str) -> Out {
    match which {
        "" => ty.checked(exact).map_or(Out::Trap, Out::Value),
        "_wrap" => Out::Value(ty.wrap(exact)),
        "_sat" => Out::Value(ty.saturate(exact)),
        "_chk" => ty.checked(exact).map_or(Out::Nil, Out::Value),
        _ => unreachable!(),
    }
}

#[test]
fn arithmetic_matches_the_oracle() {
    let vm = Vm::new().expect("vm");
    let rt = Rt::new(&vm);
    let mut rng = Lcg(0x10AD);
    let mut cases = 0;
    for ty in types() {
        let d = rt.desc(ty);
        let values = samples(ty, &mut rng);
        for &a in &values {
            for &b in &values {
                // Two big u64s multiply past even i128: then the product is
                // just "far out of range", with the right sign; wrapping only
                // needs the low bits.
                let product = a.checked_mul(b).unwrap_or(if (a < 0) != (b < 0) { i128::MIN } else { i128::MAX });
                for (op, exact, wrapped) in
                    [("add", a + b, a + b), ("sub", a - b, a - b), ("mul", product, a.wrapping_mul(b))]
                {
                    for which in ["", "_wrap", "_sat", "_chk"] {
                        let name = format!("{op}{which}");
                        let got = rt.call(&name, ty, (d.clone(), to_lua(a), to_lua(b)));
                        let want =
                            if which == "_wrap" { Out::Value(ty.wrap(wrapped)) } else { policy(ty, exact, which) };
                        assert_eq!(got, want, "{name} {} {a} {b}", ty_name(ty));
                        // The closure the generated code uses agrees.
                        if matches!(which, "" | "_wrap") {
                            let spec = rt.call_spec(&name, ty, &d, (to_lua(a), to_lua(b)));
                            assert_eq!(spec, want, "spec {name} {} {a} {b}", ty_name(ty));
                        }
                        cases += 1;
                    }
                }
                let div = if b == 0 { Out::Trap } else { ty.checked(a / b).map_or(Out::Trap, Out::Value) };
                assert_eq!(rt.call("div", ty, (d.clone(), to_lua(a), to_lua(b))), div, "div {} {a} {b}", ty_name(ty));
                let rem = if b == 0 { Out::Trap } else { Out::Value(a % b) };
                assert_eq!(rt.call("rem", ty, (d.clone(), to_lua(a), to_lua(b))), rem, "rem {} {a} {b}", ty_name(ty));
                assert_eq!(
                    rt.call_spec("div", ty, &d, (to_lua(a), to_lua(b))),
                    div,
                    "spec div {} {a} {b}",
                    ty_name(ty)
                );
                assert_eq!(
                    rt.call_spec("rem", ty, &d, (to_lua(a), to_lua(b))),
                    rem,
                    "spec rem {} {a} {b}",
                    ty_name(ty)
                );
                cases += 4;
            }
            let neg = if ty.signed { ty.checked(-a).map_or(Out::Trap, Out::Value) } else { Out::Value(0) };
            if ty.signed {
                assert_eq!(rt.call("neg", ty, (d.clone(), to_lua(a))), neg, "neg {} {a}", ty_name(ty));
            }
            let not = ty.wrap(!a);
            assert_eq!(rt.call("bnot", ty, (d.clone(), to_lua(a))), Out::Value(not), "bnot {} {a}", ty_name(ty));
            for n in [0i64, 1, 3, (ty.bits - 1) as i64, ty.bits as i64, 64, -1] {
                let in_range = n >= 0 && n < ty.bits as i64;
                let shl = if in_range { Out::Value(ty.wrap(a << n)) } else { Out::Trap };
                let shr = if in_range { Out::Value(a >> n) } else { Out::Trap };
                assert_eq!(rt.call("shl", ty, (d.clone(), to_lua(a), n)), shl, "shl {} {a} {n}", ty_name(ty));
                assert_eq!(rt.call("shr", ty, (d.clone(), to_lua(a), n)), shr, "shr {} {a} {n}", ty_name(ty));
                assert_eq!(rt.call_spec("shl", ty, &d, (to_lua(a), n)), shl, "spec shl {} {a} {n}", ty_name(ty));
                assert_eq!(rt.call_spec("shr", ty, &d, (to_lua(a), n)), shr, "spec shr {} {a} {n}", ty_name(ty));
                cases += 4;
            }
            cases += 2;
        }
    }
    assert!(cases > 10_000, "only {cases} cases");
}

#[test]
fn casts_match_the_oracle() {
    let vm = Vm::new().expect("vm");
    let rt = Rt::new(&vm);
    let mut rng = Lcg(0xCA57);
    for from in types() {
        let s = rt.desc(from);
        let values = samples(from, &mut rng);
        for to in types() {
            let d = rt.desc(to);
            for &v in &values {
                let args = || (d.clone(), s.clone(), to_lua(v));
                let fits = to.contains(v);
                let name = format!("{} -> {} {v}", ty_name(from), ty_name(to));
                assert_eq!(rt.call("cast", to, args()), if fits { Out::Value(v) } else { Out::Trap }, "as {name}");
                assert_eq!(rt.call("cast_wrap", to, args()), Out::Value(to.wrap(v)), "as% {name}");
                // The closures the generated code uses agree.
                let spec = rt.call_cast_spec("cast", to, &d, &s, to_lua(v));
                assert_eq!(spec, if fits { Out::Value(v) } else { Out::Trap }, "spec as {name}");
                let spec = rt.call_cast_spec("cast_wrap", to, &d, &s, to_lua(v));
                assert_eq!(spec, Out::Value(to.wrap(v)), "spec as% {name}");
                assert_eq!(rt.call("cast_sat", to, args()), Out::Value(to.saturate(v)), "as| {name}");
                assert_eq!(rt.call("cast_chk", to, args()), if fits { Out::Value(v) } else { Out::Nil }, "as? {name}");
            }
        }
    }
}

#[test]
fn the_docs_examples_hold() {
    // 01-syntax: `200u8 + 100`, and `300 as% u8` etc.
    let vm = Vm::new().expect("vm");
    let rt = Rt::new(&vm);
    let u8_ = IntTy { signed: false, bits: 8, size: false };
    let i32_ = IntTy { signed: true, bits: 32, size: false };
    let (d, s) = (rt.desc(u8_), rt.desc(i32_));
    assert_eq!(rt.call("add", u8_, (d.clone(), 200, 100)), Out::Trap);
    assert_eq!(rt.call("add_wrap", u8_, (d.clone(), 200, 100)), Out::Value(44));
    assert_eq!(rt.call("add_sat", u8_, (d.clone(), 200, 100)), Out::Value(255));
    assert_eq!(rt.call("add_chk", u8_, (d.clone(), 200, 100)), Out::Nil);
    assert_eq!(rt.call("cast_wrap", u8_, (d.clone(), s.clone(), 300)), Out::Value(44));
    assert_eq!(rt.call("cast_sat", u8_, (d.clone(), s.clone(), 300)), Out::Value(255));
    assert_eq!(rt.call("cast_chk", u8_, (d, s, 300)), Out::Nil);
}

#[test]
fn floats_print_like_rust() {
    let vm = Vm::new().expect("vm");
    let lua = vm.lua();
    let show = |expr: &str| -> String {
        lua.load(format!("local rt = __loomrt; return {expr}")).eval().unwrap_or_else(|e| panic!("{expr}: {e}"))
    };
    for (value, want) in [
        ("17.0", "17"),
        ("-3.0", "-3"),
        ("1.5", "1.5"),
        ("0.1", "0.1"),
        ("1/3", "0.3333333333333333"),
        ("1e20", "100000000000000000000"),
        ("1.25e-7", "0.000000125"),
        ("0/0", "NaN"),
        ("1/0", "inf"),
        ("-1/0", "-inf"),
        ("-0.0", "-0"),
    ] {
        assert_eq!(show(&format!("rt.show(rt.f64, {value}, false)")), want, "{value}");
    }
    assert_eq!(show("rt.show(rt.f64, 17.0, true)"), "17.0");
    assert_eq!(show("rt.fmt(rt.f64, 3.14159, '.2')"), "3.14");
    assert_eq!(show("rt.fmt(rt.int(8, false), 255, '08b')"), "11111111");
    assert_eq!(show("rt.fmt(rt.int(16, false), 224, '04X')"), "00E0");
    assert_eq!(show("rt.fmt(rt.int(8, true), -1, 'x')"), "ff");
    assert_eq!(show("rt.fmt(rt.int(32, true), 42, '>6')"), "    42");
    assert_eq!(show("rt.fmt(rt.str, 'ab', '*^6')"), "**ab**");
    assert_eq!(show("rt.fmt(rt.int(32, true), -5, '+05')"), "-0005");
    assert_eq!(show("rt.fmt(rt.str, 'a\"b\\n', '?')"), "\"a\\\"b\\n\"");
    assert_eq!(show("rt.show(rt.int(64, false), -1, false)"), "18446744073709551615");
    assert_eq!(show("rt.show(rt.opt(rt.int(8, false), false), nil, false)"), "none");
    assert_eq!(show("rt.show(rt.tuple({rt.int(32, true), rt.bool}), {1, true}, true)"), "(1, true)");
    assert!(RT_CORE.contains("rt.ext"));
}

fn ty_name(ty: IntTy) -> String {
    format!("{}{}", if ty.signed { 'i' } else { 'u' }, ty.bits)
}
