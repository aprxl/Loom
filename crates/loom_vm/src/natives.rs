//! Natives: the Rust functions bytecode calls with `calln`, and the context
//! they work in: the program's output, the message an unravel is being
//! built in, and the image's text.

#![allow(unsafe_code)]

use std::io::{IsTerminal as _, Write as _};
use std::sync::{Arc, Mutex, MutexGuard};

use loom_isa::natives::native;
use loom_isa::{FmtItem, Image, Op};
use loom_sys::Output;

use crate::Trap;
use crate::embed::HostFns;
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
    /// Where output goes once it is whole lines, with its own lock: the
    /// workers of a pool put theirs there without the pool's.
    shared_out: Arc<Mutex<OutBuf>>,
    /// Flush at each newline: the output is a terminal.
    line_buffered: bool,
    /// There is a pool: output reaches `shared_out` a whole line at a time.
    pub(crate) pooled: bool,
    /// How many tasks wait for a constant (the scheduler's count).
    pub(crate) const_waiting: Arc<std::sync::atomic::AtomicUsize>,
    /// What the natives work on: the output (with one worker, all of it; else
    /// the line being written), the message of an unravel being built
    /// (`msg.*`, sink 1) and the sinks `sink.push` made (2 and up).
    cur: Local,
    /// A worker of a pool is running a native, so `cur` is its own (see
    /// [`Local`]) and its output goes on the way out.
    split: bool,
    /// The partial line was already scanned on the preceding native.
    split_start: usize,
    /// What a task switch swaps for a task's partial output line, with one
    /// worker (where lines are written to `cur.out` as they come).
    task_line: Vec<u8>,
    pub heap: Heap,
    /// The functions bound to the image's host imports.
    pub hosts: HostFns,
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
    /// With `checked`: where the program's data is, which a native may read.
    pub data_range: (u64, u64),
    /// How many instructions have been dispatched (with `stats`).
    pub dispatched: u64,
    pub runtime_stats: crate::RuntimeStats,
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

/// How many bytes of whole lines a worker of a pool queues before it hands
/// them to the shared output (when that isn't a terminal).
const QUEUE_BATCH: usize = 16 * 1024;

/// Output that is whole lines, waiting to be handed to the host.
#[derive(Debug)]
pub(crate) struct OutBuf {
    buf: Vec<u8>,
    sink: Output,
    line_buffered: bool,
}

// SAFETY: `sink` may hold an `Rc` (a captured output), which the host reads
// after the run and nothing else touches during it; the buffer is reached only
// through the `Mutex` it is in.
unsafe impl Send for OutBuf {}

impl OutBuf {
    /// Adds whole lines, and hands the output to the host if it's a
    /// terminal or there is a lot.
    fn add(&mut self, lines: &[u8]) {
        self.buf.extend_from_slice(lines);
        if self.line_buffered || self.buf.len() >= OUT_BUFFER {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        match &self.sink {
            Output::Stdout => {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&self.buf);
                let _ = stdout.flush();
            }
            Output::Capture(buffer) => buffer.borrow_mut().extend_from_slice(&self.buf),
        }
        self.buf.clear();
    }
}

