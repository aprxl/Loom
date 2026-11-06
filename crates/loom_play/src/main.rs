//! `loom-play NAME MODE`: plays the Loom file read from standard input, as
//! if it were the file `NAME` given to `loom run`, `loom check` or
//! `loom test` (MODE is `run`, `check` or `test`). What the program prints
//! goes to standard output as it runs, what `loom` would write to standard
//! error follows it there, and the exit code is `loom`'s.
//!
//! The docs' playground runs this, built for WebAssembly, in the browser
//! (`docs/play/build.py`, `docs/play/play.js`).

use std::io::{Read as _, Write as _};
use std::process::ExitCode;

use loom_play::{Mode, play};
use loom_sys::Output;

/// A usage error.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(name), Some(mode)) = (args.first(), args.get(1).and_then(|m| Mode::parse(m))) else {
        eprintln!("usage: loom-play NAME run|check|test < FILE");
        return ExitCode::from(EXIT_USAGE);
    };
    let mut source = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut source) {
        eprintln!("error: can't read the program: {e}");
        return ExitCode::from(EXIT_USAGE);
    }
    let played = play(name, &source, mode, Output::Stdout);
    let _ = std::io::stdout().flush();
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(played.stderr.as_bytes());
    let _ = stderr.flush();
    ExitCode::from(played.code)
}
