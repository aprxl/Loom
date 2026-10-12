//! Natives: the Rust functions bytecode calls with `calln`, and the context
//! they work in: the program's output, the message an unravel is being
//! built in, and the image's text.

#![allow(unsafe_code)]

use std::io::{IsTerminal as _, Write as _};

use loom_isa::natives::native;
use loom_isa::{FmtItem, Image, Op};
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
    /// The files `File.append` and `write_line` write to, kept open (a few,
    /// by path) and closed when the run ends, however it ends.
    pub open_files: std::collections::HashMap<String, std::fs::File>,
    /// Set from outside (Ctrl-C), to stop the program: checked at ticks, and
    /// by natives that wait.
    pub interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The program was interrupted, and is unwinding: once, so its `defer`s
    /// run to the end.
    pub stopping: bool,
    /// What compile-time code has left to use (see [`crate::Limits`]).
    meter: Option<Meter>,
    /// Natives that touch the machine end the run (see
    /// [`crate::Config::refuse_host`]).
    pub refuse_host: bool,
    /// With `checked`: the value stack, set before each native runs, so a
    /// native can check memory on it.
    pub stack: *const crate::stack::Stack,
    /// With `checked`: the stacks of the threads that aren't running, which
    /// the running one may touch (its parent's frames, through what it
    /// captured).
    pub task_ranges: Vec<(u64, u64)>,
    /// With `checked`: where the program's data is, which a native may read.
    pub data_range: (u64, u64),
    /// How many instructions have been dispatched (with `stats`).
    pub dispatched: u64,
    /// How many of each op, by opcode (with `stats`; empty otherwise).
    pub op_counts: Vec<u64>,
    /// How many times each instruction ran, by word of the image's code
    /// (with `stats`; empty otherwise).
    pub pc_counts: Vec<u64>,
}

