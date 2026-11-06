//! A WebAssembly `loom-play` gets a big stack of its own: the CLI runs its
//! compiler on a thread with 64 MiB (`COMPILER_STACK` in
//! `crates/loom/src/main.rs`), and wasm has no threads to give it one. Half
//! that here: a module whose memory starts at 64 MiB or more crashed Node's
//! WebAssembly outright (Node 22, Windows), and wasm frames are smaller.

fn main() {
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f.split(',').any(|f| f == "wasm"));
    if wasm {
        println!("cargo:rustc-link-arg-bins=-zstack-size={}", 32 << 20);
    }
}
