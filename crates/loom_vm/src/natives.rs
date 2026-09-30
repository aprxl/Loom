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
    /// The message of an unravel being built (`msg.*`).
    msg: Vec<u8>,
    pub heap: Heap,
    /// How many instructions have been dispatched (with `stats`).
    pub dispatched: u64,
}

/// How much output is kept before it's written.
const OUT_BUFFER: usize = 32 * 1024;

impl<'i> Ctx<'i> {
    pub fn new(image: &'i Image, sink: Output) -> Ctx<'i> {
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
            heap: Heap::default(),
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
    /// `ptr` and `len` must be a text value the compiler built: the address
    /// and length of text that lives at least as long as `self`.
    pub unsafe fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        if cfg!(feature = "checked") {
            let range = self.rodata.as_ptr_range();
            let (start, end) = (range.start as u64, range.end as u64);
            if ptr < start || ptr.checked_add(len).is_none_or(|e| e > end) {
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
        "mem.alloc" => mem_alloc,
        "mem.free" => mem_free,
        "mem.grow" => mem_grow,
        _ => return None,
    };
    let sig = native(name)?;
    Some((f, usize::from(sig.args.max(sig.results))))
}

fn out_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `out.str` is passed a text value.
    let text = unsafe { ctx.text(args[0], args[1])? }.to_vec();
    ctx.write(&text);
    Ok(())
}

fn out_fmt(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    let mut buf = Vec::new();
    ctx.format(&mut buf, item, args)?;
    ctx.write(&buf);
    Ok(())
}

fn msg_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `msg.str` is passed a text value.
    let text = unsafe { ctx.text(args[0], args[1])? }.to_vec();
    ctx.msg.extend_from_slice(&text);
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
