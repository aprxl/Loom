//! Name resolution tests.

use loom_syntax::diag::{Code, codes, render_plain};
use loom_syntax::{Diagnostic, SourceDb};

use super::{DefKind, ExprKind, Program, pretty, resolve};
use crate::modules::{MemoryLoader, Source};

struct Out {
    db: SourceDb,
    program: Program,
    diags: Vec<Diagnostic>,
}

fn run_with(src: &str, user: &[(&str, &str)]) -> Out {
    let mut db = SourceDb::new();
    let root = db.add("test.loom", src);
    let loader = MemoryLoader {
        user: user
            .iter()
            .map(|(path, text)| {
                let segments = path.split('.').map(str::to_string).collect();
                (segments, Source { name: format!("{path}.loom"), text: text.to_string() })
            })
            .collect(),
    };
    let resolved = resolve(&mut db, root, &loader);
    Out { db, program: resolved.program, diags: resolved.diagnostics }
}

fn run(src: &str) -> Out {
    run_with(src, &[])
}

fn codes_of(src: &str) -> Vec<Code> {
    run(src).diags.iter().map(|d| d.code).collect()
}

fn rendered(out: &Out) -> String {
    out.diags.iter().map(|d| render_plain(&out.db, d)).collect::<Vec<_>>().join("\n")
}

/// The HIR of the root module's item `name`, asserting a clean resolution.
fn hir_of(src: &str, name: &str) -> String {
    let out = run(src);
    assert!(out.diags.is_empty(), "unexpected diagnostics:\n{}", rendered(&out));
    item(&out, name)
}

fn item(out: &Out, name: &str) -> String {
    let p = &out.program;
    let root = p.root.expect("a root module");
    let def = *p.module(root).items.get(name).unwrap_or_else(|| panic!("no item `{name}`"));
    pretty::def_str(p, def)
}

// ================================================================ locals and scopes

#[test]
fn let_binds_after_its_initializer() {
    assert_eq!(
        hir_of("fun f() {\n    let x = 1\n    let x = x + 1\n    x\n}", "f"),
        "(fun f() (block (let x#1 1) (let x#2 (+ x#1 1)) x#2))"
    );
}

#[test]
fn parameters_are_in_scope_for_later_types_and_the_body() {
    assert_eq!(
        hir_of("fun zeros(const n: usize) -> [f32; n] = [0.0; n]", "zeros"),
        "(fun zeros(const n#1: usize) -> [f32; n#1] (array-repeat 0.0 n#1))"
    );
}

#[test]
fn items_in_a_block_are_visible_in_the_whole_block() {
    assert_eq!(
        hir_of("fun f() -> i32 {\n    let y = helper()\n    fun helper() -> i32 = 1\n    y\n}", "f"),
        "(fun f() -> i32 (block (let y#1 (call helper)) (fun helper() -> i32 1) y#1))"
    );
}

#[test]
fn nested_functions_dont_see_outer_locals() {
    assert_eq!(codes_of("fun f() {\n    let x = 1\n    fun g() -> i32 = x\n}"), [codes::N0101]);
}

#[test]
fn is_bindings_flow_through_and_into_the_body() {
    assert_eq!(
        hir_of("fun f(o: ?i32) {\n    if o is some(v) and v > 0 { g(v) }\n}\nfun g(x: i32) {}", "f"),
        "(fun f(o#1: ?i32) (block (if (and (is o#1 (some v#2)) (> v#2 0)) (block (call g v#2)))))"
    );
    assert_eq!(
        hir_of("fun f(q: List[i32]) {\n    while q.pop() is some(j) { g(j) }\n}\nfun g(x: i32) {}", "f"),
        "(fun f(q#1: std.list.List[i32]) (block (while 'L1 (is (call (. q#1 pop)) (some j#2)) (block (call g j#2)))))"
    );
}

#[test]
fn is_bindings_dont_leak_out_of_plain_expressions() {
    assert_eq!(codes_of("fun f(o: ?i32) -> i32 {\n    let b = o is some(v)\n    v\n}"), [codes::N0101]);
    assert_eq!(codes_of("fun f(o: ?i32) -> i32 {\n    if o is some(v) or true { v } else { 0 }\n}"), [codes::N0101]);
    // An `and` outside a condition still lets its right side see the binding.
    assert!(codes_of("fun f(o: ?i32) -> bool = o is some(v) and v > 0").is_empty());
}

