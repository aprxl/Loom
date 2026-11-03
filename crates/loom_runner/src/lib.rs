//! Runs a compiled image on Treadle. `loom run` uses [`run`] on what it just
//! compiled, `loom run x.lmi` on an image read from a file, and the stub
//! `loom build --exe` appends an image to uses it on its own. Nothing here
//! knows about the compiler.

use std::process::ExitCode;

use loom_isa::Image;
use loom_sys::{Host, Location, Outcome, Output};
use loom_vm::{Config, HostTable, Vm};

/// Exit codes (see design/lua-backend.md).
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_INTERNAL: u8 = 70;
pub const EXIT_UNRAVEL: u8 = 101;
pub const EXIT_INTERRUPTED: u8 = 130;

/// What ends an executable `loom build --exe` writes: the length of the
/// image before it, then this.
const EXE_MAGIC: &[u8; 8] = b"LOOMEXE1";

/// The executable made of `runner` (this crate's binary) followed by
/// `image` (what [`loom_isa::encode`] gives), its length and the magic.
pub fn exe_bytes(runner: &[u8], image: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(runner.len() + image.len() + 16);
    out.extend_from_slice(runner);
    out.extend_from_slice(image);
    out.extend_from_slice(&(image.len() as u64).to_le_bytes());
    out.extend_from_slice(EXE_MAGIC);
    out
}

/// The image at the end of an executable `exe_bytes` made.
pub fn image_of_exe(exe: &[u8]) -> Result<&[u8], &'static str> {
    let at = exe.len().checked_sub(16).ok_or("no trailer")?;
    let (rest, trailer) = exe.split_at(at);
    if &trailer[8..] != EXE_MAGIC {
        return Err("no trailer");
    }
    let len = u64::from_le_bytes(trailer[..8].try_into().map_err(|_| "no trailer")?);
    let start = usize::try_from(len).ok().and_then(|len| rest.len().checked_sub(len)).ok_or("bad image length")?;
    Ok(&rest[start..])
}

/// How many workers run a program's threads on Treadle: `LOOM_THREADS`, or
/// one for each core. `LOOM_THREADS=1` gives the reference schedule
/// (design/vm.md §11.1), the one the Lua backend always runs.
pub fn worker_count() -> usize {
    std::env::var("LOOM_THREADS")
        .ok()
        .and_then(|n| n.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
}

/// Runs `image`'s entry function and returns the exit code the program
/// ends with, after printing what went wrong if it did. `program` is the
/// first of `env.args()`. A `trusted` image skips the verifier: only for one
/// whose hash [`loom_isa::decode`] has checked.
pub fn run(image: &Image, program: String, args: Vec<String>, trusted: bool) -> ExitCode {
    let args = std::iter::once(program).chain(args).collect();
    let host = Host { output: Output::Stdout, sandbox: false, args };
    let config =
        Config { interrupt: Some(loom_sys::interrupt::install()), workers: worker_count(), ..Config::default() };
    let vm = if trusted {
        // SAFETY: callers pass `trusted = true` only for an image
        // `loom_isa::decode` returned, so its hash was checked and this
        // toolchain wrote it.
        #[allow(unsafe_code)]
        unsafe {
            Vm::new_trusted(image, host, &config, HostTable::default())
        }
    } else {
        Vm::new(image, host, &config)
    };
    let mut trace = Vec::new();
    let mut gap = None;
    let outcome = match (image.entry, vm) {
        (Some(entry), Ok(mut vm)) => {
            let result = vm.call(entry, &[]);
            let outcome = vm.ended(result);
            trace = vm.backtrace().to_vec();
            gap = vm.backtrace_gap();
            // A stats build's op histogram (`LOOM_STATS=1`).
            if let Some(stats) = vm.runtime_stats()
                && std::env::var_os("LOOM_STATS").is_some_and(|v| v == "3")
            {
                for (i, category) in loom_vm::NATIVE_CATEGORIES.iter().enumerate() {
                    eprintln!("native {category}: {} calls, {} locks", stats.native_calls[i], stats.native_locks[i]);
                }
                eprintln!("task operation locks: {}", stats.task_locks);
                eprintln!(
                    "scheduler probes: {} leftover, {} unfinished, {} cancellation",
                    stats.leftover_probes, stats.unfinished_probes, stats.cancellation_probes
                );
                eprintln!("task stacks allocated: {}", stats.task_stacks);
                eprintln!("constant waits: {}", stats.const_waits);
            }
            if let (Some(counts), Some(total)) = (vm.op_counts(), vm.dispatched())
                && std::env::var_os("LOOM_STATS").is_some_and(|v| v == "1")
            {
                eprintln!("{total} instructions dispatched");
                let checks = |c| counts.iter().filter(|(op, _)| op.check() == Some(c)).map(|(_, n)| n).sum::<u64>();
                eprintln!(
                    "{} bounds checks, {} overflow checks",
                    checks(loom_isa::Check::Bounds),
                    checks(loom_isa::Check::Overflow)
                );
                for (op, n) in counts {
                    eprintln!("{n:>14}  {:5.1}%  {}", 100.0 * n as f64 / total.max(1) as f64, op.name());
                }
            }
            // `LOOM_STATS=2`: the functions that ran most, each
            // instruction after how many times it ran.
            if let Some(pcs) = vm.pc_counts()
                && std::env::var_os("LOOM_STATS").is_some_and(|v| v == "2")
            {
                let mut funcs: Vec<(u64, usize)> = image
                    .funcs
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (pcs[f.entry as usize..(f.entry + f.len) as usize].iter().sum(), i))
                    .collect();
                funcs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                for (n, i) in funcs.into_iter().take(6).filter(|(n, _)| *n > 0) {
                    eprintln!("--- {n} in {}", image.funcs[i].name);
                    eprint!("{}", loom_isa::disassemble_func(image, i, Some(pcs)));
                }
            }
            outcome
        }
        (None, _) => Outcome::Crash { message: "the image has no entry function".into(), location: None },
        (_, Err(outcome)) => outcome,
    };
    if let Some(text) = describe(&outcome) {
        eprint!("{text}");
    }
    if matches!(outcome, Outcome::Unravel { .. }) && std::env::var_os("LOOM_BACKTRACE").is_some_and(|v| v == "1") {
        eprint!("{}", backtrace_text(&trace, gap));
    }
    exit_code(&outcome)
}