fn lock_out(out: &Mutex<OutBuf>) -> MutexGuard<'_, OutBuf> {
    out.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
            shared_out: Arc::new(Mutex::new(OutBuf { buf: Vec::new(), sink, line_buffered })),
            line_buffered,
            pooled: false,
            const_waiting: Arc::default(),
            cur: Local::default(),
            split: false,
            split_start: 0,
            task_line: Vec::new(),
            heap: Heap::default(),
            hosts: HostFns::default(),
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
            data_range: (0, 0),
            dispatched: 0,
            runtime_stats: crate::RuntimeStats::default(),
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
        self.cur.msg.clear();
        while let Some(buf) = self.cur.bufs.pop() {
            self.cur.recycle(buf);
        }
    }

    /// The message an unravel (or a `const assert`) built, as of the last
    /// call.
    pub fn message(&self) -> &[u8] {
        &self.cur.msg
    }

    /// The output the workers of a pool put their lines in.
    pub(crate) fn out_shared(&self) -> Arc<Mutex<OutBuf>> {
        Arc::clone(&self.shared_out)
    }

    /// Where the image's text is.
    pub fn rodata_range(&self) -> (u64, u64) {
        let r = self.rodata.as_ptr_range();
        (r.start as u64, r.end as u64)
    }

    /// A worker of a pool is about to run a native: what it builds is its
    /// own, and its output goes to its line, so lines of different
    /// workers stay whole.
    pub(crate) fn enter(&mut self, l: &mut Local) {
        std::mem::swap(&mut self.cur, l);
        self.split = true;
        self.split_start = self.cur.out.len();
    }

    /// The native ran: what is whole lines of the worker's output goes on
    /// the shared output.
    pub(crate) fn exit(&mut self, l: &mut Local) {
        std::mem::swap(&mut self.cur, l);
        self.split = false;
        if let Some(i) = l.out[self.split_start..].iter().rposition(|&b| b == b'\n') {
            let i = self.split_start + i;
            // What the worker queued comes first.
            let mut out = lock_out(&self.shared_out);
            out.buf.append(&mut l.queue);
            out.add(&l.out[..=i]);
            drop(out);
            l.out.drain(..=i);
        }
    }

    /// Puts `bytes` (a worker's last, unfinished line) on the output.
    pub(crate) fn append_out(&mut self, bytes: &mut Vec<u8>) {
        if self.pooled {
            lock_out(&self.shared_out).buf.append(bytes);
        } else {
            self.cur.out.append(bytes);
        }
    }

    /// Hands the whole lines a worker of a pool queued to the output.
    pub(crate) fn append_queue(&mut self, queue: &mut Vec<u8>) {
        if !queue.is_empty() {
            lock_out(&self.shared_out).add(queue);
            queue.clear();
        }
    }

    /// Whether the workers of a pool queue their whole lines (and hand them
    /// over in batches): not when the output is a terminal.
    pub(crate) fn batches_output(&self) -> bool {
        !self.line_buffered
    }

    /// Everything a worker has written reaches the host: its queued lines and
    /// the line it is in the middle of (a prompt), before standard input is
    /// read.
    pub(crate) fn flush_for_read(&mut self, l: &mut Local) {
        if self.pooled {
            let mut out = lock_out(&self.shared_out);
            out.buf.append(&mut l.queue);
            out.buf.append(&mut l.out);
            out.flush();
        } else {
            self.flush();
        }
    }

    /// What the running task's natives are building (its unravel message,
    /// and the templates it has under way), which a task switch swaps.
    pub(crate) fn task_state(&mut self) -> (&mut Vec<u8>, &mut Vec<Vec<u8>>, &mut Vec<u8>) {
        (&mut self.cur.msg, &mut self.cur.bufs, &mut self.task_line)
    }

    /// Where the text literals are, and where each starts with its length:
    /// they're never changed, so workers read them without the lock.
    pub fn rodata_ptr(&self) -> *const u8 {
        self.rodata.as_ptr()
    }

    pub fn strings_ptr(&self) -> *const (usize, usize) {
        self.strings.as_ptr()
    }

    /// With `checked`: whether `len` bytes at `addr` are a live object or
    /// stack in use.
    fn owns(&self, addr: u64, len: u64) -> bool {
        // SAFETY: `stack` is set to the live stack before a native runs
        // (in the checked build, the only one that calls this).
        self.heap.holds(addr, len)
            || (!self.stack.is_null() && unsafe { (*self.stack).holds(addr, len) })
            || crate::stack::in_some_stack(addr, len)
    }

    /// With `checked`: whether the map whose header is at `header`, with
    /// entries of `esize` bytes, is all in live memory.
    pub(crate) fn check_map(&self, header: u64, esize: u64) -> Result<(), Trap> {
        if !cfg!(feature = "checked") {
            return Ok(());
        }
        let bad = || Trap::Crash(format!("loom internal: a bad map at {header:#x}"));
        if !self.owns(header, 64) {
            return Err(bad());
        }
        // SAFETY: the header is live, eight words.
        let (ebuf, ibuf, slots, used) = unsafe {
            let p = header as *const u64;
            (p.read_unaligned(), p.add(3).read_unaligned(), p.add(4).read_unaligned(), p.add(5).read_unaligned())
        };
        let entries = used.checked_mul(esize).ok_or_else(bad)?;
        let index = slots.checked_mul(8).ok_or_else(bad)?;
        if (entries > 0 && !self.owns(ebuf, entries)) || (index > 0 && !self.owns(ibuf, index)) {
            return Err(bad());
        }
        Ok(())
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

    /// Hands buffered output to the host.
    pub fn flush(&mut self) {
        if self.split {
            return;
        }
        let mut out = lock_out(&self.shared_out);
        if !self.cur.out.is_empty() {
            out.buf.append(&mut self.cur.out);
        }
        out.flush();
    }

    /// Writes a `String` holding `text` at `dst` (see [`Heap::new_string`]).
    pub(crate) fn new_string(&mut self, text: &[u8], dst: u64) -> Result<(), Trap> {
        self.heap.new_string(text, dst)
    }
}

/// What the natives that write output, build text and read constants work
/// on: the context, or (without the pool's lock, in a build that doesn't
/// check) a [`Worker`].
pub(crate) trait Sinks: Strings {
    /// The output line, message and sinks the native works on.
    fn local(&mut self) -> &mut Local;
    fn fmt_item(&self, item: u16) -> FmtItem;
    fn host_check(&self, native: &'static str) -> Result<(), Trap>;
    /// Output was appended to the line from `from`: written where it goes if
    /// that is time.
    fn written(&mut self, from: usize);
    /// Where the program's data is, which constants' flags are in.
    fn data_range(&self) -> (u64, u64);
    /// Whether any task waits for a constant.
    fn const_waiting(&self) -> bool;
}

impl Sinks for Ctx<'_> {
    fn local(&mut self) -> &mut Local {
        &mut self.cur
    }

    fn fmt_item(&self, item: u16) -> FmtItem {
        self.fmts[usize::from(item)]
    }

    fn host_check(&self, native: &'static str) -> Result<(), Trap> {
        Ctx::host_check(self, native)
    }

    fn written(&mut self, from: usize) {
        if (self.line_buffered && self.cur.out[from..].contains(&b'\n')) || self.cur.out.len() >= OUT_BUFFER {
            self.flush();
        }
    }

    fn data_range(&self) -> (u64, u64) {
        self.data_range
    }

    fn const_waiting(&self) -> bool {
        self.const_waiting.load(std::sync::atomic::Ordering::SeqCst) > 0
    }
}

/// Appends `bytes` to sink `h`.
fn put<S: Sinks>(s: &mut S, h: u64, bytes: &[u8]) -> Result<(), Trap> {
    if h == 0 {
        s.host_check("out")?;
        let from = s.local().out.len();
        s.local().out.extend_from_slice(bytes);
        s.written(from);
        return Ok(());
    }
    s.local().sink_mut(h)?.extend_from_slice(bytes);
    Ok(())
}

