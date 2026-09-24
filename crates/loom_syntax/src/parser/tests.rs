use super::Parser;
use crate::ast::{FnRet, ItemKind, MemberKind, Module, ParamMode, WarpBody, WarpModifier};
use crate::diag::{Code, codes, render_plain};
use crate::lexer::lex;
use crate::pretty;
use crate::source::SourceDb;
use crate::token::Punct;

/// Runs `f` on a parser over `src`. Returns its result, whether the parser
/// reached the end, the rendered diagnostics and their codes.
fn run<T>(src: &str, f: impl FnOnce(&mut Parser<'_>) -> T) -> (T, bool, Vec<String>, Vec<Code>) {
    let mut db = SourceDb::new();
    let file = db.add("test.loom", src);
    let lexed = lex(&db, file);
    let mut diags = lexed.diagnostics;
    let (out, at_eof) = {
        let mut parser = Parser::new(&db, file, lexed.tokens);
        let out = f(&mut parser);
        diags.append(&mut parser.diags);
        (out, parser.at_eof())
    };
    let rendered = diags.iter().map(|d| render_plain(&db, d)).collect();
    let codes = diags.iter().map(|d| d.code).collect();
    (out, at_eof, rendered, codes)
}

fn clean<T>(src: &str, f: impl FnOnce(&mut Parser<'_>) -> T) -> T {
    let (out, at_eof, diags, _) = run(src, f);
    assert!(diags.is_empty(), "unexpected diagnostics for {src:?}:\n{}", diags.join("\n"));
    assert!(at_eof, "parser stopped before the end of {src:?}");
    out
}

/// One expression (or assignment), as an S-expression.
fn e(src: &str) -> String {
    clean(src, |p| pretty::expr(&p.stmt_expr()))
}

/// Newline-separated statements, each as an S-expression.
fn stmts(src: &str) -> Vec<String> {
    clean(src, |p| {
        let mut out = Vec::new();
        loop {
            while p.eat_punct(Punct::Semi).is_some() {}
            if p.at_eof() {
                break;
            }
            out.push(pretty::stmt(&p.stmt()));
            p.stmt_terminator();
        }
        out
    })
}

fn pat(src: &str) -> String {
    clean(src, |p| pretty::pat(&p.pat_top(true)))
}

fn ty(src: &str) -> String {
    clean(src, |p| pretty::ty(&p.ty()))
}

fn module(src: &str) -> Module {
    clean(src, |p| p.module())
}

fn module_codes(src: &str) -> Vec<Code> {
    run(src, |p| p.module()).3
}

// ================================================================ precedence

#[test]
fn arithmetic_and_bitwise_precedence() {
    assert_eq!(e("a + b * c"), "(+ a (* b c))");
    assert_eq!(e("a * b + c"), "(+ (* a b) c)");
    assert_eq!(e("a - b - c"), "(- (- a b) c)");
    assert_eq!(e("-a * b"), "(* (- a) b)");
    assert_eq!(e("-after as Cents"), "(as (- after) Cents)");
    assert_eq!(e("x +% 1 * 2"), "(+% x (* 1 2))");
    assert_eq!(e("a << 8 | b"), "(| (<< a 8) b)");
    assert_eq!(e("a ~ b & c"), "(~ a (& b c))");
    // Bitwise binds tighter than comparison, unlike C.
    assert_eq!(e("flags & MASK == 0"), "(== (& flags MASK) 0)");
    assert_eq!(e(".keys >> (key as% u4) & 1 == 1"), "(== (& (>> .keys (as% key u4)) 1) 1)");
}

#[test]
fn logic_and_comparisons() {
    assert_eq!(e("not a == b and c or d"), "(or (and (not (== a b)) c) d)");
    assert_eq!(e("0 <= x < 10"), "(chain 0 <= x < 10)");
    assert_eq!(e("a == b"), "(== a b)");
    assert_eq!(run("a < b > c", |p| p.expr()).3, [codes::S0203]);
}

#[test]
fn postfix_chains() {
    assert_eq!(e("-x.eval(vars)?"), "(- (? (call (. x eval) vars)))");
    assert_eq!(
        e("env.get(\"PORT\")?.parse[u16]()? mend 8080"),
        "(mend (? (call (index (. (? (call (. env get) \"PORT\")) parse) (type u16)))) 8080)"
    );
    assert_eq!(e("_.size().0"), "(. (call (. _ size)) 0)");
    assert_eq!(e("p^.x"), "(. (deref p) x)");
    assert_eq!(e("self.(c).(f)"), "(.() (.() self c) f)");
}

#[test]
fn ranges() {
    assert_eq!(e("0..n"), "(.. 0 n)");
    assert_eq!(e("..=5"), "(..= _ 5)");
    assert_eq!(e("2.."), "(.. 2 _)");
    assert_eq!(e("a..b + 1"), "(.. a (+ b 1))");
    assert_eq!(e(".mem[FONT_AT..][..FONT.len()]"), "(index (index .mem (.. FONT_AT _)) (.. _ (call (. FONT len))))");
    // In a loop head, an open range doesn't swallow the body.
    assert_eq!(e("for n in 2.. { f(n) }"), "(for n (.. 2 _) (block (call f n)))");
}

#[test]
fn ranges_do_not_chain() {
    assert_eq!(e("(a..b)..c"), "(.. (.. a b) c)");
    assert_eq!(run("a..b..c", |p| p.expr()).3, [codes::S0200]);
    assert_eq!(run("..a..=b", |p| p.expr()).3, [codes::S0200]);
    // A range at the end of a line doesn't meet the next line's `..`.
    assert_eq!(stmts("a..\n..b"), ["(.. a _)", "(.. _ b)"]);
}

#[test]
fn pull_arrow_in_an_expression() {
    // `<~` is one token; `a<~b` is reported and read as `a < ~b`.
    let (out, _, diags, codes) = run("a<~b", |p| pretty::expr(&p.expr()));
    assert_eq!(out, "(< a (~ b))");
    assert_eq!(codes, [codes::S0200]);
    assert!(diags[0].contains("write `a < ~b` with a space"), "{}", diags[0]);
}

#[test]
fn mend_forms() {
    assert_eq!(e("f() mend e => g(e)"), "(mend (call f) (e => (call g e)))");
    assert_eq!(e("a mend b mend c"), "(mend (mend a b) c)");
    assert_eq!(e("x mend fail .Missing(k)"), "(mend x (fail (call .Missing k)))");
    assert_eq!(e("next mend { return }"), "(mend next (block (return)))");
    assert_eq!(e("a + b mend 0"), "(mend (+ a b) 0)");
}

#[test]
fn is_and_casts() {
    assert_eq!(e("x is some(c) and c > 0"), "(and (is x (some c)) (> c 0))");
    assert_eq!(e("child is b: Button"), "(is child (b: Button))");
    assert_eq!(e("x is .A | .B"), "(is x (| .A .B))");
    assert_eq!(e("x as? Cents"), "(as? x Cents)");
    assert_eq!(e("x as| u8 | y"), "(| (as| x u8) y)");
    assert_eq!(e("n as f64 / 2.0"), "(/ (as n f64) 2.0)");
}

#[test]
fn lambdas_and_holes() {
    assert_eq!(e("xs.map(x => x * 2)"), "(call (. xs map) (fn (x) (* x 2)))");
    assert_eq!(e("(a, b) => a + b"), "(fn (a, b) (+ a b))");
    assert_eq!(e("(p: Point) => p.len()"), "(fn (p: Point) (call (. p len)))");
    assert_eq!(e("() => 1"), "(fn () 1)");
    assert_eq!(e("xs.filter(_ % 2 == 0)"), "(call (. xs filter) (== (% _ 2) 0))");
    assert_eq!(e("xs.each(x => total += x)"), "(call (. xs each) (fn (x) (+= total x)))");
    assert_eq!(e("^(x => x + offset)"), "(alloc (fn (x) (+ x offset)))");
    assert_eq!(e("take x => x"), "(fn (take x) x)");
}

#[test]
fn leading_dots_and_dot_blocks() {
    assert_eq!(e(".x + .w"), "(+ .x .w)");
    assert_eq!(e("at.{ .x += 1; .y = 2 }"), "(dot-block at (block (+= .x 1) (= .y 2)))");
    assert_eq!(
        e("Window(800, 600).{\n    .title = \"Loom\"\n    .show()\n}"),
        "(dot-block (call Window 800 600) (block (= .title \"Loom\") (call .show)))"
    );
    assert_eq!(stmts("let t: Shape = .Rect(w: 3, h: 4)"), ["(let t: Shape (call .Rect (w: 3) (h: 4)))"]);
}

#[test]
fn leading_dot_tuple_fields() {
    // `.0` is to `self.0` what `.x` is to `self.x`.
    assert_eq!(e(".0 * 2"), "(* .0 2)");
    assert_eq!(e("pair.{ .1 = 5 }"), "(dot-block pair (block (= .1 5)))");
    assert_eq!(e("\"{.0}m\""), "(template {.0} \"m\")");
}

#[test]
fn collections_and_groups() {
    assert_eq!(e("[1, 2, 3]"), "(array 1 2 3)");
    assert_eq!(e("[0; 16]"), "(array-repeat 0 16)");
    assert_eq!(e("[\"x\": 1.5, \"y\": 2.0]"), "(map (\"x\" 1.5) (\"y\" 2.0))");
    assert_eq!(e("[]"), "(array)");
    assert_eq!(e("[:]"), "(map)");
    assert_eq!(e("(1,)"), "(tuple 1)");
    assert_eq!(e("()"), "()");
    assert_eq!(e("(a)"), "a");
    assert_eq!(e("[\n  1,\n  2,\n]"), "(array 1 2)");
}

#[test]
fn templates() {
    assert_eq!(e("\"x = {x + 1:>4}!\""), "(template \"x = \" {(+ x 1):>4} \"!\")");
    assert_eq!(e("\"{a.b(c)}\""), "(template {(call (. a b) c)})");
}

#[test]
fn template_errors() {
    let (_, _, diags, codes) = run("\"a {} b\"", |p| p.expr());
    assert_eq!(codes, [codes::S0200]);
    assert!(diags[0].contains("empty interpolation"), "{}", diags[0]);
    assert!(diags[0].contains("write `{{` and `}}`"), "{}", diags[0]);
    // Template tokens are described in words, not by their internal names.
    let (_, _, diags, _) = run("\"{x y}\"", |p| p.expr());
    assert!(diags[0].contains("expected `}` to end the interpolation, found `y`"), "{}", diags[0]);
    // Skipping a bad interpolation steps over nested templates in it.
    let (out, _, _, codes) = run("\"{x \"a{b}c\" y}!\"", |p| pretty::expr(&p.expr()));
    assert_eq!(codes, [codes::S0200]);
    assert_eq!(out, "(template {x} \"!\")");
}

#[test]
fn call_arguments() {
    assert_eq!(e("f(a, name: 1, mut c, given: raw)"), "(call f a (name: 1) (mut c) (given: raw))");
    assert_eq!(e("f(\n    a,\n    b,\n)"), "(call f a b)");
    assert_eq!(e("counts.entry(word, default: 0) += n"), "(+= (call (. counts entry) word (default: 0)) n)");
}

#[test]
fn brackets_decide_type_or_expression() {
    assert_eq!(e("List[i32]()"), "(call (index List (type i32)))");
    assert_eq!(e("Ring[u32, 4]()"), "(call (index Ring (type u32) 4))");
    assert_eq!(e("Map[String, u32]()"), "(call (index Map (type String) (type u32)))");
    assert_eq!(e("List[any Widget]()"), "(call (index List (type any Widget)))");
    assert_eq!(e("xs[i + 1]"), "(index xs (+ i 1))");
    assert_eq!(e("cache[Key(a, b)]"), "(index cache (call Key a b))");
}

#[test]
fn prefix_operators() {
    assert_eq!(e("^Expr.Bin(op, l, r)"), "(alloc (call (. Expr Bin) op l r))");
    assert_eq!(e("~x & 0xFF"), "(& (~ x) 255)");
    assert_eq!(e("not .held(k)"), "(not (call .held k))");
}

#[test]
fn control_flow_expressions() {
    assert_eq!(e("if a { x } else if b { y } else { z }"), "(if a (block x) (if b (block y) (block z)))");
    assert_eq!(e("if a { x }\nelse { y }"), "(if a (block x) (block y))");
    assert_eq!(
        e("match x {\n    1 => a\n    n if n > 9 => b, _ => c\n}"),
        "(match x (=> 1 a) (=> n (if (> n 9)) b) (=> _ c))"
    );
    assert_eq!(e("loop { break count }"), "(loop (block (break count)))");
    assert_eq!(e("while pull is some(x) { yield x }"), "(while (is pull (some x)) (block (yield x)))");
    assert_eq!(
        e("const if target.os == .Windows { a } else { b }"),
        "(const-if (== (. target os) .Windows) (block a) (block b))"
    );
    assert_eq!(e("const for f in T.fields { f }"), "(const-for f (. T fields) (block f))");
    assert_eq!(e("const fib(30)"), "(const (call fib 30))");
    assert_eq!(e("unsafe \"fd 1 is open\" { write(1) }"), "(unsafe \"fd 1 is open\" (block (call write 1)))");
    assert_eq!(e("loom { let a = thread f(x) }"), "(loom (block (let a (thread (call f x)))))");
    assert_eq!(e("loom for x in xs { }"), "(loom-for x xs (block))");
    assert_eq!(e("yield mut .items[i]"), "(yield mut (index .items i))");
    assert_eq!(e("x = if c { 1 } else { 2 }"), "(= x (if c (block 1) (block 2)))");
}

#[test]
fn labels_resolve_break_targets() {
    assert_eq!(
        stmts("outer: for x in xs {\n    for y in ys { if y { break outer } }\n}"),
        ["(for 'outer x xs (block (for y ys (block (if y (block (break 'outer)))))))"]
    );
    // Not a label in scope, so it's a value.
    assert_eq!(e("loop { break outer }"), "(loop (block (break outer)))");
}

#[test]
fn labeled_loops_are_expressions() {
    assert_eq!(stmts("let v = outer: loop { break outer 5 }"), ["(let v (loop 'outer (block (break 'outer 5))))"]);
    // In brackets, `k: loop` is a map entry (as `f(k: loop …)` is a named argument).
    assert_eq!(e("[k: loop { break 1 }]"), "(map (k (loop (block (break 1)))))");
    assert_eq!(e("f(k: loop { break 1 })"), "(call f (k: (loop (block (break 1)))))");
}

#[test]
fn continue_takes_only_known_labels() {
    let (_, _, diags, codes) = run("fn a() { loop { continue foo } }", |p| p.module());
    assert_eq!(codes, [codes::S0200]);
    assert!(diags[0].contains("no enclosing loop is labeled `foo`"), "{}", diags[0]);
}

// ================================================================ statements

#[test]
fn statements() {
    assert_eq!(
        stmts("let x = 1\nlet y: u8 = 2\nvar (a, b) = (1, 2)\nx += 1"),
        ["(let x 1)", "(let y: u8 2)", "(var (tuple a b) (tuple 1 2))", "(+= x 1)"]
    );
    assert_eq!(
        stmts("given log = Logger.stderr()\ngiven scratch\nspool frame = Arena(1.mib())"),
        ["(given log (call (. Logger stderr)))", "(given scratch)", "(spool frame (call Arena (call (. 1 mib))))"]
    );
    assert_eq!(
        stmts("defer tx => tx.rollback()\ndefer close(fd)"),
        ["(defer tx => (call (. tx rollback)))", "(defer (call close fd))"]
    );
    assert_eq!(stmts("let 0bc_ssss_ssss = a + b"), ["(let (twill w9 mask=0x0 value=0x0 c:u1 s:u8) (+ a b))"]);
    assert_eq!(stmts("a; b"), ["a", "b"]);
    assert_eq!(stmts("(x, y) = (y, x)"), ["(= (tuple x y) (tuple y x))"]);
}

#[test]
fn nested_items_in_blocks() {
    let out = stmts("fn helper() -> u8 = 1\nconst LIMIT = 3\nhelper()");
    assert_eq!(out.len(), 3);
    assert!(out[0].contains("fn helper() -> u8 = 1"), "{out:?}");
    assert!(out[1].contains("const LIMIT 3"), "{out:?}");
    assert_eq!(out[2], "(call helper)");
}

// ================================================================ continuation rule

#[test]
fn deeper_lines_continue_the_statement() {
    assert_eq!(
        stmts("let names = people\n    .filter(ok)\n    .map(name)"),
        ["(let names (call (. (call (. people filter) ok) map) name))"]
    );
    assert_eq!(
        stmts("next[k] = after as? Cents\n    mend fail .Insufficient(k)"),
        ["(= (index next k) (mend (as? after Cents) (fail (call .Insufficient k))))"]
    );
    // A binary operator at the end of a line always continues.
    assert_eq!(stmts("let x = a +\nb"), ["(let x (+ a b))"]);
}

#[test]
fn same_indent_lines_are_new_statements() {
    assert_eq!(stmts("a\n.b()"), ["a", "(call .b)"]);
    assert_eq!(stmts("f\n(x, y)"), ["f", "(tuple x y)"]);
    assert_eq!(stmts("let x = a\n-b"), ["(let x a)", "(- b)"]);
    // Never a call across a newline, even when deeper.
    assert_eq!(stmts("f\n    (x)"), ["f", "x"]);
}

#[test]
fn newlines_inside_parens_are_ignored() {
    assert_eq!(stmts("f(a,\nb)"), ["(call f a b)"]);
    assert_eq!(stmts("let x = (a\n+ b)"), ["(let x (+ a b))"]);
}

// ================================================================ patterns

#[test]
fn patterns() {
    assert_eq!(pat(".Circle(r)"), "(.Circle r)");
    assert_eq!(pat(".Rect(w: width, ..)"), "(.Rect (w: width) ..)");
    assert_eq!(pat("Point(x, y)"), "(Point x y)");
    assert_eq!(pat(".Sunny"), ".Sunny");
    assert_eq!(pat("Color.Red"), "Color.Red");
    assert_eq!(pat("some('a'..='z' | '_')"), "(some (| (..= 'a' 'z') '_'))");
    assert_eq!(pat("0x8xy4"), "(twill w16 mask=0xF00F value=0x8004 x:u4 y:u4)");
    assert_eq!(pat("-1"), "-1");
    assert_eq!(pat("1..=9"), "(..= 1 9)");
    assert_eq!(pat("..=9"), "(..= _ 9)");
    assert_eq!(pat("100.."), "(.. 100 _)");
    assert_eq!(pat("[first, ..rest]"), "[first ..rest]");
    assert_eq!(pat("(a, mut b)"), "(tuple a (mut b))");
    assert_eq!(pat("e: IoError"), "(e: IoError)");
    assert_eq!(pat("err(e: IoError)"), "(err (e: IoError))");
    assert_eq!(pat("Tx(_)"), "(Tx _)");
    assert_eq!(pat(".(c)"), "(.() c)");
    assert_eq!(pat("none"), "none");
    assert_eq!(pat("\"GET\" | \"HEAD\""), "(| \"GET\" \"HEAD\")");
    assert_eq!(pat("b\"x\\xff\""), "b\"x\\xff\"");
}

#[test]
fn case_patterns_through_module_paths() {
    assert_eq!(pat("ast.Expr.Lit(v)"), "(ast.Expr.Lit v)");
    assert_eq!(pat("std.fs.MAX"), "std.fs.MAX");
    assert_eq!(stmts("let ast.Point(x, y) = p"), ["(let (ast.Point x y) p)"]);
    // A plain name is still a binding.
    assert_eq!(pat("x"), "x");
}

#[test]
fn lowercase_hex_in_patterns_becomes_twill_with_a_warning() {
    let (out, _, diags, codes) = run("0x00e0", |p| pretty::pat(&p.pat_top(true)));
    assert_eq!(out, "(twill w16 mask=0xFF0F value=0x0 e:u4)");
    assert_eq!(codes, [codes::S0206]);
    assert!(diags[0].starts_with("warning[S0206]"), "{}", diags[0]);
    assert!(diags[0].contains("`0x00E0`"), "{}", diags[0]);
    // Real captures stay lowercase in the suggestion.
    let (_, _, diags, _) = run("0xDxye", |p| p.pat_top(true));
    assert!(diags[0].contains("`0xDxyE`"), "{}", diags[0]);
}

// ================================================================ types

#[test]
fn types() {
    assert_eq!(ty("^Expr@sp ! ParseError"), "(^Expr@sp ! ParseError)");
    assert_eq!(ty("Config ! (IoError | ParseError)"), "(Config ! (IoError | ParseError))");
    assert_eq!(ty("List[any Widget]"), "List[any Widget]");
    assert_eq!(ty("(A, B) => C"), "((A, B) => C)");
    assert_eq!(ty("() => ()"), "(() => ())");
    assert_eq!(ty("i32 => i32 => i32"), "((i32) => ((i32) => i32))");
    assert_eq!(ty("[u8; 16]"), "[u8; 16]");
    assert_eq!(ty("[?T; N]"), "[?T; N]");
    assert_eq!(ty("uint(N.log2() + 1)"), "uint((+ (call (. N log2)) 1))");
    assert_eq!(ty("fs.IoError"), "fs.IoError");
    assert_eq!(ty("Self.Item"), "Self.Item");
    assert_eq!(ty("any (Widget + Show)"), "any Widget + Show");
    assert_eq!(ty("Ring[u32, 4]"), "Ring[u32, 4]");
    assert_eq!(ty("A | B | C"), "(A | B | C)");
    assert_eq!(ty("type"), "type");
}

#[test]
fn function_types_keep_tuple_parameters() {
    // The parentheses right before `=>` are the parameter list.
    assert_eq!(ty("((A, B)) => C"), "(((A, B)) => C)");
    assert_eq!(ty("(()) => C"), "((()) => C)");
    assert_eq!(ty("(A) => C"), "((A) => C)");
}

#[test]
fn optional_binds_tighter_than_the_function_arrow() {
    assert_eq!(ty("?T => U"), "((?T) => U)");
    assert_eq!(ty("?(T => U)"), "?((T) => U)");
}

// ================================================================ items

#[test]
fn function_signatures() {
    let m = module(
        "fn a() -> u8 = 1\n\
         fn b() ! IoError {}\n\
         fn c(take self, mut out: String, x: u8 = 3) -> T ! E {}\n\
         fn d() ~> Token <~ char {}\n\
         fn e(mut self, i: usize) ~> mut T {}\n\
         fn f() <~ Line {}\n\
         pub fn parse(src: str) -> ^Expr@sp ! ParseError given sp: Spool {}\n\
         fn g[T: Show + Eq, const N: usize, @r](x: T) -> String where T: Hash = \"\"\n\
         const fn uint(bits: u8) -> type\n\
         unsafe fn poke(p: *u8) {}",
    );
    let rendered: Vec<String> = m.items.iter().map(|i| pretty::item(i).trim_end().to_string()).collect();
    assert_eq!(
        rendered,
        [
            "(fn a() -> u8 = 1)",
            "(fn b() -> (() ! IoError) (block))",
            "(fn c(take self, mut out: String, x: u8 = 3) -> (T ! E) (block))",
            "(fn d() ~> Token <~ char (block))",
            "(fn e(mut self, i: usize) ~> mut T (block))",
            "(fn f() <~ Line (block))",
            "(pub fn parse(src: str) -> (^Expr@sp ! ParseError) given sp: Spool (block))",
            "(fn g[T: Show + Eq, const N: usize, @r](x: T) -> String where T: Hash = \"\")",
            "(const fn uint(bits: u8) -> type)",
            "(unsafe fn poke(p: *u8) (block))",
        ]
    );
}

#[test]
fn unit_warps() {
    let m = module(
        "warp Missing\n\
         warp Gone: Error;\n\
         pub warp Empty {}\n\
         warp Nothing()\n\
         fn f() = fail Missing()\n\
         warp Braces\n{\n    x: u8\n}",
    );
    assert_eq!(m.items.len(), 6);
    for item in &m.items[..3] {
        let ItemKind::Warp(w) = &item.kind else { panic!("expected a warp") };
        assert_eq!(w.body, WarpBody::Members(Vec::new()), "{}", w.name.name);
    }
    let ItemKind::Warp(gone) = &m.items[1].kind else { panic!("expected a warp") };
    assert_eq!(gone.wefts.len(), 1);
    let ItemKind::Warp(nothing) = &m.items[3].kind else { panic!("expected a warp") };
    assert_eq!(nothing.body, WarpBody::Tuple(Vec::new()));
    // A `{` on the next line is still the body, as it is for `fn`.
    let ItemKind::Warp(braces) = &m.items[5].kind else { panic!("expected a warp") };
    assert!(matches!(&braces.body, WarpBody::Members(members) if members.len() == 1));
}

#[test]
fn warps() {
    let m = module(
        "/// A token.\n\
         pub warp Token: Show, Eq {\n    \
             pub(get) span: Span\n    \
             count: u8 = 0\n    \
             case Ident(String), Int(u64)\n    \
             case Journal(from IoError)\n    \
             case Eof\n    \
             through inner: Frame\n    \
             const MAX = 3\n    \
             type Idx = uint(4)\n    \
             const assert(MAX > 1)\n    \
             fn len(self) -> usize = .span.len()\n\
         }\n\
         warp Meters(f64)\n\
         pub once warp Tx { moves: List[Move] }\n\
         ref warp Parser { src: str, pos: usize = 0 }",
    );
    assert_eq!(m.items.len(), 4);
    assert_eq!(m.items[0].docs, ["A token."]);
    let ItemKind::Warp(token) = &m.items[0].kind else { panic!("expected a warp") };
    assert_eq!(token.wefts.len(), 2);
    let WarpBody::Members(members) = &token.body else { panic!("expected members") };
    let kinds: Vec<&str> = members
        .iter()
        .map(|m| match &m.kind {
            MemberKind::Field(_) => "field",
            MemberKind::Through(_) => "through",
            MemberKind::Cases(_) => "cases",
            MemberKind::Const(_) => "const",
            MemberKind::Type(_) => "type",
            MemberKind::Fn(_) => "fn",
            MemberKind::ConstExpr(_) => "const-expr",
        })
        .collect();
    assert_eq!(kinds, ["field", "field", "cases", "cases", "cases", "through", "const", "type", "const-expr", "fn"]);
    let rendered = pretty::item(&m.items[0]);
    assert!(rendered.contains("(pub(get) field span: Span)"), "{rendered}");
    assert!(rendered.contains("(case Ident(String), Int(u64))"), "{rendered}");
    assert!(rendered.contains("(case Journal(from IoError))"), "{rendered}");

    let ItemKind::Warp(meters) = &m.items[1].kind else { panic!() };
    assert!(matches!(&meters.body, WarpBody::Tuple(f) if f.len() == 1));
    let ItemKind::Warp(tx) = &m.items[2].kind else { panic!() };
    assert_eq!(tx.modifier, WarpModifier::Once);
    let ItemKind::Warp(parser) = &m.items[3].kind else { panic!() };
    assert_eq!(parser.modifier, WarpModifier::Ref);
}

#[test]
fn wefts_weaves_externs_uses() {
    let m = module(
        "use std.io.print\n\
         use std.fs.{File, read as read_file}\n\
         use std.{env, fs}\n\
         weft Iter: Show {\n    type Item\n    fn next(mut self) -> ?Item\n    fn name(self) -> str = \"iter\"\n}\n\
         weave[T: Show] Show into List[T] where T: Eq {\n    fn show(self, mut f: Fmt) {}\n}\n\
         weave Iter into Bytes {\n    type Item = u8\n    fn next(mut self) -> ?u8 = none\n}\n\
         extern \"C\" {\n    fn GetTickCount() -> u32\n}\n\
         type Port = u16 where 1..\n\
         type Even = i64 where _ % 2 == 0\n\
         const HOUSE: AccountId = 1\n\
         #test\n#layout(bits, big)\nfn t() {}",
    );
    let rendered: Vec<String> = m.items.iter().map(|i| pretty::item(i).trim_end().to_string()).collect();
    assert_eq!(rendered[0], "(use std.io.print)");
    assert_eq!(rendered[1], "(use std.fs.{File, read as read_file})");
    assert_eq!(rendered[2], "(use std.{env, fs})");
    assert!(
        rendered[3].starts_with("(weft Iter: Show\n  (type Item)\n  (fn next(mut self) -> ?Item)"),
        "{}",
        rendered[3]
    );
    assert!(rendered[4].starts_with("(weave[T: Show] Show into List[T] where T: Eq"), "{}", rendered[4]);
    assert!(rendered[5].starts_with("(weave Iter into Bytes\n  (type Item = u8)"), "{}", rendered[5]);
    assert!(rendered[6].contains("(fn GetTickCount() -> u32)"), "{}", rendered[6]);
    assert_eq!(rendered[7], "(type Port = u16 where (.. 1 _))");
    assert_eq!(rendered[8], "(type Even = i64 where (== (% _ 2) 0))");
    assert_eq!(rendered[9], "(const HOUSE: AccountId 1)");
    assert_eq!(rendered[10], "#test()\n#layout(bits big)\n(fn t() (block))");
}

#[test]
fn attribute_arguments() {
    let m = module("#foo(key = 1, bits)\nfn a() {}\nwarp W {\n    #disjoint\n    fn f(self) {}\n}");
    assert_eq!(pretty::item(&m.items[0]), "#foo((= key 1) bits)\n(fn a() (block))\n");
    // Member attributes show up in the dump too.
    assert!(
        pretty::item(&m.items[1]).contains("  #disjoint()\n  (fn f(self) (block))"),
        "{}",
        pretty::item(&m.items[1])
    );
}

#[test]
fn through_needs_a_colon() {
    let (_, _, diags, codes) = run("warp A { through frame Frame }", |p| p.module());
    assert_eq!(codes, [codes::S0200]);
    assert!(diags[0].contains("expected `:`, found `Frame`"), "{}", diags[0]);
}

#[test]
fn param_modes_go_first() {
    let m = module("fn f(mut a: T, take b: T, c: T, const n: usize) {}");
    let ItemKind::Fn(f) = &m.items[0].kind else { panic!() };
    let modes: Vec<ParamMode> = f.params.iter().map(|p| p.mode).collect();
    assert_eq!(modes, [ParamMode::Mut, ParamMode::Take, ParamMode::Read, ParamMode::Const]);

    // `c: mut Canvas` is accepted with a fix-it.
    let (m, _, diags, codes) = run("fn draw(c: mut Canvas) {}", |p| p.module());
    assert_eq!(codes, [codes::S0208]);
    assert!(diags[0].contains("write `mut c: …`"), "{}", diags[0]);
    let ItemKind::Fn(f) = &m.items[0].kind else { panic!() };
    assert_eq!(f.params[0].mode, ParamMode::Mut);
    assert!(matches!(f.ret, FnRet::Unit));
}

#[test]
fn name_cases_are_enforced() {
    assert_eq!(module_codes("warp point {}"), [codes::S0207]);
    assert_eq!(module_codes("fn Foo() {}"), [codes::S0207]);
    assert_eq!(module_codes("const max = 1"), [codes::S0207]);
    assert_eq!(module_codes("warp P { case lower }"), [codes::S0207]);
    // Extern declarations keep their foreign names.
    assert!(module_codes("extern \"C\" { fn GetTickCount() -> u32 }").is_empty());
    assert_eq!(module_codes("fn f() { pub(get) }"), [codes::S0209]);
}

// ================================================================ errors and recovery

#[test]
fn recovers_after_a_bad_statement() {
    let (m, _, _, codes) = run("fn a() {\n    let x = )\n    let y = 2\n}\nfn b() {}", |p| p.module());
    assert_eq!(codes, [codes::S0200]);
    assert_eq!(m.items.len(), 2);
    let body = pretty::item(&m.items[0]);
    assert!(body.contains("(let y 2)"), "{body}");
}

#[test]
fn reports_statements_on_one_line() {
    assert_eq!(module_codes("fn a() { let a = 1 let b = 2 }"), [codes::S0202]);
}

#[test]
fn reports_unclosed_delimiters() {
    let (_, _, diags, codes) = run("fn a() {\n    f(1, 2\n", |p| p.module());
    assert!(codes.contains(&codes::S0201), "{codes:?}");
    assert!(diags.iter().any(|d| d.contains("never closed")), "{diags:?}");
}

#[test]
fn a_missing_close_paren_points_at_the_opener() {
    let (_, _, diags, codes) = run("fn a() {\n    let x = f(1, 2\n    let y = 3\n}", |p| p.module());
    assert_eq!(codes, [codes::S0200]);
    assert!(diags[0].contains("expected `)`, found keyword `let`"), "{}", diags[0]);
    assert!(diags[0].contains("to close this `(`"), "{}", diags[0]);
}

#[test]
fn explains_the_continuation_rule() {
    let (_, _, diags, codes) = run("fn a() {\n    let b = x\n    + y\n}", |p| p.module());
    assert_eq!(codes, [codes::S0200]);
    assert!(diags[0].contains("only when it's indented deeper"), "{}", diags[0]);
}

#[test]
fn lexer_errors_are_not_reported_again() {
    assert_eq!(module_codes("fn a() { let x = 1 $ 2 }"), [codes::S0100]);
    assert_eq!(module_codes("fn a() {\n    let x = 'ab' + 1\n}"), [codes::S0107]);
}

#[test]
fn one_error_per_garbage_line() {
    assert_eq!(module_codes("fn a() {\n    @@@ @@\n    let y = 2\n}"), [codes::S0200]);
    assert_eq!(module_codes("warp A {\n    @@@ @@\n    x: u8\n}"), [codes::S0200]);
    assert_eq!(module_codes("fn a() {\n    match x {\n        ) ) )\n        _ => 1\n    }\n}"), [codes::S0200]);
}

#[test]
fn a_missing_brace_ends_at_the_next_item() {
    // The warp's `}` is missing: one error, and `B` and `c` still parse as items.
    let (m, _, diags, codes) = run("warp A {\n    x: u8\nwarp B {\n    y: u8\n}\nfn c() {}", |p| p.module());
    assert_eq!(codes, [codes::S0201]);
    assert!(diags[0].contains("expected `}` before this line"), "{}", diags[0]);
    assert_eq!(m.items.len(), 3);
    // Same for a `match` whose `}` is missing.
    let (m, _, _, codes) =
        run("fn a() {\n    let v = match x {\n        1 => 2\n    let w = 3\n    w\n}\nfn b() {}", |p| p.module());
    assert_eq!(codes, [codes::S0201]);
    assert_eq!(m.items.len(), 2);
    assert!(pretty::item(&m.items[0]).contains("(let w 3)"), "{}", pretty::item(&m.items[0]));
    // A statement keyword indented like an arm is an ordinary error instead.
    assert_eq!(module_codes("fn a() {\n    match x {\n        let y = 1\n    }\n}"), [codes::S0200]);
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    // Run with a big stack so a debug build reaches the limit (the CLI does the same).
    let codes = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let deep = |open: &str, mid: &str, close: &str| {
                format!("fn a() {{ {}{mid}{} }}", open.repeat(5000), close.repeat(5000))
            };
            [
                deep("(", "1", ")"),
                deep("[", "1", "]"),
                deep("{ ", "1", " }"),
                deep("-", "1", ""),
                format!("fn a() {{ let x: {}T = y }}", "?".repeat(5000)),
                format!("fn a() {{ let {}x{} = 1 }}", "(".repeat(5000), ")".repeat(5000)),
                format!("use a{}{}", ".{b".repeat(5000), "}".repeat(5000)),
            ]
            .iter()
            .map(|src| module_codes(src))
            .collect::<Vec<_>>()
        })
        .expect("spawn")
        .join()
        .expect("no stack overflow");
    for c in codes {
        assert_eq!(c, [codes::S0211]);
    }
    // Deep but reasonable nesting is fine.
    let src = format!("fn a() {{ let x = {}1{} }}", "(".repeat(60), ")".repeat(60));
    assert!(module_codes(&src).is_empty());
}

