//! Running a compiled bundle.
//!
//! The bundle is one Lua chunk that returns `main`. The host gives it a
//! `__LOOM_HOST` table (so output goes through Rust's stdout, which writes
//! UTF-8 correctly to the Windows console), calls `main`, and classifies
//! what happened. An unravel is a string starting with the runtime's
//! marker, raised at the Loom code's position; anything else that escapes
//! is a bug in the compiler or runtime.

use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;

use mlua::{Function, Lua};

use crate::srcmap::{Location, SourceMap};

/// The marker at the start of every unravel message (`rt.MARK`).
pub const UNRAVEL_MARK: &str = "\u{1f}LOOM\u{1f}";

/// Where a program's output goes.
#[derive(Clone, Debug)]
pub enum Output {
    Stdout,
    /// Collected, for tests.
    Capture(Rc<RefCell<Vec<u8>>>),
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
    /// `main ! E` returned an error, shown here.
    Failed {
        message: String,
    },
    /// A Lua error that isn't an unravel: a bug in Loom itself.
    Crash {
        message: String,
        location: Option<Location>,
    },
}

/// Runs `bundle`, mapping error positions through `map`.
pub fn run_bundle(bundle: &str, map: Option<&SourceMap>, output: Output) -> Outcome {
    let lua = Lua::new();
    let result = install_host(&lua, output.clone()).and_then(|()| {
        let main: Function = lua.load(bundle).set_name("=bundle").eval()?;
        // `main ! E` returns its error's text when it fails.
        main.call::<Option<mlua::String>>(())
    });
    if let Output::Stdout = output {
        let _ = std::io::stdout().flush();
    }
    match result {
        Ok(None) => Outcome::Finished,
        Ok(Some(message)) => Outcome::Failed { message: message.to_string_lossy() },
        Err(e) => {
            let mut outcome = classify(&error_text(&e), map);
            // Raised inside the runtime (a stream's step, say): the Loom
            // code that called into it is the nearest frame that has a
            // location.
            if let Outcome::Unravel { location: location @ None, .. }
            | Outcome::Crash { location: location @ None, .. } = &mut outcome
            {
                *location = traceback_location(&full_text(&e), map);
            }
            outcome
        }
    }
}

/// The whole text of a Lua error, traceback included.
fn full_text(e: &mlua::Error) -> String {
    match e {
        mlua::Error::RuntimeError(message) => message.clone(),
        mlua::Error::CallbackError { cause, traceback } => format!("{}\n{traceback}", full_text(cause)),
        other => other.to_string(),
    }
}

/// The first frame of a Lua traceback that maps to Loom code.
fn traceback_location(text: &str, map: Option<&SourceMap>) -> Option<Location> {
    let (_, trace) = text.split_once("stack traceback:")?;
    trace.lines().find_map(|line| {
        let (n, _) = line.trim().strip_prefix("bundle:")?.split_once(':')?;
        map?.locate(n.parse().ok()?)
    })
}

fn install_host(lua: &Lua, output: Output) -> mlua::Result<()> {
    let host = lua.create_table()?;
    let write = lua.create_function(move |_, s: mlua::String| {
        match &output {
            Output::Stdout => {
                let _ = std::io::stdout().write_all(&s.as_bytes());
            }
            Output::Capture(buffer) => buffer.borrow_mut().extend_from_slice(&s.as_bytes()),
        }
        Ok(())
    })?;
    host.set("write", write)?;
    lua.globals().set("__LOOM_HOST", host)
}

/// The message of a Lua error, without mlua's decorations or the Lua
/// traceback mlua appends.
fn error_text(e: &mlua::Error) -> String {
    let text = match e {
        mlua::Error::RuntimeError(message) | mlua::Error::SyntaxError { message, .. } => message.clone(),
        mlua::Error::CallbackError { cause, .. } => return error_text(cause),
        other => other.to_string(),
    };
    match text.split_once("\nstack traceback:") {
        Some((message, _)) => message.to_string(),
        None => text,
    }
}

/// Splits `bundle:123: message` into the Loom location of line 123 and the
/// message, and tells unravels from crashes.
fn classify(text: &str, map: Option<&SourceMap>) -> Outcome {
    let (location, rest) = match text.strip_prefix("bundle:").and_then(|t| t.split_once(':')) {
        Some((line, rest)) => {
            let location = line.parse::<u32>().ok().and_then(|l| map.and_then(|m| m.locate(l)));
            (location, rest.trim_start())
        }
        None => (None, text),
    };
    match rest.strip_prefix(UNRAVEL_MARK) {
        Some(message) => Outcome::Unravel { message: message.to_string(), location },
        None => Outcome::Crash { message: rest.to_string(), location },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srcmap::Mapping;

    fn capture(bundle: &str) -> (Outcome, String) {
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_bundle(bundle, None, Output::Capture(buffer.clone()));
        let text = String::from_utf8(buffer.borrow().clone()).expect("utf-8 output");
        (outcome, text)
    }

    #[test]
    fn runs_main_and_captures_output() {
        let (outcome, text) = capture("return function() __LOOM_HOST.write('hi\\n') end");
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(text, "hi\n");
    }

    #[test]
    fn unravels_carry_a_mapped_location() {
        let map = SourceMap {
            files: vec!["a.loom".into()],
            lines: vec![Mapping { lua_line: 2, file: 0, line: 7, column: 5 }],
        };
        let bundle = "return function()\n  error('\\31LOOM\\31boom', 1)\nend";
        let outcome = run_bundle(bundle, Some(&map), Output::Capture(Default::default()));
        assert_eq!(
            outcome,
            Outcome::Unravel {
                message: "boom".into(),
                location: Some(Location { file: "a.loom".into(), line: 7, column: 5 })
            }
        );
    }

    #[test]
    fn other_errors_are_crashes() {
        let (outcome, _) = capture("return function() local t = nil; return t.x end");
        assert!(matches!(outcome, Outcome::Crash { .. }), "{outcome:?}");
        let (outcome, _) = capture("this isn't lua");
        assert!(matches!(outcome, Outcome::Crash { .. }), "{outcome:?}");
    }
}
