//! The fixture corpora in `tests/` (see tests/README.md).
//!
//! - `tests/run/NAME.loom` is compiled and run in-process. Its output must
//!   match `NAME.out`. With `NAME.unravel`, the run must unravel with the
//!   message on that file's first line, at the Loom line its second line
//!   names (`line 4`); otherwise it must finish.
//! - `tests/examples/NAME.out` is the exact output of `examples/NAME.loom`,
//!   on both backends.
//! - `tests/test/NAME.out` is the report `loom test` prints for `NAME.loom`.
//! - `tests/fail/NAME.loom` must fail to compile with exactly the
//!   diagnostics its `//~ CODE …` comments list, on the lines they're on.
//!
//! Every `tests/run` fixture also runs on Treadle, Loom's own VM, and must
//! end the same way (`NAME.vm.out`, when there is one, is its output there:
//! design/vm.md §14). `tests/vm_skip.txt` lists the fixtures Treadle can't
//! run yet; one listed that passes is a failure too, so the list only
//! shrinks.
//!
//! Every failure in a corpus is reported at once.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{lower, treadle, typeck};
use loom_host::{Host, Outcome, run_bundle};
use loom_isa::Image;
use loom_syntax::diag::codes;
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

/// What compiling `path` for Treadle gives: its diagnostics, and the image
/// if there were none.
fn compile_vm(path: &Path, level: Level) -> (SourceDb, Vec<Diagnostic>, Option<Image>) {
    let text = std::fs::read_to_string(path).expect("readable fixture");
    let mut db = SourceDb::new();
    let root = db.add(path.display().to_string(), text);
    let resolved = resolve(&mut db, root, &FsLoader::new(path));
    if resolved.has_errors() {
        return (db, resolved.diagnostics, None);
    }
    let typed = typeck::check_for(&resolved.program, Target::VM);
    if typed.has_errors() {
        return (db, typed.diagnostics, None);
    }
    match treadle::compile_at(&db, &resolved.program, &typed, level) {
        Ok(compiled) => (db, Vec::new(), Some(compiled.image)),
        Err(diags) => (db, diags, None),
    }
}

/// The fixtures Treadle can't run yet: `tests/vm_skip.txt`, a name per
/// line followed by the milestone that fixes it (`# …` is a comment).
fn vm_skips() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/vm_skip.txt");
    let text = std::fs::read_to_string(path).expect("tests/vm_skip.txt");
    text.lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.split_whitespace().next().unwrap_or_default().to_string())
        .collect()
}

/// Checks how a run ended against a fixture's `.unravel` and `.error`
/// files; the problem, if it's not right.
fn check_outcome(path: &Path, outcome: Outcome) -> Option<String> {
    let unravel = std::fs::read_to_string(path.with_extension("unravel")).ok();
    let error = std::fs::read_to_string(path.with_extension("error")).ok();
    match (unravel, outcome) {
        (None, Outcome::Finished) if error.is_none() => None,
        (None, Outcome::Failed { message }) if error.is_some() => {
            let want = error.unwrap_or_default();
            (message != want.trim_end()).then(|| format!("failed with {message:?}, expected {:?}", want.trim_end()))
        }
        (Some(spec), Outcome::Unravel { message, location }) => {
            let mut lines = spec.lines();
            let want_message = lines.next().unwrap_or_default();
            let want_line = lines.next().and_then(|l| l.strip_prefix("line ")).and_then(|n| n.parse::<u32>().ok());
            if message != want_message {
                Some(format!("unraveled with {message:?}, expected {want_message:?}"))
            } else if want_line.is_some() && location.as_ref().map(|l| l.line) != want_line {
                Some(format!("unraveled at {location:?}, expected line {want_line:?}"))
            } else {
                None
            }
        }
        (spec, outcome) => Some(format!("ended with {outcome:?}, expected {spec:?}")),
    }
}

/// Runs a `tests/run` fixture on Treadle, its code optimized at `level`;
/// the problem, if it doesn't end the way it should, and `Err` when
/// Treadle can't compile it yet.
fn run_on_vm(path: &Path, level: Level) -> Result<Option<String>, String> {
    let (db, diags, image) = compile_vm(path, level);
    let Some(image) = image else {
        let text = rendered(&db, &diags);
        let not_yet = !diags.is_empty() && diags.iter().all(|d| d.code == codes::U0102);
        return if not_yet { Err(text) } else { Ok(Some(format!("didn't compile for Treadle:\n{text}"))) };
    };
    let buffer = Rc::new(RefCell::new(Vec::new()));
    let config = loom_vm::Config { stack_limit: 64 << 20, ..loom_vm::Config::default() };
    let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
    let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
    let vm_out = path.with_extension("vm.out");
    let expected =
        std::fs::read_to_string(if vm_out.exists() { vm_out } else { path.with_extension("out") }).unwrap_or_default();
    if out != expected {
        return Ok(Some(format!(
            "output differs on Treadle at {level:?} (it ended with {outcome:?})\n--- expected\n{expected}--- got\n{out}"
        )));
    }
    Ok(check_outcome(path, outcome).map(|p| format!("on Treadle at {level:?}: {p}")))
}