/// What compile-time code has left of its budgets.
#[derive(Debug)]
struct Meter {
    /// Ticks left, counted in chunks of [`crate::TICKS`].
    steps: Option<u64>,
    /// Ticks used, in the same chunks.
    spent: u64,
    deadline: Option<std::time::Instant>,
    /// The budget that ran out, once one has.
    over: Option<loom_sys::Budget>,
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
            open_files: std::collections::HashMap::new(),
            interrupt: None,
            stopping: false,
            meter: None,
            refuse_host: false,
            stack: std::ptr::null(),
            task_ranges: Vec::new(),
            data_range: (0, 0),
            dispatched: 0,
            op_counts: if cfg!(feature = "stats") { vec![0; Op::ALL.len()] } else { Vec::new() },
            pc_counts: if cfg!(feature = "stats") { vec![0; image.code.len()] } else { Vec::new() },
        }
    }

    /// Sets what the program may use up, and whether it may touch the
    /// machine. The time starts now.
    pub fn set_limits(&mut self, limits: &crate::Limits, refuse_host: bool) {
        self.refuse_host = refuse_host;
        if let Some(bytes) = limits.memory {
            self.heap.set_limit(bytes);
        }
        if limits.steps.is_some() || limits.time.is_some() {
            self.meter = Some(Meter {
                steps: limits.steps,
                spent: 0,
                deadline: limits.time.and_then(|t| std::time::Instant::now().checked_add(t)),
                over: None,
            });
        }
    }

    /// How many ticks have been checked off, in chunks of [`crate::TICKS`]
    /// (with limits set: otherwise 0).
    pub fn steps_spent(&self) -> u64 {
        self.meter.as_ref().map_or(0, |m| m.spent)
    }

    /// The budget the program has used up, if it has used one up.
    pub fn over_budget(&self) -> Option<loom_sys::Budget> {
        self.meter.as_ref().and_then(|m| m.over)
    }

    /// The check every [`crate::TICKS`] loop iterations and calls: whether
    /// the program should stop now, and with what. Out of the dispatch
    /// loop's way.
    #[cold]
    #[inline(never)]
    pub fn checkpoint(&mut self) -> Option<Trap> {
        if self.interrupted() {
            return Some(Trap::Interrupt);
        }
        let m = self.meter.as_mut()?;
        // Out of a budget before: it's unwinding, and still running a
        // whole chunk of ticks later.
        if let Some(budget) = m.over {
            return Some(Trap::Budget { budget, abort: true });
        }
        let mut over = None;
        m.spent += crate::TICKS;
        if let Some(steps) = &mut m.steps {
            match steps.checked_sub(crate::TICKS) {
                Some(left) => *steps = left,
                None => {
                    *steps = 0;
                    over = Some(loom_sys::Budget::Steps);
                }
            }
        }
        if over.is_none() && m.deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            over = Some(loom_sys::Budget::Time);
        }
        m.over = over;
        over.map(|budget| Trap::Budget { budget, abort: false })
    }

    /// For a native that touches the machine: the error that refuses it, if
    /// this run may not use it.
    #[inline]
    pub fn host_check(&self, native: &'static str) -> Result<(), Trap> {
        if self.refuse_host { Err(Trap::Refused(native)) } else { Ok(()) }
    }

    /// Whether the program should stop now: it was interrupted and isn't
    /// stopping yet (which this makes it).
    pub fn interrupted(&mut self) -> bool {
        let asked =
            !self.stopping && self.interrupt.as_ref().is_some_and(|i| i.load(std::sync::atomic::Ordering::Relaxed));
        self.stopping |= asked;
        asked
    }

    /// Forgets what a call that ended early (an unravel) left unfinished:
    /// the message and the buffers being built, for the next call to start
    /// clean.
    pub fn reset_sinks(&mut self) {
        self.msg.clear();
        while let Some(buf) = self.bufs.pop() {
            self.recycle(buf);
        }
    }

    /// The message an unravel (or a `const assert`) built, as of the last
    /// call.
    pub fn message(&self) -> &[u8] {
        &self.msg
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

    /// With `checked`: whether `len` bytes at `addr` are a live object or
    /// stack in use.
    fn owns(&self, addr: u64, len: u64) -> bool {
        // SAFETY: `stack` is set to the live stack before a native runs
        // (in the checked build, the only one that calls this).
        self.heap.holds(addr, len)
            || (!self.stack.is_null() && unsafe { (*self.stack).holds(addr, len) })
            || self
                .task_ranges
                .iter()
                .any(|&(start, end)| addr >= start && addr.checked_add(len).is_some_and(|stop| stop <= end))
    }

    /// With `checked`: whether `len` bytes at `addr` can be read: what
    /// [`owns`](Self::owns) says, text literals, or the program's data.
    fn readable(&self, addr: u64, len: u64) -> bool {
        let range = self.rodata.as_ptr_range();
        let inside = |(lo, hi): (u64, u64)| addr >= lo && addr.checked_add(len).is_some_and(|e| e <= hi);
        inside((range.start as u64, range.end as u64)) || inside(self.data_range) || self.owns(addr, len)
    }

    /// The bytes of a text value: a pointer and a length from registers.
    ///
    /// # Safety
    ///
    /// `ptr` and `len` must be a text value the compiler built: the address
    /// and length of text that lives at least as long as `self`.
    pub unsafe fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        // An empty text may have any address (a header's, zeroed, even):
        // never make a slice of it.
        if len == 0 {
            return Ok(&[]);
        }
        if cfg!(feature = "checked") {
            let range = self.rodata.as_ptr_range();
            let (start, end) = (range.start as u64, range.end as u64);
            let literal = ptr >= start && ptr.checked_add(len).is_some_and(|e| e <= end);
            // Text baked into the program's data, as a constant's is.
            let (lo, hi) = self.data_range;
            let baked = ptr >= lo && ptr.checked_add(len).is_some_and(|e| e <= hi);
            if !literal && !baked && !self.heap.holds(ptr, len) {
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
            0 => {
                self.host_check("out")?;
                self.write(bytes)
            }
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

    /// Writes a `String` of `len` bytes at `dst`, which `fill` writes in
    /// place: no buffer of its own to copy from.
    pub(crate) fn new_string_with(&mut self, len: usize, dst: u64, fill: impl FnOnce(&mut [u8])) -> Result<(), Trap> {
        let ptr = self.heap.alloc(len as u64, 1)?;
        // SAFETY: `ptr` is a new object of `len` bytes (or, for none, never
        // read), which nothing else refers to; `dst` is where the compiler
        // wants the string: three words of memory it owns.
        unsafe {
            if len > 0 {
                fill(std::slice::from_raw_parts_mut(ptr as *mut u8, len));
            }
            let d = dst as *mut u64;
            d.write(ptr);
            d.add(1).write(len as u64);
            d.add(2).write(len as u64);
        }
        Ok(())
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

    /// Formats `args` as format item `item` says, onto the end of sink
    /// `h`: written where it goes, with no buffer of its own.
    fn format_into(&mut self, h: u64, item: u16, args: &[u64]) -> Result<(), Trap> {
        let fmts = self.fmts;
        let item = &fmts[usize::from(item)];
        let value = match args {
            // SAFETY: a two-register value to format is text (the verifier
            // matched the native to the item's kind): a literal or a heap
            // object, never a buffer of the context's.
            [ptr, len] => Value::Text(unsafe { &*(self.text(*ptr, *len)? as *const [u8]) }),
            [r] => Value::Reg(*r),
            _ => Value::Reg(0),
        };
        match h {
            0 => {
                self.host_check("out")?;
                let from = self.out.len();
                fmt::write(&mut self.out, item, value);
                self.written(from);
            }
            1 => fmt::write(&mut self.msg, item, value),
            _ => match self.bufs.get_mut(h as usize - 2) {
                Some(buf) => fmt::write(buf, item, value),
                None => return Err(Trap::Crash(format!("loom internal: no sink {h}"))),
            },
        }
        Ok(())
    }

    /// Output was appended to `out` from `from`: flushes where `write` does.
    fn written(&mut self, from: usize) {
        if (self.line_buffered && self.out[from..].contains(&b'\n')) || self.out.len() >= OUT_BUFFER {
            self.flush();
        }
    }
}

/// A native: its context, its argument registers, and its immediate.
pub(crate) type NativeFn = fn(&mut Ctx, &mut [u64], u16) -> Result<(), Trap>;

/// The native named `name`, and how many registers it reads or writes.
pub(crate) fn resolve(name: &str) -> Option<(NativeFn, usize)> {
    let f: NativeFn = match name {
        "out.str" => out_str,
        "out.fmt" | "out.fmt2" => out_fmt,
        "msg.begin" => msg_begin,
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
        // Handled by the interpreter itself, which they ask: they switch tasks.
        "task.spawn" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::Spawn)),
        "task.join" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::Join)),
        "task.yield" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::Yield)),
        "loom.new" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::LoomNew)),
        "loom.leave" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::LoomLeave)),
        "loom.leftover" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::LoomLeftover)),
        "loom.free" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::LoomFree)),
        "task.cancelled" => |_, _, _| Err(Trap::Task(crate::sched::TaskOp::Cancelled)),
        _ => crate::methods::resolve(name).or_else(|| crate::host::resolve(name))?,
    };
    let sig = native(name)?;
    Some((f, usize::from(sig.args.max(sig.results))))
}

