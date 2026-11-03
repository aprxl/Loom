use std::cell::RefCell;
use std::rc::Rc;

use loom_embed::{Embed, Program, Scalar};
use loom_sys::{Host, Outcome};

fn host() -> Host {
    Host::captured(Rc::new(RefCell::new(Vec::new())))
}

fn compile(embed: Embed, user: &str) -> Program {
    embed.compile(user).unwrap_or_else(|d| panic!("{d:?}"))
}

fn call(program: &Program, name: &str, args: &[u64]) -> Vec<u64> {
    program.start(host()).expect("starts").call(name, args).expect("runs")
}

#[test]
fn every_arity_takes_mixed_scalars_and_returns_a_string() {
    let mut embed = Embed::new("m");
    embed.fun0("zero", || 7_i64).expect("registers");
    embed.fun1("neg", |x: i64| -x).expect("registers");
    embed.fun2("mix", |x: f64, y: f32| x * f64::from(y)).expect("registers");
    embed.fun3("pick", |a: bool, c: char, n: u8| if a { u64::from(n) + u64::from(c) } else { 0 }).expect("registers");
    embed.fun2("text", |n: i64, c: char| c.to_string().repeat(n as usize)).expect("registers");
    let program = compile(
        embed,
        "use m.{zero, neg, mix, pick, text}\n\
         pub fun t0() -> i64 { zero() }\n\
         pub fun t1(x: i64) -> i64 { neg(x) }\n\
         pub fun t2(x: f64, y: f32) -> f64 { mix(x, y) }\n\
         pub fun t3(a: bool, c: char, n: u8) -> u64 { pick(a, c, n) }\n\
         pub fun t4(n: i64, c: char) -> usize { text(n, c).len() }\n",
    );
    assert_eq!(call(&program, "t0", &[]), [7]);
    assert_eq!(i64::from_reg(call(&program, "t1", &[5_i64.to_reg()])[0]), -5);
    assert_eq!(f64::from_reg(call(&program, "t2", &[1.5_f64.to_reg(), 2.0_f32.to_reg()])[0]), 3.0);
    assert_eq!(call(&program, "t3", &[true.to_reg(), 'a'.to_reg(), 1_u8.to_reg()]), [98]);
    assert_eq!(call(&program, "t4", &[3_i64.to_reg(), 'é'.to_reg()]), [6]);
}

#[test]
fn f32_nan_and_negative_zero_round_trip_bit_exact() {
    let mut embed = Embed::new("m");
    embed.fun1("same32", |x: f32| x).expect("registers");
    let program = compile(embed, "use m.same32\npub fun id(x: f32) -> f32 { same32(x) }\n");
    for bits in [0x7fc0_0001_u32, 0x8000_0000, 0x7f80_0000, 1] {
        let out = call(&program, "id", &[u64::from(bits)]);
        assert_eq!(f32::from_reg(out[0]).to_bits(), bits);
    }
}

#[test]
fn a_raw_function_takes_text_and_returns_a_string() {
    let mut embed = Embed::new("m");
    embed
        .fun("pad_text", &["str", "i64"], Some("String"), |cx, r| {
            let text = std::str::from_utf8(cx.text(r[1], r[2])?).map_err(|e| e.to_string())?.to_string();
            let n = r[3] as i64;
            cx.new_string(text.repeat(n as usize).as_bytes(), r[0])
        })
        .expect("registers");
    let program = compile(embed, "use m.pad_text\npub fun go(n: i64) -> usize { pad_text(\"h\u{e9}llo\", n).len() }\n");
    assert_eq!(call(&program, "go", &[2]), [12]);
}

#[test]
fn a_handle_is_copy_and_its_methods_see_one_value() {
    let mut embed = Embed::new("m");
    let buffer = embed.handle::<i64>("Buffer").expect("declares");
    embed.constructor1(&buffer, "open", |x: i64| x).expect("registers");
    embed
        .method1(&buffer, "add", |v: &mut i64, d: i64| {
            *v += d;
            *v
        })
        .expect("registers");
    embed.method0(&buffer, "get", |v: &mut i64| *v).expect("registers");
    let program = compile(
        embed,
        "use m.{open}\npub fun twice(a: i64) -> i64 {\n    let b = open(a)\n    let b2 = b\n    b.add(1) + b2.get()\n}\n",
    );
    assert_eq!(call(&program, "twice", &[10]), [22]);
}

#[test]
fn outside_code_cannot_build_a_handle_from_its_id() {
    let mut embed = Embed::new("m");
    let buffer = embed.handle::<i64>("Buffer").expect("declares");
    embed.constructor1(&buffer, "open", |x: i64| x).expect("registers");
    let Err(diagnostics) = embed.compile("use m.{Buffer}\npub fun bad() -> Buffer { Buffer(id: 1) }\n") else {
        panic!("compiled")
    };
    assert!(diagnostics.iter().any(|d| d.code.id == "N0103"), "{diagnostics:?}");
}

#[test]
fn a_stale_id_is_an_unravel() {
    let mut embed = Embed::new("m");
    let buffer = embed.handle::<i64>("Buffer").expect("declares");
    embed.method0(&buffer, "get", |v: &mut i64| *v).expect("registers");
    embed.source("pub fun forged() -> i64 { Buffer(id: 99).get() }").expect("adds");
    let program = compile(embed, "use m.forged\npub fun go() -> i64 { forged() }\n");
    let Err(Outcome::Unravel { message, .. }) = program.start(host()).expect("starts").call("go", &[]) else {
        panic!("expected an unravel")
    };
    assert_eq!(message, "stale handle 99");
}

#[test]
fn a_non_snake_case_function_or_non_pascal_handle_is_rejected() {
    let mut embed = Embed::new("m");
    assert!(embed.handle::<i64>("buffer").is_err(), "a handle is PascalCase");
    assert!(embed.fun0("Zero", || 1_i64).is_err(), "a function is snake_case");
    assert!(embed.fun0("f64", || 1_i64).is_err(), "a keyword type name");
}
