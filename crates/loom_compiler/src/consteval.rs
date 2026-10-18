//! Compile-time code, run on Treadle (design/vm.md §17, V4).
//!
//! Before a program is lowered, what has to run while compiling is built
//! as Heddle (`heddle::build_jobs`): one job per constant, per `const`
//! expression and per `const assert`. They become one image, which runs in
//! a VM of its own, in this process, one `call` per job, under a budget
//! (steps, time and memory, for the whole compile) and with the machine
//! refused: output, the environment, files, time and the terminal end a job
//! with K0104. Each failure becomes a K diagnostic here.
//!
//! A constant is computed the first time anything reads it (its flag word
//! in the VM's data says whether it's been, see `heddle/build/consts.rs`),
//! so a `const` expression in a loop is computed once, and a job that
//! needs a constant another job is computing is a cycle (K0107). After a
//! job fails, the constants it was computing are marked failed: later jobs
//! that need them stop quietly rather than reporting the same failure
//! again.
//!
//! Only the bodies that have compile-time code are looked at; the program
//! itself is built afterwards (and constants are computed again when it
//! runs: baking what's computed into the program is V4's next step).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::{SourceDb, Span};
use loom_sys::{Budget, Host, Location, Outcome};
use loom_vm::{Config, Limits, Vm};

use crate::baked::{Baked, BakedConst, Reader};
use crate::heddle::{self, ConstSlot, Job, JobKind, Jobs};
use crate::hir::Program;
use crate::typeck::Typed;

/// How many loop iterations and calls compile-time code may run, for a
/// whole build (see `loom_vm::Limits`: counted in chunks of 65 536).
pub const STEP_BUDGET: u64 = 100_000_000;
/// How long compile-time code may run, for a whole build.
pub const TIME_LIMIT: Duration = Duration::from_secs(20);
/// How much memory compile-time code may have at once.
pub const MEMORY_LIMIT: u64 = 1 << 30;
/// How much stack compile-time code gets: deep recursion is an unravel.
const STACK_LIMIT: usize = 32 << 20;

