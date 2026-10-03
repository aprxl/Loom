//! The interpreter on hand-assembled images.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use loom_isa::image::Pad;
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
        natives: vec![
            "out.fmt".into(),
            "out.str".into(),
            "msg.str".into(),
            "msg.fmt".into(),
            "msg.unravel".into(),
            "mem.alloc".into(),
            "mem.free".into(),
            "mem.grow".into(),
        ],
        files: vec!["test.loom".into()],
        entry: Some(0),
        ..Image::default()
    };
    for (name, frame, params, results, code) in funcs {
        let entry = image.code.len() as u32;
        let len = code.len() as u32;
        image.code.extend(code);
        let lines = vec![Line { pc: 0, file: 0, line: entry + 1, column: 1 }];
        image.funcs.push(Func { name: name.into(), entry, len, frame, params, results, lines, pads: vec![] });
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
    assert_eq!(outcome, Outcome::Interrupted);
}

#[test]
fn a_broken_image_never_runs() {
    let (outcome, _) = exec(&image(vec![("main", 1, 0, 0, vec![word(Op::Mov, 5, 0, 0)])]), &Config::default());
    assert!(
        matches!(&outcome, Outcome::Crash { message, .. } if message.starts_with("invalid bytecode")),
        "{outcome:?}"
    );
}

// Native numbers in `image`'s list.
const OUT_FMT: u16 = 0;
const OUT_STR: u16 = 1;
const ALLOC: u16 = 5;
const FREE: u16 = 6;
const GROW: u16 = 7;

/// Prints the number in `r` and a newline, using `r0` and `r1`.
fn print_reg(r: u16) -> Vec<u64> {
    vec![
        word(Op::CallN, r, OUT_FMT, 0),
        word_i32(Op::Ldstr, 0, 0),
        word_i32(Op::Ldi, 1, 1),
        word(Op::CallN, 0, OUT_STR, 0),
    ]
}

#[test]
fn loads_and_stores_extend_by_width() {
    // r2: the object; r3: -2 stored as a byte; r4, r5: it loaded signed
    // and unsigned; r6: their sum.
    let mut code = vec![
        word_i32(Op::Ldi, 0, 16),
        word_i32(Op::Ldi, 1, 8),
        word(Op::CallN, 0, ALLOC, 0),
        word(Op::Mov, 2, 0, 0),
        word(Op::Zero, 2, 0, 0),
        16,
        word_i32(Op::Ldi, 3, -2),
        word(Op::St8, 2, 3, 9),
        word(Op::Ld8S, 4, 2, 9),
        word(Op::Ld8U, 5, 2, 9),
        word(Op::AddTI64, 6, 4, 5),
    ];
    code.extend(print_reg(6));
    // A 32-bit store, read back zero-extended, over the byte.
    code.extend([word_i32(Op::Ldi, 3, -1), word(Op::St32, 2, 3, 8), word(Op::Ld32U, 6, 2, 8)]);
    code.extend(print_reg(6));
    // Copy the eight bytes at 8 to 0, zero those at 8, and read both.
    code.extend([
        word(Op::AddWKU64, 3, 2, 8),
        word(Op::Copy, 2, 3, 0),
        8,
        word(Op::Zero, 3, 0, 0),
        8,
        word(Op::Ld64, 6, 2, 0),
        word(Op::Ld64, 7, 2, 8),
        word(Op::AddTI64, 6, 6, 7),
    ]);
    code.extend(print_reg(6));
    code.extend([
        word(Op::Mov, 0, 2, 0),
        word_i32(Op::Ldi, 1, 16),
        word_i32(Op::Ldi, 2, 8),
        word(Op::CallN, 0, FREE, 0),
        word(Op::Ret, 0, 0, 0),
    ]);
    let (outcome, out) = exec(&image(vec![("main", 8, 0, 0, code)]), &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "252\n4294967295\n4294967295\n"));
}

#[test]
fn frame_slots_are_addressed_from_the_frame() {
    // Bytes 32..40 of the frame are r4: a store there through `lea` shows
    // up in the register, and `stf`/`ldf` reach it by offset.
    let mut code =
        vec![word_i32(Op::Lea, 2, 32), word_i32(Op::Ldi, 3, 1234), word(Op::St64, 2, 3, 0), word(Op::Mov, 5, 4, 0)];
    code.extend(print_reg(5));
    code.extend([word_i32(Op::Ldi, 3, -5), word_i32(Op::StF16, 3, 34), word_i32(Op::LdF16S, 5, 34)]);
    code.extend(print_reg(5));
    code.push(word(Op::Ret, 0, 0, 0));
    let (outcome, out) = exec(&image(vec![("main", 6, 0, 0, code)]), &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "1234\n-5\n"));
}

