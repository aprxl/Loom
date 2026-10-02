//! Natives: the Rust functions bytecode calls with `calln`, and the context
//! they work in: the program's output, the message an unravel is being
//! built in, and the image's text.

#![allow(unsafe_code)]

use std::io::{IsTerminal as _, Write as _};

use loom_isa::natives::native;
use loom_isa::{FmtItem, Image};
use loom_sys::Output;

use crate::Trap;
use crate::fmt::{self, Value};
use crate::heap::Heap;

/// What natives read and write.
#[derive(Debug)]
pub(crate) struct Ctx<'i> {
    fmts: &'i [FmtItem],
    /// Every text literal of the image, one after another.
    rodata: Box<[u8]>,
    /// Where each literal starts in `rodata`, and its length.
    strings: Vec<(usize, usize)>,
    out: Vec<u8>,
    sink: Output,
    /// Flush at each newline: the output is a terminal.
    line_buffered: bool,
    /// The message of an unravel being built (`msg.*`, sink 1).
    msg: Vec<u8>,
    /// Sinks 2 and up: text being built (`sink.push`).
    bufs: Vec<Vec<u8>>,
    /// Buffers of sinks gone, emptied, for the next ones.
    spare: Vec<Vec<u8>>,
    pub heap: Heap,
    /// A run that doesn't see the machine (see `host`).
    pub sandbox: bool,
    /// `env.args()`.
    pub args: Vec<String>,
    pub term: loom_sys::term::Term,
    /// A sandboxed run's files, by path.
    pub files: std::collections::HashMap<String, Vec<u8>>,
    /// With `checked`: the value stack, set before each native runs, so a
    /// native can check memory on it.
    pub stack: *const crate::stack::Stack,
    /// How many instructions have been dispatched (with `stats`).
    pub dispatched: u64,
}

/// How much output is kept before it's written.
const OUT_BUFFER: usize = 32 * 1024;

