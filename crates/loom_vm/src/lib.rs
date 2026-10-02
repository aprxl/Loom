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
mod stack;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use loom_isa::{CRASHES, Image, IntSpec};
use loom_sys::{Host, Location, Outcome};

/// How a run may use the machine.
#[derive(Clone, Debug)]
pub struct Config {
    /// The most bytes the value stack may take. Past it, a call unravels
    /// with `stack overflow`.
    pub stack_limit: usize,
    /// Set from outside (Ctrl-C) to stop the program at its next loop
    /// iteration or call.
    pub interrupt: Option<Arc<AtomicBool>>,
}

impl Default for Config {
    fn default() -> Self {
        Config { stack_limit: 1 << 30, interrupt: None }
    }
}

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
    match vm.call(entry, &[]) {
        Ok(_) => vm.leaks().unwrap_or(Outcome::Finished),
        // `main` returned an error: the run ended normally all the same.
        Err(failed @ Outcome::Failed { .. }) => vm.leaks().unwrap_or(failed),
        Err(outcome) => outcome,
    }
}

/// A loaded image, whose functions can be called one after another.
#[derive(Debug)]
pub struct Vm<'i> {
    image: &'i Image,
    machine: exec::Machine<'i>,
    /// The frames of the last unravel: each function, and where in it.
    trace: Vec<(String, Option<Location>)>,
}

impl<'i> Vm<'i> {
    /// Verifies and loads `image`.
    pub fn new(image: &'i Image, host: Host, config: &Config) -> Result<Vm<'i>, Outcome> {
        if let Err(e) = loom_isa::verify(image) {
            return Err(Outcome::Crash { message: format!("invalid bytecode: {e}"), location: None });
        }
        let machine =
            exec::Machine::load(image, host, config).map_err(|message| Outcome::Crash { message, location: None })?;
        Ok(Vm { image, machine, trace: Vec::new() })
    }

    /// The frames the last unravel happened in, innermost first: each
    /// function's name, and where in it (`LOOM_BACKTRACE=1`).
    pub fn backtrace(&self) -> &[(String, Option<Location>)] {
        &self.trace
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

    /// How many instructions have been dispatched so far; `None` unless
    /// Treadle is built with the `stats` feature.
    pub fn dispatched(&self) -> Option<u64> {
        cfg!(feature = "stats").then_some(self.machine.ctx.dispatched)
    }

    /// Calls function `func` with `args`, one register each, and gives its
    /// results; or how it ended when it didn't return. Output is flushed
    /// before this returns.
    pub fn call(&mut self, func: u32, args: &[u64]) -> Result<Vec<u64>, Outcome> {
        let result = self.machine.call(func as usize, args);
        self.machine.ctx.flush();
        // Raw mode left on (an unravel on the way out) is turned off.
        self.machine.ctx.term.restore();
        result.map_err(|fault| {
            let image = self.image;
            self.trace = fault
                .trace
                .iter()
                .map(|&pc| {
                    let name = image.func_at(pc as u32).map(|f| image.funcs[f].name.clone()).unwrap_or_default();
                    let at = image.line_at(pc as u32).map(|(file, line, column)| Location {
                        file: file.to_string(),
                        line,
                        column,
                    });
                    (name, at)
                })
                .collect();
            let location = self.image.line_at(fault.pc as u32).map(|(file, line, column)| Location {
                file: file.to_string(),
                line,
                column,
            });
            match fault.trap {
                Trap::Crash(message) => Outcome::Crash { message, location },
                Trap::Failed(message) => Outcome::Failed { message },
                trap => Outcome::Unravel { message: trap.message(), location },
            }
        })
    }
}

/// What `crash k` says.
fn crash_message(k: usize) -> String {
    CRASHES.get(k).copied().unwrap_or("loom internal: unknown crash").to_string()
}