#[test]
fn skips_garbage_between_items() {
    let (m, _, _, codes) = run("42 + 1\nfn b() {}\n)\nwarp C {}", |p| p.module());
    assert_eq!(codes, [codes::S0200, codes::S0200]);
    assert_eq!(m.items.len(), 2);
}

#[test]
fn rejects_assignments_in_expressions() {
    assert_eq!(module_codes("fn a() { if x = 5 { } }"), [codes::S0205]);
    assert_eq!(module_codes("fn a() { let a = b = c }"), [codes::S0205]);
}

#[test]
fn rejects_twill_in_expressions() {
    assert_eq!(module_codes("fn a() { let x = 0x8xy4 }"), [codes::S0204]);
}

#[test]
fn missing_initializer() {
    assert_eq!(module_codes("fn a() { let x: u8 }"), [codes::S0210]);
}

#[test]
fn unsafe_requires_a_reason() {
    assert_eq!(module_codes("fn a() { unsafe { poke() } }"), [codes::S0200]);
}

// ================================================================ the examples

#[test]
fn every_example_parses_cleanly() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("loom") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable example");
        let mut db = SourceDb::new();
        let file = db.add(path.display().to_string(), text);
        let parsed = crate::parse(&db, file);
        let rendered: Vec<String> = parsed.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
        assert!(rendered.is_empty(), "{}:\n{}", path.display(), rendered.join("\n"));
        assert!(!parsed.module.items.is_empty(), "{}", path.display());
        seen += 1;
    }
    assert_eq!(seen, 8);
}