/// Formats `args` as format item `item` says, onto the end of sink `h`:
/// written where it goes, with no buffer of its own.
fn format_into<S: Sinks>(s: &mut S, h: u64, item: u16, args: &[u64]) -> Result<(), Trap> {
    let item = s.fmt_item(item);
    let value = match args {
        // SAFETY: a two-register value to format is text (the verifier
        // matched the native to the item's kind): a literal or a heap
        // object, never a buffer of the context's.
        [ptr, len] => Value::Text(unsafe { &*(s.text(*ptr, *len)? as *const [u8]) }),
        [r] => Value::Reg(*r),
        _ => Value::Reg(0),
    };
    if h == 0 {
        s.host_check("out")?;
        let from = s.local().out.len();
        fmt::write(&mut s.local().out, &item, value);
        s.written(from);
    } else {
        fmt::write(s.local().sink_mut(h)?, &item, value);
    }
    Ok(())
}

/// Where a native gets the text a value names: the context, which checks the
/// address in the checked build, or [`Raw`], for the natives that run
/// without the pool's lock.
pub(crate) trait Texts {
    fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap>;
}

impl Texts for Ctx<'_> {
    fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        // SAFETY: callers pass a text value the compiler built, as `Ctx::text`
        // requires.
        unsafe { Ctx::text(self, ptr, len) }
    }
}

/// What a native that builds strings works on: a source of text, and the heap
/// the strings go in.
pub(crate) trait Strings: Texts {
    fn heap(&mut self) -> &mut Heap;
}

impl Strings for Ctx<'_> {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }
}

/// Text read from its address alone, with no context to check it against.
#[cfg(not(feature = "checked"))]
pub(crate) struct Raw;

#[cfg(not(feature = "checked"))]
impl Texts for Raw {
    fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        if len == 0 {
            return Ok(&[]);
        }
        // SAFETY: callers pass a text value the compiler built: `len` bytes at
        // `ptr`, alive until the native returns (the promise `Ctx::text` has
        // in a build that doesn't check it).
        Ok(unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) })
    }
}

/// A worker's own heap, for the natives that build strings without the pool's
/// lock.
#[cfg(not(feature = "checked"))]
pub(crate) struct Own<'h>(pub &'h mut Heap);

#[cfg(not(feature = "checked"))]
impl Texts for Own<'_> {
    fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        Raw.text(ptr, len)
    }
}

#[cfg(not(feature = "checked"))]
impl Strings for Own<'_> {
    fn heap(&mut self) -> &mut Heap {
        self.0
    }
}

/// What a worker of a pool has of its own, which a native it runs works on
/// in the context's place: the message and sinks of the task it runs, its
/// output line (not yet whole), and the buffers of sinks gone.
#[derive(Debug, Default)]
pub(crate) struct Local {
    pub msg: Vec<u8>,
    pub bufs: Vec<Vec<u8>>,
    pub out: Vec<u8>,
    /// Buffers of sinks gone, emptied, for the next ones.
    pub spare: Vec<Vec<u8>>,
    /// Whole lines written, waiting to be handed to the shared output, which
    /// they are in batches (the worker's own, not its task's).
    pub queue: Vec<u8>,
}

impl Local {
    /// The buffer of sink `h`: 0 is the output line, 1 the message, and 2 and
    /// up the ones `sink.push` made.
    fn sink_mut(&mut self, h: u64) -> Result<&mut Vec<u8>, Trap> {
        match h {
            0 => Ok(&mut self.out),
            1 => Ok(&mut self.msg),
            _ => self.bufs.get_mut(h as usize - 2).ok_or_else(|| Trap::Crash(format!("loom internal: no sink {h}"))),
        }
    }

    /// A new sink, which is the last one: its handle.
    fn push(&mut self) -> u64 {
        let buf = self.spare.pop().unwrap_or_default();
        self.bufs.push(buf);
        self.bufs.len() as u64 + 1
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
}

/// What a worker of a pool runs a native on without the pool's lock: its own
/// heap and [`Local`], the shared output (which has a lock of its own), and
/// what is read-only. Output is put on the shared output a whole line at a
/// time, when the native is done.
pub(crate) struct Worker<'w> {
    pub heap: &'w mut Heap,
    pub local: &'w mut Local,
    pub out: &'w Mutex<OutBuf>,
    /// Whole lines are queued and handed over in batches (the output isn't a
    /// terminal), not one by one.
    pub batched: bool,
    pub fmts: &'w [FmtItem],
    pub data_range: (u64, u64),
    pub const_waiting: &'w std::sync::atomic::AtomicUsize,
}

impl Worker<'_> {
    /// The native ran: the whole lines it wrote, from `from` on, are queued,
    /// and go on the shared output when there are enough (or at once, for a
    /// terminal).
    pub(crate) fn publish(&mut self, from: usize) {
        let Some(i) = self.local.out[from..].iter().rposition(|&b| b == b'\n') else { return };
        let end = from + i + 1;
        if !self.batched {
            lock_out(self.out).add(&self.local.out[..end]);
        } else {
            self.local.queue.extend_from_slice(&self.local.out[..end]);
            if self.local.queue.len() >= QUEUE_BATCH {
                lock_out(self.out).add(&self.local.queue);
                self.local.queue.clear();
            }
        }
        self.local.out.drain(..end);
    }
}

impl Texts for Worker<'_> {
    fn text(&self, ptr: u64, len: u64) -> Result<&[u8], Trap> {
        if len == 0 {
            return Ok(&[]);
        }
        // SAFETY: callers pass a text value the compiler built: `len` bytes at
        // `ptr`, alive until the native returns (workers run only in a build
        // that doesn't check it).
        Ok(unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) })
    }
}

impl Strings for Worker<'_> {
    fn heap(&mut self) -> &mut Heap {
        self.heap
    }
}

