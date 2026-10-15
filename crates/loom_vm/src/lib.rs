//! Treadle, Loom's own VM (design/vm.md): runs an [`Image`] the compiler
//! built from Heddle.
//!
//! - `exec` is the interpreter: one `match` over [`loom_isa::Op`], with the
//!   state that matters kept in locals.
//! - `stack` holds frames, in segments that never move.
//! - `heap` allocates what lists, strings and boxes own.
//! - `ints` and `fmt` are the integer semantics and the formatting, in safe
//!   Rust.
//! - `natives` are the Rust functions bytecode calls.
//!
//! `unsafe` is used where the verifier (or the compiler's guarantees) make a
//! check redundant: reading registers and code, and reading text. Each use
//! says why it's sound. The `checked` feature adds run-time checks where the
//! compiler is trusted (design/vm.md §5.6).

mod exec;
mod fmt;
mod heap;
mod host;
mod ints;
mod methods;
mod natives;
mod pool;
mod sched;
mod stack;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use loom_isa::{CRASHES, Image, IntSpec};
use loom_sys::{Budget, Host, Location, Outcome};

/// How a run may use the machine.
#[derive(Clone, Debug)]
pub struct Config {
    /// The most bytes the value stack may take. Past it, a call unravels
    /// with `stack overflow`.
    pub stack_limit: usize,
    /// Set from outside (Ctrl-C) to stop the program at its next loop
    /// iteration or call.
    pub interrupt: Option<Arc<AtomicBool>>,
    /// What the program may use up: none, unless it's compile-time code.
    pub limits: Limits,
    /// Compile-time code: the natives that touch the machine (output, the
    /// environment, files, time, the terminal) end the run with
    /// [`Outcome::Refused`] instead of working (or being faked, as in a
    /// sandboxed run).
    pub refuse_host: bool,
    /// How many workers run the program's threads (design/vm.md §11): 1
    /// runs them one at a time, in the order of the reference schedule.
    /// Several are started only for a program that has threads.
    pub workers: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { stack_limit: 256 << 20, interrupt: None, limits: Limits::default(), refuse_host: false, workers: 1 }
    }
}

/// What a run may use up before it's stopped with [`Outcome::OverBudget`]
/// (design/vm.md §9.5). All are checked where Ctrl-C is, every [`TICKS`]
/// loop iterations and calls, so the hot path pays nothing for them, and
/// each is exact only to that: a run overshoots by up to 65 536 ticks, and
/// by the time those take.
///
/// The counts persist across calls of one [`Vm`]: the budget is for all the
/// jobs of one compile, not for each.
///
/// [`TICKS`]: crate::TICKS
#[derive(Clone, Copy, Debug, Default)]
pub struct Limits {
    /// Loop iterations and calls in all (counted in chunks of 65 536).
    pub steps: Option<u64>,
    /// Wall-clock time from when the [`Vm`] is made.
    pub time: Option<std::time::Duration>,
    /// Bytes allocated at once: past it, an allocation ends the run.
    pub memory: Option<u64>,
}

/// Loop iterations and calls between the checks for Ctrl-C and budgets: the
/// unit [`Limits::steps`] is counted in.
pub const TICKS: u64 = 1 << 16;

/// Why a program stopped early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Trap {
    Overflow,
    DivZero,
    ShiftRange,
    /// A value doesn't fit the integer type it's cast to.
    IntRange(IntSpec),
    /// A float doesn't fit the integer type it's cast to.
    FloatRange(IntSpec),
    BadChar,
    /// An index past the end of what it indexes.
    Index {
        index: u64,
        len: u64,
    },
    OutOfMemory,
    StackOverflow,
    Interrupt,
    /// Not a failure: a task operation (`task.*`, `loom.*`), which a native
    /// can't do (it switches tasks). The dispatch loop's trap path runs it,
    /// out of the way of the calls that don't need it.
    Task(crate::sched::TaskOp),
    /// Another worker ended the program: this one stops (never reported).
    Halt,
    /// A thread is being cancelled: its loom is leaving early. It unwinds
    /// like an unravel, runs `defer`s and drops, and is seen by nobody.
    Cancel,
    /// Compile-time code used up `budget`. The first time it unwinds (what
    /// the program owns is freed and its `defer`s run); if it's still
    /// running at the next check (a `defer` that never ends), `abort` is
    /// set, and nothing more runs.
    Budget {
        budget: Budget,
        abort: bool,
    },
    /// Compile-time code reached a native that touches the machine (see
    /// [`Config::refuse_host`]): the native's name, or its family's.
    Refused(&'static str),
    /// `process.exit(code)`: unwinds as an unravel does, then ends the run.
    Exit(u8),
    Unravel(String),
    /// `main` returned this error.
    Failed(String),
    /// A bug in Loom.
    Crash(String),
}

