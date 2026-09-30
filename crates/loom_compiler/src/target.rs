//! What a program is compiled for: `target` in Loom code, and the facts type
//! checking depends on (how wide `usize` is).
//!
//! There are two targets. The Lua backend has 63-bit pointers (`usize` is
//! `u63`, see design/lua-backend.md); Treadle, Loom's own VM, has 64-bit ones
//! (design/vm.md, D3). Both run on the machine doing the build, so the
//! operating system, architecture and byte order are that machine's.

use crate::ty::IntTy;

/// Which backend a program is compiled for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Lua 5.4, through `lower`.
    Lua,
    /// Treadle, through `heddle` and `codegen`.
    Vm,
}

/// The machine a program is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub backend: Backend,
    /// `target.pointer_bits`, and the width of `usize` and `isize`.
    pub pointer_bits: u8,
}

impl Target {
    pub const LUA: Target = Target { backend: Backend::Lua, pointer_bits: 63 };
    pub const VM: Target = Target { backend: Backend::Vm, pointer_bits: 64 };

    /// `target.os`: a case of `std.target.Os`.
    pub const OS: &'static str = if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "MacOS"
    } else {
        "Other"
    };

    /// `target.arch`: a case of `std.target.Arch`.
    pub const ARCH: &'static str = if cfg!(target_arch = "x86_64") {
        "Amd64"
    } else if cfg!(target_arch = "aarch64") {
        "Arm64"
    } else {
        "Other"
    };

    /// `target.endian`: a case of `std.target.Endian`.
    pub const ENDIAN: &'static str = if cfg!(target_endian = "little") { "Little" } else { "Big" };

    pub fn usize(self) -> IntTy {
        IntTy { signed: false, bits: self.pointer_bits, size: true }
    }

    pub fn isize(self) -> IntTy {
        IntTy { signed: true, bits: self.pointer_bits, size: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usize_follows_the_pointer_width() {
        assert_eq!(Target::LUA.usize().max(), (1i128 << 63) - 1);
        assert_eq!(Target::VM.usize().max(), u64::MAX as i128);
        assert_eq!(Target::VM.isize().min(), i64::MIN as i128);
        assert!(Target::VM.usize().size && !Target::VM.usize().signed);
    }
}