pub fn exit_code(outcome: &Outcome) -> ExitCode {
    ExitCode::from(match outcome {
        Outcome::Finished => 0,
        Outcome::Unravel { .. } => EXIT_UNRAVEL,
        Outcome::Failed { .. } => EXIT_FAILED,
        // Only compile-time code, run with limits and refusals, ends so.
        Outcome::Crash { .. } | Outcome::OverBudget { .. } | Outcome::Refused { .. } => EXIT_INTERNAL,
        Outcome::Exit(code) => *code,
        Outcome::Interrupted => EXIT_INTERRUPTED,
    })
}

/// What went wrong, for a run that didn't finish: the message, and where.
pub fn describe(outcome: &Outcome) -> Option<String> {
    Some(match outcome {
        Outcome::Finished | Outcome::Exit(_) => return None,
        Outcome::Interrupted => "interrupted (Ctrl-C)\n".into(),
        Outcome::Unravel { message, location, thread } => {
            let mut text = format!("unravel: {message}\n{}", location_text(location.as_ref()));
            if thread.is_some() {
                text.push_str("  = note: the thread started here unravelled\n");
                text.push_str(&location_text(thread.as_ref()));
            }
            text
        }
        Outcome::Failed { message } => format!("error: {message}\n"),
        Outcome::Crash { message, location } => format!(
            "internal error: {message}\n{}  = note: this is a bug in Loom, not in your program\n",
            location_text(location.as_ref())
        ),
        Outcome::OverBudget { location, .. } | Outcome::Refused { location, .. } => format!(
            "internal error: a run was stopped by compile-time limits\n{}  = note: this is a bug in Loom, not in your program\n",
            location_text(location.as_ref())
        ),
    })
}

/// Every frame an unravel happened in (`LOOM_BACKTRACE=1`, on Treadle),
/// innermost first.
fn backtrace_text(trace: &[(String, Option<Location>)], gap: Option<(usize, usize)>) -> String {
    let mut out = String::from("backtrace:\n");
    for (i, (name, at)) in trace.iter().enumerate() {
        if let Some((before, skipped)) = gap
            && i == before
        {
            out.push_str(&format!("  ... {skipped} more frames ...\n"));
        }
        match at {
            Some(at) => out.push_str(&format!("  {i}: {name} at {}:{}:{}\n", at.file, at.line, at.column)),
            None => out.push_str(&format!("  {i}: {name}\n")),
        }
    }
    out
}

/// `  --> file:line:col` and the source line, with a caret under the column.
fn location_text(location: Option<&Location>) -> String {
    let Some(at) = location else { return String::new() };
    let mut out = format!("  --> {}:{}:{}\n", at.file, at.line, at.column);
    let Ok(text) = std::fs::read_to_string(&at.file) else { return out };
    let Some(line) = text.lines().nth(at.line.saturating_sub(1) as usize) else { return out };
    let number = at.line.to_string();
    let pad = " ".repeat(number.len());
    let caret = " ".repeat(at.column.saturating_sub(1) as usize);
    out.push_str(&format!("{pad} |\n{number} | {line}\n{pad} | {caret}^\n"));
    out
}