/// Runs the program's compile-time code. `tests`: the tests' too. What it
/// computed that can be baked into the program is given back.
pub fn evaluate(db: &SourceDb, p: &Program, typed: &Typed, tests: bool) -> Result<Baked, Vec<Diagnostic>> {
    let Some(jobs) = heddle::build_jobs(p, typed, tests)? else { return Ok(Baked::default()) };
    let image = crate::codegen::generate(db, &jobs.module).map_err(crate::treadle::codegen_diagnostics)?;
    let config = Config {
        stack_limit: STACK_LIMIT,
        limits: Limits { steps: Some(STEP_BUDGET), time: Some(TIME_LIMIT), memory: Some(MEMORY_LIMIT) },
        refuse_host: true,
        ..Config::default()
    };
    let host = Host::captured(Rc::new(RefCell::new(Vec::new())));
    let mut vm = match Vm::new(&image, host, &config) {
        Ok(vm) => vm,
        Err(outcome) => return Err(vec![internal(jobs.jobs[0].span, &outcome)]),
    };
    let mut diags = Vec::new();
    // Out of steps or time, or an internal error: the rest isn't run.
    let mut stopped = false;
    for job in &jobs.jobs {
        if stopped {
            break;
        }
        let ran = match job.kind {
            JobKind::Const { slot, .. } | JobKind::Value { slot } => {
                // Computed already, as another's operand, or failed.
                if matches!(vm.data()[slot as usize / 8], heddle::DONE | heddle::FAILED) {
                    continue;
                }
                let dst = vm.data_addr() + u64::from(slot) + 8;
                vm.call(job.func.0, &[dst]).map(|_| false)
            }
            // 1: its condition is false.
            JobKind::Assert { .. } => vm.call(job.func.0, &[]).map(|results| results.first() == Some(&1)),
        };
        let outcome = match ran {
            Ok(false) => continue,
            Ok(true) => {
                let message = String::from_utf8_lossy(vm.message()).into_owned();
                diags.push(with_instance(assert_failed(job, &message), typed, job));
                continue;
            }
            Err(outcome) => outcome,
        };
        // What it was computing failed with it.
        for slot in &jobs.slots {
            let word = slot.offset as usize / 8;
            if vm.data()[word] == heddle::COMPUTING {
                vm.set_data(word, heddle::FAILED);
            }
        }
        let diag = match &outcome {
            Outcome::Unravel { message, location, .. } => {
                if let Some(rest) = message.strip_prefix(heddle::CYCLE) {
                    // The unravel is in the constant's init: the read is
                    // where that was called from.
                    let read = vm.backtrace().get(1).and_then(|(_, at)| at.clone()).or_else(|| location.clone());
                    cycle(&jobs, job, rest, read)
                } else if message.starts_with(heddle::GAVE_UP) {
                    continue;
                } else {
                    with_location(
                        Diagnostic::error(codes::K0103, format!("compile-time code unravelled: {message}"))
                            .primary(job.span, what(p, job)),
                        "it unravelled at",
                        location.clone(),
                    )
                    .help("unravelling while compiling is a compile error")
                }
            }
            Outcome::Refused { native, location } => with_location(
                Diagnostic::error(codes::K0104, format!("`{}` can't run at compile time", user_path(native)))
                    .primary(job.span, what(p, job)),
                "called at",
                location.clone(),
            )
            .note("compile-time code computes; it can't read or write anything outside the compiler"),
            Outcome::OverBudget { budget, location } => {
                stopped |= matches!(budget, Budget::Steps | Budget::Time);
                over_budget(job, p, *budget, location.clone())
            }
            Outcome::Crash { .. } => {
                // Nothing about the machine can be trusted now.
                stopped = true;
                internal(job.span, &outcome)
            }
            _ => internal(job.span, &outcome),
        };
        diags.push(with_instance(diag, typed, job));
    }
    // `LOOM_CONST_STATS=1` says how much compile-time code ran (the budget
    // is `STEP_BUDGET`; bench/RESULTS.md has what the corpus uses).
    if std::env::var_os("LOOM_CONST_STATS").is_some() {
        eprintln!("compile-time code: {} jobs, {} steps", jobs.jobs.len(), vm.steps_used());
    }
    // In source order, rather than the order the jobs ran in.
    diags.sort_by_key(|d| d.labels.first().map(|l| (l.span.file, l.span.lo)));
    if !diags.is_empty() {
        return Err(diags);
    }
    // What every job computed, read back by type.
    let mut baked = Baked::default();
    let mut reader = Reader::new(p, typed);
    for slot in &jobs.slots {
        if vm.data()[slot.offset as usize / 8] != heddle::DONE {
            continue;
        }
        let addr = vm.data_addr() + u64::from(slot.offset) + 8;
        if let Some(val) = reader.read(&vm, slot.ty, addr) {
            baked.consts.insert(slot.key, BakedConst { ty: slot.ty, val });
        }
    }
    // What the constants own is freed as a program's is at its end, and the
    // checked build says if that wasn't everything.
    if let Some(exit) = jobs.exit {
        let ended = vm.call(exit.0, &[]);
        let leaked = vm.ended(ended);
        if !matches!(leaked, Outcome::Finished) {
            return Err(vec![internal(jobs.jobs[0].span, &leaked)]);
        }
    }
    Ok(baked)
}

/// What a job was doing, for the label of its diagnostics.
fn what(p: &Program, job: &Job) -> String {
    match job.kind {
        JobKind::Const { def, .. } => format!("while computing `{}`", p.def(def).name),
        JobKind::Value { .. } | JobKind::Assert { .. } => "while running this".to_string(),
    }
}

