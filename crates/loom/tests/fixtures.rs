//! The fixture corpora in `tests/` (see tests/README.md).
//!
//! - `tests/run/NAME.loom` is compiled and run in-process. Its output must
//!   match `NAME.out`. With `NAME.unravel`, the run must unravel with the
//!   message on that file's first line, at the Loom line its second line
//!   names (`line 4`), and, if there is a third line, with the thread it
//!   came from started on the line it names (`thread line 9`; `no thread`
//!   if it isn't a thread's); otherwise it must finish.
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
    let typed = typeck::check_for(&resolved.program, Target::LUA);
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
        (Some(spec), Outcome::Unravel { message, location, thread }) => {
            let mut lines = spec.lines();
            let want_message = lines.next().unwrap_or_default();
            let want_line = lines.next().and_then(|l| l.strip_prefix("line ")).and_then(|n| n.parse::<u32>().ok());
            // `thread line N`: the thread whose unravel it is was started on
            // that line; `no thread`: it isn't a thread's.
            let want_thread = match lines.next() {
                Some("no thread") => Some(None),
                Some(l) => l.strip_prefix("thread line ").and_then(|n| n.parse::<u32>().ok()).map(Some),
                None => None,
            };
            if message != want_message {
                Some(format!("unraveled with {message:?}, expected {want_message:?}"))
            } else if want_line.is_some() && location.as_ref().map(|l| l.line) != want_line {
                Some(format!("unraveled at {location:?}, expected line {want_line:?}"))
            } else if want_thread.is_some_and(|want| thread.as_ref().map(|l| l.line) != want) {
                Some(format!("the thread was started at {thread:?}, expected line {want_thread:?}"))
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
    let workers = std::env::var("LOOM_TEST_WORKERS").ok().and_then(|w| w.parse().ok()).unwrap_or(1);
    let config = loom_vm::Config { stack_limit: 64 << 20, workers, ..loom_vm::Config::default() };
    let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
    let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
    let vm_out = path.with_extension("vm.out");
    let expected =
        std::fs::read_to_string(if vm_out.exists() { vm_out } else { path.with_extension("out") }).unwrap_or_default();
    // With several workers the threads' lines come in any order: the same
    // lines, then.
    let sorted = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    let same = if workers > 1 { sorted(&out) == sorted(&expected) } else { out == expected };
    if !same {
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

/// `tests/par`: programs whose output doesn't depend on the order the
/// threads run in, on Treadle with several workers (design/vm.md §11.4).
/// Each runs a few times at each count: a lost update, a torn message or a
/// wake that never comes shows up as a wrong total or a hang.
#[test]
fn par_corpus() {
    let mut failures = Vec::new();
    for path in corpus("par") {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let expected = std::fs::read_to_string(path.with_extension("out")).unwrap_or_default();
        for level in [Level::O2, Level::O0] {
            let (db, diags, image) = compile_vm(&path, level);
            let Some(image) = image else {
                failures.push(format!("{name}: didn't compile:\n{}", rendered(&db, &diags)));
                continue;
            };
            for workers in [2, 4, 8] {
                for _ in 0..5 {
                    let buffer = Rc::new(RefCell::new(Vec::new()));
                    let config = loom_vm::Config { stack_limit: 64 << 20, workers, ..loom_vm::Config::default() };
                    let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
                    let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
                    // Threads print in any order, but each line whole: the
                    // `.out` files are in sorted order.
                    let mut lines: Vec<&str> = out.lines().collect();
                    lines.sort_unstable();
                    let out = lines.iter().map(|l| format!("{l}\n")).collect::<String>();
                    if out != expected || !matches!(outcome, Outcome::Finished) {
                        failures.push(format!(
                            "{name} at {level:?}, {workers} workers: ended with {outcome:?}\n--- expected\n{expected}--- got\n{out}"
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} par runs failed:\n{}", failures.len(), failures.join("\n"));
}

/// `tests/par_lines/print_lines.loom` on many workers: eight tasks print 10,000
/// built lines each. The lines come in any order across tasks, but every line
/// is whole, none is missing or repeated, and a task's own lines are in order.
#[test]
fn par_output_stays_whole() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/par_lines/print_lines.loom");
    for level in [Level::O2, Level::O0] {
        let (db, diags, image) = compile_vm(&path, level);
        let image = image.unwrap_or_else(|| {
            panic!(
                "didn't compile:
{}",
                rendered(&db, &diags)
            )
        });
        for workers in [2, 4, 8] {
            for _ in 0..3 {
                let buffer = Rc::new(RefCell::new(Vec::new()));
                let config = loom_vm::Config { stack_limit: 64 << 20, workers, ..loom_vm::Config::default() };
                let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
                assert!(matches!(outcome, Outcome::Finished), "{level:?}, {workers} workers: {outcome:?}");
                let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
                let mut next = [0u64; 8];
                let mut lines = 0;
                for line in out.lines() {
                    let parts: Vec<&str> = line.split(' ').collect();
                    let whole = parts.len() == 7
                        && parts[0] == "task"
                        && parts[2] == "line"
                        && parts[4] == "sum"
                        && parts[6] == "end";
                    assert!(whole, "{level:?}, {workers} workers: a torn line: {line:?}");
                    let (task, n, sum): (usize, u64, u64) = (
                        parts[1].parse().expect("a task"),
                        parts[3].parse().expect("a line number"),
                        parts[5].parse().expect("a sum"),
                    );
                    assert_eq!(parts[3].len(), 6, "{level:?}, {workers} workers: {line:?}");
                    assert_eq!(sum, task as u64 * 100_000 + n, "{level:?}, {workers} workers: {line:?}");
                    assert_eq!(n, next[task], "{level:?}, {workers} workers: task {task} out of order: {line:?}");
                    next[task] += 1;
                    lines += 1;
                }
                assert_eq!(lines, 80_000, "{level:?}, {workers} workers");
            }
        }
    }
}

/// Eight tasks wait for one slow constant (`tests/par/const_slow.loom`): they
/// park until it is done, they don't spin. `const_waits` counts the times a
/// task found the constant being computed by another; a task that waits by
/// yielding in a loop would count thousands.
#[cfg(feature = "vm-stats")]
#[test]
fn tasks_wait_for_a_slow_constant_without_spinning() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/par/const_slow.loom");
    for level in [Level::O2, Level::O0] {
        let (db, diags, image) = compile_vm(&path, level);
        let image = image.unwrap_or_else(|| {
            panic!(
                "didn't compile:
{}",
                rendered(&db, &diags)
            )
        });
        let entry = image.entry.expect("a main");
        let (mut most, mut waited) = (0, 0);
        for _ in 0..10 {
            let buffer = Rc::new(RefCell::new(Vec::new()));
            let config = loom_vm::Config { stack_limit: 64 << 20, workers: 8, ..loom_vm::Config::default() };
            let mut vm = loom_vm::Vm::new(&image, Host::captured(buffer.clone()), &config).expect("verifies");
            vm.call(entry, &[]).expect("runs");
            assert_eq!(
                String::from_utf8_lossy(&buffer.borrow()),
                "total 9600004
"
            );
            let waits = vm.runtime_stats().expect("a stats build").const_waits;
            most = most.max(waits);
            waited += waits;
        }
        println!("{level:?}: at most {most} waits in a run, {waited} in 10");
        assert!(most <= 24, "{level:?}: a run waited {most} times for the constant");
        assert!(waited > 0, "{level:?}: no task ever waited, so the test proves nothing");
    }
}

/// A task that unravels cancels the ones parked waiting for a slow constant
/// (`tests/run/threads_const_cancel.loom`): the program ends with the unravel,
/// not a hang, whichever way the tasks interleave.
#[test]
fn a_waiter_for_a_constant_is_cancelled() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run/threads_const_cancel.loom");
    let (db, diags, image) = compile_vm(&path, Level::O2);
    let image = image.unwrap_or_else(|| {
        panic!(
            "didn't compile:
{}",
            rendered(&db, &diags)
        )
    });
    let (done, ended) = std::sync::mpsc::channel();
    // Detached, so a hung run fails the test instead of hanging it.
    std::thread::spawn(move || {
        for round in 0..40 {
            let buffer = Rc::new(RefCell::new(Vec::new()));
            let config = loom_vm::Config { stack_limit: 64 << 20, workers: 8, ..loom_vm::Config::default() };
            let outcome = loom_vm::run(&image, Host::captured(buffer), &config);
            let ok = matches!(&outcome, Outcome::Unravel { message, .. } if message == "stop");
            if done.send((round, ok, format!("{outcome:?}"))).is_err() || !ok {
                return;
            }
        }
    });
    for _ in 0..40 {
        let (round, ok, outcome) = ended
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("a run hung: a waiter for the constant was never woken");
        assert!(ok, "round {round}: {outcome}");
    }
}

/// A constant that owns memory is dropped on every way out of the program
/// (`tests/const_drop`): the checked build reports what is left allocated as a
/// crash.
#[cfg(feature = "vm-checked")]
#[test]
fn constants_that_own_memory_are_dropped_on_every_exit() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/const_drop");
    let run = |name: &str, workers: usize| {
        let (db, diags, image) = compile_vm(&dir.join(name), Level::O2);
        let image = image.unwrap_or_else(|| {
            panic!(
                "{name} didn't compile:
{}",
                rendered(&db, &diags)
            )
        });
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let config = loom_vm::Config { stack_limit: 64 << 20, workers, ..loom_vm::Config::default() };
        let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
        (outcome, String::from_utf8_lossy(&buffer.borrow()).into_owned())
    };
    let stop = |o: &Outcome| matches!(o, Outcome::Unravel { message, .. } if message == "stop");
    for workers in [1, 8] {
        let (outcome, _) = run("unravel.loom", workers);
        assert!(stop(&outcome), "unravel at {workers} workers: {outcome:?}");
        let (outcome, _) = run("exit.loom", workers);
        assert_eq!(outcome, Outcome::Exit(3), "exit at {workers} workers");
        let (outcome, out) = run("failed.loom", workers);
        assert!(matches!(&outcome, Outcome::Failed { message } if message == "bad"), "{outcome:?}");
        assert_eq!(
            out,
            "10
"
        );
    }
    for round in 0..30 {
        let (outcome, _) = run("threads.loom", 8);
        assert!(stop(&outcome), "round {round}: {outcome:?}");
    }
}

/// The `loom for` fixtures over ranges, on 8 workers, where a task takes a
/// run of items (design/vm.md §11.4): the same output, and the failing one
/// ends the same way and frees what its items own (a checked build, as in
/// `cargo test --features vm-checked`, catches a leak).
#[test]
fn loom_for_chunks_on_many_workers() {
    let mut failures = Vec::new();
    for name in ["loom_for_chunks", "loom_for_chunks_failed"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run").join(format!("{name}.loom"));
        let expected = std::fs::read_to_string(path.with_extension("out")).unwrap_or_default();
        for level in [Level::O2, Level::O0] {
            let (db, diags, image) = compile_vm(&path, level);
            let Some(image) = image else {
                failures.push(format!(
                    "{name}: didn't compile:
{}",
                    rendered(&db, &diags)
                ));
                continue;
            };
            for _ in 0..10 {
                let buffer = Rc::new(RefCell::new(Vec::new()));
                let config = loom_vm::Config { stack_limit: 64 << 20, workers: 8, ..loom_vm::Config::default() };
                let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
                let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
                let problem =
                    check_outcome(&path, outcome).or_else(|| (out != expected).then(|| format!("printed {out:?}")));
                if let Some(problem) = problem {
                    failures.push(format!("{name} at {level:?}: {problem}"));
                    break;
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}",
        failures.join(
            "
"
        )
    );
}

/// The `tests/par` programs again and again, on many workers: a race that
/// shows once in a thousand runs. `cargo test -- --ignored par_stress`.
#[test]
#[ignore = "slow: run with --ignored"]
fn par_stress() {
    let mut failures = Vec::new();
    for path in corpus("par") {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let expected = std::fs::read_to_string(path.with_extension("out")).unwrap_or_default();
        let (_, _, image) = compile_vm(&path, Level::O2);
        let image = image.expect("compiles");
        for run in 0..1000 {
            let workers = [2, 4, 8, 16][run % 4];
            let buffer = Rc::new(RefCell::new(Vec::new()));
            let config = loom_vm::Config { stack_limit: 64 << 20, workers, ..loom_vm::Config::default() };
            let outcome = loom_vm::run(&image, Host::captured(buffer.clone()), &config);
            let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
            let mut lines: Vec<&str> = out.lines().collect();
            lines.sort_unstable();
            let out = lines.iter().map(|l| format!("{l}\n")).collect::<String>();
            if out != expected || !matches!(outcome, Outcome::Finished) {
                failures.push(format!("{name} run {run} on {workers} workers: {outcome:?}\n{out}"));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The thread fixtures of `tests/run` on 8 workers, again and again: the
/// output may come in another order, so only how each ends is compared
/// (never a crash, a leak (in a checked build) or a hang, and the same kind
/// of ending as with one worker). `cargo test -- --ignored thread_fixtures`.
#[test]
#[ignore = "slow: run with --ignored"]
fn thread_fixtures_on_many_workers() {
    let mut failures = Vec::new();
    let threaded = ["threads_", "mutex_", "pipe_", "loom_for", "atomic_"];
    for path in corpus("run") {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        if !threaded.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        if std::env::var("LOOM_FIXTURE").is_ok_and(|only| only != name) {
            continue;
        }
        let (_, _, image) = compile_vm(&path, Level::O2);
        let Some(image) = image else { continue };
        let run = |workers: usize| {
            let buffer = Rc::new(RefCell::new(Vec::new()));
            let config = loom_vm::Config { stack_limit: 64 << 20, workers, ..loom_vm::Config::default() };
            loom_vm::run(&image, Host::captured(buffer), &config)
        };
        let reference = std::mem::discriminant(&run(1));
        for round in 0..1000 {
            let workers = std::env::var("LOOM_TEST_WORKERS").ok().and_then(|w| w.parse().ok()).unwrap_or(8);
            let outcome = run(workers);
            if matches!(outcome, Outcome::Crash { .. }) || std::mem::discriminant(&outcome) != reference {
                failures.push(format!("{name}, round {round}: {outcome:?}"));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
