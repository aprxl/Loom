//! The jobs of compile-time evaluation (design/vm.md §17, V4), built for
//! every program of the corpus: what `heddle::build_jobs` finds, and that
//! nothing the Lua backend accepts is refused by Treadle's builder just
//! because compile-time code needs it.

use std::path::{Path, PathBuf};

use loom_compiler::heddle::{self, JobKind};
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::typeck;
use loom_syntax::SourceDb;

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("a directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "loom"))
        .collect();
    files.sort();
    files
}

/// Every program that type-checks for the Lua target builds its jobs, with
/// no diagnostic: the builder meets only what compile-time code reaches.
#[test]
fn every_program_builds_its_jobs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut programs = 0;
    let mut with_jobs = 0;
    let mut refused = Vec::new();
    for dir in ["tests/run", "tests/fail", "examples", "bench"] {
        for path in sources(&root.join(dir)) {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let mut db = SourceDb::new();
            let file = db.add(path.display().to_string(), text);
            let resolved = resolve(&mut db, file, &FsLoader::new(&path));
            if resolved.has_errors() {
                continue;
            }
            let typed = typeck::check_for(&resolved.program, Target::LUA);
            if typed.has_errors() {
                continue;
            }
            programs += 1;
            match heddle::build_jobs(&resolved.program, &typed, true) {
                Ok(None) => {}
                Ok(Some(jobs)) => {
                    with_jobs += 1;
                    assert!(!jobs.jobs.is_empty());
                    for job in &jobs.jobs {
                        if let JobKind::Const { slot, .. } | JobKind::Value { slot } = job.kind {
                            assert!(
                                jobs.slots.iter().any(|s| s.offset == slot),
                                "{}: a job without a slot",
                                path.display()
                            );
                        }
                    }
                }
                Err(diags) => {
                    let first = diags.first().map(|d| d.message.clone()).unwrap_or_default();
                    refused.push(format!("{}: {first}", path.strip_prefix(&root).unwrap_or(&path).display()));
                }
            }
        }
    }
    println!("{programs} programs, {with_jobs} with compile-time code");
    assert!(refused.is_empty(), "the builder refused:\n{}", refused.join("\n"));
    assert!(with_jobs > 5, "only {with_jobs} programs had jobs");
}
