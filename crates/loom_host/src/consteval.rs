//! Compile-time evaluation: running `const` code in a sandbox.
//!
//! The compiler lowers the program as usual, but the bundle returns a
//! list of jobs rather than `main`: one per constant and per `const`
//! expression or `const assert`. Each job returns nothing when it
//! succeeds, or the message of the `const assert` it checks when that
//! fails. The sandbox has no host I/O (std's I/O fails with
//! [`JobResult::NotConst`]), a step budget and a memory limit shared by
//! every job, so compile-time code can't hang the compiler or depend on
//! the machine.
//!
//! The bundle also returns an `abort` function: after a job fails, the
//! constants it was computing are marked failed, and later jobs that read
//! them are [`JobResult::Skipped`] rather than reporting the same failure
//! again.

use std::cell::Cell;
use std::rc::Rc;

use mlua::{Function, HookTriggers, Lua, Table, VmState};

use crate::run::{Outcome, error_text, full_text, install_host, outcome};
use crate::srcmap::{Location, SourceMap};
use crate::{Host, Output};

/// How many Lua instructions compile-time code may run, for a whole build.
pub const STEP_BUDGET: u64 = 100_000_000;
/// How much memory the compile-time VM may use.
pub const MEMORY_LIMIT: usize = 1 << 30;

const NOT_CONST: &str = "\u{1f}LOOMK\u{1f}";
const CYCLE: &str = "\u{1f}LOOMC\u{1f}";
const GAVE_UP: &str = "\u{1f}LOOMS\u{1f}";
const OUT_OF_STEPS: &str = "\u{1f}LOOMB\u{1f}";

/// How one job went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobResult {
    Ok,
    /// A `const assert` was false: its message.
    AssertFailed(String),
    /// The code unravelled (or crashed: [`Outcome::Crash`]).
    Failed(Outcome),
    /// It called `path`, which does I/O.
    NotConst {
        path: String,
        location: Option<Location>,
    },
    /// It read the constant `key` (its Lua name) while computing it.
    Cycle {
        key: String,
        location: Option<Location>,
    },
    /// It ran out of steps or memory.
    OutOfBudget {
        memory: bool,
        location: Option<Location>,
    },
    /// It needed a constant that already failed.
    Skipped,
}

/// Runs the jobs of a bundle built for compile-time evaluation, in order.
/// `Err` if the bundle doesn't even load.
pub fn run_jobs(bundle: &str, map: Option<&SourceMap>) -> Result<Vec<JobResult>, Outcome> {
    let lua = Lua::new();
    let host = Host { output: Output::Capture(Default::default()), sandbox: true, args: vec!["const".into()] };
    let loaded = install_host(&lua, host, Rc::new(Default::default()))
        .and_then(|()| lua.globals().get::<Table>("__LOOM_HOST"))
        .and_then(|table| table.set("const_eval", true))
        .and_then(|()| lua.set_memory_limit(MEMORY_LIMIT))
        .and_then(|_| lua.load(bundle).set_name("=bundle").eval::<Table>());
    let steps = Rc::new(Cell::new(0u64));
    let counter = steps.clone();
    lua.set_hook(HookTriggers::new().every_nth_instruction(1000), move |_, _| {
        counter.set(counter.get() + 1000);
        if counter.get() > STEP_BUDGET {
            return Err(mlua::Error::runtime(OUT_OF_STEPS));
        }
        Ok(VmState::Continue)
    })
    .map_err(|e| outcome(Err(e), map))?;
    let entry = loaded.map_err(|e| outcome(Err(e), map))?;
    let abort: Function = entry.get("abort").map_err(|e| outcome(Err(e), map))?;
    let jobs: Table = entry.get("jobs").map_err(|e| outcome(Err(e), map))?;
    let mut results = Vec::new();
    let mut out_of_steps = false;
    for job in jobs.sequence_values::<Function>() {
        let job = job.map_err(|e| outcome(Err(e), map))?;
        if out_of_steps {
            results.push(JobResult::Skipped);
            continue;
        }
        let result = match job.call::<Option<mlua::String>>(()) {
            Ok(None) => JobResult::Ok,
            Ok(Some(message)) => JobResult::AssertFailed(message.to_string_lossy()),
            Err(e) => {
                let _ = abort.call::<()>(());
                let result = classify(&e, map);
                out_of_steps |= matches!(result, JobResult::OutOfBudget { memory: false, .. });
                result
            }
        };
        results.push(result);
    }
    Ok(results)
}

