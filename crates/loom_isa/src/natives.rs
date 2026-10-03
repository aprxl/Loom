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
    /// For a formatting native: how many registers come before the value
    /// it formats (a sink's handle).
    pub lead: u16,
}

const fn sig(name: &'static str, args: u16, results: u16, imm: NativeImm) -> NativeSig {
    NativeSig { name, args, results, imm, lead: 0 }
}

/// A formatting native that writes to the sink whose handle comes first.
const fn sink_fmt(name: &'static str, args: u16) -> NativeSig {
    NativeSig { name, args, results: 0, imm: NativeImm::Fmt, lead: 1 }
}

/// Every native. `out.*` write to the program's output; `msg.*` build the
/// message of an unravel, which `msg.unravel` raises; `mem.*` allocate.
///
/// `sink.*` write text to a *sink*, named by a handle: 0 is the program's
/// output, 1 the message of an unravel being built, and `sink.push` makes a
/// buffer, which `sink.string` makes a `String` of and `sink.pad` pads
/// into another sink (both take the buffer made last). `str.new` makes a
/// `String` of text.
pub const NATIVES: &[NativeSig] = &[
    sig("sink.str", 3, 0, NativeImm::None),
    sink_fmt("sink.fmt", 2),
    sink_fmt("sink.fmt2", 3),
    sig("sink.push", 0, 1, NativeImm::None),
    sig("sink.string", 2, 0, NativeImm::None),
    NativeSig { name: "sink.pad", args: 2, results: 0, imm: NativeImm::Fmt, lead: 2 },
    sig("str.new", 3, 0, NativeImm::None),
    // A `String` of two, three or four texts, one after another: each an
    // address and a length, then where the string goes.
    sig("str.concat2", 5, 0, NativeImm::None),
    sig("str.concat3", 7, 0, NativeImm::None),
    sig("str.concat4", 9, 0, NativeImm::None),
    // Text compared byte by byte: (a address, a length, b address, b
    // length) to whether they're equal, or to -1, 0 or 1.
    sig("str.eq", 4, 1, NativeImm::None),
    sig("str.cmp", 4, 1, NativeImm::None),
    // Checked operations: (value, whether it fits) from two operands and a
    // packed integer type (or a cast's two).
    sig("int.add_chk", 3, 2, NativeImm::None),
    sig("int.sub_chk", 3, 2, NativeImm::None),
    sig("int.mul_chk", 3, 2, NativeImm::None),
    sig("int.cast_chk", 2, 2, NativeImm::None),
    sig("float.to_int_chk", 2, 2, NativeImm::None),
    sig("int.to_char_chk", 2, 2, NativeImm::None),
    // Ends the run as `main` returning the error in sink 1 does.
    sig("exit.fail", 0, 0, NativeImm::None),
    sig("mem.alloc", 2, 1, NativeImm::None),
    sig("mem.free", 3, 0, NativeImm::None),
    sig("mem.grow", 4, 0, NativeImm::None),
    // Std's methods of text, integers and floats (loom_vm's `methods`);
    // the immediate picks a variant where there are several.
    sig("str.char_count", 2, 1, NativeImm::None),
    sig("str.find", 4, 2, NativeImm::None),
    sig("str.starts_with", 4, 1, NativeImm::None),
    sig("str.ends_with", 4, 1, NativeImm::None),
    sig("str.trim", 2, 2, NativeImm::None),
    sig("str.case", 3, 0, NativeImm::None),
    sig("str.replace", 7, 0, NativeImm::None),
    sig("str.repeat", 4, 0, NativeImm::None),
    sig("str.split", 5, 0, NativeImm::None),
    sig("str.parse_int", 3, 2, NativeImm::None),
    sig("str.parse_float", 3, 2, NativeImm::None),
    sig("string.of", 3, 0, NativeImm::None),
    sig("string.push_char", 2, 0, NativeImm::None),
    sig("string.push_str", 3, 0, NativeImm::None),
    sig("string.pop", 1, 2, NativeImm::None),
    sig("char.digit", 1, 1, NativeImm::None),
    // Streams of text: from byte `at`, the next item and where the rest
    // starts (and for `next_word`, whether there's one).
    sig("str.next_char", 3, 2, NativeImm::None),
    sig("str.next_line", 3, 3, NativeImm::None),
    sig("str.next_word", 3, 4, NativeImm::None),
    sig("int.bits", 2, 1, NativeImm::None),
    sig("int.rotate", 3, 1, NativeImm::None),
    sig("float.math", 2, 1, NativeImm::None),
    // The host (loom_vm's `host`): the environment, files, time and the
    // terminal. The file ones give 3 when they work, or the case of
    // `IoError` they failed with.
    sig("env.get", 3, 0, NativeImm::None),
    sig("env.args", 1, 0, NativeImm::None),
    sig("time.sleep", 1, 0, NativeImm::None),
    // `process.exit(code)`: never returns (it unwinds).
    sig("process.exit", 1, 0, NativeImm::None),
    sig("term.raw_mode", 1, 0, NativeImm::None),
    sig("term.poll_keys", 2, 1, NativeImm::None),
    sig("fs.append", 3, 1, NativeImm::None),
    sig("fs.write_line", 5, 1, NativeImm::None),
    sig("fs.read", 3, 1, NativeImm::None),
    // Maps' fast paths: the entry with a key (text, or one word) in the
    // map at a header, and the key's hash, from a packed layout (the key's
    // offset and size, and the entry's size).
    sig("map.find_str", 4, 2, NativeImm::None),
    sig("map.find_int", 3, 2, NativeImm::None),
    // `mem.calloc(size, align) -> address`: zeroed.
    sig("mem.calloc", 2, 1, NativeImm::None),
    // `str.hash(text) -> u64`: Treadle's hash of the text (D8).
    sig("str.hash", 2, 1, NativeImm::None),
    // `mem.move(dst, src, bytes)`: copies bytes that may overlap.
    sig("mem.move", 3, 0, NativeImm::None),
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