/// K0101: a `const assert` that's false.
fn assert_failed(job: &Job, message: &str) -> Diagnostic {
    let JobKind::Assert { cond } = job.kind else { unreachable!("only an assert fails like this") };
    let title = match message {
        "" => "const assertion failed".to_string(),
        m => format!("const assertion failed: {m}"),
    };
    Diagnostic::error(codes::K0101, title).primary(cond, "this is false")
}

/// K0107: reading the constant whose flag word is at offset `slot` (the
/// marker's text) while computing it.
fn cycle(jobs: &Jobs, job: &Job, slot: &str, location: Option<Location>) -> Diagnostic {
    let found: Option<&ConstSlot> = slot.parse::<u32>().ok().and_then(|o| jobs.slots.iter().find(|s| s.offset == o));
    let (span, name) = match found {
        Some(s) => (s.span, s.name.clone()),
        None => (job.span, "this constant".to_string()),
    };
    with_location(
        Diagnostic::error(codes::K0107, format!("{name} needs its own value"))
            .primary(span, "computing this reads it again"),
        "read again at",
        location,
    )
}

/// K0102: compile-time code used up a budget.
fn over_budget(job: &Job, p: &Program, budget: Budget, location: Option<Location>) -> Diagnostic {
    let (title, note) = match budget {
        Budget::Memory => {
            ("compile-time code ran out of memory".to_string(), "it may build something that never ends".to_string())
        }
        Budget::Steps => (
            "compile-time code ran out of steps".to_string(),
            format!("compile-time code may take {STEP_BUDGET} steps in all; this may never stop"),
        ),
        Budget::Time => (
            "compile-time code ran out of time".to_string(),
            format!("compile-time code may run for {} seconds in all; this may never stop", TIME_LIMIT.as_secs()),
        ),
    };
    with_location(Diagnostic::error(codes::K0102, title).primary(job.span, what(p, job)), "stopped at", location)
        .note(note)
}

/// What a program would call a native a job was refused.
fn user_path(native: &str) -> &str {
    match native {
        "out" => "std.io.print",
        "env.get" => "std.env.get",
        "env.args" => "std.env.args",
        "time.sleep" => "std.time.sleep",
        "time.now" => "std.time.monotonic_nanos",
        "io.read_line" => "std.io.read_line",
        "io.read_all" => "std.io.read_all",
        "io.write_err" => "std.io.write_err",
        "term.raw_mode" => "std.term.raw_mode",
        "term.poll_keys" => "std.term.poll_keys",
        "fs.read" => "std.fs.read",
        "fs.write" => "std.fs.write_text",
        "fs.exists" => "std.fs.exists",
        "fs.remove" => "std.fs.remove_file",
        "fs.append" => "std.fs.File.append",
        "fs.write_line" => "std.fs.File.write_line",
        other => other,
    }
}

fn with_instance(diag: Diagnostic, typed: &Typed, job: &Job) -> Diagnostic {
    match typed.results.instance_names.get(&job.body) {
        Some(name) => diag.note(format!("in {name}")),
        None => diag,
    }
}

fn with_location(diag: Diagnostic, what: &str, location: Option<Location>) -> Diagnostic {
    match location {
        Some(at) => diag.note(format!("{what} {}:{}:{}", at.file, at.line, at.column)),
        None => diag,
    }
}

/// A failure of the compile-time machinery itself: a bug in Loom.
fn internal(span: Span, outcome: &Outcome) -> Diagnostic {
    let message = match outcome {
        Outcome::Unravel { message, .. } | Outcome::Crash { message, .. } | Outcome::Failed { message } => {
            message.clone()
        }
        Outcome::Finished => "it finished".into(),
        Outcome::Exit(code) => format!("it called `process.exit({code})`"),
        Outcome::Interrupted => "it was interrupted".into(),
        Outcome::OverBudget { .. } | Outcome::Refused { .. } => "it was stopped".into(),
    };
    Diagnostic::error(codes::K0103, format!("compile-time code failed inside Loom: {message}"))
        .primary(span, "while running this")
        .note("this is a bug in Loom, not in your program")
}
