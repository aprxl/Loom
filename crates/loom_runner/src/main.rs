//! The stub `loom build --exe` copies: this file, then an image, its length
//! and `LOOMEXE1`. It reads the image from its own file and runs it,
//! trusting it: the hash [`loom_isa::decode`] checks is all this toolchain
//! gives an image it built itself.

use std::process::ExitCode;

use loom_runner::EXIT_INTERNAL;

fn corrupt(why: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("loom: corrupt executable: {why}");
    ExitCode::from(EXIT_INTERNAL)
}

fn main() -> ExitCode {
    let exe = match std::env::current_exe().and_then(std::fs::read) {
        Ok(exe) => exe,
        Err(e) => return corrupt(&e),
    };
    let bytes = match loom_runner::image_of_exe(&exe) {
        Ok(bytes) => bytes,
        Err(e) => return corrupt(&e),
    };
    let image = match loom_isa::decode(bytes) {
        Ok(image) => image,
        Err(e) => return corrupt(&e),
    };
    let mut args = std::env::args();
    let program = args.next().unwrap_or_default();
    loom_runner::run(&image, program, args.collect(), true)
}