impl Trap {
    fn message(&self) -> String {
        match self {
            Trap::Overflow => "arithmetic overflow".into(),
            Trap::DivZero => "division by zero".into(),
            Trap::ShiftRange => "shift amount out of range".into(),
            Trap::IntRange(to) => format!("value out of range for `{}`", to.name()),
            Trap::FloatRange(to) => format!("float out of range for `{}`", to.name()),
            Trap::BadChar => "not a valid `char`".into(),
            Trap::Index { index, len } => format!("index {index} is out of bounds (len {len})"),
            Trap::OutOfMemory => "out of memory".into(),
            Trap::StackOverflow => "stack overflow: too many calls inside each other".into(),
            Trap::Interrupt => "interrupted (Ctrl-C)".into(),
            Trap::Cancel => "loom internal: a cancelled thread's unwind reached the host".into(),
            Trap::Halt => "loom internal: a worker stopped, and its stop was reported".into(),
            Trap::Task(_) => "loom internal: a task operation reached the host".into(),
            Trap::Budget { .. } => "compile-time code used more than its budget".into(),
            Trap::Refused(native) => format!("`{native}` can't run at compile time"),
            Trap::Exit(code) => format!("exit {code}"),
            Trap::Unravel(message) | Trap::Failed(message) | Trap::Crash(message) => message.clone(),
        }
    }
}

/// A trap, and the word of the instruction it happened at.
#[derive(Debug)]
pub(crate) struct Fault {
    pub trap: Trap,
    pub pc: usize,
    /// Every frame it happened in, innermost first (see `exec::trace_at`).
    pub trace: Vec<usize>,
    /// How many frames were left out of `trace`, after its first 64.
    pub skipped: usize,
}

/// Runs `image`'s entry function, with `host` for its output.
pub fn run(image: &Image, host: Host, config: &Config) -> Outcome {
    let Some(entry) = image.entry else {
        return Outcome::Crash { message: "the image has no entry function".into(), location: None };
    };
    let mut vm = match Vm::new(image, host, config) {
        Ok(vm) => vm,
        Err(outcome) => return outcome,
    };
    let result = vm.call(entry, &[]);
    vm.ended(result)
}

/// A loaded image, whose functions can be called one after another.
#[derive(Debug)]
pub struct Vm<'i> {
    image: &'i Image,
    machine: exec::Machine<'i>,
    /// The frames of the last unravel: each function, and where in it.
    trace: Vec<(String, Option<Location>)>,
    /// Where frames of the trace were left out: how many come before the
    /// gap, and how many calls it stands for.
    gap: Option<(usize, usize)>,
    /// A memory limit was set: running out of memory is running out of
    /// budget.
    memory_limited: bool,
}