impl Sinks for Worker<'_> {
    fn local(&mut self) -> &mut Local {
        self.local
    }

    fn fmt_item(&self, item: u16) -> FmtItem {
        self.fmts[usize::from(item)]
    }

    /// A pool never runs a program that may not touch the machine.
    fn host_check(&self, _: &'static str) -> Result<(), Trap> {
        Ok(())
    }

    /// The line is put on the shared output when the native is done.
    fn written(&mut self, _: usize) {}

    fn data_range(&self) -> (u64, u64) {
        self.data_range
    }

    fn const_waiting(&self) -> bool {
        self.const_waiting.load(std::sync::atomic::Ordering::SeqCst) > 0
    }
}

/// A native that a worker runs without the pool's lock, on its own state.
pub(crate) type WorkerFn = fn(&mut Worker, &mut [u64], u16) -> Result<(), Trap>;

/// The natives that write output, build text and read constants, for a
/// worker to run without the pool's lock. (The checked build runs them on
/// the context, with the worker's state swapped in.)
pub(crate) fn resolve_worker(name: &str) -> Option<WorkerFn> {
    if cfg!(feature = "checked") {
        return None;
    }
    Some(match name {
        "out.str" => |w, a, i| out_str(w, a, i),
        "out.fmt" | "out.fmt2" => |w, a, i| out_fmt(w, a, i),
        "msg.begin" => |w, a, i| msg_begin(w, a, i),
        "msg.str" => |w, a, i| msg_str(w, a, i),
        "msg.fmt" | "msg.fmt2" => |w, a, i| msg_fmt(w, a, i),
        "msg.unravel" => |w, a, i| msg_unravel(w, a, i),
        "sink.str" => |w, a, i| sink_str(w, a, i),
        "sink.fmt" | "sink.fmt2" => |w, a, i| sink_fmt(w, a, i),
        "sink.push" => |w, a, i| sink_push(w, a, i),
        "sink.string" => |w, a, i| sink_string(w, a, i),
        "seq.join_text" => |w, a, i| seq_join_text(w, a, i),
        "sink.pad" => |w, a, i| sink_pad(w, a, i),
        "str.new_in" => |w, a, i| str_new_in(w, a, i),
        "const.load" => |w, a, i| const_load(w, a, i),
        "const.store" => |w, a, i| const_store(w, a, i),
        _ => return None,
    })
}

/// A native: its context, its argument registers, and its immediate.
pub(crate) type NativeFn = fn(&mut Ctx, &mut [u64], u16) -> Result<(), Trap>;

// ---- `Atomic[T]`: with one worker, a task runs until it parks, so these
// are plain reads and writes of the integer at the address; they're natives
// so that nothing is reordered or kept in a register around them, and
// several workers can make them real atomic operations in this one place.

/// Copies `n` bytes (an item of a pipe), checked in the checked build.
pub(crate) fn copy_bytes(ctx: &Ctx, dst: u64, src: u64, n: u64) -> Result<(), Trap> {
    if n == 0 {
        return Ok(());
    }
    if cfg!(feature = "checked") && !(ctx.owns(dst, n) && ctx.readable(src, n)) {
        return Err(Trap::Crash(format!("loom internal: copying {n} bytes from {src:#x} to {dst:#x}")));
    }
    // SAFETY: the compiler passes an item's address and a slot of a pipe's
    // buffer, `n` bytes each, which don't overlap (a pipe's buffer is its
    // own allocation); the checked build checked they're live.
    unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, n as usize) };
    Ok(())
}

/// The bit of a mutex's lock word that says a task may be waiting for it. The
/// rest of the word is 0 when the mutex is free, else the holder's task index
/// and one.
pub(crate) const WAITERS: u64 = 1 << 63;

/// The lock word of the mutex at `addr`, checked in the checked build. Every
/// access to it is atomic: a task takes or gives back an uncontended mutex
/// without the pool's lock (`try_lock` and `try_unlock`), and the rest, under
/// the lock, is atomic too.
pub(crate) fn mutex_word<'a>(ctx: &Ctx, addr: u64) -> Result<&'a std::sync::atomic::AtomicU64, Trap> {
    if addr == 0 || !addr.is_multiple_of(8) || (cfg!(feature = "checked") && !ctx.owns(addr, 8)) {
        return Err(Trap::Crash("loom internal: a bad mutex word".into()));
    }
    // SAFETY: the compiler supplies a live, aligned mutex word, which is only
    // ever accessed atomically (here, and in `try_lock` and `try_unlock`), and
    // lives as long as the mutex, which outlives each native invocation.
    Ok(unsafe { std::sync::atomic::AtomicU64::from_ptr(addr as *mut u64) })
}