impl<'i> Ctx<'i> {
    pub fn new(image: &'i Image, host: loom_sys::Host) -> Ctx<'i> {
        let sink = host.output;
        let mut rodata = Vec::new();
        let mut strings = Vec::with_capacity(image.strings.len());
        for s in &image.strings {
            strings.push((rodata.len(), s.len()));
            rodata.extend_from_slice(s);
        }
        let line_buffered = matches!(sink, Output::Stdout) && std::io::stdout().is_terminal();
        Ctx {
            fmts: &image.fmts,
            rodata: rodata.into_boxed_slice(),
            strings,
            out: Vec::new(),
            sink,
            line_buffered,
            msg: Vec::new(),
            bufs: Vec::new(),
            spare: Vec::new(),
            heap: Heap::default(),
            sandbox: host.sandbox,
            args: host.args,
            term: loom_sys::term::Term::default(),
            files: std::collections::HashMap::new(),
            stack: std::ptr::null(),
            dispatched: 0,
        }
    }

    /// Where the image's text is.
    pub fn rodata_range(&self) -> (u64, u64) {
        let r = self.rodata.as_ptr_range();
        (r.start as u64, r.end as u64)
    }

    /// The address of text literal `index`, which the verifier has checked.
    pub fn string_ptr(&self, index: usize) -> u64 {
        let (start, _) = self.strings[index];
        self.rodata[start..].as_ptr() as u64
    }

    /// The bytes of a text value: a pointer and a length from registers.
    ///
    /// # Safety
    ///
    /// With `checked`: whether `len` bytes at `addr` are a live object or
    /// stack in use.
    fn owns(&self, addr: u64, len: u64) -> bool {
        // SAFETY: `stack` is set to the live stack before a native runs
        // (in the checked build, the only one that calls this).
        self.heap.holds(addr, len) || (!self.stack.is_null() && unsafe { (*self.stack).holds(addr, len) })
    }

    /// `ptr` and `len` must be a text value the compiler built: the address
    /// and length of text that lives at least as long as `self`.
    pub unsafe fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        if cfg!(feature = "checked") && len > 0 {
            let range = self.rodata.as_ptr_range();
            let (start, end) = (range.start as u64, range.end as u64);
            let literal = ptr >= start && ptr.checked_add(len).is_some_and(|e| e <= end);
            if !literal && !self.heap.holds(ptr, len) {
                return Err(Trap::Crash(format!("text at {ptr:#x} ({len} bytes) isn't a live string")));
            }
        }
        // SAFETY: the caller promises this is live text.
        Ok(unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) })
    }

    fn write(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
        if (self.line_buffered && bytes.contains(&b'\n')) || self.out.len() >= OUT_BUFFER {
            self.flush();
        }
    }

    /// Hands buffered output to the host.
    pub fn flush(&mut self) {
        if self.out.is_empty() {
            return;
        }
        match &self.sink {
            Output::Stdout => {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&self.out);
                let _ = stdout.flush();
            }
            Output::Capture(buffer) => buffer.borrow_mut().extend_from_slice(&self.out),
        }
        self.out.clear();
    }

    /// Appends `bytes` to sink `h`.
    fn put(&mut self, h: u64, bytes: &[u8]) -> Result<(), Trap> {
        match h {
            0 => self.write(bytes),
            1 => self.msg.extend_from_slice(bytes),
            _ => match self.bufs.get_mut(h as usize - 2) {
                Some(buf) => buf.extend_from_slice(bytes),
                None => return Err(Trap::Crash(format!("loom internal: no sink {h}"))),
            },
        }
        Ok(())
    }

    /// Takes the buffer made last, which must be sink `h`.
    fn pop(&mut self, h: u64) -> Result<Vec<u8>, Trap> {
        if h < 2 || h as usize - 2 != self.bufs.len().wrapping_sub(1) {
            return Err(Trap::Crash(format!("loom internal: sink {h} isn't the last one made")));
        }
        Ok(self.bufs.pop().unwrap_or_default())
    }

    /// Keeps a sink's buffer for the next sink.
    fn recycle(&mut self, mut buf: Vec<u8>) {
        if self.spare.len() < 8 && buf.capacity() <= 64 * 1024 {
            buf.clear();
            self.spare.push(buf);
        }
    }

    /// Writes a `String` holding `text` at `dst`.
    pub(crate) fn new_string(&mut self, text: &[u8], dst: u64) -> Result<(), Trap> {
        let len = text.len() as u64;
        let ptr = self.heap.alloc(len, 1)?;
        // SAFETY: `ptr` is a new object of `len` bytes (or, for none, never
        // read), and `dst` is where the compiler wants the string: three
        // words of memory it owns.
        unsafe {
            if len > 0 {
                std::ptr::copy_nonoverlapping(text.as_ptr(), ptr as *mut u8, text.len());
            }
            let d = dst as *mut u64;
            d.write(ptr);
            d.add(1).write(len);
            d.add(2).write(len);
        }
        Ok(())
    }

    fn format(&self, into: &mut Vec<u8>, item: u16, args: &[u64]) -> Result<(), Trap> {
        let item = &self.fmts[usize::from(item)];
        let value = match args {
            // SAFETY: a two-register value to format is text (the verifier
            // matched the native to the item's kind).
            [ptr, len] => Value::Text(unsafe { self.text(*ptr, *len)? }),
            [r] => Value::Reg(*r),
            _ => Value::Reg(0),
        };
        fmt::write(into, item, value);
        Ok(())
    }
}

/// A native: its context, its argument registers, and its immediate.
pub(crate) type NativeFn = fn(&mut Ctx, &mut [u64], u16) -> Result<(), Trap>;

/// The native named `name`, and how many registers it reads or writes.
pub(crate) fn resolve(name: &str) -> Option<(NativeFn, usize)> {
    let f: NativeFn = match name {
        "out.str" => out_str,
        "out.fmt" | "out.fmt2" => out_fmt,
        "msg.str" => msg_str,
        "msg.fmt" | "msg.fmt2" => msg_fmt,
        "msg.unravel" => msg_unravel,
        "sink.str" => sink_str,
        "sink.fmt" | "sink.fmt2" => sink_fmt,
        "sink.push" => sink_push,
        "sink.string" => sink_string,
        "sink.pad" => sink_pad,
        "str.new" => str_new,
        "str.eq" => str_eq,
        "str.cmp" => str_cmp,
        "int.add_chk" => |_, a, _| checked(a, crate::ints::add_t),
        "int.sub_chk" => |_, a, _| checked(a, crate::ints::sub_t),
        "int.mul_chk" => |_, a, _| checked(a, crate::ints::mul_t),
        "int.cast_chk" => int_cast_chk,
        "float.to_int_chk" => float_to_int_chk,
        "int.to_char_chk" => int_to_char_chk,
        "exit.fail" => exit_fail,
        "mem.alloc" => mem_alloc,
        "mem.free" => mem_free,
        "mem.grow" => mem_grow,
        "mem.move" => mem_move,
        "mem.calloc" => mem_calloc,
        _ => crate::methods::resolve(name).or_else(|| crate::host::resolve(name))?,
    };
    let sig = native(name)?;
    Some((f, usize::from(sig.args.max(sig.results))))
}

