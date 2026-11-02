//! Type checking tests.

use loom_syntax::SourceDb;
use loom_syntax::diag::{Code, codes, render_plain};

use super::check;
use crate::hir::{ExprKind, StmtKind, resolve};
use crate::modules::MemoryLoader;

/// Resolves and checks `src`; returns the codes of every diagnostic, and
/// the rendered text for failure messages.
fn run(src: &str) -> (Vec<Code>, String) {
    let mut db = SourceDb::new();
    let root = db.add("test.loom", src);
    let resolved = resolve(&mut db, root, &MemoryLoader::default());
    let rendered: Vec<String> = resolved.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
    assert!(resolved.diagnostics.is_empty(), "resolution failed:\n{}", rendered.join("\n"));
    let typed = check(&resolved.program);
    let rendered: Vec<String> = typed.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
    (typed.diagnostics.iter().map(|d| d.code).collect(), rendered.join("\n"))
}

fn ok(src: &str) {
    let (codes, text) = run(src);
    assert!(codes.is_empty(), "unexpected diagnostics:\n{text}");
}

fn codes(src: &str) -> Vec<Code> {
    run(src).0
}

/// The type of the local `name` in `f` after checking.
fn local_type(src: &str, name: &str) -> String {
    let mut db = SourceDb::new();
    let root = db.add("test.loom", src);
    let resolved = resolve(&mut db, root, &MemoryLoader::default());
    let typed = check(&resolved.program);
    let p = &resolved.program;
    // The root module is lowered first, so its locals come before std's.
    let (id, _) = p.locals.iter().enumerate().find(|(_, l)| l.name == name).expect("local exists");
    let local = crate::hir::LocalId(id as u32);
    let ty = typed.results.bodies.iter().find_map(|b| b.local_ty.get(&local)).copied().expect("local has a type");
    typed.tcx.display(p, ty)
}

// ================================================================ things that work

#[test]
fn core_programs_check() {
    ok("use std.io.print\n\
        const LIMIT = 15\n\
        fun fizzbuzz(n: u32) -> String = match (n % 3, n % 5) {\n\
            (0, 0) => \"fizzbuzz\"\n\
            (0, _) => \"fizz\"\n\
            (_, 0) => \"buzz\"\n\
            _ => \"{n}\"\n\
        }\n\
        fun main() {\n\
            for i in 1..=LIMIT { print(fizzbuzz(i)) }\n\
        }");
    ok("fun collatz(start: u64) -> u32 {\n\
            var n = start\n\
            var steps: u32 = 0\n\
            while n != 1 {\n\
                n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 }\n\
                steps += 1\n\
            }\n\
            steps\n\
        }");
}

#[test]
fn literals_take_the_type_they_meet_first() {
    let src = "fun takes(x: u8) {}\nfun f() {\n    for i in 0..10 { takes(i) }\n}";
    ok(src);
    assert_eq!(local_type(src, "i"), "u8");
    // Nothing constrains it: the default.
    assert_eq!(local_type("fun f() {\n    let n = 5\n}", "n"), "i32");
    assert_eq!(local_type("fun f() {\n    let x = 1.5\n}", "x"), "f64");
    // An integer literal meeting a float becomes a float.
    assert_eq!(local_type("fun f() {\n    let y = 1 + 2.5\n}", "y"), "f64");
    assert_eq!(local_type("fun f() {\n    let z: f32 = 0\n}", "z"), "f32");
}

#[test]
fn untyped_constants_fit_each_use() {
    ok("const NUM = 200\nfun a(x: u8) {}\nfun b(x: i64) {}\nfun f() {\n    a(NUM)\n    b(NUM)\n}");
    assert_eq!(codes("const NUM = 300\nfun a(x: u8) {}\nfun f() {\n    a(NUM)\n}"), [codes::T0102]);
}

#[test]
fn widening_is_the_only_implicit_numeric_conversion() {
    ok("fun f(a: u8, b: u16) -> u32 {\n    let c: u16 = a\n    let lt = a < b\n    a + b\n}");
    ok("fun f(hi: u8, lo: u8) -> u16 = hi << 8 | lo");
    assert_eq!(codes("fun f(a: u16) -> u8 = a"), [codes::T0101]);
    assert_eq!(codes("fun f(a: u8) -> i8 = a"), [codes::T0101]);
    assert_eq!(codes("fun f(a: i32) -> f64 = a"), [codes::T0101]);
}