#[test]
fn a_list_header_grows() {
    // r2..r4: a header (address, length, capacity) in the frame, empty;
    // growing it to hold 3 elements of 8 bytes gives a capacity of 4.
    let mut code =
        vec![word_i32(Op::Ldi, 2, 8), word_i32(Op::Ldi, 3, 0), word_i32(Op::Ldi, 4, 0), word_i32(Op::Lea, 5, 16)];
    // mem.grow(header, 8, 8, 3) with its arguments in r8..r11.
    code.extend([
        word(Op::Mov, 8, 5, 0),
        word_i32(Op::Ldi, 9, 8),
        word_i32(Op::Ldi, 10, 8),
        word_i32(Op::Ldi, 11, 3),
        word(Op::CallN, 8, GROW, 0),
    ]);
    code.extend(print_reg(4));
    // Free the buffer: address, 4 * 8 bytes, aligned to 8.
    code.extend([
        word(Op::Mov, 8, 2, 0),
        word_i32(Op::Ldi, 9, 32),
        word_i32(Op::Ldi, 10, 8),
        word(Op::CallN, 8, FREE, 0),
        word(Op::Ret, 0, 0, 0),
    ]);
    let (outcome, out) = exec(&image(vec![("main", 12, 0, 0, code)]), &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "4\n"));
}

#[test]
fn the_data_area_holds_the_image_data() {
    let mut code = vec![word_i32(Op::Gaddr, 2, 8), word(Op::Ld64, 3, 2, 0), word(Op::AddTKI64, 3, 3, 1)];
    code.extend([word(Op::St64, 2, 3, 0), word(Op::Ld64, 3, 2, 0)]);
    code.extend(print_reg(3));
    code.push(word(Op::Ret, 0, 0, 0));
    let mut img = image(vec![("main", 4, 0, 0, code)]);
    img.data = vec![1, 41];
    let (outcome, out) = exec(&img, &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "42\n"));
}

#[test]
fn bounds_unravel_past_the_end() {
    let code =
        vec![word_i32(Op::Ldi, 0, 5), word_i32(Op::Ldi, 1, 3), word(Op::Bounds, 0, 1, 0), word(Op::Ret, 0, 0, 0)];
    let (outcome, _) = exec(&image(vec![("main", 2, 0, 0, code)]), &Config::default());
    assert!(
        matches!(&outcome, Outcome::Unravel { message, .. } if message == "index 5 is out of bounds (len 3)"),
        "{outcome:?}"
    );
}

#[test]
fn a_function_value_is_called_by_its_number() {
    // fib's number in r0; its window at r4 (linkage r1..r3).
    let mut code = vec![word_i32(Op::Ldfn, 0, 1), word_i32(Op::Ldi, 4, 20), word(Op::CallR, 4, 0, 0x0101)];
    code.extend(print_reg(4));
    code.push(word(Op::Ret, 0, 0, 0));
    let (outcome, out) = exec(&image(vec![("main", 5, 0, 0, code), fib()]), &Config::default());
    assert_eq!((outcome, out.as_str()), (Outcome::Finished, "6765\n"));
    // Called as if it took two arguments: an internal error.
    let code = vec![word_i32(Op::Ldfn, 0, 1), word(Op::CallR, 4, 0, 0x0201), word(Op::Ret, 0, 0, 0)];
    let (outcome, _) = exec(&image(vec![("main", 6, 0, 0, code), fib()]), &Config::default());
    assert!(matches!(&outcome, Outcome::Crash { message, .. } if message.contains("wrong number")), "{outcome:?}");
}

/// Prints `text` (string 2 of `unwinding_image`'s) with `r0` and `r1`.
fn print_text(index: i32, len: i32) -> Vec<u64> {
    vec![word_i32(Op::Ldstr, 0, index), word_i32(Op::Ldi, 1, len), word(Op::CallN, 0, OUT_STR, 0)]
}

/// `main` calls `f`, which overflows. Each has a landing pad around its
/// call (or its addition) that prints and resumes.
fn unwinding_image(f_pad: Vec<u64>) -> Image {
    let mut main = vec![word_i32(Op::Call, 3, 1), word(Op::Ret, 0, 0, 0)];
    main.extend(print_text(2, 6));
    main.push(word(Op::Resume, 0, 0, 0));
    let mut f = vec![word_i32(Op::Ldi, 0, 127), word(Op::AddTI8, 0, 0, 0), word(Op::Ret, 0, 0, 0)];
    f.extend(f_pad);
    let mut img = image(vec![("main", 4, 0, 0, main), ("f", 2, 0, 0, f)]);
    img.strings.push(b"outer\n".to_vec());
    img.strings.push(b"inner\n".to_vec());
    img.strings.push(b"again".to_vec());
    img.funcs[0].pads = vec![Pad { start: 0, end: 1, pad: 2 }];
    img.funcs[1].pads = vec![Pad { start: 1, end: 2, pad: 3 }];
    img
}

