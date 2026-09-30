//! The natives Treadle provides: Rust functions bytecode calls with
//! `calln`. An image names the natives it uses, and the VM finds each by
//! name when it loads the image.
//!
//! A native reads its arguments from consecutive registers and writes its
//! results over them. It never calls Loom code (design/vm.md §9.3).

/// What a native's `c` immediate means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeImm {
    None,
    /// An index into the image's format items.
    Fmt,
}

/// A native's signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSig {
    pub name: &'static str,
    /// How many registers it reads, from the call's base.
    pub args: u16,
    /// How many it writes, from the call's base.
    pub results: u16,
    pub imm: NativeImm,
}

const fn sig(name: &'static str, args: u16, results: u16, imm: NativeImm) -> NativeSig {
    NativeSig { name, args, results, imm }
}

/// Every native. `out.*` write to the program's output; `msg.*` build the
/// message of an unravel, which `msg.unravel` raises; `mem.*` allocate.
pub const NATIVES: &[NativeSig] = &[
    sig("mem.alloc", 2, 1, NativeImm::None),
    sig("mem.free", 3, 0, NativeImm::None),
    sig("mem.grow", 4, 0, NativeImm::None),
    sig("out.str", 2, 0, NativeImm::None),
    sig("out.fmt", 1, 0, NativeImm::Fmt),
    sig("out.fmt2", 2, 0, NativeImm::Fmt),
    sig("msg.str", 2, 0, NativeImm::None),
    sig("msg.fmt", 1, 0, NativeImm::Fmt),
    sig("msg.fmt2", 2, 0, NativeImm::Fmt),
    sig("msg.unravel", 0, 0, NativeImm::None),
];

/// The signature of the native named `name`.
pub fn native(name: &str) -> Option<&'static NativeSig> {
    NATIVES.iter().find(|n| n.name == name)
}
