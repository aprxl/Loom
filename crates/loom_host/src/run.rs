//! Running a compiled bundle.
//!
//! The bundle is one Lua chunk that returns `main`, or for `loom test` a
//! list of `{name, run}` tests. The host gives it a `__LOOM_HOST` table
//! (so output goes through Rust's stdout, which writes UTF-8 correctly to
//! the Windows console), calls what it returned, and classifies what
//! happened. An unravel is a string starting with the runtime's marker,
//! raised at the Loom code's position; anything else that escapes is a bug
//! in the compiler or runtime.

use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;

use mlua::{Function, Lua, Table};

pub use loom_sys::{Host, Outcome, Output};

use crate::srcmap::{Location, SourceMap};
use loom_sys::term::Term;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The marker at the start of every unravel message (`rt.MARK`).
pub const UNRAVEL_MARK: &str = "\u{1f}LOOM\u{1f}";
/// After [`UNRAVEL_MARK`]: `process.exit`, and its code follows.
const EXIT_MARK: &str = "\u{1f}exit ";
/// After [`UNRAVEL_MARK`]: Ctrl-C.
const INTERRUPT_MARK: &str = "\u{1f}interrupt";

/// How much memory a program may use. Past it, it unravels; without a
/// limit, an allocation the system refuses (`s.repeat(1 << 40)`) aborts
/// the process.
pub const RUN_MEMORY_LIMIT: usize = 8 << 30;

thread_local! {
    /// What the program has printed that isn't written yet (a file or a
    /// pipe: one write for many lines, as Treadle's output does).
    static STDOUT_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// Whether stdout is a terminal, which sees each line as it's printed.
    static STDOUT_TERMINAL: bool = std::io::IsTerminal::is_terminal(&std::io::stdout());
}

/// How much output is kept before it's written.
const STDOUT_LIMIT: usize = 32 * 1024;

/// Prints `bytes`, buffered unless stdout is a terminal.
fn stdout_write(bytes: &[u8]) {
    let terminal = STDOUT_TERMINAL.with(|t| *t);
    STDOUT_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.extend_from_slice(bytes);
        if terminal || buffer.len() >= STDOUT_LIMIT {
            write_stdout(&mut buffer);
        }
    });
}

fn write_stdout(buffer: &mut Vec<u8>) {
    if !buffer.is_empty() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(buffer);
        let _ = out.flush();
        buffer.clear();
    }
}

/// Writes what's buffered: before anything that waits, reads from the
/// terminal or ends the run, so what's printed is seen when it should be.
pub(crate) fn flush_stdout() {
    STDOUT_BUFFER.with(|buffer| write_stdout(&mut buffer.borrow_mut()));
}

/// Runs `bundle`, mapping error positions through `map`.
pub fn run_bundle(bundle: &str, map: Option<&SourceMap>, host: Host) -> Outcome {
    let lua = Lua::new();
    let _ = lua.set_memory_limit(RUN_MEMORY_LIMIT);
    let stdout = matches!(host.output, Output::Stdout);
    let term = Rc::new(RefCell::new(Term::default()));
    let watch = watch_interrupt(&lua);
    let result = install_host(&lua, host, term.clone()).and_then(|()| {
        // Not `eval`, which loads the chunk as an expression first and, if
        // that fails for any reason (an interrupt included), as statements.
        let main: Function = lua.load(bundle).set_name("=bundle").into_function()?.call(())?;
        // `main ! E` returns its error's text when it fails.
        main.call::<Option<mlua::String>>(())
    });
    drop(watch);
    term.borrow_mut().restore();
    if stdout {
        flush_stdout();
    }
    outcome(result, map)
}

/// The Lua states a Ctrl-C has to reach: the main one's address, and those
/// of the program's coroutines (shuttles) that are alive. A hook is a
/// state's own, so one set on the main state never runs while a coroutine
/// does. (Only while the watch is installed: one program at a time, in the
/// `loom` binary.)
static MAIN_STATE: AtomicUsize = AtomicUsize::new(0);
static COROUTINES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// The program was interrupted, and is unwinding: once, so its `defer`s run
/// to the end (a `sleep` in one doesn't raise it again).
static RAISED: AtomicBool = AtomicBool::new(false);

