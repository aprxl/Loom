//! Treadle's instruction set: what the compiler emits and the VM runs
//! (design/vm.md §8).
//!
//! - [`op`] declares every instruction, once, in a table.
//! - [`word`] packs instructions into 64-bit words.
//! - [`image`] holds a compiled program, and [`image_file`] reads and writes
//!   it as a file.
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
pub mod image_file;
pub mod natives;
pub mod op;
pub mod verify;
pub mod word;

pub use disasm::{disassemble, disassemble_func};
pub use fmt::{Align, FmtItem, Spec, ValueKind};
pub use image::{CRASHES, Func, HostImport, Image, Line, NO_SITE, Site};
pub use image_file::{ImageError, decode, encode};
pub use op::{Check, Form, Op};
pub use verify::{VerifyError, verify};
pub use word::IntSpec;

/// How many slots just below a frame hold what a call needs to return: the
/// return address, the caller's frame, and where the results go. They're
/// the caller's, just before the call's window, which must leave room.
pub const LINK_SLOTS: usize = 3;
