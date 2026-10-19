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
    // Lazy constants publish their payload through an acquire/release flag.
    sig("const.load", 1, 1, NativeImm::None),
    sig("const.store", 2, 0, NativeImm::None),
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
    sig("char.uni", 1, 1, NativeImm::None),
    // Streams of text: from byte `at`, the next item and where the rest
    // starts (and for `next_word`, whether there's one).
    sig("str.next_char", 3, 2, NativeImm::None),
    sig("str.next_line", 3, 3, NativeImm::None),
    sig("str.next_word", 3, 4, NativeImm::None),
    sig("int.bits", 2, 1, NativeImm::None),
    sig("int.rotate", 3, 1, NativeImm::None),
    sig("int.math", 3, 1, NativeImm::None),
    sig("float.math", 2, 1, NativeImm::None),
    // The host (loom_vm's `host`): the environment, files, time and the
    // terminal. The file ones give 3 when they work, or the case of
    // `IoError` they failed with.
    sig("env.get", 3, 0, NativeImm::None),
    sig("env.args", 1, 0, NativeImm::None),
    sig("time.sleep", 1, 0, NativeImm::None),
    sig("time.now", 0, 1, NativeImm::None),
    sig("io.read_line", 1, 0, NativeImm::None),
    sig("io.read_all", 1, 0, NativeImm::None),
    sig("io.write_err", 2, 0, NativeImm::None),
    // `process.exit(code)`: never returns (it unwinds).
    sig("process.exit", 1, 0, NativeImm::None),
    sig("term.raw_mode", 1, 0, NativeImm::None),
    sig("term.poll_keys", 2, 1, NativeImm::None),
    sig("fs.append", 3, 1, NativeImm::None),
    sig("fs.write_line", 5, 1, NativeImm::None),
    sig("fs.write", 5, 1, NativeImm::None),
    sig("fs.exists", 2, 1, NativeImm::None),
    sig("fs.remove", 3, 1, NativeImm::None),
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
    // Starts a message: empties it of what an unfinished one left.
    sig("msg.begin", 0, 0, NativeImm::None),
    sig("msg.str", 2, 0, NativeImm::None),
    sig("msg.fmt", 1, 0, NativeImm::Fmt),
    sig("msg.fmt2", 2, 0, NativeImm::Fmt),
    sig("msg.unravel", 0, 0, NativeImm::None),
    // Threads (design/vm.md §11), which the interpreter runs itself since
    // they switch tasks. `task.spawn(group, entry, env, env bytes, drop
    // function, result bytes, env drop function) -> handle` starts `entry(result address, env)`
    // as a task of the loom `group` at once; `task.join(handle, dest)` waits
    // for it and copies its result to `dest`; `task.yield` goes to the back
    // of the queue.
    sig("task.spawn", 7, 1, NativeImm::None),
    sig("task.join", 2, 0, NativeImm::None),
    sig("task.yield", 0, 0, NativeImm::None),
    // `loom.new() -> group`. `loom.leave(group, mode)` waits for the group's
    // threads (cancelling them first unless the end is a normal one), where
    // mode is 0 for falling off the end, 1 for leaving early, 2 for an
    // unwind. `loom.leftover(group) -> (drop function, address)` gives each
    // result nobody took that needs dropping, then (0, 0).
    // `loom.free(group, mode)` ends the group and raises its failure.
    sig("loom.new", 0, 1, NativeImm::None),
    sig("loom.leave", 2, 0, NativeImm::None),
    sig("loom.leftover", 1, 2, NativeImm::None),
    sig("loom.free", 2, 0, NativeImm::None),
    // Unwinds with a cancel if the running thread has been cancelled.
    sig("task.cancelled", 0, 0, NativeImm::None),
    // `task.grain(n) -> size`: how many of a `loom for`'s `n` iterations go
    // to each task: 1 with one worker, else about `n` over eight times the
    // workers.
    sig("task.grain", 1, 1, NativeImm::None),
    // `Mutex[T]`: `mutex.lock(addr)` takes the lock word at `addr` for the
    // running thread, waiting while another has it; `mutex.unlock(addr)`
    // gives it back and wakes the first waiter. (Interpreter-run, like the
    // `task.*` ones: they switch tasks.)
    sig("mutex.lock", 1, 0, NativeImm::None),
    sig("mutex.unlock", 1, 0, NativeImm::None),
    // `Pipe[T]` (std.sync): `pipe.new(capacity, element size, drop function)
    // -> handle` (0 is no drop); `pipe.send(handle, item address)` and
    // `pipe.recv(handle, destination) -> whether it got one` wait, as the
    // task operations do; `pipe.close(handle)`; `pipe.leftover(handle) ->
    // (drop function, address)` for each item nobody received that needs
    // dropping, then (0, 0); `pipe.free(handle)`.
    sig("pipe.new", 3, 1, NativeImm::None),
    sig("pipe.send", 2, 0, NativeImm::None),
    sig("pipe.recv", 2, 1, NativeImm::None),
    sig("pipe.close", 1, 0, NativeImm::None),
    sig("pipe.leftover", 1, 2, NativeImm::None),
    sig("pipe.free", 1, 0, NativeImm::None),
    // `Atomic[T]` (std.sync), on the integer at an address, whose type is
    // the `IntSpec` last (packed, as the checked arithmetic's is). A bool
    // is a `u8`. `atomic.cas(addr, expected, desired, spec) -> (swapped,
    // found)`. `atomic.add` and `atomic.sub` give the value before, and
    // unravel on overflow, changing nothing.
    // `const.begin(flag address) -> 1 | 0 | 2`: a constant that isn't
    // computed yet is claimed by the first to ask (1, which then computes
    // it); 0 if it's done, 2 if another is computing it.
    sig("const.begin", 1, 1, NativeImm::None),
    sig("atomic.load", 2, 1, NativeImm::None),
    sig("atomic.store", 3, 0, NativeImm::None),
    sig("atomic.swap", 3, 1, NativeImm::None),
    sig("atomic.cas", 4, 2, NativeImm::None),
    sig("atomic.add", 3, 1, NativeImm::None),
    sig("atomic.sub", 3, 1, NativeImm::None),
];

/// The signature of the native named `name`.
pub fn native(name: &str) -> Option<&'static NativeSig> {
    NATIVES.iter().find(|n| n.name == name)
}