fn out_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: `out.str` is passed a text value, which writing doesn't
    // touch: it's a literal or a heap object, not the output buffer.
    let text: *const [u8] = unsafe { ctx.text(args[0], args[1])? };
    ctx.host_check("out")?;
    ctx.write(unsafe { &*text });
    Ok(())
}

fn out_fmt(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    ctx.format_into(0, item, args)
}

/// `msg.begin()`: a new message. Building one can run the program's code (a
/// hand-written `show`), which may unravel with a message of its own; that
/// one must not find the half-built text of this one.
fn msg_begin(ctx: &mut Ctx, _: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.msg.clear();
    Ok(())
}

fn msg_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    // SAFETY: as for `out.str`.
    let text: *const [u8] = unsafe { ctx.text(args[0], args[1])? };
    ctx.msg.extend_from_slice(unsafe { &*text });
    Ok(())
}

fn msg_fmt(ctx: &mut Ctx, args: &mut [u64], item: u16) -> Result<(), Trap> {
    ctx.format_into(1, item, args)
}

fn msg_unravel(ctx: &mut Ctx, _: &mut [u64], _: u16) -> Result<(), Trap> {
    let msg = std::mem::take(&mut ctx.msg);
    Err(Trap::Unravel(String::from_utf8_lossy(&msg).into_owned()))
}

/// `mem.alloc(size, align) -> address`.
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_alloc")]
fn mem_alloc(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    args[0] = ctx.heap.alloc(args[0], args[1])?;
    Ok(())
}

/// `mem.free(address, size, align)`.
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_free")]
fn mem_free(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.heap.free(args[0], args[1], args[2])
}

/// `mem.calloc(size, align) -> address`: zeroed memory.
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_calloc")]
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
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_move")]
fn mem_move(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (dst, src, n) = (args[0], args[1], args[2]);
    if n == 0 {
        return Ok(());
    }
    if cfg!(feature = "checked") && !(ctx.owns(dst, n) && ctx.readable(src, n)) {
        return Err(Trap::Crash(format!("loom internal: moving {n} bytes from {src:#x} to {dst:#x}")));
    }
    // SAFETY: the compiler passes two runs of `n` bytes the program owns,
    // in the buffers of lists, or a literal's to copy (the checked build
    // checks they're live).
    unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, n as usize) };
    Ok(())
}

/// `mem.grow(header, element size, align, at least)`: makes room in the
/// buffer of the list (or string) whose header (address, length, capacity)
/// is at `header` for at least `at least` elements, at least doubling it.
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_grow")]
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
    ctx.format_into(args[0], item, &args[1..])
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
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_str_eq")]
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