#[test]
fn overflow_operators_and_casts() {
    ok("fun f(x: u8) -> ?u8 {\n    let a: u8 = x +% 10\n    let b: u8 = x +| 10\n    x +? 10\n}");
    ok("fun f(x: f64, n: i32) -> u8 {\n    let a = n as f64\n    let b: ?u8 = 300 as? u8\n    x as| u8\n}");
    assert_eq!(codes("fun f(x: f64) -> f64 = x +% 1.0"), [codes::T0103]);
    assert_eq!(codes("fun f(x: f64) -> u8 = x as% u8"), [codes::T0109]);
    assert_eq!(codes("fun f(s: str) -> u8 = s as u8"), [codes::T0109]);
}

#[test]
fn loops_labels_and_break_values() {
    ok(
        "fun f() -> i32 {\n    var i = 0\n    let found = loop {\n        i += 1\n        if i > 3 { break i }\n    }\n    found\n}",
    );
    ok("fun f() {\n    outer: for a in 0..3 {\n        for b in 0..3 { if a == b { continue outer } }\n    }\n}");
    assert_eq!(codes("fun f() {\n    while true { break 1 }\n}"), [codes::T0111]);
    // A `loop` with no `break` never finishes, so it fits anywhere.
    ok("fun f() -> i32 = loop {}");
}

#[test]
fn named_and_default_arguments() {
    let greet = "fun greet(name: str, loud: bool = false) -> str = name\n";
    ok(&format!(
        "{greet}fun f() {{\n    greet(\"a\")\n    greet(\"a\", loud: true)\n    greet(loud: true, name: \"b\")\n}}"
    ));
    assert_eq!(codes(&format!("{greet}fun f() {{\n    greet()\n}}")), [codes::T0104]);
    assert_eq!(codes(&format!("{greet}fun f() {{\n    greet(\"a\", true, 3)\n}}")), [codes::T0104]);
    assert_eq!(codes(&format!("{greet}fun f() {{\n    greet(\"a\", quiet: true)\n}}")), [codes::T0105]);
    assert_eq!(codes(&format!("{greet}fun f() {{\n    greet(\"a\", name: \"b\")\n}}")), [codes::T0105]);
}

#[test]
fn strings_and_templates() {
    ok("fun f(name: str) -> String = if name == \"\" { \"nobody\" } else { \"hi {name}\" }");
    ok("use std.io.print\nfun f() {\n    print(\"a\")\n    print(3)\n    print(\"{1.5:.2} {'c'} {true}\")\n}");
    // A plain `str` value doesn't become a String by itself.
    assert_eq!(codes("fun f(s: str) -> String = s"), [codes::T0101]);
    // A String can be viewed as a `str` only for the length of a call.
    ok("fun takes(s: str) {}\nfun f(n: i32) {\n    takes(\"{n}\")\n}");
    assert_eq!(codes("fun f(n: i32) -> str = \"{n} items\""), [codes::T0101]);
    assert_eq!(codes("fun f(n: i32) {\n    var s = \"\"\n    s = \"{n}\"\n}"), [codes::T0101]);
}

// ================================================================ mistakes

#[test]
fn literal_ranges_are_checked() {
    assert_eq!(codes("fun f() {\n    let x: u8 = 256\n}"), [codes::T0102]);
    assert_eq!(codes("fun f() {\n    let x: u4 = 16\n}"), [codes::T0102]);
    ok("fun f() {\n    let x: i8 = -128\n    let y: u4 = 15\n}");
    assert_eq!(codes("fun f() {\n    let x: u8 = -1\n}"), [codes::T0102]);
    assert_eq!(codes("fun f(x: u8) -> bool = match x {\n    300 => true\n    _ => false\n}"), [codes::T0102]);
}

#[test]
fn negating_unsigned_values_is_an_error() {
    assert_eq!(codes("fun f(x: u32) -> u32 = -x"), [codes::T0103]);
    assert_eq!(codes("fun f() -> u32 {\n    let a = 5\n    -a\n}"), [codes::T0103]);
}

#[test]
fn mismatches() {
    assert_eq!(codes("fun f() -> i32 = \"no\""), [codes::T0101]);
    assert_eq!(codes("fun f(x: i32) -> i32 = if x { 1 } else { 2 }"), [codes::T0101]);
    assert_eq!(codes("fun f(b: bool) -> i32 = if b { 1 } else { \"two\" }"), [codes::T0101]);
    assert_eq!(codes("fun f() -> i32 {\n    return \"x\"\n}"), [codes::T0101]);
    assert_eq!(codes("fun f() -> i32 {\n    let x = 1\n}"), [codes::T0101]);
    assert_eq!(codes("fun f(a: i32, s: str) -> bool = a == s"), [codes::T0101]);
    assert_eq!(codes("fun f(s: str) -> str = s + s"), [codes::T0103]);
}

#[test]
fn tuple_elements() {
    ok("fun f(p: (i32, str)) -> str = p.1");
    assert_eq!(codes("fun f(p: (i32, str)) -> i32 = p.2"), [codes::T0111]);
}