#[test]
fn an_unravel_runs_each_frames_landing_pad() {
    let mut pad = print_text(3, 6);
    pad.push(word(Op::Resume, 0, 0, 0));
    let (outcome, out) = exec(&unwinding_image(pad), &Config::default());
    assert_eq!(out, "inner\nouter\n");
    let Outcome::Unravel { message, location } = outcome else { panic!("{outcome:?}") };
    assert_eq!(message, "arithmetic overflow");
    assert_eq!(location.map(|l| l.line), Some(7), "where it overflowed, in `f` (at word 6)");
}

#[test]
fn an_unravel_in_a_landing_pad_replaces_the_first() {
    // f's pad unravels "again": main's pad still runs, and "again" is
    // what the run ends with.
    let pad = vec![word_i32(Op::Ldstr, 0, 4), word_i32(Op::Ldi, 1, 5), word(Op::Unravel, 0, 1, 0)];
    let (outcome, out) = exec(&unwinding_image(pad), &Config::default());
    assert_eq!(out, "outer\n");
    assert!(matches!(&outcome, Outcome::Unravel { message, .. } if message == "again"), "{outcome:?}");
}

#[test]
fn landing_pads_run_across_stack_segments() {
    // `down(n)` recurses a million deep and overflows at the bottom; the
    // unwind pops every frame, across segments, to main's pad.
    let mut main = vec![word_i32(Op::Ldi, 3, 1_000_000), word_i32(Op::Call, 3, 1), word(Op::Ret, 0, 0, 0)];
    main.extend(print_text(2, 6));
    main.push(word(Op::Resume, 0, 0, 0));
    let down = vec![
        word_i32(Op::Ldi, 1, 0),
        word(Op::JEq, 0, 1, 5),
        word_i32(Op::Ldi, 1, 1),
        word(Op::SubTI64, 5, 0, 1),
        word_i32(Op::Call, 5, 1),
        word(Op::AddTI64, 0, 5, 1),
        word(Op::Ret, 0, 1, 0),
        word_i32(Op::Ldi, 1, i32::MAX),
        word(Op::MulTI32, 0, 1, 1),
        word(Op::Ret, 0, 1, 0),
    ];
    let mut img = image(vec![("main", 4, 0, 0, main), ("down", 6, 1, 1, down)]);
    img.strings.push(b"outer\n".to_vec());
    // `down(0)` jumps to its overflow at 7.
    img.code[img.funcs[1].entry as usize + 1] = word(Op::JEq, 0, 1, 6);
    img.funcs[0].pads = vec![Pad { start: 1, end: 2, pad: 3 }];
    let (outcome, out) = exec(&img, &Config::default());
    assert_eq!(out, "outer\n");
    assert!(matches!(&outcome, Outcome::Unravel { message, .. } if message == "arithmetic overflow"), "{outcome:?}");
}

#[cfg(feature = "checked")]
#[test]
fn the_checked_build_catches_leaks_and_bad_frees() {
    let alloc = vec![word_i32(Op::Ldi, 0, 16), word_i32(Op::Ldi, 1, 8), word(Op::CallN, 0, ALLOC, 0)];
    let mut leak = alloc.clone();
    leak.push(word(Op::Ret, 0, 0, 0));
    let (outcome, _) = exec(&image(vec![("main", 3, 0, 0, leak)]), &Config::default());
    assert!(matches!(&outcome, Outcome::Crash { message, .. } if message.contains("never freed")), "{outcome:?}");
    let mut twice = alloc.clone();
    twice.extend([word_i32(Op::Ldi, 1, 16), word_i32(Op::Ldi, 2, 8), word(Op::CallN, 0, FREE, 0)]);
    twice.extend(alloc);
    twice.extend([word_i32(Op::Ldi, 1, 8), word_i32(Op::Ldi, 2, 8), word(Op::CallN, 0, FREE, 0)]);
    twice.push(word(Op::Ret, 0, 0, 0));
    let (outcome, _) = exec(&image(vec![("main", 3, 0, 0, twice)]), &Config::default());
    assert!(matches!(&outcome, Outcome::Crash { message, .. } if message.contains("isn't an object")), "{outcome:?}");
    // A load past the end of an object.
    let mut past = vec![word_i32(Op::Ldi, 0, 16), word_i32(Op::Ldi, 1, 8), word(Op::CallN, 0, ALLOC, 0)];
    past.extend([word(Op::Ld64, 1, 0, 16), word(Op::Ret, 0, 0, 0)]);
    let (outcome, _) = exec(&image(vec![("main", 3, 0, 0, past)]), &Config::default());
    assert!(matches!(&outcome, Outcome::Crash { message, .. } if message.contains("bad memory access")), "{outcome:?}");
}
