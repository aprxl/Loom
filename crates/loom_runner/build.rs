//! Has `user32.dll` loaded when a program first reads keys (`term.poll_keys`
//! is all that calls into it), not when the program starts: loading it, and
//! what it loads in turn, took 1.2 ms of every run (as in `loom`'s build
//! script).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() != "msvc" {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:user32.dll");
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}
