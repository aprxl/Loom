//! Records the rustflags this crate is built with, so a test can check that
//! the dispatch-loop flags of `.cargo/config.toml` are in effect (a
//! `RUSTFLAGS` environment variable replaces them, silently).

fn main() {
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default().replace('\x1f', " ");
    println!("cargo:rustc-env=LOOM_VM_RUSTFLAGS={flags}");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo:rerun-if-changed=build.rs");
}