#[test]
fn match_arms_guards_and_mend_bind_their_names() {
    assert_eq!(
        hir_of("fun f(o: ?i32) -> i32 = match o {\n    some(v) if v > 1 => v\n    _ => 0\n}", "f"),
        "(fun f(o#1: ?i32) -> i32 (match o#1 (=> (some v#2) (if (> v#2 1)) v#2) (=> _ 0)))"
    );
    assert_eq!(
        hir_of("fun f(r: i32 ! Missing) -> i32 = r mend e => g(e)\nfun g(e: Missing) -> i32 = 0", "f"),
        "(fun f(r#1: (i32 ! std.error.Missing)) -> i32 (mend r#1 (e#2 => (call g e#2))))"
    );
}

#[test]
fn or_patterns_share_their_bindings() {
    assert_eq!(
        hir_of("fun f(p: (i32, i32)) -> i32 = match p {\n    (0, n) | (n, 0) => n\n    _ => 0\n}", "f"),
        "(fun f(p#1: (i32, i32)) -> i32 (match p#1 (=> (| (0, n#2) (n#2, 0)) n#2) (=> _ 0)))"
    );
    assert_eq!(
        codes_of("fun f(p: (i32, i32)) -> i32 = match p {\n    (0, n) | (m, 0) => 0\n    _ => 0\n}"),
        [codes::N0115, codes::N0115]
    );
}

#[test]
fn twill_captures_and_defer_bindings_are_locals() {
    assert_eq!(
        hir_of("fun f(op: u16) -> u16 = match op {\n    0x8xy4 => x + y\n    _ => 0\n}", "f"),
        "(fun f(op#1: u16) -> u16 (match op#1 (=> (twill w16 mask=0xF00F value=0x8004 x:u4 y:u4 [x#2 y#3]) (+ x#2 y#3)) (=> _ 0)))"
    );
    assert_eq!(
        hir_of("fun f() {\n    var tx = 1\n    defer tx => g(tx)\n}\nfun g(x: i32) {}", "f"),
        "(fun f() (block (var (mut tx#1) 1) (defer tx#1 => (call g tx#1))))"
    );
    assert_eq!(codes_of("fun f() {\n    defer nope => g()\n}\nfun g() {}"), [codes::N0101]);
}

// ================================================================ hole lambdas

#[test]
fn holes_belong_to_the_innermost_argument() {
    let f = |body: &str| {
        hir_of(
            &format!(
                "fun f(xs: List[i32]) = {body}\nfun clamp(x: i32, lo: i32, hi: i32) -> i32 = x\nfun g(h: i32 => i32) = 0"
            ),
            "f",
        )
    };
    assert_eq!(f("xs.map(_ * 2)"), "(fun f(xs#1: std.list.List[i32]) (call (. xs#1 map) (fun (_0#2) (* _0#2 2))))");
    assert_eq!(
        f("xs.reduce(_ + _)"),
        "(fun f(xs#1: std.list.List[i32]) (call (. xs#1 reduce) (fun (_0#2, _1#3) (+ _0#2 _1#3))))"
    );
    assert_eq!(
        f("xs.map(clamp(_, 0, 255))"),
        "(fun f(xs#1: std.list.List[i32]) (call (. xs#1 map) (fun (_0#2) (call clamp _0#2 0 255))))"
    );
    // The inner argument is its own level.
    assert_eq!(
        f("g(clamp(_ + 1, 0, 9))"),
        "(fun f(xs#1: std.list.List[i32]) (call g (call clamp (fun (_0#2) (+ _0#2 1)) 0 9)))"
    );
    assert_eq!(
        f("xs.filter(_.len() > 1)"),
        "(fun f(xs#1: std.list.List[i32]) (call (. xs#1 filter) (fun (_0#2) (> (call (. _0#2 len)) 1))))"
    );
}

#[test]
fn a_bare_hole_argument_makes_the_call_a_lambda() {
    assert_eq!(
        hir_of("fun f() {\n    let h = clamp(_, 0, 255)\n}\nfun clamp(x: i32, lo: i32, hi: i32) -> i32 = x", "f"),
        "(fun f() (block (let h#2 (fun (_0#1) (call clamp _0#1 0 255)))))"
    );
}

#[test]
fn holes_elsewhere_are_errors() {
    assert_eq!(codes_of("fun f() {\n    let x = _ + 1\n}"), [codes::N0109]);
    assert_eq!(codes_of("fun f(xs: List[i32]) = xs.map(x => _ + x)"), [codes::N0109]);
}

#[test]
fn gauge_predicates_are_hole_lambdas() {
    assert_eq!(
        hir_of("type Even = i64 where _ % 2 == 0", "Even"),
        "(type Even = i64 where (fun (_0#1) (== (% _0#1 2) 0)))"
    );
}

// ================================================================ loops