fn classify(e: &mlua::Error, map: Option<&SourceMap>) -> JobResult {
    let text = error_text(e);
    let full = full_text(e);
    if matches!(e, mlua::Error::MemoryError(_))
        || matches!(e, mlua::Error::CallbackError { cause, .. } if matches!(**cause, mlua::Error::MemoryError(_)))
    {
        return JobResult::OutOfBudget { memory: true, location: None };
    }
    let location = || match outcome(Err(e.clone()), map) {
        Outcome::Unravel { location, .. } | Outcome::Crash { location, .. } => location,
        _ => None,
    };
    let rest = strip_location(&text);
    if full.contains(OUT_OF_STEPS) {
        return JobResult::OutOfBudget { memory: false, location: location() };
    }
    if rest.starts_with(GAVE_UP) {
        return JobResult::Skipped;
    }
    if let Some(path) = rest.strip_prefix(NOT_CONST) {
        return JobResult::NotConst { path: path.to_string(), location: location() };
    }
    if let Some(key) = rest.strip_prefix(CYCLE) {
        return JobResult::Cycle { key: key.to_string(), location: location() };
    }
    JobResult::Failed(outcome(Err(e.clone()), map))
}

/// `message` without the `bundle:123: ` Lua puts in front.
fn strip_location(message: &str) -> &str {
    match message.strip_prefix("bundle:").and_then(|t| t.split_once(':')) {
        Some((line, rest)) if line.parse::<u32>().is_ok() => rest.trim_start(),
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jobs(body: &str) -> Vec<JobResult> {
        let rt = crate::RT_CORE;
        let bundle = format!(
            "local rt = (function()\n{rt}end)()\nlocal C, CI = {{}}, {{}}\nlocal abort = rt.lazy(C, CI)\n{body}"
        );
        run_jobs(&bundle, None).expect("the bundle loads")
    }

    #[test]
    fn runs_each_job_and_reports_failed_asserts() {
        let results = jobs(
            "return { abort = abort, jobs = {\n\
                            function() end,\n\
                            function() return 'too big' end,\n\
                            function() error(rt.MARK .. 'nope', 1) end,\n\
                            } }",
        );
        assert_eq!(results[0], JobResult::Ok);
        assert_eq!(results[1], JobResult::AssertFailed("too big".into()));
        assert!(matches!(&results[2], JobResult::Failed(Outcome::Unravel { message, .. }) if message == "nope"));
    }

    #[test]
    fn io_is_not_const() {
        let results =
            jobs("return { abort = abort, jobs = { function() rt.ext['std.io.print']({1, {d = rt.i32}}) end } }");
        assert!(matches!(&results[0], JobResult::NotConst { path, .. } if path == "std.io.print"), "{results:?}");
    }

    #[test]
    fn constants_are_computed_once_and_cycles_are_caught() {
        let results = jobs(
            "local n, none = 0, 0\n\
                            CI.A = function() n = n + 1; return 7 end\n\
                            CI.B = function() return C.B end\n\
                            CI.D = function() return C.B + 1 end\n\
                            CI.N = function() none = none + 1; return nil end\n\
                            return { abort = abort, jobs = {\n\
                            function() local _ = C.A + C.A; if n ~= 1 then return 'twice' end end,\n\
                            function() local _ = C.B end,\n\
                            function() local _ = C.D end,\n\
                            function() local _, _ = C.N, C.N; if none ~= 1 then return 'nil twice' end end,\n\
                            } }",
        );
        assert_eq!(results[0], JobResult::Ok);
        assert!(matches!(&results[1], JobResult::Cycle { key, .. } if key == "B"), "{results:?}");
        assert_eq!(results[2], JobResult::Skipped);
        // A constant that's `none` is computed once too.
        assert_eq!(results[3], JobResult::Ok);
    }

    #[test]
    fn endless_code_runs_out_of_steps() {
        let results = jobs("return { abort = abort, jobs = { function() while true do end end, function() end } }");
        assert!(matches!(results[0], JobResult::OutOfBudget { memory: false, .. }), "{results:?}");
        assert_eq!(results[1], JobResult::Skipped);
    }
}