impl<'i> Vm<'i> {
    /// Verifies and loads `image`.
    pub fn new(image: &'i Image, host: Host, config: &Config) -> Result<Vm<'i>, Outcome> {
        if let Err(e) = loom_isa::verify(image) {
            return Err(Outcome::Crash { message: format!("invalid bytecode: {e}"), location: None });
        }
        let machine =
            exec::Machine::load(image, host, config).map_err(|message| Outcome::Crash { message, location: None })?;
        Ok(Vm { image, machine, trace: Vec::new(), gap: None, memory_limited: config.limits.memory.is_some() })
    }

    /// The frames the last unravel happened in, innermost first: each
    /// function's name, and where in it (`LOOM_BACKTRACE=1`).
    pub fn backtrace(&self) -> &[(String, Option<Location>)] {
        &self.trace
    }

    /// Where [`backtrace`](Self::backtrace) has frames left out (a stack
    /// overflow is millions deep): the number of frames before the gap, and
    /// how many calls are in it.
    pub fn backtrace_gap(&self) -> Option<(usize, usize)> {
        self.gap
    }

    /// With `checked`: an internal error if anything the program allocated
    /// is still there. A program that ends without unravelling frees all
    /// it allocated (design/vm.md §5.4), so a leak is a compiler bug.
    pub fn leaks(&self) -> Option<Outcome> {
        let (objects, bytes) = self.machine.ctx.heap.leaked();
        (cfg!(feature = "checked") && objects > 0).then(|| Outcome::Crash {
            message: format!("loom internal: {objects} objects ({bytes} bytes) were never freed"),
            location: None,
        })
    }

    /// How a call from [`call`](Self::call) ended, as an outcome: with
    /// `checked`, a leak turns an ending that should have freed everything
    /// (a return, an error `main` gave, an unravel, an exit, Ctrl-C: all
    /// unwind through the pads) into an internal error. An internal error is
    /// left alone: nothing about its state can be trusted.
    pub fn ended(&self, result: Result<Vec<u64>, Outcome>) -> Outcome {
        let outcome = match result {
            Ok(_) => Outcome::Finished,
            Err(outcome) => outcome,
        };
        // An internal error, or a run cut off for its budget (which isn't
        // unwound if it kept running): nothing about its state can be
        // trusted.
        if matches!(outcome, Outcome::Crash { .. } | Outcome::OverBudget { .. }) {
            return outcome;
        }
        self.leaks().unwrap_or(outcome)
    }

    /// How many instructions have been dispatched so far; `None` unless
    /// Treadle is built with the `stats` feature.
    pub fn dispatched(&self) -> Option<u64> {
        cfg!(feature = "stats").then_some(self.machine.ctx.dispatched)
    }

    /// How many times each op was dispatched, the ones that were, most
    /// first; `None` unless Treadle is built with the `stats` feature.
    pub fn op_counts(&self) -> Option<Vec<(loom_isa::Op, u64)>> {
        cfg!(feature = "stats").then(|| {
            let mut out: Vec<(loom_isa::Op, u64)> = loom_isa::Op::ALL
                .iter()
                .zip(&self.machine.ctx.op_counts)
                .filter(|(_, n)| **n > 0)
                .map(|(op, n)| (*op, *n))
                .collect();
            out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.name().cmp(b.0.name())));
            out
        })
    }

    /// How many times each instruction ran, by word of the image's code;
    /// `None` unless Treadle is built with the `stats` feature.
    pub fn pc_counts(&self) -> Option<&[u64]> {
        cfg!(feature = "stats").then_some(self.machine.ctx.pc_counts.as_slice())
    }

    /// The program's data as it is now: the image's, plus what running
    /// changed (a constant's flag and value, once computed). Word `i` is at
    /// address [`data_addr`](Self::data_addr) `+ 8 * i`.
    pub fn data(&self) -> &[u64] {
        self.machine.data()
    }

    /// Sets word `index` of [`data`](Self::data): how compile-time
    /// evaluation marks a constant whose computing failed.
    pub fn set_data(&mut self, index: usize, value: u64) {
        self.machine.set_data(index, value);
    }

    /// How many loop iterations and calls have run so far, to the nearest
    /// [`TICKS`]: counted only when [`Limits`] are set.
    pub fn steps_used(&self) -> u64 {
        self.machine.ctx.steps_spent()
    }

    /// The message built by the last call's `msg.*` natives: an unravel's,
    /// or a failed `const assert`'s.
    pub fn message(&self) -> &[u8] {
        self.machine.ctx.message()
    }

    /// The address of the first word of [`data`](Self::data), which is what
    /// a pointer into the data holds (`gaddr` yields it plus an offset).
    pub fn data_addr(&self) -> u64 {
        self.machine.data().as_ptr() as u64
    }

    /// The text literals of the image: where they are in memory, so a
    /// `str` pointing into them can be told from one in the heap.
    pub fn rodata_range(&self) -> (u64, u64) {
        self.machine.ctx.rodata_range()
    }

    /// Reads `len` bytes at `addr`.
    ///
    /// # Safety
    ///
    /// `addr` must be an address the program holds: of its data, one of the
    /// image's text literals, or a live object of at least `len` bytes (a
    /// pointer read out of a value of a type whose layout says it points
    /// there, and that the program hasn't freed). The result is only valid
    /// until the next call of this [`Vm`].
    #[allow(unsafe_code)]
    pub unsafe fn read(&self, addr: u64, len: usize) -> &[u8] {
        if len == 0 {
            return &[];
        }
        // SAFETY: the caller promises it.
        unsafe { std::slice::from_raw_parts(addr as *const u8, len) }
    }

    /// Calls function `func` with `args`, one register each, and gives its
    /// results; or how it ended when it didn't return. Output is flushed
    /// before this returns.
    pub fn call(&mut self, func: u32, args: &[u64]) -> Result<Vec<u64>, Outcome> {
        // A run that has used up its budget runs nothing more.
        if let Some(budget) = self.machine.ctx.over_budget() {
            return Err(Outcome::OverBudget { budget, location: None });
        }
        let result = self.machine.call(func as usize, args);
        self.machine.ctx.flush();
        // Raw mode left on (an unravel on the way out) is turned off.
        self.machine.ctx.term.restore();
        result.map_err(|fault| {
            let image = self.image;
            // A frame's instruction may be from calls the compiler inlined:
            // a frame each.
            self.trace.clear();
            self.gap = None;
            for (i, &pc) in fault.trace.iter().enumerate() {
                if i == exec::TRACE_ENDS && fault.skipped > 0 {
                    self.gap = Some((self.trace.len(), fault.skipped));
                }
                for (name, at) in image.frames_at(pc as u32) {
                    let at = at.map(|(file, line, column)| Location { file: file.to_string(), line, column });
                    self.trace.push((name.to_string(), at));
                }
            }
            let location = self.image.line_at(fault.pc as u32).map(|(file, line, column)| Location {
                file: file.to_string(),
                line,
                column,
            });
            match fault.trap {
                Trap::Crash(message) => Outcome::Crash { message, location },
                Trap::Failed(message) => Outcome::Failed { message },
                Trap::Exit(code) => Outcome::Exit(code),
                Trap::Interrupt => Outcome::Interrupted,
                Trap::Budget { budget, .. } => Outcome::OverBudget { budget, location },
                Trap::OutOfMemory if self.memory_limited => Outcome::OverBudget { budget: Budget::Memory, location },
                Trap::Refused(native) => Outcome::Refused { native: native.into(), location },
                trap => Outcome::Unravel { message: trap.message(), location },
            }
        })
    }
}

/// What `crash k` says.
fn crash_message(k: usize) -> String {
    CRASHES.get(k).copied().unwrap_or("loom internal: unknown crash").to_string()
}

#[cfg(test)]
mod tests {
    /// The dispatch loop's speed depends on LLVM giving each op its own
    /// indirect jump (design/vm.md §9.1), which `.cargo/config.toml` asks
    /// for with `-tail-dup-*` flags, and on where each op's code lands,
    /// which `-align-all-nofallthru-blocks` keeps steady.
    #[test]
    fn the_dispatch_flags_are_in_effect() {
        let flags = env!("LOOM_VM_RUSTFLAGS");
        assert!(
            flags.contains("tail-dup-pred-size") && flags.contains("align-all-nofallthru-blocks"),
            "Treadle was built without the `-C llvm-args=-tail-dup-*` flags of .cargo/config.toml              (its dispatch loop is up to 30% slower without them). A `RUSTFLAGS` environment              variable replaces the config's flags: put them in it too, or unset it. Flags: {flags:?}"
        );
    }
}