fn out_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `out.str` is passed a text value, which writing doesn't
    // touch: it's a literal or a heap object, not the output buffer.
    let text: *const [u8] = unsafe { ctx.text(args[0], args[1])? };
    ctx.write(unsafe { &*text });
    Ok(())
}

fn out_fmt(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    let mut buf = Vec::new();
    ctx.format(&mut buf, item, args)?;
    ctx.write(&buf);
    Ok(())
}

fn msg_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: as for `out.str`.
    let text: *const [u8] = unsafe { ctx.text(args[0], args[1])? };
    ctx.msg.extend_from_slice(unsafe { &*text });
    Ok(())
}

fn msg_fmt(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    let mut buf = std::mem::take(&mut ctx.msg);
    let done = ctx.format(&mut buf, item, args);
    ctx.msg = buf;
    done
}

fn msg_unravel(ctx: &mut Ctx, _: &mut [u64], _: u16) -> Result<(), Trap> {
    let msg = std::mem::take(&mut ctx.msg);
    Err(Trap::Unravel(String::from_utf8_lossy(&msg).into_owned()))
}

/// `mem.alloc(size, align) -> address`.
fn mem_alloc(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    args[0] = ctx.heap.alloc(args[0], args[1])?;
    Ok(())
}

/// `mem.free(address, size, align)`.
fn mem_free(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.heap.free(args[0], args[1], args[2])
}

/// `mem.calloc(size, align) -> address`: zeroed memory.
fn mem_calloc(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (size, align) = (args[0], args[1]);
    let ptr = ctx.heap.alloc(size, align)?;
    if size > 0 {
        // SAFETY: `ptr` is a new object of `size` bytes.
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, size as usize) };
    }
    args[0] = ptr;
    Ok(())
}

/// `mem.move(dst, src, bytes)`: copies `bytes` bytes, which may overlap.
fn mem_move(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (dst, src, n) = (args[0], args[1], args[2]);
    if n == 0 {
        return Ok(());
    }
    if cfg!(feature = "checked") && !(ctx.owns(dst, n) && ctx.owns(src, n)) {
        return Err(Trap::Crash(format!("loom internal: moving {n} bytes from {src:#x} to {dst:#x}")));
    }
    // SAFETY: the compiler passes two runs of `n` bytes the program owns,
    // in the buffers of lists (the checked build checks they're live).
    unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, n as usize) };
    Ok(())
}

/// `mem.grow(header, element size, align, at least)`: makes room in the
/// buffer of the list (or string) whose header (address, length, capacity)
/// is at `header` for at least `at least` elements, at least doubling it.
fn mem_grow(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (header, size, align, want) = (args[0], args[1], args[2], args[3]);
    // SAFETY: the compiler passes the address of a list's header, three
    // words it owns (the checked build checks the buffer below).
    let (ptr, cap) = unsafe {
        let h = header as *const u64;
        (h.read(), h.add(2).read())
    };
    if want <= cap {
        return Ok(());
    }
    let new_cap = want.max(cap.saturating_mul(2)).max(4);
    let (old_bytes, new_bytes) = match (cap.checked_mul(size), new_cap.checked_mul(size)) {
        (Some(o), Some(n)) if n <= crate::heap::LIMIT => (o, n),
        _ => return Err(Trap::OutOfMemory),
    };
    let moved = ctx.heap.realloc(ptr, old_bytes, new_bytes, align)?;
    // SAFETY: as above.
    unsafe {
        let h = header as *mut u64;
        h.write(moved);
        h.add(2).write(if size == 0 { u64::MAX } else { new_cap });
    }
    Ok(())
}

