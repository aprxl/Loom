//! The docs' playground: one Loom file, compiled and run in memory, saying
//! what `loom run`, `loom check` or `loom test` would say of it.
//!
//! Built for WebAssembly (`wasm32-wasip1`, by `docs/play/build.py`), the
//! `loom-play` command runs the docs' samples in the browser, edited or not.
//! Natively, `tests/samples.rs` checks that it prints exactly what the CLI
//! prints for every sample of the docs, so the two can't drift apart.
//!
//! A run here differs from the CLI's on purpose in three ways: the program
//! is sandboxed (no environment variables, files in memory, no terminal, no
//! waiting), its threads run on one worker (the reference schedule, as the
//! docs' samples are checked), and nothing installs a Ctrl-C handler (the
//! browser stops a run by ending its worker).

use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::{self, Resolved};
use loom_compiler::lower;
use loom_compiler::modules::MemoryLoader;
use loom_compiler::target::Target;
use loom_compiler::treadle;
use loom_compiler::typeck::{self, Typed};
use loom_runner::{describe_in, exit_status, test_report};
use loom_syntax::diag::{MAX_SHOWN, render_plain, report_tail};
use loom_syntax::{Diagnostic, SourceDb};
use loom_sys::{Host, Outcome, Output, TestRun};
use loom_vm::{Config, Vm};

/// The exit code of a program that doesn't compile.
pub const EXIT_COMPILE_ERROR: u8 = 2;

/// What to do with the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `loom run`: compile for Treadle and run `main`.
    Run,
    /// `loom check`: report errors, run nothing.
    Check,
    /// `loom test`: run the file's tests and print a report.
    Test,
}

impl Mode {
    pub fn parse(word: &str) -> Option<Mode> {
        match word {
            "run" => Some(Mode::Run),
            "check" => Some(Mode::Check),
            "test" => Some(Mode::Test),
            _ => None,
        }
    }
}

/// How a play ended: the exit code `loom` would give, and what it would
/// write to standard error (diagnostics, an unravel and where it happened).
/// Standard output went to the [`Output`] given to [`play`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Played {
    pub code: u8,
    pub stderr: String,
}

/// Compiles the file `name` holding `source`, and does what `mode` says.
/// What the program (or `check`, or the test report) prints goes to
/// `output`.
pub fn play(name: &str, source: &str, mode: Mode, output: Output) -> Played {
    let mut stderr = String::new();
    let code = match mode {
        Mode::Run => run(name, source, &output, &mut stderr),
        Mode::Check => check(name, source, &output, &mut stderr),
        Mode::Test => test(name, source, &output, &mut stderr),
    }
    .unwrap_or_else(|code| code);
    Played { code, stderr }
}

/// The source line under a location: only the file being played is known.
fn lookup<'a>(name: &'a str, source: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
    move |file| (file == name).then(|| source.to_string())
}

fn write(output: &Output, text: &str) {
    match output {
        Output::Stdout => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
        Output::Capture(buffer) => buffer.borrow_mut().extend_from_slice(text.as_bytes()),
    }
}

/// The diagnostics of one stage, as `loom` reports them without color.
fn report(db: &SourceDb, name: &str, diagnostics: &[Diagnostic], stage: &str, stderr: &mut String) -> Result<(), u8> {
    for d in &diagnostics[..diagnostics.len().min(MAX_SHOWN)] {
        stderr.push_str(&render_plain(db, d));
    }
    let (tail, failed) = report_tail(name, diagnostics, stage);
    stderr.push_str(&tail);
    if failed { Err(EXIT_COMPILE_ERROR) } else { Ok(()) }
}

/// Parses, resolves and type-checks the file for `target`. When it has
/// syntax errors only those are reported, as the CLI does.
fn front(name: &str, source: &str, target: Target, stderr: &mut String) -> Result<(SourceDb, Resolved, Typed), u8> {
    let mut db = SourceDb::new();
    let id = db.add(name.to_string(), source.to_string());
    let resolved = hir::resolve(&mut db, id, &MemoryLoader::default());
    let syntax_errors = resolved.diagnostics.iter().any(|d| d.is_error() && d.code.id.starts_with('S'));
    if syntax_errors {
        let syntax: Vec<Diagnostic> =
            resolved.diagnostics.iter().filter(|d| d.code.id.starts_with('S')).cloned().collect();
        report(&db, name, &syntax, "parse", stderr)?;
    }
    report(&db, name, &resolved.diagnostics, "resolve", stderr)?;
    let typed = typeck::check_for(&resolved.program, target);
    report(&db, name, &typed.diagnostics, "type-check", stderr)?;
    Ok((db, resolved, typed))
}

fn config() -> Config {
    Config { workers: 1, ..Config::default() }
}

fn run(name: &str, source: &str, output: &Output, stderr: &mut String) -> Result<u8, u8> {
    let (db, resolved, typed) = front(name, source, Target::VM, stderr)?;
    let compiled = match treadle::compile_at(&db, &resolved.program, &typed, Level::O2) {
        Ok(compiled) => compiled,
        Err(diagnostics) => {
            report(&db, name, &diagnostics, "compile", stderr)?;
            return Err(EXIT_COMPILE_ERROR);
        }
    };
    let host = Host { output: output.clone(), sandbox: true, args: vec![name.to_string()] };
    let outcome = match (compiled.image.entry, Vm::new(&compiled.image, host, &config())) {
        (Some(entry), Ok(mut vm)) => {
            let result = vm.call(entry, &[]);
            vm.ended(result)
        }
        (None, _) => Outcome::Crash { message: "the image has no entry function".into(), location: None },
        (_, Err(outcome)) => outcome,
    };
    if let Some(text) = describe_in(&outcome, &lookup(name, source)) {
        stderr.push_str(&text);
    }
    Ok(exit_status(&outcome))
}

fn check(name: &str, source: &str, output: &Output, stderr: &mut String) -> Result<u8, u8> {
    let (db, resolved, typed) = front(name, source, Target::LUA, stderr)?;
    let p = &resolved.program;
    report(&db, name, &lower::check(&db, p, &typed), "compile", stderr)?;
    let n = p.root.map_or(0, |root| p.module(root).decls.len());
    write(output, &format!("{name}: {n} item{}, no errors\n", if n == 1 { "" } else { "s" }));
    Ok(0)
}

fn test(name: &str, source: &str, output: &Output, stderr: &mut String) -> Result<u8, u8> {
    let (db, resolved, typed) = front(name, source, Target::VM, stderr)?;
    let (compiled, tests) = match treadle::compile_tests(&db, &resolved.program, &typed, Level::O2) {
        Ok(done) => done,
        Err(diagnostics) => {
            report(&db, name, &diagnostics, "compile", stderr)?;
            return Err(EXIT_COMPILE_ERROR);
        }
    };
    let mut runs = Vec::with_capacity(tests.len());
    for (test, func) in tests {
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = match Vm::new(&compiled.image, Host::captured(buffer.clone()), &config()) {
            Ok(mut vm) => {
                let result = vm.call(func, &[]);
                vm.ended(result)
            }
            Err(outcome) => outcome,
        };
        let printed = String::from_utf8_lossy(&buffer.borrow()).into_owned();
        runs.push(TestRun { name: test, outcome, output: printed });
    }
    let (text, code) = test_report(&runs, &lookup(name, source));
    write(output, &text);
    Ok(code)
}
