//! The fixture corpora in `tests/` (see tests/README.md).
//!
//! - `tests/run/NAME.loom` is compiled and run in-process. Its output must
//!   match `NAME.out`. With `NAME.unravel`, the run must unravel with the
//!   message on that file's first line, at the Loom line its second line
//!   names (`line 4`); otherwise it must finish.
//! - `tests/examples/NAME.out` is the exact output of `examples/NAME.loom`.
//! - `tests/fail/NAME.loom` must fail to compile with exactly the
//!   diagnostics its `//~ CODE …` comments list, on the lines they're on.
//!
//! Every failure in a corpus is reported at once.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::{lower, typeck};
use loom_host::{Outcome, Output, run_bundle};
use loom_syntax::diag::render_plain;
use loom_syntax::{Diagnostic, SourceDb};

fn corpus(name: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests").join(name);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "loom"))
        .collect();
    paths.sort();
    paths
}

/// Every diagnostic from compiling `path`, and the bundle if there were none.
fn compile(path: &Path) -> (SourceDb, Vec<Diagnostic>, Option<lower::Bundle>) {
    let text = std::fs::read_to_string(path).expect("readable fixture");
    let mut db = SourceDb::new();
    let root = db.add(path.display().to_string(), text);
    let resolved = resolve(&mut db, root, &FsLoader::new(path));
    if resolved.has_errors() {
        return (db, resolved.diagnostics, None);
    }
    let typed = typeck::check(&resolved.program);
    if typed.has_errors() {
        return (db, typed.diagnostics, None);
    }
    match lower::lower(&db, &resolved.program, &typed) {
        Ok(bundle) => (db, Vec::new(), Some(bundle)),
        Err(diags) => (db, diags, None),
    }
}

fn rendered(db: &SourceDb, diags: &[Diagnostic]) -> String {
    diags.iter().map(|d| render_plain(db, d)).collect::<Vec<_>>().join("\n")
}

#[test]
fn run_corpus() {
    let mut failures = Vec::new();
    let paths = corpus("run");
    assert!(!paths.is_empty());
    for path in &paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let (db, diags, bundle) = compile(path);
        let Some(bundle) = bundle else {
            failures.push(format!("{name}: didn't compile:\n{}", rendered(&db, &diags)));
            continue;
        };
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_bundle(&bundle.lua, Some(&bundle.map), Output::Capture(buffer.clone()));
        let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
        let expected = std::fs::read_to_string(path.with_extension("out")).unwrap_or_default();
        if out != expected {
            failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{out}"));
        }
        let unravel = std::fs::read_to_string(path.with_extension("unravel")).ok();
        let error = std::fs::read_to_string(path.with_extension("error")).ok();
        match (unravel, outcome) {
            (None, Outcome::Finished) if error.is_none() => {}
            (None, Outcome::Failed { message }) if error.is_some() => {
                let want = error.unwrap_or_default();
                if message != want.trim_end() {
                    failures.push(format!("{name}: failed with {message:?}, expected {:?}", want.trim_end()));
                }
            }
            (Some(spec), Outcome::Unravel { message, location }) => {
                let mut lines = spec.lines();
                let want_message = lines.next().unwrap_or_default();
                let want_line = lines.next().and_then(|l| l.strip_prefix("line ")).and_then(|n| n.parse::<u32>().ok());
                if message != want_message {
                    failures.push(format!("{name}: unraveled with {message:?}, expected {want_message:?}"));
                }
                if want_line.is_some() && location.as_ref().map(|l| l.line) != want_line {
                    failures.push(format!("{name}: unraveled at {location:?}, expected line {want_line:?}"));
                }
            }
            (spec, outcome) => failures.push(format!("{name}: ended with {outcome:?}, expected {spec:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} run fixtures failed:\n\n{}",
        failures.len(),
        paths.len(),
        failures.join("\n\n")
    );
}

/// `tests/examples/NAME.out` is the output of `examples/NAME.loom`, which
/// must run to the end.
#[test]
fn examples_corpus() {
    let mut failures = Vec::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("tests/examples");
    let mut outs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "out"))
        .collect();
    outs.sort();
    assert!(!outs.is_empty());
    for out_path in &outs {
        let name = out_path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let (db, diags, bundle) = compile(&root.join("examples").join(format!("{name}.loom")));
        let Some(bundle) = bundle else {
            failures.push(format!("{name}: didn't compile:\n{}", rendered(&db, &diags)));
            continue;
        };
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_bundle(&bundle.lua, Some(&bundle.map), Output::Capture(buffer.clone()));
        let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
        let expected = std::fs::read_to_string(out_path).unwrap_or_default();
        if out != expected {
            failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{out}"));
        }
        if !matches!(outcome, Outcome::Finished) {
            failures.push(format!("{name}: ended with {outcome:?}"));
        }
    }
    assert!(failures.is_empty(), "{} examples failed:\n\n{}", failures.len(), failures.join("\n\n"));
}

#[test]
fn fail_corpus() {
    let mut failures = Vec::new();
    let paths = corpus("fail");
    assert!(!paths.is_empty());
    for path in &paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let text = std::fs::read_to_string(path).expect("readable fixture");
        // `//~ T0101 T0102` on a line: those diagnostics, on that line.
        let mut expected: Vec<(u32, String)> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if let Some((_, codes)) = line.split_once("//~") {
                for code in codes.split_whitespace() {
                    expected.push((i as u32 + 1, code.to_string()));
                }
            }
        }
        let (db, diags, bundle) = compile(path);
        if bundle.is_some() {
            failures.push(format!("{name}: compiled, but shouldn't have"));
            continue;
        }
        let mut got: Vec<(u32, String)> = diags
            .iter()
            .filter_map(|d| {
                let primary = d.labels.iter().find(|l| l.primary)?;
                Some((db.line_col(primary.span).line, d.code.id.to_string()))
            })
            .collect();
        expected.sort();
        got.sort();
        if got != expected {
            failures
                .push(format!("{name}: expected {expected:?}\n        got      {got:?}\n{}", rendered(&db, &diags)));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fail fixtures failed:\n\n{}",
        failures.len(),
        paths.len(),
        failures.join("\n\n")
    );
}
