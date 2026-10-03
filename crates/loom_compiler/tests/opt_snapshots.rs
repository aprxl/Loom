//! Snapshot tests of the optimizer's passes (design/vm.md §7, a V3 gate):
//! each `tests/opt/NAME.loom` starts with
//!
//! ```text
//! // passes: inline
//! // fns: main
//! ```
//!
//! and `NAME.heddle` is the Heddle of those functions after those passes
//! ran on the module as built. `LOOM_BLESS=1` writes the snapshots anew.

use std::path::{Path, PathBuf};

use loom_compiler::heddle::{self, Module};
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::typeck;
use loom_syntax::SourceDb;

fn build(path: &Path, text: &str) -> Module {
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{}: {:?}", path.display(), resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{}: {:?}", path.display(), typed.diagnostics);
    heddle::build(&resolved.program, &typed).unwrap_or_else(|d| panic!("{}: {d:?}", path.display()))
}

/// The words after `// key:` on one of the first lines.
fn header<'t>(text: &'t str, key: &str) -> Vec<&'t str> {
    let prefix = format!("// {key}:");
    text.lines()
        .take_while(|l| l.starts_with("//"))
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .map(|rest| rest.split([',', ' ']).filter(|w| !w.is_empty()).collect())
        .unwrap_or_default()
}

#[test]
fn passes_match_their_snapshots() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/opt");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "loom"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    let bless = std::env::var_os("LOOM_BLESS").is_some();
    let mut failures = Vec::new();
    let mut tested: Vec<&str> = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(path).expect("readable");
        let passes = header(&text, "passes");
        let fns = header(&text, "fns");
        assert!(!passes.is_empty() && !fns.is_empty(), "{}: needs `// passes:` and `// fns:`", path.display());
        let module =
            heddle::opt::run(build(path, &text), &passes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let printed = heddle::print(&module);
        let mut got = String::new();
        for name in &fns {
            let start = printed
                .find(&format!("fn {name}("))
                .unwrap_or_else(|| panic!("{}: no function `{name}`", path.display()));
            let end = printed[start..].find("\n}\n").map_or(printed.len(), |e| start + e + 3);
            got.push_str(&printed[start..end]);
        }
        let snap = path.with_extension("heddle");
        if bless {
            std::fs::write(&snap, &got).expect("writable");
        } else {
            let want = std::fs::read_to_string(&snap).unwrap_or_default();
            if want.replace("\r\n", "\n") != got {
                failures.push(format!("{}: differs\n--- want\n{want}--- got\n{got}", path.display()));
            }
        }
        for p in passes {
            if let Some(name) = heddle::opt::passes().find(|n| *n == p) {
                tested.push(name);
            }
        }
    }
    // Every pass has a snapshot of its own.
    for pass in heddle::opt::passes() {
        if !tested.contains(&pass) {
            failures.push(format!("no snapshot in tests/opt runs the `{pass}` pass"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
