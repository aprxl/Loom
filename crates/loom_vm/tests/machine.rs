//! The interpreter on hand-assembled images.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use loom_isa::word::{word, word_i32};
use loom_isa::{FmtItem, Func, Image, IntSpec, Line, Op, ValueKind};
use loom_sys::{Host, Location, Outcome};
use loom_vm::{Config, run};

/// Functions as (name, frame, params, results, code), laid out one after
/// another. The first is the entry.
fn image(funcs: Vec<(&str, u16, u16, u16, Vec<u64>)>) -> Image {
    let mut image = Image {
        strings: vec![b"\n".to_vec(), b"boom: ".to_vec()],
        fmts: vec![FmtItem { kind: ValueKind::Int(IntSpec::new(64, true)), debug: false, spec: None }],
        natives: vec!["out.fmt".into(), "out.str".into(), "msg.str".into(), "msg.fmt".into(), "msg.unravel".into()],
        files: vec!["test.loom".into()],
        entry: Some(0),
        ..Image::default()
    };
    for (name, frame, params, results, code) in funcs {
        let entry = image.code.len() as u32;
        let len = code.len() as u32;
        image.code.extend(code);
        let lines = vec![Line { pc: 0, file: 0, line: entry + 1, column: 1 }];
        image.funcs.push(Func { name: name.into(), entry, len, frame, params, results, lines });
    }
    image
}

fn exec(image: &Image, config: &Config) -> (Outcome, String) {
    let buffer = Rc::new(RefCell::new(Vec::new()));
    let outcome = run(image, Host::captured(buffer.clone()), config);
    let out = String::from_utf8(buffer.borrow().clone()).expect("utf-8");
    (outcome, out)
}

/// `main`: calls function 1 with `arg`, prints the result and a newline.
fn main_printing(arg: i32) -> (&'static str, u16, u16, u16, Vec<u64>) {
    // r0, r1: text; r0..r2: linkage; r3: window.
    let code = vec![
        word_i32(Op::Ldi, 3, arg),
        word_i32(Op::Call, 3, 1),
        word(Op::CallN, 3, 0, 0),
        word_i32(Op::Ldstr, 0, 0),
        word_i32(Op::Ldi, 1, 1),
        word(Op::CallN, 0, 1, 0),
        word(Op::Ret, 0, 0, 0),
    ];
    ("main", 4, 0, 0, code)
}

fn fib() -> (&'static str, u16, u16, u16, Vec<u64>) {
    // r0: n, r1: a constant, r2: fib(n - 1), r3..r5: linkage, r6: window.
    let code = vec![
        word_i32(Op::Ldi, 1, 2),
        word(Op::JLtS, 0, 1, 9),
        word_i32(Op::Ldi, 1, 1),
        word(Op::SubTI64, 6, 0, 1),
        word_i32(Op::Call, 6, 1),
        word(Op::Mov, 2, 6, 0),
        word_i32(Op::Ldi, 1, 2),
        word(Op::SubTI64, 6, 0, 1),
        word_i32(Op::Call, 6, 1),
        word(Op::AddTI64, 0, 2, 6),
        word(Op::Ret, 0, 1, 0),
    ];
    ("fib", 7, 1, 1, code)
}

#[test]
fn runs_fib() {
    let (outcome, out) = exec(&image(vec![main_printing(25), fib()]), &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "75025\n"));
}

/// `down(n) = if n == 0 { 0 } else { down(n - 1) + 1 }`.
fn down() -> (&'static str, u16, u16, u16, Vec<u64>) {
    // r0: n, r1: a constant, r2..r4: linkage, r5: window.
    let code = vec![
        word_i32(Op::Ldi, 1, 0),
        word(Op::JEq, 0, 1, 5),
        word_i32(Op::Ldi, 1, 1),
        word(Op::SubTI64, 5, 0, 1),
        word_i32(Op::Call, 5, 1),
        word(Op::AddTI64, 0, 5, 1),
        word(Op::Ret, 0, 1, 0),
    ];
    ("down", 6, 1, 1, code)
}

#[test]
fn deep_recursion_crosses_stack_segments() {
    // A million frames of 6 slots each is many segments; each result must
    // come back across the boundary its frame started.
    let (outcome, out) = exec(&image(vec![main_printing(1_000_000), down()]), &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "1000000\n"));
}

#[test]
fn endless_recursion_is_a_stack_overflow() {
    let forever = vec![word_i32(Op::Call, 4, 1), word(Op::Ret, 4, 1, 0)];
    let config = Config { stack_limit: 4 << 20, ..Config::default() };
    let (outcome, _) = exec(&image(vec![main_printing(0), ("forever", 5, 1, 1, forever)]), &config);
    let Outcome::Unravel { message, location } = outcome else { panic!("{outcome:?}") };
    assert_eq!(message, "stack overflow: too many calls inside each other");
    assert_eq!(location.map(|l| l.line), Some(8), "at the call in `forever`");
}

#[test]
fn traps_carry_the_location_of_their_code() {
    let code = vec![word_i32(Op::Ldi, 0, 100), word(Op::AddTI8, 0, 0, 0), word(Op::Ret, 0, 0, 0)];
    let mut img = image(vec![("main", 1, 0, 0, code)]);
    img.funcs[0].lines = vec![Line { pc: 0, file: 0, line: 3, column: 5 }, Line { pc: 1, file: 0, line: 4, column: 9 }];
    let (outcome, _) = exec(&img, &Config::default());
    let location = Some(Location { file: "test.loom".into(), line: 4, column: 9 });
    assert_eq!(outcome, Outcome::Unravel { message: "arithmetic overflow".into(), location });
}

#[test]
fn unravels_with_a_built_message() {
    let code = vec![
        word_i32(Op::Ldstr, 0, 1),
        word_i32(Op::Ldi, 1, 6),
        word(Op::CallN, 0, 2, 0),
        word_i32(Op::Ldi, 0, -42),
        word(Op::CallN, 0, 3, 0),
        word(Op::CallN, 0, 4, 0),
        word(Op::Ret, 0, 0, 0),
    ];
    let (outcome, _) = exec(&image(vec![("main", 2, 0, 0, code)]), &Config::default());
    assert!(matches!(&outcome, Outcome::Unravel { message, .. } if message == "boom: -42"), "{outcome:?}");
}

#[test]
fn an_interrupt_stops_a_loop() {
    let code = vec![word_i32(Op::Loop, 0, 0)];
    let config = Config { interrupt: Some(Arc::new(AtomicBool::new(true))), ..Config::default() };
    let (outcome, _) = exec(&image(vec![("main", 1, 0, 0, code)]), &config);
    assert!(matches!(&outcome, Outcome::Unravel { message, .. } if message == "interrupted (Ctrl-C)"), "{outcome:?}");
}

#[test]
fn a_broken_image_never_runs() {
    let (outcome, _) = exec(&image(vec![("main", 1, 0, 0, vec![word(Op::Mov, 5, 0, 0)])]), &Config::default());
    assert!(
        matches!(&outcome, Outcome::Crash { message, .. } if message.starts_with("invalid bytecode")),
        "{outcome:?}"
    );
}