#[test]
fn break_and_continue_find_their_loop() {
    assert_eq!(
        hir_of(
            "fun f(g: List[List[i32]]) {\n    outer: for r in g {\n        for c in r { if c == 0 { break outer } else { continue } }\n    }\n}",
            "f"
        ),
        "(fun f(g#1: std.list.List[std.list.List[i32]]) (block (for 'L1 r#2 g#1 (block (for 'L2 c#3 r#2 (block (if (== c#3 0) (block (break 'L1)) (block (continue 'L2)))))))))"
    );
    assert_eq!(codes_of("fun f() {\n    break\n}"), [codes::N0108]);
    // A lambda is its own function.
    assert_eq!(codes_of("fun f(xs: List[i32]) {\n    loop { xs.each(x => { break }) }\n}"), [codes::N0108]);
}

#[test]
fn yield_and_pull_need_a_shuttle() {
    assert!(codes_of("fun gen() ~> i32 {\n    yield 1\n}").is_empty());
    assert_eq!(codes_of("fun f() {\n    yield 1\n    pull\n}"), [codes::N0114, codes::N0114]);
}

// ================================================================ self and Self

#[test]
fn self_only_in_methods() {
    assert_eq!(codes_of("fun f(self) {}"), [codes::N0106]);
    assert_eq!(codes_of("warp P {\n    x: i32\n    fun origin() -> i32 = self.x\n}"), [codes::N0106]);
    assert_eq!(codes_of("fun f() -> Self = 0"), [codes::N0107]);
    assert_eq!(
        hir_of("warp P {\n    x: i32\n    fun get(self) -> Self = self\n}", "P"),
        "(warp P\n  (field x: i32)\n  (fun get(self) -> Self self#1))"
    );
}

// ================================================================ paths, types and modules

#[test]
fn modules_items_and_types_in_expressions() {
    assert_eq!(
        hir_of(
            "use std.env\nwarp Shape {\n    case Dot\n}\nfun f() {\n    let a = env.get(\"X\")\n    let b = Shape.Dot\n    let c = List[i32]()\n    let d = u32.MAX\n}",
            "f"
        ),
        "(fun f() (block (let a#1 (call std.env.get \"X\")) (let b#2 (:: Shape Dot)) (let c#3 (call (type std.list.List[i32]))) (let d#4 (:: u32 MAX))))"
    );
}

#[test]
fn std_is_always_in_scope() {
    assert_eq!(hir_of("fun f() = std.io.print(\"hi\")", "f"), "(fun f() (call std.io.print \"hi\"))");
}

#[test]
fn brackets_are_generic_arguments_on_types_and_functions_only() {
    let src = "fun id[T](x: T) -> T = x\nconst TABLE: [u8; 2] = [1, 2]\nfun f(i: usize) {\n    let a = id[u8](1)\n    let b = TABLE[i]\n}";
    assert_eq!(hir_of(src, "f"), "(fun f(i#1: usize) (block (let a#2 (call id[u8] 1)) (let b#3 (index TABLE i#1))))");
}

#[test]
fn use_forms() {
    let src = "use std.fs.{read as read_file, IoError}\nuse std.{env, time}\nfun f() -> List[u8] ! IoError {\n    time.sleep(time.ms(1))\n    read_file(env.get(\"P\") mend \"\")\n}";
    assert_eq!(
        hir_of(src, "f"),
        "(fun f() -> (std.list.List[u8] ! std.fs.IoError) (block (call std.time.sleep (call std.time.ms 1)) (call std.fs.read (mend (call std.env.get \"P\") \"\"))))"
    );
}

#[test]
fn unknown_names_and_modules() {
    assert_eq!(codes_of("use std.nope"), [codes::N0105]);
    assert_eq!(codes_of("use nowhere.thing"), [codes::N0102]);
    assert_eq!(codes_of("use std.io.nope"), [codes::N0105]);
    assert_eq!(codes_of("fun f() = nope()"), [codes::N0101]);
    assert_eq!(codes_of("use std.fs\nfun f() = fs.nope()"), [codes::N0105]);
}

#[test]
fn unknown_std_names_suggest_a_use() {
    let out = run("fun main() {\n    print(\"hi\")\n}");
    assert_eq!(out.diags.len(), 1);
    assert!(rendered(&out).contains("add `use std.io.print`"), "{}", rendered(&out));
}

#[test]
fn values_types_and_modules_in_the_wrong_place() {
    assert_eq!(codes_of("use std.io.print\nfun f(x: print) {}"), [codes::N0111]);
    assert_eq!(codes_of("use std.fs\nfun f() {\n    let x = fs\n}"), [codes::N0112]);
}