/// Takes the free mutex whose lock word is at `addr` for task `me` (its index
/// and one), if it is free and nobody waits for it: the same as `mutex.lock`
/// without the pool's lock. `false` leaves everything to `mutex.lock`.
#[cfg(not(feature = "checked"))]
pub(crate) fn try_lock(addr: u64, me: u64) -> bool {
    if addr == 0 || !addr.is_multiple_of(8) {
        return false;
    }
    // SAFETY: as for `mutex_word`.
    let word = unsafe { std::sync::atomic::AtomicU64::from_ptr(addr as *mut u64) };
    word.compare_exchange(0, me, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok()
}

/// Gives back the mutex at `addr` held by task `me`, if nobody waits for it:
/// the same as `mutex.unlock` without the pool's lock. `false` leaves
/// everything to `mutex.unlock`.
#[cfg(not(feature = "checked"))]
pub(crate) fn try_unlock(addr: u64, me: u64) -> bool {
    if addr == 0 || !addr.is_multiple_of(8) {
        return false;
    }
    // SAFETY: as for `mutex_word`.
    let word = unsafe { std::sync::atomic::AtomicU64::from_ptr(addr as *mut u64) };
    word.compare_exchange(me, 0, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok()
}

/// The word at `addr`, which is in the program's data (a constant's flag).
pub(crate) fn data_word<S: Sinks>(s: &S, addr: u64) -> Result<u64, Trap> {
    Ok(data_atomic(s.data_range(), addr)?.load(std::sync::atomic::Ordering::SeqCst))
}

fn data_atomic<'a>((lo, hi): (u64, u64), addr: u64) -> Result<&'a std::sync::atomic::AtomicU64, Trap> {
    if addr < lo || addr.checked_add(8).is_none_or(|end| end > hi) || !addr.is_multiple_of(8) {
        return Err(Trap::Crash(format!("loom internal: {addr:#x} isn't a word of the program's data")));
    }
    // SAFETY: inside the data and aligned (checked above). The compiler
    // uses these words only as constant flags: all concurrent accesses
    // are atomic, and the data outlives each native invocation.
    Ok(unsafe { std::sync::atomic::AtomicU64::from_ptr(addr as *mut u64) })
}

fn const_load<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    args[0] = data_word(s, args[0])?;
    Ok(())
}

fn const_store<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    data_atomic(s.data_range(), args[0])?.store(args[1], std::sync::atomic::Ordering::SeqCst);
    // A task waiting for the constant is woken when it is done or failed (the
    // flag's value 2 is "computing", from `heddle/build/consts.rs`). The
    // waiter counted itself before it looked at the flag, so one of us sees
    // the other.
    if args[1] != 2 && s.const_waiting() {
        return Err(Trap::Task(crate::sched::TaskOp::ConstWake));
    }
    Ok(())
}

/// Task operations are classified once, before generic native dispatch.
pub(crate) fn resolve_task(name: &str) -> Option<crate::sched::TaskOp> {
    use crate::sched::TaskOp as T;
    Some(match name {
        "task.spawn" => T::Spawn,
        "task.join" => T::Join,
        "task.yield" => T::Yield,
        "task.cancelled" => T::Cancelled,
        "task.grain" => T::Grain,
        "loom.new" => T::LoomNew,
        "loom.crew" => T::LoomCrew,
        "loom.leave" => T::LoomLeave,
        "loom.leftover" => T::LoomLeftover,
        "loom.free" => T::LoomFree,
        "pipe.new" => T::PipeNew,
        "pipe.send" => T::PipeSend,
        "pipe.recv" => T::PipeRecv,
        "pipe.close" => T::PipeClose,
        "pipe.leftover" => T::PipeLeftover,
        "pipe.free" => T::PipeFree,
        "mutex.lock" => T::MutexLock,
        "mutex.unlock" => T::MutexUnlock,
        "const.begin" => T::ConstBegin,
        "time.sleep" => T::Sleep,
        "io.read_line" => T::ReadLine,
        "io.read_all" => T::ReadAll,
        _ => return None,
    })
}

/// The native named `name`, and how many registers it reads or writes.
pub(crate) fn resolve(name: &str) -> Option<(NativeFn, usize)> {
    let f: NativeFn = if resolve_task(name).is_some() {
        |_, _, _| Err(Trap::Crash("loom internal: a task native bypassed dispatch".into()))
    } else {
        match name {
            "const.load" => |c, a, i| const_load(c, a, i),
            "const.store" => |c, a, i| const_store(c, a, i),
            "out.str" => |c, a, i| out_str(c, a, i),
            "out.fmt" | "out.fmt2" => |c, a, i| out_fmt(c, a, i),
            "msg.begin" => |c, a, i| msg_begin(c, a, i),
            "msg.str" => |c, a, i| msg_str(c, a, i),
            "msg.fmt" | "msg.fmt2" => |c, a, i| msg_fmt(c, a, i),
            "msg.unravel" => |c, a, i| msg_unravel(c, a, i),
            "sink.str" => |c, a, i| sink_str(c, a, i),
            "sink.fmt" | "sink.fmt2" => |c, a, i| sink_fmt(c, a, i),
            "sink.push" => |c, a, i| sink_push(c, a, i),
            "sink.string" => |c, a, i| sink_string(c, a, i),
            "seq.join_text" => |c, a, i| seq_join_text(c, a, i),
            "sink.pad" => |c, a, i| sink_pad(c, a, i),
            "str.new" => |c, a, i| str_new(c, a, i),
            "str.eq" => str_eq,
            "str.cmp" => |c, a, i| str_cmp(c, a, i),
            "int.add_chk" => |_, a, _| checked(a, crate::ints::add_t),
            "int.sub_chk" => |_, a, _| checked(a, crate::ints::sub_t),
            "int.mul_chk" => |_, a, _| checked(a, crate::ints::mul_t),
            "int.cast_chk" => |c, a, i| int_cast_chk(&mut c.heap, a, i),
            "float.to_int_chk" => |c, a, i| float_to_int_chk(&mut c.heap, a, i),
            "int.to_char_chk" => |c, a, i| int_to_char_chk(&mut c.heap, a, i),
            "host.call" => host_call,
            "exit.fail" => exit_fail,
            "mem.alloc" => mem_alloc,
            "mem.free" => mem_free,
            "mem.grow" => mem_grow,
            "mem.move" => mem_move,
            "mem.calloc" => mem_calloc,
            "str.new_in" => |c, a, i| str_new_in(c, a, i),
            "sarena.new" => |c, a, _| arena_new(&mut c.heap, a),
            "sarena.alloc" => |c, a, _| arena_alloc(&mut c.heap, a),
            "sarena.reset" => |c, a, _| crate::arena::reset(&mut c.heap, a[0]),
            "sarena.free" => |c, a, _| crate::arena::free(&mut c.heap, a[0]),
            "pool.new" => |c, a, _| pool_new(&mut c.heap, a),
            "pool.alloc" => |_, a, _| pool_alloc(a),
            "pool.free" => |_, a, _| {
                crate::slab::free(a[0], a[1]);
                Ok(())
            },
            "pool.reset" => |_, a, _| {
                crate::slab::reset(a[0]);
                Ok(())
            },
            "pool.drop" => |c, a, _| crate::slab::drop_pool(&mut c.heap, a[0]),
            "arena.new" => |c, a, _| arena_new(&mut c.heap, a),
            "arena.alloc" => |c, a, _| arena_alloc(&mut c.heap, a),
            "arena.reset" => |c, a, _| crate::arena::reset(&mut c.heap, a[0]),
            "arena.free" => |c, a, _| crate::arena::free(&mut c.heap, a[0]),
            "atomic.load" => |c, a, _| crate::atomic::run(&c.heap, a, crate::atomic::Op::Load),
            "atomic.store" => |c, a, _| crate::atomic::run(&c.heap, a, crate::atomic::Op::Store),
            "atomic.swap" => |c, a, _| crate::atomic::run(&c.heap, a, crate::atomic::Op::Swap),
            "atomic.cas" => |c, a, _| crate::atomic::run(&c.heap, a, crate::atomic::Op::Cas),
            "atomic.add" => |c, a, _| crate::atomic::run(&c.heap, a, crate::atomic::Op::Add),
            "atomic.sub" => |c, a, _| crate::atomic::run(&c.heap, a, crate::atomic::Op::Sub),
            _ => crate::methods::resolve(name).or_else(|| crate::host::resolve(name))?,
        }
    };
    let sig = native(name)?;
    Some((f, usize::from(sig.args.max(sig.results))))
}