#[test]
fn assignment_needs_var() {
    assert_eq!(codes("fun f() {\n    let x = 1\n    x = 2\n}"), [codes::T0107]);
    ok("fun f() {\n    var x = 1\n    x = 2\n    x += 3\n    x <<= 1\n}");
    ok("fun f() {\n    var (a, b) = (1, 2.5)\n    (a, b) = (b as i32, 0.5)\n}");
    assert_eq!(codes("fun f() {\n    var a = 1\n    let b = 2\n    (a, b) = (b, a)\n}"), [codes::T0107]);
    assert_eq!(codes("fun f() {\n    3 = 4\n}"), [codes::T0107]);
}

#[test]
fn none_needs_a_type() {
    assert_eq!(codes("fun f() {\n    let x = none\n}"), [codes::T0110]);
    ok("fun f() -> ?u8 {\n    let x: ?u8 = none\n    let y: ?u8 = 5\n    x\n}");
}

#[test]
fn matches_must_cover_everything() {
    assert_eq!(codes("fun f(x: i32) -> bool = match x {\n    0 => true\n}"), [codes::P0101]);
    ok("fun f(b: bool) -> i32 = match b {\n    true => 1\n    false => 0\n}");
    assert_eq!(codes("fun f() {\n    let 5 = 5\n}"), [codes::P0102]);
}

#[test]
fn show_is_required_in_templates_and_print() {
    assert_eq!(codes("use std.io.print\nfun f() {\n    print(())\n}"), [codes::T0108]);
    assert_eq!(codes("fun f() -> String = \"{()}\""), [codes::T0108]);
    ok("fun f() -> String = \"{():?}\"");
    ok("fun f(x: u8) -> String = \"{x +? 10}\"");
}

#[test]
fn unsupported_features_say_so_once_per_item() {
    assert_eq!(codes("fun f(xs: str) {\n    xs.encode_utf16()\n    xs.encode_utf16()\n}"), [codes::U0102]);
}

// ================================================================ results for lowering

#[test]
fn calls_record_where_each_argument_goes() {
    let mut db = SourceDb::new();
    let root = db.add("test.loom", "fun g(a: i32, b: i32 = 2, c: i32 = 3) {}\nfun f() {\n    g(1, c: 9)\n}");
    let resolved = resolve(&mut db, root, &MemoryLoader::default());
    let typed = check(&resolved.program);
    assert!(typed.diagnostics.is_empty());
    let p = &resolved.program;
    let f = p.module(p.root.expect("root")).items["f"];
    let crate::hir::DefKind::Fn(f) = &p.def(f).kind else { panic!() };
    let ExprKind::Block(block) = &f.body.as_ref().expect("body").kind else { panic!() };
    let StmtKind::Expr(call) = &block.stmts[0].kind else { panic!() };
    let call = typed.results.bodies.iter().find_map(|b| b.calls.get(&call.id)).expect("call was checked");
    use super::ArgSource::{Arg, Default};
    assert_eq!(call.args, [Arg(0), Default, Arg(1)]);
}

#[test]
fn every_example_fails_only_with_not_implemented_yet() {
    // The checker doesn't cover everything the examples use yet, but it must
    // never reject them for any other reason.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("loom") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable example");
        let mut db = SourceDb::new();
        let root = db.add(path.display().to_string(), text);
        let resolved = resolve(&mut db, root, &crate::modules::FsLoader::new(&path));
        let typed = check(&resolved.program);
        let wrong: Vec<String> =
            typed.diagnostics.iter().filter(|d| d.code != codes::U0102).map(|d| render_plain(&db, d)).collect();
        assert!(wrong.is_empty(), "{}:\n{}", path.display(), wrong.join("\n"));
        seen += 1;
    }
    assert_eq!(seen, 8);
}

#[test]
fn usize_and_target_follow_the_target() {
    use crate::target::Target;
    // 2^63 is a `usize` only where pointers have 64 bits, and `const if`
    // sees the target's pointer width.
    let src = "const BIG: usize = 9223372036854775808\n\
               const assert(target.pointer_bits == 64)\n\
               fun main() {}";
    let mut db = SourceDb::new();
    let root = db.add("test.loom", src);
    let resolved = resolve(&mut db, root, &MemoryLoader::default());
    let vm = super::check_for(&resolved.program, Target::VM);
    let rendered: Vec<String> = vm.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
    assert!(vm.diagnostics.is_empty(), "{}", rendered.join("\n"));
    let lua = check(&resolved.program);
    let codes: Vec<Code> = lua.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(codes, [codes::T0102, codes::K0101], "{codes:?}");
}