/// `sink.str(sink, text address, length)`.
fn sink_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: as for `out.str`: a sink's buffer is never a text value's.
    let text: *const [u8] = unsafe { ctx.text(args[1], args[2])? };
    ctx.put(args[0], unsafe { &*text })
}

/// `sink.fmt(sink, value)` and `sink.fmt2(sink, text address, length)`.
fn sink_fmt(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    let mut buf = Vec::new();
    ctx.format(&mut buf, item, &args[1..])?;
    ctx.put(args[0], &buf)
}

/// `sink.push() -> sink`: a new buffer.
fn sink_push(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let buf = ctx.spare.pop().unwrap_or_default();
    ctx.bufs.push(buf);
    args[0] = ctx.bufs.len() as u64 + 1;
    Ok(())
}

/// `sink.string(sink, destination)`: the buffer's text as a `String`.
fn sink_string(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text = ctx.pop(args[0])?;
    ctx.new_string(&text, args[1])?;
    ctx.recycle(text);
    Ok(())
}

/// `sink.pad(sink, into)`: the buffer's text, padded as the format item's
/// spec says, into another sink.
fn sink_pad(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    let text = ctx.pop(args[0])?;
    let spec = ctx.fmts[usize::from(item)].spec;
    let mut out = Vec::new();
    match spec {
        Some(spec) => fmt::pad(&mut out, &text, &spec),
        None => out.extend_from_slice(&text),
    }
    ctx.recycle(text);
    ctx.put(args[1], &out)
}

/// `str.new(text address, length, destination)`: a `String` of the text.
fn str_new(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `str.new` is passed a text value, which allocating doesn't
    // move or free.
    let text: *const [u8] = unsafe { ctx.text(args[0], args[1])? };
    ctx.new_string(unsafe { &*text }, args[2])
}

/// `str.eq(a, a length, b, b length)`: whether the texts are the same.
fn str_eq(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `str.eq` is passed two text values.
    let same = unsafe { ctx.text(args[0], args[1])? == ctx.text(args[2], args[3])? };
    args[0] = u64::from(same);
    Ok(())
}

/// `str.cmp(a, a length, b, b length)`: -1, 0 or 1 as `a` comes before,
/// with or after `b`, byte by byte.
fn str_cmp(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `str.cmp` is passed two text values.
    let order = unsafe { ctx.text(args[0], args[1])?.cmp(ctx.text(args[2], args[3])?) };
    args[0] = order as i64 as u64;
    Ok(())
}

/// Puts `r`'s value and whether there is one in the first two registers.
fn result_pair(args: &mut [u64], r: Result<u64, crate::ints::IntErr>) -> Result<(), Trap> {
    let (v, ok) = match r {
        Ok(v) => (v, 1),
        Err(_) => (0, 0),
    };
    args[0] = v;
    args[1] = ok;
    Ok(())
}

/// `int.add_chk(a, b, type)` and friends: `a op b`, if it fits.
fn checked(
    args: &mut [u64],
    op: fn(u64, u64, loom_isa::IntSpec) -> Result<u64, crate::ints::IntErr>,
) -> Result<(), Trap> {
    let r = op(args[0], args[1], crate::ints::spec(args[2]));
    result_pair(args, r)
}

/// `int.cast_chk(v, cast)`: `v` as the cast's target type, if it fits.
fn int_cast_chk(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (to, from) =
        loom_isa::word::unpack_cast(args[1]).ok_or_else(|| Trap::Crash("loom internal: a bad cast".into()))?;
    let r = crate::ints::cast_t(args[0], to, from);
    result_pair(args, r)
}

/// `float.to_int_chk(f64 bits, type)`: the float truncated, if it fits.
fn float_to_int_chk(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let r = crate::ints::ftoi_t(f64::from_bits(args[0]), crate::ints::spec(args[1]));
    result_pair(args, r)
}

/// `int.to_char_chk(v, type)`: the `char` `v` is the code point of, if it's one.
fn int_to_char_chk(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let r = crate::ints::to_char(args[0], crate::ints::spec(args[1]));
    result_pair(args, r)
}

/// `exit.fail()`: `main` returned an error, whose text is in sink 1.
fn exit_fail(ctx: &mut Ctx, _: &mut [u64], _: u16) -> Result<(), Trap> {
    let msg = std::mem::take(&mut ctx.msg);
    Err(Trap::Failed(String::from_utf8_lossy(&msg).into_owned()))
}