/// `host.call`: the host function bound to import `index`, over the registers at `args`.
fn host_call(ctx: &mut Ctx, args: &mut [u64], index: u16) -> Result<(), Trap> {
    ctx.host_check("host.call")?;
    let (window, f) =
        ctx.hosts.0.get_mut(usize::from(index)).ok_or_else(|| Trap::Crash("loom internal: no host function".into()))?;
    f(&mut args[..*window]).map_err(Trap::Unravel)
}

fn out_str<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text: *const [u8] = s.text(args[0], args[1])?;
    // SAFETY: `out.str` is passed a text value, which writing doesn't
    // touch: it's a literal or a heap object, not the output buffer.
    put(s, 0, unsafe { &*text })
}

fn out_fmt<S: Sinks>(s: &mut S, args: &mut [u64], item: u16) -> Result<(), Trap> {
    format_into(s, 0, item, args)
}

/// `msg.begin()`: a new message. Building one can run the program's code (a
/// hand-written `show`), which may unravel with a message of its own; that
/// one must not find the half-built text of this one.
fn msg_begin<S: Sinks>(s: &mut S, _: &mut [u64], _: u16) -> Result<(), Trap> {
    s.local().msg.clear();
    Ok(())
}

fn msg_str<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text: *const [u8] = s.text(args[0], args[1])?;
    // SAFETY: as for `out.str`.
    s.local().msg.extend_from_slice(unsafe { &*text });
    Ok(())
}

fn msg_fmt<S: Sinks>(s: &mut S, args: &mut [u64], item: u16) -> Result<(), Trap> {
    format_into(s, 1, item, args)
}

fn msg_unravel<S: Sinks>(s: &mut S, _: &mut [u64], _: u16) -> Result<(), Trap> {
    let msg = std::mem::take(&mut s.local().msg);
    Err(Trap::Unravel(String::from_utf8_lossy(&msg).into_owned()))
}

/// A native that touches only arguments, program memory and the worker
/// heap, never the shared context.
pub(crate) type LocalFn = fn(&mut Heap, &mut [u64], u16) -> Result<(), Trap>;

/// Worker-local leaves. Checked mem.move still uses the shared validation
/// path; atomics validate against shared heap and stack registries.
pub(crate) fn resolve_local(name: &str) -> Option<LocalFn> {
    Some(match name {
        "mem.alloc" => |h, a, _| heap_alloc(h, a),
        "mem.free" => |h, a, _| heap_free(h, a),
        "mem.grow" => |h, a, _| heap_grow(h, a),
        "mem.calloc" => |h, a, _| heap_calloc(h, a),
        "pool.new" => |h, a, _| pool_new(h, a),
        "pool.alloc" => |_, a, _| pool_alloc(a),
        "pool.free" => |_, a, _| {
            crate::slab::free(a[0], a[1]);
            Ok(())
        },
        "pool.reset" => |_, a, _| {
            crate::slab::reset(a[0]);
            Ok(())
        },
        "pool.drop" => |h, a, _| crate::slab::drop_pool(h, a[0]),
        "arena.new" => |h, a, _| arena_new(h, a),
        "arena.alloc" => |h, a, _| arena_alloc(h, a),
        "arena.reset" => |h, a, _| crate::arena::reset(h, a[0]),
        "arena.free" => |h, a, _| crate::arena::free(h, a[0]),
        "mem.move" if !cfg!(feature = "checked") => |h, a, _| heap_move(h, a),
        "int.add_chk" => |_, a, _| checked(a, crate::ints::add_t),
        "int.sub_chk" => |_, a, _| checked(a, crate::ints::sub_t),
        "int.mul_chk" => |_, a, _| checked(a, crate::ints::mul_t),
        "int.cast_chk" => int_cast_chk,
        "float.to_int_chk" => float_to_int_chk,
        "int.to_char_chk" => int_to_char_chk,
        #[cfg(not(feature = "checked"))]
        "str.new" => |h, a, i| str_new(&mut Own(h), a, i),
        #[cfg(not(feature = "checked"))]
        "str.eq" => |_, a, i| str_eq_in(&Raw, a, i),
        #[cfg(not(feature = "checked"))]
        "str.cmp" => |_, a, i| str_cmp(&Raw, a, i),
        _ => return crate::atomic::resolve(name).or_else(|| crate::methods::resolve_scalar(name)),
    })
}

