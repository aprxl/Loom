//! Treadle's instruction set: what the compiler emits and the VM runs
//! (design/vm.md §8).
//!
//! - [`op`] declares every instruction, once, in a table.
//! - [`word`] packs instructions into 64-bit words.
//! - [`image`] holds a compiled program.
//! - [`natives`] lists the Rust functions bytecode may call.
//! - [`fmt`] says what formatting instructions format.
//! - [`verify`] checks an image before it runs.
//! - [`disasm`] makes listings.
//!
//! This crate is plain data and checks, with no `unsafe`, so the compiler
//! and the VM can share it.

pub mod disasm;
pub mod fmt;
pub mod image;
pub mod natives;
pub mod op;
pub mod verify;
pub mod word;

pub use disasm::disassemble;
pub use fmt::{Align, FmtItem, Spec, ValueKind};
pub use image::{CRASHES, Func, Image, Line};
pub use op::{Form, Op};
pub use verify::{VerifyError, verify};
pub use word::IntSpec;

/// How many registers past a frame hold what a call needs to return: the
/// return address, the caller's frame, and where the results go.
pub const LINK_SLOTS: usize = 3;
