//! Snapshot tests of Heddle, the IR Treadle's code is made from: each
//! `tests/heddle/NAME.loom`, checked for Treadle and built, must print as
//! `NAME.heddle`. Run with `LOOM_BLESS=1` to write the snapshots anew.

use std::path::Path;

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{heddle, typeck};
use loom_syntax::SourceDb;

#[test]
fn snapshots_match() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/heddle");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/heddle")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "loom"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    let bless = std::env::var_os("LOOM_BLESS").is_some();
    let mut failures = Vec::new();
    for path in &paths {
        let mut db = SourceDb::new();
        let root = db.add(path.display().to_string(), std::fs::read_to_string(path).expect("readable"));
        let resolved = resolve(&mut db, root, &FsLoader::new(path));
        assert!(!resolved.has_errors(), "{}: {:?}", path.display(), resolved.diagnostics);
        let typed = typeck::check_for(&resolved.program, Target::VM);
        assert!(!typed.has_errors(), "{}: {:?}", path.display(), typed.diagnostics);
        let module = heddle::build(&resolved.program, &typed).expect("builds");
        let got = heddle::print(&module);
        let snapshot = path.with_extension("heddle");
        if bless {
            std::fs::write(&snapshot, &got).expect("writable");
            continue;
        }
        let want = std::fs::read_to_string(&snapshot).unwrap_or_default().replace("\r\n", "\n");
        if got != want {
            failures.push(format!("{}\n--- expected\n{want}--- got\n{got}", path.display()));
        }
    }
    assert!(failures.is_empty(), "{} snapshots differ:\n\n{}", failures.len(), failures.join("\n\n"));
}