/// Every state a hook goes on or comes off.
fn states() -> Vec<usize> {
    let mut all = vec![MAIN_STATE.load(Ordering::Relaxed)];
    if let Ok(coroutines) = COROUTINES.lock() {
        all.extend(coroutines.iter().copied());
    }
    all
}

/// Takes the hook off every state (the unwinding runs `defer`s, which no hook
/// may interrupt), the lock released before anything raises.
///
/// # Safety
///
/// As [`stop`]: the states are alive (the main one, and the coroutines the
/// runtime lists until they're finished).
#[allow(unsafe_code)]
unsafe fn clear_hooks() {
    for state in states() {
        // SAFETY: as the caller promises.
        unsafe { mlua::ffi::lua_sethook(state as *mut mlua::ffi::lua_State, None, 0, 0) };
    }
}

/// With Ctrl-C's handler installed, a watch that unwinds the program at
/// its next instruction once Ctrl-C comes, as the standalone `lua` does: a
/// hook installed then (in the main state and in every coroutine alive),
/// which raises the interrupt and takes itself away (so the unwinding runs
/// `defer`s to the end). A hook installed all along would send every
/// instruction through Lua's slow path.
fn watch_interrupt(lua: &Lua) -> Option<loom_sys::interrupt::Watch> {
    loom_sys::interrupt::installed()?;
    MAIN_STATE.store(lua.exec_raw_lua(|raw| raw.state()) as usize, Ordering::Relaxed);
    RAISED.store(false, Ordering::Relaxed);
    if let Ok(mut coroutines) = COROUTINES.lock() {
        coroutines.clear();
    }
    Some(loom_sys::interrupt::watch(move || {
        let mask = mlua::ffi::LUA_MASKCALL | mlua::ffi::LUA_MASKRET | mlua::ffi::LUA_MASKCOUNT;
        // Held while the hooks go on, so a coroutine can't be forgotten
        // (and freed) in between.
        let Ok(coroutines) = COROUTINES.lock() else { return };
        let all = std::iter::once(MAIN_STATE.load(Ordering::Relaxed)).chain(coroutines.iter().copied());
        for state in all {
            // SAFETY: `lua_sethook` may be called while the state runs (the
            // standalone `lua` calls it from a signal handler): it only sets
            // the hook's fields, read at the next instruction. The main state
            // is alive: the watch goes before it, and can't while this runs.
            // A coroutine is, while it's in the list, which the runtime keeps
            // it in only until it's finished (and holds it meanwhile).
            #[allow(unsafe_code)]
            unsafe {
                mlua::ffi::lua_sethook(state as *mut mlua::ffi::lua_State, Some(stop), mask, 1)
            }
        }
    }))
}

/// The hook `watch_interrupt` installs: raises the interrupt.
///
/// # Safety
///
/// Only Lua calls it, as a hook, with the state it's running in.
#[allow(unsafe_code)]
unsafe extern "C-unwind" fn stop(state: *mut mlua::ffi::lua_State, _: *mut mlua::ffi::lua_Debug) {
    let message = format!("{UNRAVEL_MARK}{INTERRUPT_MARK}");
    RAISED.store(true, Ordering::Relaxed);
    // SAFETY: as above; Lua calls the hook with its state; the message is
    // copied in before it's freed, and `lua_error` doesn't return (nothing
    // of this frame is left to drop).
    unsafe {
        clear_hooks();
        mlua::ffi::lua_sethook(state, None, 0, 0);
        mlua::ffi::lua_pushlstring(state, message.as_ptr().cast(), message.len());
        drop(message);
        mlua::ffi::lua_error(state);
    }
}

/// One test of a test bundle, and how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRun {
    pub name: String,
    pub outcome: Outcome,
    /// What it printed.
    pub output: String,
}

