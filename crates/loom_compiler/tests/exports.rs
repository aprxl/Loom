//! A program with no `main` whose `pub` functions are called from outside
//! (`treadle::compile_exports`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_isa::Image;
use loom_syntax::SourceDb;
use loom_syntax::diag::codes;
use loom_sys::Host;
use loom_vm::{Config, Vm};

fn compile(text: &str) -> Result<Image, Vec<loom_syntax::Diagnostic>> {
    let path = Path::new("exports.loom");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    treadle::compile_exports(&db, &resolved.program, &typed, Level::O2).map(|c| c.image)
}

fn find(image: &Image, name: &str) -> Option<u32> {
    image.funcs.iter().position(|f| f.name == name).map(|i| i as u32)
}

fn call(image: &Image, name: &str, args: &[u64]) -> Vec<u64> {
    let host = Host::captured(Rc::new(RefCell::new(Vec::new())));
    let mut vm = Vm::new(image, host, &Config::default()).expect("loads");
    vm.call(find(image, name).expect("an export"), args).expect("runs")
}

const CALC: &str = "pub fun calc(a: i64, b: i64) -> i64 { a * b + 1 }\n";

#[test]
fn a_program_with_no_main_exports_its_pub_functions() {
    let image = compile(CALC).expect("compiles");
    assert_eq!(call(&image, "calc", &[2, 3]), vec![7]);
}

#[test]
fn a_private_function_is_not_reachable() {
    let image = compile(&format!("{CALC}fun secret() -> i64 {{ 1 }}\n")).expect("compiles");
    assert!(find(&image, "calc").is_some());
    assert!(find(&image, "secret").is_none());
}

#[test]
fn a_generic_function_is_not_exported() {
    let image = compile(&format!("{CALC}pub fun same[T](x: T) -> T {{ x }}\n")).expect("compiles");
    assert!(find(&image, "calc").is_some());
    assert!(image.funcs.iter().all(|f| !f.name.starts_with("same")));
}

#[test]
fn adding_main_changes_nothing() {
    let image = compile(&format!("{CALC}fun main() {{}}\n")).expect("compiles");
    assert_eq!(call(&image, "calc", &[2, 3]), vec![7]);
}

#[test]
fn an_export_that_can_fail_is_refused() {
    let Err(diagnostics) = compile("pub warp Oops {}\n\npub fun risky() -> i64 ! Oops { 1 }\n") else {
        panic!("compiled")
    };
    assert!(diagnostics.iter().any(|d| d.code == codes::U0102 && d.message.contains("risky")), "{diagnostics:?}");
}