#[test]
fn duplicates_and_import_conflicts() {
    assert_eq!(codes_of("fun f() {}\nfun f() {}"), [codes::N0104]);
    assert_eq!(codes_of("warp P {\n    x: i32\n    x: i32\n}"), [codes::N0104]);
    assert_eq!(codes_of("use std.io.print\nfun print() {}"), [codes::N0110]);
    assert_eq!(codes_of("use std.io.print\nuse std.env.get as print"), [codes::N0110]);
}

#[test]
fn the_prelude_can_be_shadowed() {
    let src = "warp String {\n    s: i32\n}\nfun f(x: String) -> List[String] = [x]";
    assert_eq!(hir_of(src, "f"), "(fun f(x#1: String) -> std.list.List[String] (array x#1))");
}

#[test]
fn regions() {
    assert_eq!(
        hir_of("use std.mem.Spool\nwarp Node {\n    v: i32\n}\nfun f() -> ^Node@out given out: Spool = ^Node(1)", "f"),
        "(fun f() -> ^Node@out#1 given out#1: std.mem.Spool (alloc (call (type Node) 1)))"
    );
    assert_eq!(codes_of("fun f(x: ^i32@nowhere) {}"), [codes::N0113]);
    assert!(codes_of("fun f() {\n    spool s = std.mem.Arena(1)\n    let x: ^i32@s = ^1\n}").is_empty());
}

#[test]
fn const_if_defers_errors_to_type_checking() {
    let out = run("fun f() -> i32 {\n    const if target.os == .Windows { nope() } else { 1 }\n}");
    assert!(out.diags.is_empty(), "{}", rendered(&out));
    let p = &out.program;
    let root = p.root.expect("root");
    let def = p.module(root).items["f"];
    let DefKind::Fn(f) = &p.def(def).kind else { panic!("not a fun") };
    let Some(body) = &f.body else { panic!("no body") };
    let ExprKind::Block(block) = &body.kind else { panic!("not a block") };
    let super::StmtKind::Expr(e) = &block.stmts[0].kind else { panic!("not an expr") };
    let ExprKind::If { deferred: Some(deferred), .. } = &e.kind else { panic!("not a const if") };
    assert_eq!(deferred.then.iter().map(|d| d.code).collect::<Vec<_>>(), [codes::N0101]);
    assert!(deferred.else_.is_empty());
}

// ================================================================ user modules

#[test]
fn user_modules_and_visibility() {
    let helpers = "pub fun shout(s: str) -> String = s\nfun secret() {}\npub use std.io.print";
    let out = run_with(
        "use util.helpers\nfun main() {\n    helpers.print(helpers.shout(\"hi\"))\n}",
        &[("util.helpers", helpers)],
    );
    assert!(out.diags.is_empty(), "{}", rendered(&out));
    assert_eq!(item(&out, "main"), "(fun main() (block (call std.io.print (call util.helpers.shout \"hi\"))))");

    let out = run_with("use util.helpers.secret", &[("util.helpers", helpers)]);
    assert_eq!(out.diags.iter().map(|d| d.code).collect::<Vec<_>>(), [codes::N0103]);
    let out = run_with("use util.helpers\nfun f() = helpers.secret()", &[("util.helpers", helpers)]);
    assert_eq!(out.diags.iter().map(|d| d.code).collect::<Vec<_>>(), [codes::N0103]);
}

#[test]
fn re_exports_resolve_in_any_order() {
    // `a` re-exports from `b`, which re-exports from std: the root's import
    // can only resolve after both of theirs.
    let out =
        run_with("use a.print\nfun main() = print(\"x\")", &[("a", "pub use b.print"), ("b", "pub use std.io.print")]);
    assert!(out.diags.is_empty(), "{}", rendered(&out));
    assert_eq!(item(&out, "main"), "(fun main() (call std.io.print \"x\"))");
}

// ================================================================ corpora

#[test]
fn std_resolves_cleanly() {
    let out = run("");
    assert!(out.diags.is_empty(), "{}", rendered(&out));
    let pending: Vec<String> = out
        .program
        .defs
        .iter()
        .enumerate()
        .filter(|(_, d)| matches!(d.kind, DefKind::Pending))
        .map(|(i, _)| out.program.def_path(super::DefId(i as u32)))
        .collect();
    assert!(pending.is_empty(), "never lowered: {pending:?}");
}

#[test]
fn every_example_resolves_cleanly() {
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
        let rendered: Vec<String> = resolved.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
        assert!(rendered.is_empty(), "{}:\n{}", path.display(), rendered.join("\n"));
        seen += 1;
    }
    assert_eq!(seen, 8);
}