/// Runs the tests of a bundle built by `lower_tests`, one after another,
/// each with its own output. `Err` if the bundle doesn't even load.
pub fn run_tests(bundle: &str, map: Option<&SourceMap>, sandbox: bool) -> Result<Vec<TestRun>, Outcome> {
    let lua = Lua::new();
    let _ = lua.set_memory_limit(RUN_MEMORY_LIMIT);
    let buffer = Rc::new(RefCell::new(Vec::new()));
    let host = Host { output: Output::Capture(buffer.clone()), sandbox, args: vec!["test".into()] };
    let term = Rc::new(RefCell::new(Term::default()));
    let tests = install_host(&lua, host, term.clone())
        .and_then(|()| lua.load(bundle).set_name("=bundle").eval::<Table>())
        .map_err(|e| outcome(Err(e), map))?;
    let mut runs = Vec::new();
    for test in tests.sequence_values::<Table>() {
        let (name, run) =
            test.and_then(|t| Ok((t.get::<String>(1)?, t.get::<Function>(2)?))).map_err(|e| outcome(Err(e), map))?;
        let result = run.call::<Option<mlua::String>>(());
        term.borrow_mut().restore();
        let output = String::from_utf8_lossy(&std::mem::take(&mut *buffer.borrow_mut())).into_owned();
        runs.push(TestRun { name, outcome: outcome(result, map), output });
    }
    Ok(runs)
}

/// How a call into the bundle ended.
pub(crate) fn outcome(result: mlua::Result<Option<mlua::String>>, map: Option<&SourceMap>) -> Outcome {
    match result {
        Ok(None) => Outcome::Finished,
        Ok(Some(message)) => Outcome::Failed { message: message.to_string_lossy() },
        Err(e) if is_memory_error(&e) => Outcome::Unravel { message: "out of memory".into(), location: None },
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

/// Whether `e` is Lua running out of memory, maybe inside a callback.
fn is_memory_error(e: &mlua::Error) -> bool {
    match e {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } => is_memory_error(cause),
        _ => false,
    }
}

/// The whole text of a Lua error, traceback included.
pub(crate) fn full_text(e: &mlua::Error) -> String {
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

pub(crate) fn install_host(lua: &Lua, host: Host, term: Rc<RefCell<Term>>) -> mlua::Result<()> {
    let table = lua.create_table()?;
    let sandbox = host.sandbox;
    let output = host.output;
    let write = lua.create_function(move |_, s: mlua::String| {
        match &output {
            Output::Stdout => stdout_write(&s.as_bytes()),
            Output::Capture(buffer) => buffer.borrow_mut().extend_from_slice(&s.as_bytes()),
        }
        Ok(())
    })?;
    table.set("write", write)?;
    let getenv =
        lua.create_function(move |_, name: String| Ok(if sandbox { None } else { std::env::var(name).ok() }))?;
    table.set("getenv", getenv)?;
    // A sandboxed run's files live in memory (see runtime/rt/core.lua).
    table.set("memory_fs", sandbox)?;
    table.set("args", lua.create_sequence_from(host.args)?)?;
    let sleep = lua.create_function(move |_, nanos: i64| {
        if sandbox {
            return Ok(());
        }
        flush_stdout();
        // In slices, so Ctrl-C ends a long sleep, which then unwinds.
        let slice = std::time::Duration::from_millis(50);
        let end = std::time::Instant::now().checked_add(std::time::Duration::from_nanos(nanos.max(0) as u64));
        let flag = loom_sys::interrupt::installed();
        loop {
            if flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed)) && !RAISED.swap(true, Ordering::Relaxed) {
                // SAFETY: the states `clear_hooks` takes the hook off are
                // alive: this runs in the program, which holds them.
                #[allow(unsafe_code)]
                unsafe {
                    clear_hooks()
                };
                return Err(mlua::Error::runtime(format!("{UNRAVEL_MARK}{INTERRUPT_MARK}")));
            }
            let left = end.map_or(slice, |end| end.saturating_duration_since(std::time::Instant::now()));
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(slice));
        }
    })?;
    table.set("sleep", sleep)?;
    // The terminal: each returns an error message when it fails, which the
    // runtime unravels with. A sandboxed run has no terminal and no keys held.
    let raw_term = term.clone();
    let raw_mode = lua.create_function(move |_, on: bool| {
        if !sandbox {
            flush_stdout();
        }
        Ok(if sandbox { None } else { raw_term.borrow_mut().raw_mode(on).err() })
    })?;
    table.set("raw_mode", raw_mode)?;
    let poll_keys = lua.create_function(move |_, keymap: String| {
        if sandbox {
            return Ok((Some(0), None));
        }
        flush_stdout();
        Ok(match term.borrow_mut().poll_keys(&keymap) {
            Ok(bits) => (Some(bits), None),
            Err(message) => (None, Some(message)),
        })
    })?;
    table.set("poll_keys", poll_keys)?;
    // The runtime tells which coroutines are alive, for Ctrl-C (see
    // `watch_interrupt`); it holds each one until it says it's done.
    let watch_thread = lua.create_function(|_, co: mlua::Thread| {
        if loom_sys::interrupt::installed().is_some()
            && let Ok(mut coroutines) = COROUTINES.lock()
        {
            coroutines.push(co.to_pointer() as usize);
        }
        Ok(())
    })?;
    table.set("watch_thread", watch_thread)?;
    let unwatch_thread = lua.create_function(|_, co: mlua::Thread| {
        if let Ok(mut coroutines) = COROUTINES.lock() {
            let at = co.to_pointer() as usize;
            coroutines.retain(|&c| c != at);
        }
        Ok(())
    })?;
    table.set("unwatch_thread", unwatch_thread)?;
    lua.globals().set("__LOOM_HOST", table)
}

