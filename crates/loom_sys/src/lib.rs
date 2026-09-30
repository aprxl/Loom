//! What a running Loom program gets from its host, shared by both backends:
//! the Lua VM (`loom_host`) and Treadle (`loom_vm`).
//!
//! [`Host`] says where output goes and what the program may see; a run ends
//! with an [`Outcome`], whose unravels and crashes carry the Loom
//! [`Location`] they happened at.

use std::cell::RefCell;
use std::rc::Rc;

/// Where a program's output goes.
#[derive(Clone, Debug)]
pub enum Output {
    Stdout,
    /// Collected in a buffer.
    Capture(Rc<RefCell<Vec<u8>>>),
}

/// What a program run gets from the machine it runs on.
#[derive(Clone, Debug)]
pub struct Host {
    pub output: Output,
    /// No environment variables, files in memory (starting empty), no
    /// terminal and no waiting: a run whose output doesn't depend on the
    /// machine, for the fixture tests.
    pub sandbox: bool,
    /// `env.args()`: the program's name, then its arguments.
    pub args: Vec<String>,
}

impl Host {
    /// A sandboxed run whose output lands in `buffer`.
    pub fn captured(buffer: Rc<RefCell<Vec<u8>>>) -> Host {
        Host { output: Output::Capture(buffer), sandbox: true, args: vec!["main".into()] }
    }
}

/// A Loom source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    /// The program gave up (`unravel`, overflow, a failed assertion…).
    Unravel {
        message: String,
        location: Option<Location>,
    },
    /// `main ! E` (or a test) returned an error, shown here.
    Failed {
        message: String,
    },
    /// Something that isn't the program's fault: a bug in Loom itself.
    Crash {
        message: String,
        location: Option<Location>,
    },
}
