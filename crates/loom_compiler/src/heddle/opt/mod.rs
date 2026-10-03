//! The optimizer (design/vm.md §7): passes over Heddle, between building it
//! and generating code.
//!
//! At `-O0` nothing runs: the code is what the builder made, which compiles
//! fastest and keeps every call (so every frame of a backtrace). `-O2`, the
//! default, runs the passes in order. Each pass keeps what Heddle promises
//! (`verify`), which the optimizer's tests check after every pass.

mod inline;
mod tidy;

use super::Module;

/// How hard to optimize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Level {
    /// Nothing: the code as built.
    O0,
    /// Every pass.
    #[default]
    O2,
}

/// `module`, optimized at `level`.
pub fn optimize(module: Module, level: Level) -> Module {
    match level {
        Level::O0 => module,
        Level::O2 => inline::inline(&module),
    }
}