/// The message of a Lua error, without mlua's decorations or the Lua
/// traceback mlua appends.
pub(crate) fn error_text(e: &mlua::Error) -> String {
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
        // `process.exit(code)` and Ctrl-C unwind as unravels do.
        Some(message) if message.starts_with(EXIT_MARK) => match message[EXIT_MARK.len()..].parse() {
            Ok(code) => Outcome::Exit(code),
            Err(_) => Outcome::Crash { message: format!("a bad exit code: {message}"), location },
        },
        Some(message) if message == INTERRUPT_MARK => Outcome::Interrupted,
        Some(message) => Outcome::Unravel { message: message.to_string(), location },
        // Recursion that doesn't stop is the program's, not Loom's.
        None if rest.starts_with("stack overflow") || rest.starts_with("C stack overflow") => {
            Outcome::Unravel { message: "stack overflow: too many calls inside each other".into(), location }
        }
        None => Outcome::Crash { message: rest.to_string(), location },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srcmap::Mapping;

    fn capture(bundle: &str) -> (Outcome, String) {
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let outcome = run_bundle(bundle, None, Host::captured(buffer.clone()));
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
        let outcome = run_bundle(bundle, Some(&map), Host::captured(Default::default()));
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

    #[test]
    fn runs_each_test_with_its_own_output() {
        let bundle = "local w = __LOOM_HOST.write\n\
                      return {\n\
                      {'quiet', function() end},\n\
                      {'loud', function() w('one\\n') return 'it broke' end},\n\
                      {'gives_up', function() w('two\\n') error('\\31LOOM\\31nope', 1) end},\n\
                      }";
        let runs = run_tests(bundle, None, true).expect("the bundle loads");
        let summary: Vec<(&str, &Outcome, &str)> =
            runs.iter().map(|r| (r.name.as_str(), &r.outcome, r.output.as_str())).collect();
        assert_eq!(
            summary,
            [
                ("quiet", &Outcome::Finished, ""),
                ("loud", &Outcome::Failed { message: "it broke".into() }, "one\n"),
                ("gives_up", &Outcome::Unravel { message: "nope".into(), location: None }, "two\n"),
            ]
        );
    }

    #[test]
    fn sees_its_arguments() {
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let host = Host { args: vec!["prog".into(), "rom.ch8".into()], ..Host::captured(buffer.clone()) };
        let bundle = "return function() local a = __LOOM_HOST.args __LOOM_HOST.write(a[1] .. ' ' .. a[2]) end";
        assert_eq!(run_bundle(bundle, None, host), Outcome::Finished);
        assert_eq!(buffer.borrow().as_slice(), b"prog rom.ch8");
    }
}