fn rendered(db: &SourceDb, diags: &[Diagnostic]) -> String {
    diags.iter().map(|d| render_plain(db, d)).collect::<Vec<_>>().join("\n")
}

#[test]
fn run_corpus() {
    let mut failures = Vec::new();
    let paths = corpus("run");
    assert!(!paths.is_empty());
    let skips = vm_skips();
    for skip in &skips {
        if !paths.iter().any(|p| p.file_stem().and_then(|s| s.to_str()) == Some(skip)) {
            failures.push(format!("tests/vm_skip.txt lists `{skip}`, which isn't a fixture"));
        }
    }
    for path in &paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let skipped = skips.contains(&name);
        // The optimized code (what `loom run` runs) first; the code as
        // built must end the same way.
        for level in [Level::O2, Level::O0] {
            match run_on_vm(path, level) {
                Ok(None) if skipped => {
                    failures.push(format!("{name}: passes on Treadle now: take it out of tests/vm_skip.txt"));
                }
                Ok(Some(problem)) if !skipped => failures.push(format!("{name}: {problem}")),
                Err(diags) if !skipped => failures.push(format!(
                    "{name}: Treadle can't run this yet: list it in tests/vm_skip.txt with its milestone\n{diags}"
                )),
                _ => {}
            }
            if skipped {
                break;
            }
        }
        let (db, diags, bundle) = compile(path);
        let Some(bundle) = bundle else {
            failures.push(format!("{name}: didn't compile:\n{}", rendered(&db, &diags)));
            continue;
        };
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_bundle(&bundle.lua, Some(&bundle.map), Host::captured(buffer.clone()));
        let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
        let expected = std::fs::read_to_string(path.with_extension("out")).unwrap_or_default();
        if out != expected {
            failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{out}"));
        }
        if let Some(problem) = check_outcome(path, outcome) {
            failures.push(format!("{name}: {problem}"));
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
/// must run to the end, on Lua and on Treadle.
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
        let path = root.join("examples").join(format!("{name}.loom"));
        let expected = std::fs::read_to_string(out_path).unwrap_or_default();
        for level in [Level::O2, Level::O0] {
            match compile_vm(&path, level) {
                (_, _, Some(image)) => {
                    let buffer = Rc::new(RefCell::new(Vec::new()));
                    let config = loom_vm::Config { stack_limit: 64 << 20, ..loom_vm::Config::default() };
                    let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
                    let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
                    if out != expected {
                        failures.push(format!(
                            "{name}: output differs on Treadle at {level:?}\n--- expected\n{expected}--- got\n{out}"
                        ));
                    }
                    if !matches!(outcome, Outcome::Finished) {
                        failures.push(format!("{name}: ended with {outcome:?} on Treadle at {level:?}"));
                    }
                }
                (db, diags, None) => {
                    failures.push(format!("{name}: didn't compile for Treadle:\n{}", rendered(&db, &diags)));
                }
            }
        }
        let (db, diags, bundle) = compile(&path);
        let Some(bundle) = bundle else {
            failures.push(format!("{name}: didn't compile:\n{}", rendered(&db, &diags)));
            continue;
        };
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_bundle(&bundle.lua, Some(&bundle.map), Host::captured(buffer.clone()));
        let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
        if out != expected {
            failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{out}"));
        }
        if !matches!(outcome, Outcome::Finished) {
            failures.push(format!("{name}: ended with {outcome:?}"));
        }
    }
    assert!(failures.is_empty(), "{} examples failed:\n\n{}", failures.len(), failures.join("\n\n"));
}

/// `tests/test/NAME.out` is what `loom test tests/test/NAME.loom` prints,
/// run from the repository root, on Lua and on Treadle. It exits with 0
/// when every test passed, and 1 otherwise.
#[test]
fn test_corpus() {
    let mut failures = Vec::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let paths = corpus("test");
    assert!(!paths.is_empty());
    for path in &paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        for (backend, level) in [("lua", "-O2"), ("vm", "-O2"), ("vm", "-O0")] {
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_loom"))
                .current_dir(&root)
                .args(["--color", "never", level, "test", "--backend", backend, &format!("tests/test/{name}.loom")])
                .output()
                .expect("the loom binary runs");
            let report = String::from_utf8_lossy(&out.stdout).into_owned();
            let expected = std::fs::read_to_string(path.with_extension("out")).unwrap_or_default();
            if report != expected {
                let stderr = String::from_utf8_lossy(&out.stderr);
                failures.push(format!(
                    "{name} ({backend} {level}): report differs\n--- expected\n{expected}--- got\n{report}{stderr}"
                ));
            }
            let want = if expected.contains("\ntest result: ok.") { 0 } else { 1 };
            if out.status.code() != Some(want) {
                failures
                    .push(format!("{name} ({backend} {level}): exited with {:?}, expected {want}", out.status.code()));
            }
        }
    }
    assert!(failures.is_empty(), "{} test fixtures failed:\n\n{}", failures.len(), failures.join("\n\n"));
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