/// `str.new_in(arena, text, length, destination)`.
fn str_new_in<S: Strings>(ctx: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text: *const [u8] = ctx.text(args[1], args[2])?;
    // SAFETY: `text` is the pointer `ctx.text` checked above, and the arena
    // allocation below only adds memory, so the bytes stay where they are.
    let text = unsafe { &*text };
    let len = text.len() as u64;
    let ptr = crate::arena::alloc(ctx.heap(), args[0], len)?;
    // SAFETY: `ptr` is `len` bytes of the arena, which nothing else refers
    // to; `args[3]` is where the compiler wants the string: three words of
    // memory it owns. A capacity of 0 says the buffer isn't the string's to free.
    unsafe {
        if len > 0 {
            std::ptr::copy_nonoverlapping(text.as_ptr(), ptr as *mut u8, text.len());
        }
        let d = args[3] as *mut u64;
        d.write_unaligned(ptr);
        d.add(1).write_unaligned(len);
        d.add(2).write_unaligned(0);
    }
    Ok(())
}

/// `pool.new(slot size, slots) -> pool`.
fn pool_new(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    args[0] = crate::slab::new(heap, args[0], args[1])?;
    Ok(())
}

/// `pool.alloc(pool) -> slot`.
fn pool_alloc(args: &mut [u64]) -> Result<(), Trap> {
    args[0] = crate::slab::alloc(args[0])?;
    Ok(())
}

/// `arena.new(capacity) -> arena`.
fn arena_new(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    args[0] = crate::arena::new(heap, args[0])?;
    Ok(())
}

/// `arena.alloc(arena, size) -> address`.
fn arena_alloc(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    args[0] = crate::arena::alloc(heap, args[0], args[1])?;
    Ok(())
}

/// `mem.alloc(size, align) -> address`.
#[inline(always)]
fn heap_alloc(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    args[0] = heap.alloc(args[0], args[1])?;
    Ok(())
}

// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_alloc")]
fn mem_alloc(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    heap_alloc(&mut ctx.heap, args)
}

/// `mem.free(address, size, align)`.
#[inline(always)]
fn heap_free(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    heap.free(args[0], args[1], args[2])
}

// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_free")]
fn mem_free(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    heap_free(&mut ctx.heap, args)
}

/// `mem.calloc(size, align) -> address`: zeroed memory.
#[inline(always)]
fn heap_calloc(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    let (size, align) = (args[0], args[1]);
    let ptr = heap.alloc(size, align)?;
    if size > 0 {
        // SAFETY: `ptr` is a new object of `size` bytes.
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, size as usize) };
    }
    args[0] = ptr;
    Ok(())
}

// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_calloc")]
fn mem_calloc(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    heap_calloc(&mut ctx.heap, args)
}

/// `mem.move(dst, src, bytes)`: copies `bytes` bytes, which may overlap.
#[inline(always)]
fn heap_move(_: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    let (dst, src, n) = (args[0], args[1], args[2]);
    if n > 0 {
        // SAFETY: the compiler passes two runs of `n` bytes the program owns,
        // in the buffers of lists, or a literal's to copy (the checked build
        // checks they're live, in `mem_move`).
        unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, n as usize) };
    }
    Ok(())
}

// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_move")]
fn mem_move(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (dst, src, n) = (args[0], args[1], args[2]);
    if cfg!(feature = "checked") && n > 0 && !(ctx.owns(dst, n) && ctx.readable(src, n)) {
        return Err(Trap::Crash(format!("loom internal: moving {n} bytes from {src:#x} to {dst:#x}")));
    }
    heap_move(&mut ctx.heap, args)
}

/// `mem.grow(header, element size, align, at least)`: makes room in the
/// buffer of the list (or string) whose header (address, length, capacity)
/// is at `header` for at least `at least` elements, at least doubling it.
#[inline(always)]
fn heap_grow(heap: &mut Heap, args: &mut [u64]) -> Result<(), Trap> {
    let (header, size, align, want) = (args[0], args[1], args[2], args[3]);
    // SAFETY: the compiler passes the address of a list's header, three
    // words it owns (the checked build checks the buffer below).
    let (ptr, len, cap) = unsafe {
        let h = header as *const u64;
        (h.read_unaligned(), h.add(1).read_unaligned(), h.add(2).read_unaligned())
    };
    if want <= cap {
        return Ok(());
    }
    let new_cap = want.max(cap.saturating_mul(2)).max(4);
    let (old_bytes, new_bytes) = match (cap.checked_mul(size), new_cap.checked_mul(size)) {
        (Some(o), Some(n)) if n <= crate::heap::LIMIT => (o, n),
        _ => return Err(Trap::OutOfMemory),
    };
    let moved = if cap == 0 && ptr != 0 && len > 0 {
        // A buffer an arena owns: what it holds moves to one of its own.
        let moved = heap.alloc(new_bytes, align)?;
        // SAFETY: `ptr` has `len` elements of `size` bytes, and `moved` room for more.
        unsafe { std::ptr::copy_nonoverlapping(ptr as *const u8, moved as *mut u8, (len * size) as usize) };
        moved
    } else {
        heap.realloc(ptr, old_bytes, new_bytes, align)?
    };
    // SAFETY: as above.
    unsafe {
        let h = header as *mut u64;
        h.write_unaligned(moved);
        h.add(2).write_unaligned(if size == 0 { u64::MAX } else { new_cap });
    }
    Ok(())
}

// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_mem_grow")]
fn mem_grow(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    heap_grow(&mut ctx.heap, args)
}

/// `sink.str(sink, text address, length)`.
fn sink_str<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text: *const [u8] = s.text(args[1], args[2])?;
    // SAFETY: as for `out.str`: a sink's buffer is never a text value's.
    put(s, args[0], unsafe { &*text })
}

/// `sink.fmt(sink, value)` and `sink.fmt2(sink, text address, length)`.
fn sink_fmt<S: Sinks>(s: &mut S, args: &mut [u64], item: u16) -> Result<(), Trap> {
    format_into(s, args[0], item, &args[1..])
}

/// `sink.push() -> sink`: a new buffer.
fn sink_push<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    args[0] = s.local().push();
    Ok(())
}

/// `sink.string(sink, destination)`: the buffer's text as a `String`.
fn sink_string<S: Sinks>(s: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text = s.local().pop(args[0])?;
    s.heap().new_string(&text, args[1])?;
    s.local().recycle(text);
    Ok(())
}

/// `seq.join_text(buf, len, elem size, sep, sep len, destination)`: the texts
/// of a list, slice or array of `String` or `str`, with `sep` between them.
fn seq_join_text<S: Strings>(ctx: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (buf, len, size) = (args[0], args[1] as usize, args[2]);
    // The compiler passes a `str` for the separator.
    let sep: *const [u8] = ctx.text(args[3], args[4])?;
    // SAFETY: `sep` is the text `ctx.text` just checked, which nothing frees
    // during this native.
    let sep: &[u8] = unsafe { &*sep };
    let item = |i: usize| -> (u64, u64) {
        // SAFETY: `buf` holds `len` elements of `size` bytes, each starting
        // with a text's address and length.
        unsafe {
            let e = (buf + i as u64 * size) as *const u64;
            (e.read_unaligned(), e.add(1).read_unaligned())
        }
    };
    let mut total = len.saturating_sub(1) * sep.len();
    for i in 0..len {
        total += item(i).1 as usize;
    }
    let mut out = Vec::with_capacity(total);
    for i in 0..len {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        let (p, n) = item(i);
        // Every element is a text value the program owns.
        out.extend_from_slice(ctx.text(p, n)?);
    }
    ctx.heap().new_string(&out, args[5])
}

/// `sink.pad(sink, into)`: the buffer's text, padded as the format item's
/// spec says, into another sink.
fn sink_pad<S: Sinks>(s: &mut S, args: &mut [u64], item: u16) -> Result<(), Trap> {
    let text = s.local().pop(args[0])?;
    let spec = s.fmt_item(item).spec;
    let mut out = Vec::new();
    match spec {
        Some(spec) => fmt::pad(&mut out, &text, &spec),
        None => out.extend_from_slice(&text),
    }
    s.local().recycle(text);
    put(s, args[1], &out)
}

/// `str.new(text address, length, destination)`: a `String` of the text.
fn str_new<S: Strings>(ctx: &mut S, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let text: *const [u8] = ctx.text(args[0], args[1])?;
    // SAFETY: `str.new` is passed a text value, which allocating doesn't
    // move or free.
    ctx.heap().new_string(unsafe { &*text }, args[2])
}

/// `str.eq(a, a length, b, b length)`: whether the texts are the same.
// SAFETY: no other symbol in the program has this name, and nothing calls
// it through the name.
#[allow(unsafe_code)]
#[unsafe(export_name = "loom_treadle_str_eq")]
fn str_eq(ctx: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    str_eq_in(ctx, args, imm)
}

/// `str.eq` on any source of text.
fn str_eq_in<T: Texts>(texts: &T, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let same = texts.text(args[0], args[1])? == texts.text(args[2], args[3])?;
    args[0] = u64::from(same);
    Ok(())
}

/// `str.cmp(a, a length, b, b length)`: -1, 0 or 1 as `a` comes before,
/// with or after `b`, byte by byte.
fn str_cmp<T: Texts>(texts: &T, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let order = texts.text(args[0], args[1])?.cmp(texts.text(args[2], args[3])?);
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
fn int_cast_chk(_: &mut Heap, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (to, from) =
        loom_isa::word::unpack_cast(args[1]).ok_or_else(|| Trap::Crash("loom internal: a bad cast".into()))?;
    let r = crate::ints::cast_t(args[0], to, from);
    result_pair(args, r)
}

/// `float.to_int_chk(f64 bits, type)`: the float truncated, if it fits.
fn float_to_int_chk(_: &mut Heap, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let r = crate::ints::ftoi_t(f64::from_bits(args[0]), crate::ints::spec(args[1]));
    result_pair(args, r)
}

/// `int.to_char_chk(v, type)`: the `char` `v` is the code point of, if it's one.
fn int_to_char_chk(_: &mut Heap, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let r = crate::ints::to_char(args[0], crate::ints::spec(args[1]));
    result_pair(args, r)
}

/// `exit.fail()`: `main` returned an error, whose text is in sink 1.
fn exit_fail(ctx: &mut Ctx, _: &mut [u64], _: u16) -> Result<(), Trap> {
    let msg = std::mem::take(&mut ctx.cur.msg);
    Err(Trap::Failed(String::from_utf8_lossy(&msg).into_owned()))
}
