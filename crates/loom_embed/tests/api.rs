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
fn add_goes_through_calc() {
    let mut embed = Embed::new("editor");
    embed.fun("add", |a: i64, b: i64| a + b).expect("registers");
    let program = compile(embed, "use editor.add\npub fun calc(a: i64, b: i64) -> i64 { add(a, b) }");
    assert_eq!(call(&program, "calc", &[2, 3]), [5]);
}

#[test]
fn a_constant_and_hardcoded_source_go_through_width() {
    let mut embed = Embed::new("editor");
    embed.constant("TAB_WIDTH", 4_i64).expect("registers");
    embed.source("pub fun width() -> i64 { TAB_WIDTH * 2 }").expect("adds");
    let program = compile(embed, "use editor.width\npub fun calc() -> i64 { width() }");
    assert_eq!(call(&program, "calc", &[]), [8]);
}

#[test]
fn narrow_signed_integers_round_trip() {
    let mut embed = Embed::new("m");
    embed.fun("same32", |x: i32| x).expect("registers");
    embed.fun("same8", |x: i8| x).expect("registers");
    let program = compile(
        embed,
        "use m.{same32, same8}\npub fun neg(x: i32) -> i32 { same32(x) }\npub fun small(x: i8) -> i8 { same8(x) }",
    );
    let out = call(&program, "neg", &[(-1_i32).to_reg()]);
    assert_eq!((out[0], i32::from_reg(out[0])), (u64::MAX, -1));
    let out = call(&program, "small", &[(-128_i8).to_reg()]);
    assert_eq!(i8::from_reg(out[0]), -128);
}

#[test]
fn bool_and_char_round_trip() {
    let mut embed = Embed::new("m");
    embed.fun("flip", |b: bool| !b).expect("registers");
    embed.fun("next", |c: char| char::from_u32(u32::from(c) + 1).unwrap_or('?')).expect("registers");
    let program = compile(
        embed,
        "use m.{flip, next}\npub fun f(b: bool) -> bool { flip(b) }\npub fun n(c: char) -> char { next(c) }",
    );
    assert_eq!(call(&program, "f", &[true.to_reg()]), [0]);
    assert_eq!(char::from_reg(call(&program, "n", &['a'.to_reg()])[0]), 'b');
}

#[test]
fn a_raw_function_over_registers() {
    let mut embed = Embed::new("m");
    embed
        .raw_fun("twice", &["u32"], Some("u32"), |_cx, r: &mut [u64]| {
            r[0] *= 2;
            Ok(())
        })
        .expect("registers");
    let program = compile(embed, "use m.twice\npub fun go(x: u32) -> u32 { twice(x) }");
    assert_eq!(call(&program, "go", &[21]), [42]);
}

#[test]
fn an_error_from_a_raw_function_is_an_unravel() {
    let mut embed = Embed::new("m");
    embed.raw_fun("explode", &[], None, |_cx, _r: &mut [u64]| Err("boom".to_string())).expect("registers");
    let program = compile(embed, "use m.explode\npub fun go() { explode() }");
    let Err(Outcome::Unravel { message, .. }) = program.start(host()).expect("starts").call("go", &[]) else {
        panic!("expected an unravel")
    };
    assert_eq!(message, "boom");
}

#[test]
fn bad_registrations_are_errors() {
    let mut embed = Embed::new("m");
    embed.fun("one", || 1_i64).expect("registers");
    assert!(embed.fun("one", || 2_i64).is_err(), "twice");
    assert!(embed.constant("one", 3_i64).is_err(), "a constant with a function's name");
    assert!(embed.fun("fail", || 1_i64).is_err(), "a keyword");
    assert!(embed.fun("LOUD", || 1_i64).is_err(), "a function in the case of a constant");
    assert!(embed.constant("quiet", 1_i64).is_err(), "a constant in the case of a function");
    assert!(embed.fun("two words", || 1_i64).is_err(), "not one name");
    assert!(embed.fun("x // y", || 1_i64).is_err(), "a comment");
    assert!(embed.raw_fun("t", &["String"], None, |_cx, _r: &mut [u64]| Ok(())).is_err(), "a type that doesn't cross");
}

#[test]
fn an_unknown_name_comes_back_as_diagnostics() {
    let embed = Embed::new("m");
    let Err(diagnostics) = embed.compile("pub fun go() -> i64 { nothing_here }") else { panic!("compiled") };
    assert!(!diagnostics.is_empty());
}

#[test]
fn a_program_starts_twice_and_each_run_has_its_own_state() {
    let mut embed = Embed::new("m");
    let mut count = 0_i64;
    embed
        .fun("next", move || {
            count += 1;
            count
        })
        .expect("registers");
    let program = compile(embed, "use m.next\npub fun go() -> i64 { next() }");
    let (mut a, mut b) = (program.start(host()).expect("starts"), program.start(host()).expect("starts"));
    assert_eq!(a.call("go", &[]).expect("runs"), [1]);
    assert_eq!(a.call("go", &[]).expect("runs"), [2]);
    assert_eq!(b.call("go", &[]).expect("runs"), [1]);
    assert_eq!(program.start(host()).expect("starts").call("go", &[]).expect("runs"), [1]);
}

#[test]
fn calling_what_is_not_there_is_an_error() {
    let program = compile(Embed::new("m"), "pub fun go(x: i64) -> i64 { x }\nfun hidden() {}");
    let mut run = program.start(host()).expect("starts");
    assert!(matches!(run.call("hidden", &[]), Err(Outcome::Crash { .. })));
    assert!(matches!(run.call("go", &[]), Err(Outcome::Crash { .. })), "wrong arity");
}

#[test]
fn text_goes_in_and_a_string_comes_out() {
    let mut embed = Embed::new("m");
    embed
        .raw_fun("len_of", &["str"], Some("u64"), |cx, r| {
            let n = cx.text(r[0], r[1])?.len() as u64;
            r[0] = n;
            Ok(())
        })
        .expect("registers");
    embed
        .raw_fun("to_text", &["u64"], Some("String"), |cx, r| {
            let text = r[1].to_string();
            cx.new_string(text.as_bytes(), r[0])
        })
        .expect("registers");
    embed.raw_fun("empty", &[], Some("String"), |cx, r| cx.new_string(b"", r[0])).expect("registers");
    let program = compile(
        embed,
        "use m.{len_of, to_text, empty}\npub fun bytes() -> u64 { len_of(\"h\u{e9}llo\") }\npub fun digits(n: u64) -> usize { to_text(n).len() }\npub fun nothing() -> usize { empty().len() }",
    );
    assert_eq!(call(&program, "bytes", &[]), [6]);
    assert_eq!(call(&program, "digits", &[42]), [2]);
    assert_eq!(call(&program, "nothing", &[]), [0]);
}
