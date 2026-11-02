//! What loading a program costs the bytecode verifier: the time of
//! `loom_isa::verify` on the larger examples, with their sizes. Report only.

use std::path::Path;
use std::time::Instant;

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_syntax::SourceDb;

fn image(path: &Path) -> loom_isa::Image {
    let text = std::fs::read_to_string(path).expect("readable example");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text);
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{}: {:?}", path.display(), resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{}: {:?}", path.display(), typed.diagnostics);
    treadle::compile(&db, &resolved.program, &typed).expect("compiles").image
}

#[test]
#[ignore = "timing: run with --release -- --ignored --nocapture"]
fn verify_cost() {
    const RUNS: u32 = 1000;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for name in ["chip8", "tour"] {
        let image = image(&root.join("examples").join(format!("{name}.loom")));
        loom_isa::verify(&image).expect("valid");
        let start = Instant::now();
        for _ in 0..RUNS {
            loom_isa::verify(std::hint::black_box(&image)).expect("valid");
        }
        let mean = start.elapsed() / RUNS;
        println!(
            "{name}: {} functions, {} code words, verify {:?} per image",
            image.funcs.len(),
            image.code.len(),
            mean
        );
    }
}
