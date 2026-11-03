//! A user `extern "host"` function compiles to `hostcall` on Treadle when
//! it takes and gives integers, `bool` or `char`; anything else is U0102.

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_syntax::SourceDb;
use loom_syntax::diag::codes;

fn compile(text: &str) -> Result<loom_isa::Image, Vec<loom_syntax::Diagnostic>> {
    let path = std::path::Path::new("host_extern.loom");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    treadle::compile(&db, &resolved.program, &typed).map(|c| c.image)
}

#[test]
fn a_scalar_host_function_is_a_host_call() {
    let image = compile("extern \"host\" {\n    pub fun step(x: u64) -> u64\n}\n\nfun main() {\n    step(1)\n}\n")
        .expect("compiles");
    assert_eq!(image.host_imports.len(), 1);
    let import = &image.host_imports[0];
    assert_eq!((import.name.as_str(), import.args, import.results), ("host_extern.step", 1, 1));
    assert!(!image.natives.iter().any(|n| n == "host.call"));
    assert!(image.code.iter().any(|&w| loom_isa::word::opcode(w) == loom_isa::Op::HostCall as u16));
    loom_isa::verify(&image).expect("valid");
}

#[test]
fn a_function_called_twice_is_one_import() {
    let image = compile(
        "extern \"host\" {\n    pub fun step(x: u64) -> u64\n    pub fun log(c: char)\n}\n\nfun main() {\n    step(step(1))\n    log('a')\n}\n",
    )
    .expect("compiles");
    let shape: Vec<_> = image.host_imports.iter().map(|h| (h.name.as_str(), h.args, h.results)).collect();
    assert_eq!(shape, [("host_extern.step", 1, 1), ("host_extern.log", 1, 0)]);
    loom_isa::verify(&image).expect("valid");
}

fn rejected(text: &str, what: &str) {
    let Err(diagnostics) = compile(text) else { panic!("compiled: {text}") };
    assert!(diagnostics.iter().all(|d| d.code == codes::U0102), "{diagnostics:?}");
    assert!(diagnostics.iter().any(|d| d.message.contains(what)), "{what}: {diagnostics:?}");
}

#[test]
fn other_types_are_not_implemented_yet() {
    let cases = [
        ("pub fun f(s: str)", "f(\"a\")", "a parameter of type str"),
        ("pub fun f() -> String", "f()", "a result of type String"),
        ("pub fun f(x: f64)", "f(1.0)", "a parameter of type f64"),
        ("pub fun f[T](x: T)", "f(1)", "generic parameters"),
        ("pub fun f(p: P)", "f(P(x: 1))", "a parameter of type P"),
    ];
    for (decl, call, what) in cases {
        rejected(
            &format!("warp P {{ x: u32 }}\n\nextern \"host\" {{\n    {decl}\n}}\n\nfun main() {{\n    {call}\n}}\n"),
            what,
        );
    }
}
