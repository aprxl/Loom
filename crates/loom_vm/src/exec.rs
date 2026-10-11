//! The interpreter loop (design/vm.md §9.1).
//!
//! Only two things live in locals, so they stay in machine registers: `pc`
//! (the next instruction) and `fp` (the current frame's first register).
//! The rest (`Env`: `seg_end`, where the current stack segment ends, and
//! `ticks`, the loop iterations and calls until the next check for Ctrl-C,
//! among it) is read from memory where it's needed: kept as locals, it took
//! the registers `pc` and `fp` needed, and every instruction paid. Just below a
//! frame's first register, [`LINK_SLOTS`] slots of its caller's frame say
//! where to return: the return address, the caller's frame, and where the
//! results go (the frame itself, unless the frame started a new stack
//! segment and the caller's window is in the one before).
//!
//! Nothing in the loop may take the address of `pc` or `fp`: a closure that
//! reads either captures it by reference, and LLVM then keeps it in a stack
//! slot for the whole function (every dispatch a store and a load of it).
//! Read them into a local first (see `Resume`). Check with `--emit llvm-ir`:
//! `loom_treadle_execute` has no `%pc` alloca.
//!
//! Every register index, jump target and call window was checked by the
//! verifier before the loop starts, so the loop reads registers and code
//! without bounds checks. Rare work (traps, growing the stack) is out of
//! line.
//!
//! Loads and stores go straight to the address in a register: the compiler
//! only makes addresses of memory the program owns (design/vm.md §5.6).
//! The checked build verifies each against the live objects, the stack and
//! the image's data, and reports a bad one as an internal error.
//!
//! A trap unwinds (§9.4): the current function's table of landing pads says
//! where its `defer`s and drops for that instruction are, and the pad ends
//! by jumping to the next pad out or with `resume`, which goes on in the
//! caller at its call. The first trap is the one reported, unless a pad
//! unravels in turn, which replaces it.

#![allow(unsafe_code)]

use loom_isa::word::{a, b, bc_i32, bc_u32, c, c_i16, unpack_cast};
use loom_isa::{Image, LINK_SLOTS, Op};

use crate::heap::Heap;
use loom_sys::Host;

use crate::ints::{self, IntErr};
use crate::natives::{Ctx, NativeFn, resolve};
use crate::stack::Stack;
use crate::{Config, Fault, Trap, crash_message};

/// How many frames of each end of a trace are kept (see `trace_at`).
pub(crate) const TRACE_ENDS: usize = 64;

/// Loop iterations and calls between checks for Ctrl-C.
const TICKS: i32 = 1 << 16;

/// For the negated float comparisons: whether `x < y` (or `x <= y`, with
/// `or_equal`) is false, which it is when either is NaN too.
#[inline(always)]
fn not_below(order: Option<std::cmp::Ordering>, or_equal: bool) -> bool {
    use std::cmp::Ordering::{Equal, Less};
    !matches!(order, Some(Less)) && !(or_equal && order == Some(Equal))
}

/// A function, as a call needs it.
#[derive(Debug)]
struct Callee {
    entry: *const u64,
    frame: usize,
    params: usize,
    results: usize,
}

/// What the dispatch loop needs now and then (see `execute`).
struct Env<'e, 'i> {
    image: &'i Image,
    code_base: *const u64,
    funcs: *const Callee,
    nfuncs: usize,
    natives: &'e [(NativeFn, usize)],
    ctx: &'e mut Ctx<'i>,
    stack: &'e mut Stack,
    data_range: (u64, u64),
    entry_results: usize,
    /// The end of the stack segment in use.
    seg_end: *mut u64,
    /// Instructions until the next check for an interrupt.
    ticks: i32,
    /// What's unwinding the stack, while landing pads run.
    in_flight: Option<Fault>,
}

#[derive(Debug)]
pub(crate) struct Machine<'i> {
    image: &'i Image,
    code: &'i [u64],
    funcs: Vec<Callee>,
    /// The program's data: the image's, which the program may change
    /// (constants computed on first read).
    data: Box<[u64]>,
    natives: Vec<(NativeFn, usize)>,
    pub ctx: Ctx<'i>,
    stack_limit: usize,
    /// Kept between calls from outside.
    stack: Option<Stack>,
}

impl<'i> Machine<'i> {
    /// Prepares a verified image to run.
    pub fn load(image: &'i Image, host: Host, config: &Config) -> Result<Machine<'i>, String> {
        let natives = image
            .natives
            .iter()
            .map(|name| resolve(name).ok_or_else(|| format!("Treadle has no native `{name}`")))
            .collect::<Result<Vec<_>, _>>()?;
        let funcs = image
            .funcs
            .iter()
            .map(|f| Callee {
                entry: image.code[f.entry as usize..].as_ptr(),
                frame: usize::from(f.frame),
                params: usize::from(f.params),
                results: usize::from(f.results),
            })
            .collect();
        Ok(Machine {
            image,
            code: &image.code,
            funcs,
            data: image.data.clone().into_boxed_slice(),
            natives,
            ctx: {
                let mut ctx = Ctx::new(image, host);
                ctx.interrupt = config.interrupt.clone();
                ctx
            },
            stack_limit: config.stack_limit,
            stack: None,
        })
    }

    /// Calls function `entry` with `args` (one per parameter register) and
    /// gives its results.
    pub fn call(&mut self, entry: usize, args: &[u64]) -> Result<Vec<u64>, Fault> {
        let Some(start) = self.funcs.get(entry) else {
            return Err(Fault {
                trap: Trap::Crash(format!("there's no function {entry}")),
                pc: 0,
                trace: Vec::new(),
                skipped: 0,
            });
        };
        if args.len() != start.params {
            let trap = Trap::Crash(format!("{} arguments for {} parameters", args.len(), start.params));
            return Err(Fault {
                trap,
                pc: (start.entry as usize - self.code.as_ptr() as usize) / 8,
                trace: Vec::new(),
                skipped: 0,
            });
        }
        let need = LINK_SLOTS + start.frame;
        let mut stack = match self.stack.take() {
            Some(mut stack) if stack.first_holds(need) => {
                stack.reset();
                stack
            }
            _ => Stack::new(self.stack_limit, need).map_err(|trap| Fault {
                trap,
                pc: 0,
                trace: Vec::new(),
                skipped: 0,
            })?,
        };
        let result = self.execute(&mut stack, entry, args);
        self.stack = Some(stack);
        result
    }

    // At the start of the code (crates/loom/build.rs asks the linker for it
    // by this name), so what changes elsewhere doesn't move it (design/vm.md
    // §9.1).
    // SAFETY: no other symbol in the program has this name, and nothing
    // calls it through the name.
    #[inline(never)]
    #[allow(unsafe_code)]
    #[unsafe(export_name = "loom_treadle_execute")]
    fn execute(&mut self, stack: &mut Stack, entry: usize, args: &[u64]) -> Result<Vec<u64>, Fault> {
        let start = &self.funcs[entry];
        let (base, seg_end) = stack.bounds();
        // SAFETY: the first segment holds the entry's linkage and frame,
        // whose parameters are its first registers.
        let mut fp = unsafe { base.add(LINK_SLOTS) };
        // SAFETY: as above.
        unsafe {
            *base = 0; // no return address: back to the host
            *base.add(1) = 0;
            *base.add(2) = fp as u64;
            std::ptr::copy_nonoverlapping(args.as_ptr(), fp, args.len());
        }
        let mut pc = start.entry;
        let data_range = {
            let r = self.data.as_mut_ptr_range();
            (r.start as u64, r.end as u64)
        };
        self.ctx.stopping = false;
        self.ctx.data_range = data_range;
        self.ctx.reset_sinks();
        let mut env = Env {
            image: self.image,
            code_base: self.code.as_ptr(),
            funcs: self.funcs.as_ptr(),
            nfuncs: self.funcs.len(),
            natives: &self.natives,
            ctx: &mut self.ctx,
            stack,
            data_range,
            entry_results: start.results,
            seg_end,
            ticks: TICKS,
            in_flight: None,
        };
        // Only `pc` and `fp` (and each instruction's word) stay in registers
        // through the loop; the rest is read from `env` where it's needed.
        // Kept as locals, they took the registers `pc` or `fp` needed, which
        // then went to memory and back for every instruction.
        let env = std::hint::black_box(&mut env);

        macro_rules! at_index {
            ($at:expr) => {
                ($at as usize - env.code_base as usize) / 8
            };
        }
        macro_rules! fault {
            ($trap:expr, $at:expr $(,)?) => {
                Fault { trap: $trap, pc: at_index!($at), trace: Vec::new(), skipped: 0 }
            };
        }
        'run: loop {
            // SAFETY: `pc` is always at an instruction of a verified
            // function: every jump, call and return lands on one, and no
            // function runs off its end.
            let w = unsafe { *pc };
            pc = unsafe { pc.add(1) };
            if cfg!(feature = "stats") {
                env.ctx.dispatched += 1;
                env.ctx.op_counts[usize::from(w as u16)] += 1;
                env.ctx.pc_counts[(pc as usize - env.code_base as usize) / 8 - 1] += 1;
            }
            let op = if cfg!(feature = "checked") {
                Op::from_u16(w as u16).expect("the verifier checked every opcode")
            } else {
                // SAFETY: the verifier checked that every opcode is an `Op`
                // discriminant, and `Op` is `repr(u16)`.
                unsafe { std::mem::transmute::<u16, Op>(w as u16) }
            };
            // Where this instruction starts, worked out only to report a
            // trap (kept in a register, it cost every instruction): the one
            // the word before `pc` is part of, which every instruction is
            // until it moves `pc` itself, after anything that can trap.
            macro_rules! here {
                () => {
                    instruction_at(env.image, env.code_base, pc)
                };
            }

            // An instruction either goes on to the next one (`continue`) or
            // ends this block with a trap, which one place, out of the way
            // of the instructions, handles: inlined at each of them, it made
            // the loop too big to keep its state in registers.
            let trap: Trap = 'op: {
                // Registers are `fp` plus an index the verifier checked is
                // inside the frame, and a call makes sure the whole frame is
                // inside the segment.
                macro_rules! r {
                    ($i:expr) => {{
                        let i = $i;
                        // SAFETY: see above.
                        debug_assert!(unsafe { fp.add(i) } < env.seg_end);
                        // SAFETY: see above.
                        unsafe { *fp.add(i) }
                    }};
                }
                macro_rules! set {
                    ($i:expr, $v:expr) => {{
                        let (i, v) = ($i, $v);
                        // SAFETY: see above.
                        debug_assert!(unsafe { fp.add(i) } < env.seg_end);
                        // SAFETY: see above.
                        unsafe { *fp.add(i) = v }
                    }};
                }
                // The extension word after this instruction.
                macro_rules! ext {
                    () => {{
                        // SAFETY: the verifier checked that this op's extension
                        // word is inside the function.
                        let e = unsafe { *pc };
                        // SAFETY: as above.
                        pc = unsafe { pc.add(1) };
                        e
                    }};
                }
                macro_rules! jump {
                    ($off:expr) => {{
                        // SAFETY: the verifier checked that every jump lands on
                        // an instruction of the same function.
                        pc = unsafe { pc.offset($off - 1) };
                    }};
                }
                // A conditional jump: one backwards is a loop's back edge,
                // which counts a tick as `loop` does.
                macro_rules! cjump {
                    ($off:expr) => {{
                        let off: isize = $off as isize;
                        if off < 0 {
                            tick!();
                        }
                        jump!(off);
                    }};
                }
                macro_rules! tick {
                    () => {{
                        env.ticks -= 1;
                        if env.ticks < 0 {
                            env.ticks = TICKS;
                            // Once: the unwinding runs `defer`s to the end.
                            if env.ctx.interrupted() {
                                throw!(Trap::Interrupt);
                            }
                        }
                    }};
                }
                // Enters `callee` with the window at register `a`: its frame
                // starts there, unless the segment is too short for it (its
                // linkage goes in this frame, just below the window).
                macro_rules! enter {
                    ($callee:expr) => {{
                        let callee: &Callee = $callee;
                        tick!();
                        // SAFETY: the window is inside this frame (verified).
                        let window = unsafe { fp.add(a(w)) };
                        let mut new_fp = window;
                        if (env.seg_end as usize - window as usize) / 8 < callee.frame {
                            // SAFETY: the window holds the callee's arguments.
                            match unsafe { enter_segment(env.stack, window, callee) } {
                                Ok((start, end)) => {
                                    new_fp = start;
                                    env.seg_end = end;
                                }
                                Err(trap) => throw!(trap),
                            }
                        }
                        // SAFETY: the linkage is just below the callee's frame:
                        // in this frame (the verifier left room below the
                        // window), or at the start of a new segment.
                        unsafe {
                            let link = new_fp.sub(LINK_SLOTS);
                            *link = pc as u64;
                            *link.add(1) = fp as u64;
                            *link.add(2) = window as u64;
                        }
                        fp = new_fp;
                        pc = callee.entry;
                    }};
                }
                // Unwinds with `trap` (see below).
                macro_rules! throw {
                    ($trap:expr) => {
                        break 'op $trap
                    };
                }
                macro_rules! int_trap {
                    ($e:expr) => {
                        throw!(match $e {
                            IntErr::Overflow | IntErr::Range => Trap::Overflow,
                            IntErr::DivZero => Trap::DivZero,
                            IntErr::ShiftRange => Trap::ShiftRange,
                        })
                    };
                }
                // In the checked build, an access must be inside a live object,
                // the stack in use, the image's text or the program's data.
                macro_rules! mem_check {
                    ($addr:expr, $len:expr) => {{
                        if cfg!(feature = "checked") {
                            let (addr, len) = ($addr, $len as u64);
                            if !valid(&env.ctx.heap, env.stack, env.ctx.rodata_range(), env.data_range, addr, len) {
                                return Err(fault!(
                                    Trap::Crash(format!(
                                        "loom internal: a bad memory access: {len} bytes at {addr:#x}"
                                    )),
                                    here!(),
                                ));
                            }
                        }
                    }};
                }
                // A load of a `$t` at register `b` plus the offset in `c`,
                // widened to 64 bits by `$ext`.
                macro_rules! load {
                    ($t:ty, $ext:expr) => {{
                        let addr = r!(b(w)).wrapping_add(c_i16(w) as i64 as u64);
                        mem_check!(addr, std::mem::size_of::<$t>());
                        // SAFETY: the compiler only loads from memory the
                        // program owns (see the module's documentation).
                        let v = unsafe { (addr as *const $t).read_unaligned() };
                        set!(a(w), $ext(v))
                    }};
                }
                macro_rules! store {
                    ($t:ty) => {{
                        let addr = r!(a(w)).wrapping_add(c_i16(w) as i64 as u64);
                        mem_check!(addr, std::mem::size_of::<$t>());
                        let v = r!(b(w)) as $t;
                        // SAFETY: as for loads.
                        unsafe { (addr as *mut $t).write_unaligned(v) }
                    }};
                }
                // The same at a byte offset from the frame pointer, which the
                // verifier checked is inside the frame.
                macro_rules! load_f {
                    ($t:ty, $ext:expr) => {{
                        // SAFETY: inside this frame (verified), which a call
                        // made sure is inside the segment.
                        let v = unsafe { ((fp as *const u8).add(bc_u32(w) as usize) as *const $t).read_unaligned() };
                        set!(a(w), $ext(v))
                    }};
                }
                macro_rules! store_f {
                    ($t:ty) => {{
                        let v = r!(a(w)) as $t;
                        // SAFETY: as for `load_f`.
                        unsafe { ((fp as *mut u8).add(bc_u32(w) as usize) as *mut $t).write_unaligned(v) }
                    }};
                }
                macro_rules! bin {
                    ($f:expr) => {{
                        let (x, y) = (r!(b(w)), r!(c(w)));
                        set!(a(w), $f(x, y))
                    }};
                }
                macro_rules! bin_t {
                    ($f:expr) => {{
                        let (x, y) = (r!(b(w)), r!(c(w)));
                        match $f(x, y) {
                            Ok(v) => set!(a(w), v),
                            Err(e) => int_trap!(e),
                        }
                    }};
                }
                // `c` is a 16-bit constant, sign-extended to 64 bits (the
                // register of a value of any integer type that holds it).
                macro_rules! imm {
                    ($f:expr) => {{
                        let x = r!(b(w));
                        set!(a(w), $f(x, c_i16(w) as i64 as u64))
                    }};
                }
                macro_rules! imm_t {
                    ($f:expr) => {{
                        let x = r!(b(w));
                        match $f(x, c_i16(w) as i64 as u64) {
                            Ok(v) => set!(a(w), v),
                            Err(e) => int_trap!(e),
                        }
                    }};
                }
                macro_rules! branch_k {
                    ($f:expr) => {{
                        if $f(r!(a(w)), b(w) as u16 as i16 as i64 as u64) {
                            cjump!(c_i16(w));
                        }
                    }};
                }
                macro_rules! un {
                    ($f:expr) => {{
                        let x = r!(b(w));
                        set!(a(w), $f(x))
                    }};
                }
                macro_rules! un_t {
                    ($f:expr) => {{
                        let x = r!(b(w));
                        match $f(x) {
                            Ok(v) => set!(a(w), v),
                            Err(e) => int_trap!(e),
                        }
                    }};
                }
                macro_rules! f64s {
                    ($f:expr) => {
                        bin!(|x: u64, y: u64| $f(f64::from_bits(x), f64::from_bits(y)).to_bits())
                    };
                }
                macro_rules! f32s {
                    ($f:expr) => {
                        bin!(|x: u64, y: u64| u64::from(
                            $f(f32::from_bits(x as u32), f32::from_bits(y as u32)).to_bits()
                        ))
                    };
                }
                macro_rules! cmp {
                    ($f:expr) => {
                        bin!(|x: u64, y: u64| u64::from($f(x, y)))
                    };
                }
                // The address of element `index` (each `scale` bytes) of the
                // list whose header (buffer, length) is at `header`, plus `off`:
                // unravels unless the index is below the length. `$n` bytes are
                // read or written there.
                macro_rules! elem {
                    ($header:expr, $index:expr, $scale:expr, $off:expr, $n:expr) => {{
                        let (header, index, scale, off) = ($header, $index, $scale, $off);
                        mem_check!(header, 16);
                        // SAFETY: the compiler passes the address of a list's
                        // (or a slice's) header: its buffer, then its length.
                        let (buf, len) = unsafe {
                            let h = header as *const u64;
                            (h.read(), h.add(1).read())
                        };
                        if index >= len {
                            throw!(Trap::Index { index, len });
                        }
                        let p = buf.wrapping_add(index.wrapping_mul(scale)).wrapping_add(off);
                        mem_check!(p, $n);
                        p
                    }};
                }
                // The operands of a load or a store at an element: the header
                // in the frame, or at a register's address.
                macro_rules! el_at {
                    (frame, $n:expr) => {{
                        let ext = ext!();
                        elem!((fp as u64).wrapping_add(ext >> 32), r!(b(w)), ext & 0xFFFF_FFFF, c(w) as u64, $n)
                    }};
                    (reg, $n:expr) => {{
                        let ext = ext!();
                        elem!(r!(b(w)), r!(c(w)), ext & 0xFFFF_FFFF, ext >> 32, $n)
                    }};
                }
                macro_rules! ld_el {
                    ($at:tt, $t:ty, $ext:expr) => {{
                        let p = el_at!($at, std::mem::size_of::<$t>());
                        // SAFETY: inside the list's buffer (checked above).
                        let v = unsafe { (p as *const $t).read_unaligned() };
                        set!(a(w), $ext(v))
                    }};
                }
                macro_rules! st_el {
                    ($at:tt, $t:ty) => {{
                        let p = el_at!($at, std::mem::size_of::<$t>());
                        let v = r!(a(w)) as $t;
                        // SAFETY: as for `ld_el`.
                        unsafe { (p as *mut $t).write_unaligned(v) }
                    }};
                }
                // The address of a load or a store at `b + c * scale + off`
                // (`ldx`, `stx`): an index the compiler checked, or checks
                // here against the length in a register (`ldxc`, `stxc`).
                macro_rules! x_at {
                    (plain, $n:expr) => {{
                        let ext = ext!();
                        let p = r!(b(w)).wrapping_add(r!(c(w)).wrapping_mul(ext & 0xFFFF_FFFF)).wrapping_add(ext >> 32);
                        mem_check!(p, $n);
                        p
                    }};
                    (checked, $n:expr) => {{
                        let ext = ext!();
                        let (index, len) = (r!(c(w)), r!((ext >> 48) as usize));
                        if index >= len {
                            throw!(Trap::Index { index, len });
                        }
                        let p = r!(b(w))
                            .wrapping_add(index.wrapping_mul(ext & 0xFFFF_FFFF))
                            .wrapping_add((ext >> 32) & 0xFFFF);
                        mem_check!(p, $n);
                        p
                    }};
                }
                macro_rules! ld_x {
                    ($at:tt, $t:ty, $ext:expr) => {{
                        let p = x_at!($at, std::mem::size_of::<$t>());
                        // SAFETY: an element of a list's buffer, whose index
                        // the compiler checked (or `x_at` just did).
                        let v = unsafe { (p as *const $t).read_unaligned() };
                        set!(a(w), $ext(v))
                    }};
                }
                macro_rules! st_x {
                    ($at:tt, $t:ty) => {{
                        let p = x_at!($at, std::mem::size_of::<$t>());
                        let v = r!(a(w)) as $t;
                        // SAFETY: as for `ld_x`.
                        unsafe { (p as *mut $t).write_unaligned(v) }
                    }};
                }
                // Frees the buffer of the list whose header is at `header`, of
                // `size`-byte elements, unless its capacity is zero.
                macro_rules! free_buf {
                    ($header:expr, $size:expr, $align:expr) => {{
                        let header = $header;
                        mem_check!(header, 24);
                        // SAFETY: the compiler passes the address of a list's
                        // header: its buffer, length and capacity.
                        let (buf, cap) = unsafe {
                            let h = header as *const u64;
                            (h.read(), h.add(2).read())
                        };
                        if cap != 0
                            && let Err(trap) = env.ctx.heap.free(buf, cap.wrapping_mul($size), $align)
                        {
                            throw!(trap);
                        }
                    }};
                }
                macro_rules! branch {
                    ($f:expr) => {{
                        if $f(r!(a(w)), r!(b(w))) {
                            cjump!(c_i16(w));
                        }
                    }};
                }

                match op {
                    Op::Mov => un!(|x| x),
                    Op::Ldi => set!(a(w), bc_i32(w) as i64 as u64),
                    Op::Ldk => {
                        let k = ext!();
                        set!(a(w), k)
                    }
                    Op::Ldstr => set!(a(w), env.ctx.string_ptr(bc_u32(w) as usize)),
                    Op::AddTI8 => bin_t!(|x, y| ints::add_t(x, y, ints::I8)),
                    Op::AddTI16 => bin_t!(|x, y| ints::add_t(x, y, ints::I16)),
                    Op::AddTI32 => bin_t!(|x, y| ints::add_t(x, y, ints::I32)),
                    Op::AddTI64 => bin_t!(|x, y| ints::add_t(x, y, ints::I64)),
                    Op::AddTU8 => bin_t!(|x, y| ints::add_t(x, y, ints::U8)),
                    Op::AddTU16 => bin_t!(|x, y| ints::add_t(x, y, ints::U16)),
                    Op::AddTU32 => bin_t!(|x, y| ints::add_t(x, y, ints::U32)),
                    Op::AddTU64 => bin_t!(|x, y| ints::add_t(x, y, ints::U64)),
                    Op::AddTG => {
                        let s = ints::spec(ext!());
                        bin_t!(|x, y| ints::add_t(x, y, s))
                    }
                    Op::AddWI8 => bin!(|x, y| ints::add_w(x, y, ints::I8)),
                    Op::AddWI16 => bin!(|x, y| ints::add_w(x, y, ints::I16)),
                    Op::AddWI32 => bin!(|x, y| ints::add_w(x, y, ints::I32)),
                    Op::AddWI64 => bin!(|x, y| ints::add_w(x, y, ints::I64)),
                    Op::AddWU8 => bin!(|x, y| ints::add_w(x, y, ints::U8)),
                    Op::AddWU16 => bin!(|x, y| ints::add_w(x, y, ints::U16)),
                    Op::AddWU32 => bin!(|x, y| ints::add_w(x, y, ints::U32)),
                    Op::AddWU64 => bin!(|x, y| ints::add_w(x, y, ints::U64)),
                    Op::AddWG => {
                        let s = ints::spec(ext!());
                        bin!(|x, y| ints::add_w(x, y, s))
                    }
                    Op::AddSI8 => bin!(|x, y| ints::add_s(x, y, ints::I8)),
                    Op::AddSI16 => bin!(|x, y| ints::add_s(x, y, ints::I16)),
                    Op::AddSI32 => bin!(|x, y| ints::add_s(x, y, ints::I32)),
                    Op::AddSI64 => bin!(|x, y| ints::add_s(x, y, ints::I64)),
                    Op::AddSU8 => bin!(|x, y| ints::add_s(x, y, ints::U8)),
                    Op::AddSU16 => bin!(|x, y| ints::add_s(x, y, ints::U16)),
                    Op::AddSU32 => bin!(|x, y| ints::add_s(x, y, ints::U32)),
                    Op::AddSU64 => bin!(|x, y| ints::add_s(x, y, ints::U64)),
                    Op::AddSG => {
                        let s = ints::spec(ext!());
                        bin!(|x, y| ints::add_s(x, y, s))
                    }
                    Op::SubTI8 => bin_t!(|x, y| ints::sub_t(x, y, ints::I8)),
                    Op::SubTI16 => bin_t!(|x, y| ints::sub_t(x, y, ints::I16)),
                    Op::SubTI32 => bin_t!(|x, y| ints::sub_t(x, y, ints::I32)),
                    Op::SubTI64 => bin_t!(|x, y| ints::sub_t(x, y, ints::I64)),
                    Op::SubTU8 => bin_t!(|x, y| ints::sub_t(x, y, ints::U8)),
                    Op::SubTU16 => bin_t!(|x, y| ints::sub_t(x, y, ints::U16)),
                    Op::SubTU32 => bin_t!(|x, y| ints::sub_t(x, y, ints::U32)),
                    Op::SubTU64 => bin_t!(|x, y| ints::sub_t(x, y, ints::U64)),
                    Op::SubTG => {
                        let s = ints::spec(ext!());
                        bin_t!(|x, y| ints::sub_t(x, y, s))
                    }
                    Op::SubWI8 => bin!(|x, y| ints::sub_w(x, y, ints::I8)),
                    Op::SubWI16 => bin!(|x, y| ints::sub_w(x, y, ints::I16)),
                    Op::SubWI32 => bin!(|x, y| ints::sub_w(x, y, ints::I32)),
                    Op::SubWI64 => bin!(|x, y| ints::sub_w(x, y, ints::I64)),
                    Op::SubWU8 => bin!(|x, y| ints::sub_w(x, y, ints::U8)),
                    Op::SubWU16 => bin!(|x, y| ints::sub_w(x, y, ints::U16)),
                    Op::SubWU32 => bin!(|x, y| ints::sub_w(x, y, ints::U32)),
                    Op::SubWU64 => bin!(|x, y| ints::sub_w(x, y, ints::U64)),
                    Op::SubWG => {
                        let s = ints::spec(ext!());
                        bin!(|x, y| ints::sub_w(x, y, s))
                    }
                    Op::SubSI8 => bin!(|x, y| ints::sub_s(x, y, ints::I8)),
                    Op::SubSI16 => bin!(|x, y| ints::sub_s(x, y, ints::I16)),
                    Op::SubSI32 => bin!(|x, y| ints::sub_s(x, y, ints::I32)),
                    Op::SubSI64 => bin!(|x, y| ints::sub_s(x, y, ints::I64)),
                    Op::SubSU8 => bin!(|x, y| ints::sub_s(x, y, ints::U8)),
                    Op::SubSU16 => bin!(|x, y| ints::sub_s(x, y, ints::U16)),
                    Op::SubSU32 => bin!(|x, y| ints::sub_s(x, y, ints::U32)),
                    Op::SubSU64 => bin!(|x, y| ints::sub_s(x, y, ints::U64)),
                    Op::SubSG => {
                        let s = ints::spec(ext!());
                        bin!(|x, y| ints::sub_s(x, y, s))
                    }
                    Op::MulTI8 => bin_t!(|x, y| ints::mul_t(x, y, ints::I8)),
                    Op::MulTI16 => bin_t!(|x, y| ints::mul_t(x, y, ints::I16)),
                    Op::MulTI32 => bin_t!(|x, y| ints::mul_t(x, y, ints::I32)),
                    Op::MulTI64 => bin_t!(|x, y| ints::mul_t(x, y, ints::I64)),
                    Op::MulTU8 => bin_t!(|x, y| ints::mul_t(x, y, ints::U8)),
                    Op::MulTU16 => bin_t!(|x, y| ints::mul_t(x, y, ints::U16)),
                    Op::MulTU32 => bin_t!(|x, y| ints::mul_t(x, y, ints::U32)),
                    Op::MulTU64 => bin_t!(|x, y| ints::mul_t(x, y, ints::U64)),
                    Op::MulTG => {
                        let s = ints::spec(ext!());
                        bin_t!(|x, y| ints::mul_t(x, y, s))
                    }
                    Op::MulWI8 => bin!(|x, y| ints::mul_w(x, y, ints::I8)),
                    Op::MulWI16 => bin!(|x, y| ints::mul_w(x, y, ints::I16)),
                    Op::MulWI32 => bin!(|x, y| ints::mul_w(x, y, ints::I32)),
                    Op::MulWI64 => bin!(|x, y| ints::mul_w(x, y, ints::I64)),
                    Op::MulWU8 => bin!(|x, y| ints::mul_w(x, y, ints::U8)),
                    Op::MulWU16 => bin!(|x, y| ints::mul_w(x, y, ints::U16)),
                    Op::MulWU32 => bin!(|x, y| ints::mul_w(x, y, ints::U32)),
                    Op::MulWU64 => bin!(|x, y| ints::mul_w(x, y, ints::U64)),
                    Op::MulWG => {
                        let s = ints::spec(ext!());
                        bin!(|x, y| ints::mul_w(x, y, s))
                    }
                    Op::MulSI8 => bin!(|x, y| ints::mul_s(x, y, ints::I8)),
                    Op::MulSI16 => bin!(|x, y| ints::mul_s(x, y, ints::I16)),
                    Op::MulSI32 => bin!(|x, y| ints::mul_s(x, y, ints::I32)),
                    Op::MulSI64 => bin!(|x, y| ints::mul_s(x, y, ints::I64)),
                    Op::MulSU8 => bin!(|x, y| ints::mul_s(x, y, ints::U8)),
                    Op::MulSU16 => bin!(|x, y| ints::mul_s(x, y, ints::U16)),
                    Op::MulSU32 => bin!(|x, y| ints::mul_s(x, y, ints::U32)),
                    Op::MulSU64 => bin!(|x, y| ints::mul_s(x, y, ints::U64)),
                    Op::MulSG => {
                        let s = ints::spec(ext!());
                        bin!(|x, y| ints::mul_s(x, y, s))
                    }
                    Op::DivI8 => bin_t!(|x, y| ints::div(x, y, ints::I8)),
                    Op::DivI16 => bin_t!(|x, y| ints::div(x, y, ints::I16)),
                    Op::DivI32 => bin_t!(|x, y| ints::div(x, y, ints::I32)),
                    Op::DivI64 => bin_t!(|x, y| ints::div(x, y, ints::I64)),
                    Op::DivU8 => bin_t!(|x, y| ints::div(x, y, ints::U8)),
                    Op::DivU16 => bin_t!(|x, y| ints::div(x, y, ints::U16)),
                    Op::DivU32 => bin_t!(|x, y| ints::div(x, y, ints::U32)),
                    Op::DivU64 => bin_t!(|x, y| ints::div(x, y, ints::U64)),
                    Op::DivG => {
                        let s = ints::spec(ext!());
                        bin_t!(|x, y| ints::div(x, y, s))
                    }
                    Op::RemS => bin_t!(ints::rem_s),
                    Op::RemU => bin_t!(ints::rem_u),
                    Op::NegI8 => un_t!(|x| ints::neg(x, ints::I8)),
                    Op::NegI16 => un_t!(|x| ints::neg(x, ints::I16)),
                    Op::NegI32 => un_t!(|x| ints::neg(x, ints::I32)),
                    Op::NegI64 => un_t!(|x| ints::neg(x, ints::I64)),
                    Op::NegG => {
                        let s = ints::spec(ext!());
                        un_t!(|x| ints::neg(x, s))
                    }
                    Op::BnotS => un!(|x: u64| !x),
                    Op::BnotU8 => un!(|x| ints::bnot(x, ints::U8)),
                    Op::BnotU16 => un!(|x| ints::bnot(x, ints::U16)),
                    Op::BnotU32 => un!(|x| ints::bnot(x, ints::U32)),
                    Op::BnotU64 => un!(|x| ints::bnot(x, ints::U64)),
                    Op::BnotG => {
                        let s = ints::spec(ext!());
                        un!(|x| ints::bnot(x, s))
                    }
                    Op::ShlI8 => bin_t!(|x, y| ints::shl(x, y, ints::I8)),
                    Op::ShlI16 => bin_t!(|x, y| ints::shl(x, y, ints::I16)),
                    Op::ShlI32 => bin_t!(|x, y| ints::shl(x, y, ints::I32)),
                    Op::ShlI64 => bin_t!(|x, y| ints::shl(x, y, ints::I64)),
                    Op::ShlU8 => bin_t!(|x, y| ints::shl(x, y, ints::U8)),
                    Op::ShlU16 => bin_t!(|x, y| ints::shl(x, y, ints::U16)),
                    Op::ShlU32 => bin_t!(|x, y| ints::shl(x, y, ints::U32)),
                    Op::ShlU64 => bin_t!(|x, y| ints::shl(x, y, ints::U64)),
                    Op::ShlG => {
                        let s = ints::spec(ext!());
                        bin_t!(|x, y| ints::shl(x, y, s))
                    }
                    Op::ShrI8 => bin_t!(|x, y| ints::shr(x, y, ints::I8)),
                    Op::ShrI16 => bin_t!(|x, y| ints::shr(x, y, ints::I16)),
                    Op::ShrI32 => bin_t!(|x, y| ints::shr(x, y, ints::I32)),
                    Op::ShrI64 => bin_t!(|x, y| ints::shr(x, y, ints::I64)),
                    Op::ShrU8 => bin_t!(|x, y| ints::shr(x, y, ints::U8)),
                    Op::ShrU16 => bin_t!(|x, y| ints::shr(x, y, ints::U16)),
                    Op::ShrU32 => bin_t!(|x, y| ints::shr(x, y, ints::U32)),
                    Op::ShrU64 => bin_t!(|x, y| ints::shr(x, y, ints::U64)),
                    Op::ShrG => {
                        let s = ints::spec(ext!());
                        bin_t!(|x, y| ints::shr(x, y, s))
                    }
                    Op::AddTKI8 => imm_t!(|x, k| ints::add_t(x, k, ints::I8)),
                    Op::AddTKI16 => imm_t!(|x, k| ints::add_t(x, k, ints::I16)),
                    Op::AddTKI32 => imm_t!(|x, k| ints::add_t(x, k, ints::I32)),
                    Op::AddTKI64 => imm_t!(|x, k| ints::add_t(x, k, ints::I64)),
                    Op::AddTKU8 => imm_t!(|x, k| ints::add_t(x, k, ints::U8)),
                    Op::AddTKU16 => imm_t!(|x, k| ints::add_t(x, k, ints::U16)),
                    Op::AddTKU32 => imm_t!(|x, k| ints::add_t(x, k, ints::U32)),
                    Op::AddTKU64 => imm_t!(|x, k| ints::add_t(x, k, ints::U64)),
                    Op::SubTKI8 => imm_t!(|x, k| ints::sub_t(x, k, ints::I8)),
                    Op::SubTKI16 => imm_t!(|x, k| ints::sub_t(x, k, ints::I16)),
                    Op::SubTKI32 => imm_t!(|x, k| ints::sub_t(x, k, ints::I32)),
                    Op::SubTKI64 => imm_t!(|x, k| ints::sub_t(x, k, ints::I64)),
                    Op::SubTKU8 => imm_t!(|x, k| ints::sub_t(x, k, ints::U8)),
                    Op::SubTKU16 => imm_t!(|x, k| ints::sub_t(x, k, ints::U16)),
                    Op::SubTKU32 => imm_t!(|x, k| ints::sub_t(x, k, ints::U32)),
                    Op::SubTKU64 => imm_t!(|x, k| ints::sub_t(x, k, ints::U64)),
                    Op::AddWKI8 => imm!(|x, k| ints::add_w(x, k, ints::I8)),
                    Op::AddWKI16 => imm!(|x, k| ints::add_w(x, k, ints::I16)),
                    Op::AddWKI32 => imm!(|x, k| ints::add_w(x, k, ints::I32)),
                    Op::AddWKI64 => imm!(|x, k| ints::add_w(x, k, ints::I64)),
                    Op::AddWKU8 => imm!(|x, k| ints::add_w(x, k, ints::U8)),
                    Op::AddWKU16 => imm!(|x, k| ints::add_w(x, k, ints::U16)),
                    Op::AddWKU32 => imm!(|x, k| ints::add_w(x, k, ints::U32)),
                    Op::AddWKU64 => imm!(|x, k| ints::add_w(x, k, ints::U64)),
                    Op::AndK => set!(a(w), r!(b(w)) & c(w) as u64),
                    Op::OrK => set!(a(w), r!(b(w)) | c(w) as u64),
                    Op::XorK => set!(a(w), r!(b(w)) ^ c(w) as u64),
                    // The verifier checked that the constant is below the
                    // width, and above zero for a division.
                    Op::ShlKI8 => imm!(|x: u64, k| ints::wrap(x << k, ints::I8)),
                    Op::ShlKI16 => imm!(|x: u64, k| ints::wrap(x << k, ints::I16)),
                    Op::ShlKI32 => imm!(|x: u64, k| ints::wrap(x << k, ints::I32)),
                    Op::ShlKI64 => imm!(|x: u64, k| x << k),
                    Op::ShlKU8 => imm!(|x: u64, k| ints::wrap(x << k, ints::U8)),
                    Op::ShlKU16 => imm!(|x: u64, k| ints::wrap(x << k, ints::U16)),
                    Op::ShlKU32 => imm!(|x: u64, k| ints::wrap(x << k, ints::U32)),
                    Op::ShlKU64 => imm!(|x: u64, k| x << k),
                    Op::ShrKS => imm!(|x: u64, k| ((x as i64) >> k) as u64),
                    Op::ShrKU => imm!(|x: u64, k| x >> k),
                    Op::DivKS => imm!(|x: u64, k: u64| ((x as i64) / (k as i64)) as u64),
                    Op::DivKU => imm!(|x: u64, k: u64| x / k),
                    Op::RemKS => imm!(|x: u64, k: u64| ((x as i64) % (k as i64)) as u64),
                    Op::RemKU => imm!(|x: u64, k: u64| x % k),
                    Op::EqK => imm!(|x, k| u64::from(x == k)),
                    Op::NeK => imm!(|x, k| u64::from(x != k)),
                    Op::LtSK => imm!(|x, k| u64::from((x as i64) < (k as i64))),
                    Op::LeSK => imm!(|x, k| u64::from((x as i64) <= (k as i64))),
                    Op::GtSK => imm!(|x, k| u64::from((x as i64) > (k as i64))),
                    Op::GeSK => imm!(|x, k| u64::from((x as i64) >= (k as i64))),
                    Op::LtUK => imm!(|x: u64, k: u64| u64::from(x < k)),
                    Op::LeUK => imm!(|x: u64, k: u64| u64::from(x <= k)),
                    Op::GtUK => imm!(|x: u64, k: u64| u64::from(x > k)),
                    Op::GeUK => imm!(|x: u64, k: u64| u64::from(x >= k)),
                    Op::And => bin!(|x, y| x & y),
                    Op::Or => bin!(|x, y| x | y),
                    Op::Xor => bin!(|x, y| x ^ y),
                    Op::Not => un!(|x| x ^ 1),

                    Op::Eq => cmp!(|x, y| x == y),
                    Op::Ne => cmp!(|x, y| x != y),
                    Op::LtS => cmp!(|x, y| (x as i64) < (y as i64)),
                    Op::LeS => cmp!(|x, y| (x as i64) <= (y as i64)),
                    Op::LtU => cmp!(|x: u64, y: u64| x < y),
                    Op::LeU => cmp!(|x: u64, y: u64| x <= y),
                    Op::EqF64 => cmp!(|x, y| f64::from_bits(x) == f64::from_bits(y)),
                    Op::NeF64 => cmp!(|x, y| f64::from_bits(x) != f64::from_bits(y)),
                    Op::LtF64 => cmp!(|x, y| f64::from_bits(x) < f64::from_bits(y)),
                    Op::LeF64 => cmp!(|x, y| f64::from_bits(x) <= f64::from_bits(y)),
                    Op::EqF32 => cmp!(|x, y| f32::from_bits(x as u32) == f32::from_bits(y as u32)),
                    Op::NeF32 => cmp!(|x, y| f32::from_bits(x as u32) != f32::from_bits(y as u32)),
                    Op::LtF32 => cmp!(|x, y| f32::from_bits(x as u32) < f32::from_bits(y as u32)),
                    Op::LeF32 => cmp!(|x, y| f32::from_bits(x as u32) <= f32::from_bits(y as u32)),

                    Op::JEq => branch!(|x, y| x == y),
                    Op::JNe => branch!(|x, y| x != y),
                    Op::JLtS => branch!(|x, y| (x as i64) < (y as i64)),
                    Op::JLeS => branch!(|x, y| (x as i64) <= (y as i64)),
                    Op::JLtU => branch!(|x: u64, y: u64| x < y),
                    Op::JLeU => branch!(|x: u64, y: u64| x <= y),
                    Op::JEqF64 => branch!(|x, y| f64::from_bits(x) == f64::from_bits(y)),
                    Op::JNeF64 => branch!(|x, y| f64::from_bits(x) != f64::from_bits(y)),
                    Op::JLtF64 => branch!(|x, y| f64::from_bits(x) < f64::from_bits(y)),
                    Op::JLeF64 => branch!(|x, y| f64::from_bits(x) <= f64::from_bits(y)),
                    Op::JNLtF64 => branch!(|x, y| not_below(f64::from_bits(x).partial_cmp(&f64::from_bits(y)), false)),
                    Op::JNLeF64 => branch!(|x, y| not_below(f64::from_bits(x).partial_cmp(&f64::from_bits(y)), true)),
                    Op::JEqF32 => branch!(|x, y| f32::from_bits(x as u32) == f32::from_bits(y as u32)),
                    Op::JNeF32 => branch!(|x, y| f32::from_bits(x as u32) != f32::from_bits(y as u32)),
                    Op::JLtF32 => branch!(|x, y| f32::from_bits(x as u32) < f32::from_bits(y as u32)),
                    Op::JLeF32 => branch!(|x, y| f32::from_bits(x as u32) <= f32::from_bits(y as u32)),
                    Op::JNLtF32 => {
                        branch!(|x, y| not_below(
                            f32::from_bits(x as u32).partial_cmp(&f32::from_bits(y as u32)),
                            false
                        ))
                    }
                    Op::JNLeF32 => {
                        branch!(|x, y| not_below(f32::from_bits(x as u32).partial_cmp(&f32::from_bits(y as u32)), true))
                    }

                    Op::JEqK => branch_k!(|x, k| x == k),
                    Op::JNeK => branch_k!(|x, k| x != k),
                    Op::JLtSK => branch_k!(|x, k| (x as i64) < (k as i64)),
                    Op::JLeSK => branch_k!(|x, k| (x as i64) <= (k as i64)),
                    Op::JGtSK => branch_k!(|x, k| (x as i64) > (k as i64)),
                    Op::JGeSK => branch_k!(|x, k| (x as i64) >= (k as i64)),
                    Op::JLtUK => branch_k!(|x: u64, k: u64| x < k),
                    Op::JLeUK => branch_k!(|x: u64, k: u64| x <= k),
                    Op::JGtUK => branch_k!(|x: u64, k: u64| x > k),
                    Op::JGeUK => branch_k!(|x: u64, k: u64| x >= k),

                    Op::AddJLtU => {
                        let x = r!(a(w)).wrapping_add(1);
                        set!(a(w), x);
                        if x < r!(b(w)) {
                            cjump!(c_i16(w));
                        }
                    }
                    Op::AddJLtS => {
                        let x = r!(a(w)).wrapping_add(1);
                        set!(a(w), x);
                        if (x as i64) < (r!(b(w)) as i64) {
                            cjump!(c_i16(w));
                        }
                    }
                    Op::AddJLtS32 => {
                        let x = ints::wrap(r!(a(w)).wrapping_add(1), ints::I32);
                        set!(a(w), x);
                        if (x as i64) < (r!(b(w)) as i64) {
                            cjump!(c_i16(w));
                        }
                    }
                    Op::AddJLtUK => {
                        let x = r!(a(w)).wrapping_add(1);
                        set!(a(w), x);
                        if x < b(w) as u16 as i16 as i64 as u64 {
                            cjump!(c_i16(w));
                        }
                    }
                    Op::AddJLtSK => {
                        let x = r!(a(w)).wrapping_add(1);
                        set!(a(w), x);
                        if (x as i64) < b(w) as u16 as i16 as i64 {
                            cjump!(c_i16(w));
                        }
                    }
                    Op::AddJLtSK32 => {
                        let x = ints::wrap(r!(a(w)).wrapping_add(1), ints::I32);
                        set!(a(w), x);
                        if (x as i64) < b(w) as u16 as i16 as i64 {
                            cjump!(c_i16(w));
                        }
                    }
                    Op::Jmp => jump!(bc_i32(w) as isize),
                    Op::Loop => {
                        tick!();
                        jump!(bc_i32(w) as isize);
                    }
                    Op::Jz => {
                        if r!(a(w)) == 0 {
                            cjump!(bc_i32(w));
                        }
                    }
                    Op::Jnz => {
                        if r!(a(w)) != 0 {
                            cjump!(bc_i32(w));
                        }
                    }

                    Op::AddF64 => f64s!(|x: f64, y: f64| x + y),
                    Op::SubF64 => f64s!(|x: f64, y: f64| x - y),
                    Op::MulF64 => f64s!(|x: f64, y: f64| x * y),
                    Op::DivF64 => f64s!(|x: f64, y: f64| x / y),
                    Op::RemF64 => f64s!(|x: f64, y: f64| x % y),
                    Op::NegF64 => un!(|x| (-f64::from_bits(x)).to_bits()),
                    Op::AddF32 => f32s!(|x: f32, y: f32| x + y),
                    Op::SubF32 => f32s!(|x: f32, y: f32| x - y),
                    Op::MulF32 => f32s!(|x: f32, y: f32| x * y),
                    Op::DivF32 => f32s!(|x: f32, y: f32| x / y),
                    Op::RemF32 => f32s!(|x: f32, y: f32| x % y),
                    Op::NegF32 => un!(|x| u64::from((-f32::from_bits(x as u32)).to_bits())),

                    Op::SqrtF64 => un!(|x| f64::from_bits(x).sqrt().to_bits()),
                    Op::SqrtF32 => un!(|x| u64::from(f32::from_bits(x as u32).sqrt().to_bits())),
                    Op::AbsF64 => un!(|x| f64::from_bits(x).abs().to_bits()),
                    Op::AbsF32 => un!(|x| u64::from(f32::from_bits(x as u32).abs().to_bits())),

                    Op::CastT => {
                        let (to, from) = unpack_cast(ext!()).expect("the verifier checked every cast");
                        match ints::cast_t(r!(b(w)), to, from) {
                            Ok(v) => set!(a(w), v),
                            Err(_) => throw!(Trap::IntRange(to)),
                        }
                    }
                    Op::CastW => {
                        let (to, _) = unpack_cast(ext!()).expect("the verifier checked every cast");
                        un!(|x| ints::cast_w(x, to))
                    }
                    Op::CastS => {
                        let (to, from) = unpack_cast(ext!()).expect("the verifier checked every cast");
                        un!(|x| ints::cast_s(x, to, from))
                    }
                    Op::ItofS64 => un!(|x| (x as i64 as f64).to_bits()),
                    Op::ItofU64 => un!(|x| (x as f64).to_bits()),
                    Op::ItofS32 => un!(|x| u64::from((x as i64 as f32).to_bits())),
                    Op::ItofU32 => un!(|x| u64::from((x as f32).to_bits())),
                    Op::FtoiT64 | Op::FtoiT32 => {
                        let to = ints::spec(ext!());
                        let x = r!(b(w));
                        let f = if op == Op::FtoiT64 { f64::from_bits(x) } else { f64::from(f32::from_bits(x as u32)) };
                        match ints::ftoi_t(f, to) {
                            Ok(v) => set!(a(w), v),
                            Err(_) => throw!(Trap::FloatRange(to)),
                        }
                    }
                    Op::FtoiS64 | Op::FtoiS32 => {
                        let to = ints::spec(ext!());
                        let x = r!(b(w));
                        let f = if op == Op::FtoiS64 { f64::from_bits(x) } else { f64::from(f32::from_bits(x as u32)) };
                        set!(a(w), ints::ftoi_s(f, to))
                    }
                    Op::F64toF32 => un!(|x| u64::from((f64::from_bits(x) as f32).to_bits())),
                    Op::F32toF64 => un!(|x| f64::from(f32::from_bits(x as u32)).to_bits()),
                    Op::Itoc => {
                        let from = ints::spec(ext!());
                        match ints::to_char(r!(b(w)), from) {
                            Ok(v) => set!(a(w), v),
                            Err(_) => throw!(Trap::BadChar),
                        }
                    }

                    Op::Call => {
                        // SAFETY: the verifier checked the function index.
                        let callee = unsafe { &*env.funcs.add(bc_u32(w) as usize) };
                        enter!(callee);
                    }
                    Op::CallN => {
                        let (native, regs) = env.natives[b(w)];
                        // SAFETY: the verifier checked that the native's
                        // registers are inside the frame.
                        let args = unsafe { std::slice::from_raw_parts_mut(fp.add(a(w)), regs) };
                        if cfg!(feature = "checked") {
                            env.ctx.stack = &*env.stack;
                        }
                        if let Err(trap) = native(env.ctx, args, c(w) as u16) {
                            throw!(trap);
                        }
                    }
                    Op::Ret => {
                        let (src, n) = (a(w), b(w));
                        // SAFETY: the results are inside this frame (verified),
                        // the linkage is just below it (the call put it there),
                        // and `dst` is the caller's window, which holds `n`
                        // results (verified at the call).
                        unsafe {
                            if src != 0 {
                                if n == 1 {
                                    *fp = *fp.add(src);
                                } else {
                                    for i in 0..n {
                                        *fp.add(i) = *fp.add(src + i);
                                    }
                                }
                            }
                            let link = fp.sub(LINK_SLOTS);
                            let back = *link as *const u64;
                            let caller = *link.add(1) as *mut u64;
                            let dst = *link.add(2) as *mut u64;
                            if dst != fp {
                                // This frame started a segment: the results go
                                // back to the caller's window in the one before.
                                env.seg_end = leave_segment(env.stack, fp, dst, n);
                            }
                            if back.is_null() {
                                // Back to the host: the entry's results are in
                                // its first registers.
                                return Ok(std::slice::from_raw_parts(fp, env.entry_results).to_vec());
                            }
                            pc = back;
                            fp = caller;
                        }
                    }

                    Op::Unravel => {
                        let (ptr, len) = (r!(a(w)), r!(b(w)));
                        // SAFETY: `unravel` is given a text value.
                        let text = match unsafe { env.ctx.text(ptr, len) } {
                            Ok(t) => String::from_utf8_lossy(t).into_owned(),
                            Err(trap) => throw!(trap),
                        };
                        throw!(Trap::Unravel(text));
                    }
                    Op::Crash => return Err(fault!(Trap::Crash(crash_message(a(w))), here!())),

                    Op::Ld8S => load!(i8, |v: i8| v as i64 as u64),
                    Op::Ld8U => load!(u8, u64::from),
                    Op::Ld16S => load!(i16, |v: i16| v as i64 as u64),
                    Op::Ld16U => load!(u16, u64::from),
                    Op::Ld32S => load!(i32, |v: i32| v as i64 as u64),
                    Op::Ld32U => load!(u32, u64::from),
                    Op::Ld64 => load!(u64, |v: u64| v),
                    Op::St8 => store!(u8),
                    Op::St16 => store!(u16),
                    Op::St32 => store!(u32),
                    Op::St64 => store!(u64),
                    Op::LdF8S => load_f!(i8, |v: i8| v as i64 as u64),
                    Op::LdF8U => load_f!(u8, u64::from),
                    Op::LdF16S => load_f!(i16, |v: i16| v as i64 as u64),
                    Op::LdF16U => load_f!(u16, u64::from),
                    Op::LdF32S => load_f!(i32, |v: i32| v as i64 as u64),
                    Op::LdF32U => load_f!(u32, u64::from),
                    Op::LdF64 => load_f!(u64, |v: u64| v),
                    Op::StF8 => store_f!(u8),
                    Op::StF16 => store_f!(u16),
                    Op::StF32 => store_f!(u32),
                    Op::StF64 => store_f!(u64),
                    Op::Lea => set!(a(w), (fp as u64).wrapping_add(u64::from(bc_u32(w)))),
                    Op::Gaddr => set!(a(w), env.data_range.0 + u64::from(bc_u32(w))),
                    Op::Idx => {
                        let scale = ext!();
                        let v = r!(b(w)).wrapping_add(r!(c(w)).wrapping_mul(scale));
                        set!(a(w), v)
                    }
                    Op::Elem => {
                        let ext = ext!();
                        let p = elem!(r!(b(w)), r!(c(w)), ext, 0, 0);
                        set!(a(w), p)
                    }
                    Op::ElemF => {
                        let ext = ext!();
                        let p = elem!((fp as u64).wrapping_add(ext >> 32), r!(b(w)), ext & 0xFFFF_FFFF, 0, 0);
                        set!(a(w), p)
                    }
                    Op::LdElF8S => ld_el!(frame, i8, |v: i8| v as i64 as u64),
                    Op::LdElF8U => ld_el!(frame, u8, u64::from),
                    Op::LdElF16S => ld_el!(frame, i16, |v: i16| v as i64 as u64),
                    Op::LdElF16U => ld_el!(frame, u16, u64::from),
                    Op::LdElF32S => ld_el!(frame, i32, |v: i32| v as i64 as u64),
                    Op::LdElF32U => ld_el!(frame, u32, u64::from),
                    Op::LdElF64 => ld_el!(frame, u64, |v: u64| v),
                    Op::StElF8 => st_el!(frame, u8),
                    Op::StElF16 => st_el!(frame, u16),
                    Op::StElF32 => st_el!(frame, u32),
                    Op::StElF64 => st_el!(frame, u64),
                    Op::LdEl8S => ld_el!(reg, i8, |v: i8| v as i64 as u64),
                    Op::LdEl8U => ld_el!(reg, u8, u64::from),
                    Op::LdEl16S => ld_el!(reg, i16, |v: i16| v as i64 as u64),
                    Op::LdEl16U => ld_el!(reg, u16, u64::from),
                    Op::LdEl32S => ld_el!(reg, i32, |v: i32| v as i64 as u64),
                    Op::LdEl32U => ld_el!(reg, u32, u64::from),
                    Op::LdEl64 => ld_el!(reg, u64, |v: u64| v),
                    Op::StEl8 => st_el!(reg, u8),
                    Op::StEl16 => st_el!(reg, u16),
                    Op::StEl32 => st_el!(reg, u32),
                    Op::StEl64 => st_el!(reg, u64),
                    Op::LdX8S => ld_x!(plain, i8, |v: i8| v as i64 as u64),
                    Op::LdX8U => ld_x!(plain, u8, u64::from),
                    Op::LdX16S => ld_x!(plain, i16, |v: i16| v as i64 as u64),
                    Op::LdX16U => ld_x!(plain, u16, u64::from),
                    Op::LdX32S => ld_x!(plain, i32, |v: i32| v as i64 as u64),
                    Op::LdX32U => ld_x!(plain, u32, u64::from),
                    Op::LdX64 => ld_x!(plain, u64, |v: u64| v),
                    Op::StX8 => st_x!(plain, u8),
                    Op::StX16 => st_x!(plain, u16),
                    Op::StX32 => st_x!(plain, u32),
                    Op::StX64 => st_x!(plain, u64),
                    Op::LdXC8S => ld_x!(checked, i8, |v: i8| v as i64 as u64),
                    Op::LdXC8U => ld_x!(checked, u8, u64::from),
                    Op::LdXC16S => ld_x!(checked, i16, |v: i16| v as i64 as u64),
                    Op::LdXC16U => ld_x!(checked, u16, u64::from),
                    Op::LdXC32S => ld_x!(checked, i32, |v: i32| v as i64 as u64),
                    Op::LdXC32U => ld_x!(checked, u32, u64::from),
                    Op::LdXC64 => ld_x!(checked, u64, |v: u64| v),
                    Op::StXC8 => st_x!(checked, u8),
                    Op::StXC16 => st_x!(checked, u16),
                    Op::StXC32 => st_x!(checked, u32),
                    Op::StXC64 => st_x!(checked, u64),
                    Op::FreeBufF => {
                        let ext = ext!();
                        free_buf!((fp as u64).wrapping_add(ext >> 32), ext & 0xFFFF_FFFF, 1u64 << a(w))
                    }
                    Op::FreeBuf => {
                        let ext = ext!();
                        free_buf!(r!(a(w)), ext, 1u64 << b(w))
                    }
                    Op::Copy => {
                        let n = ext!();
                        let (dst, src) = (r!(a(w)), r!(b(w)));
                        mem_check!(dst, n);
                        mem_check!(src, n);
                        // SAFETY: both are memory the program owns, `n` bytes
                        // each (checked in the checked build); they may overlap,
                        // so the words are all read before any is written.
                        unsafe {
                            let (d, s) = (dst as *mut u64, src as *const u64);
                            match n {
                                8 => d.write_unaligned(s.read_unaligned()),
                                16 => {
                                    let v = (s.read_unaligned(), s.add(1).read_unaligned());
                                    d.write_unaligned(v.0);
                                    d.add(1).write_unaligned(v.1);
                                }
                                24 => {
                                    let v = (s.read_unaligned(), s.add(1).read_unaligned(), s.add(2).read_unaligned());
                                    d.write_unaligned(v.0);
                                    d.add(1).write_unaligned(v.1);
                                    d.add(2).write_unaligned(v.2);
                                }
                                _ => std::ptr::copy(src as *const u8, dst as *mut u8, n as usize),
                            }
                        }
                    }
                    Op::Zero => {
                        let n = ext!();
                        let dst = r!(a(w));
                        mem_check!(dst, n);
                        // SAFETY: as for `copy`.
                        unsafe {
                            let d = dst as *mut u64;
                            match n {
                                8 => d.write_unaligned(0),
                                16 => {
                                    d.write_unaligned(0);
                                    d.add(1).write_unaligned(0);
                                }
                                24 => {
                                    d.write_unaligned(0);
                                    d.add(1).write_unaligned(0);
                                    d.add(2).write_unaligned(0);
                                }
                                _ => std::ptr::write_bytes(dst as *mut u8, 0, n as usize),
                            }
                        }
                    }
                    Op::Bounds => {
                        let (i, n) = (r!(a(w)), r!(b(w)));
                        if i >= n {
                            throw!(Trap::Index { index: i, len: n });
                        }
                    }
                    Op::BoundsK => {
                        let (i, n) = (r!(a(w)), u64::from(bc_i32(w) as u32));
                        if i >= n {
                            throw!(Trap::Index { index: i, len: n });
                        }
                    }
                    Op::Ldfn => set!(a(w), u64::from(bc_u32(w))),
                    Op::CallR => {
                        let index = r!(b(w)) as usize;
                        if index >= env.nfuncs {
                            return Err(fault!(no_function(index), here!()));
                        }
                        // SAFETY: just checked.
                        let callee = unsafe { &*env.funcs.add(index) };
                        if callee.params != c(w) >> 8 || callee.results != c(w) & 0xFF {
                            return Err(fault!(Trap::Crash(crash_message(2)), here!()));
                        }
                        enter!(callee);
                    }
                    Op::Resume => {
                        // This frame's pads are done: on to the caller's.
                        match unwind(env.image, env.code_base, env.stack, fp, here!(), true) {
                            Some((to, frame, end)) => {
                                pc = to;
                                fp = frame;
                                env.seg_end = end;
                            }
                            None => {
                                // Not in a closure: one that reads `pc` takes
                                // its address, and `pc` then lives in memory
                                // through the whole loop.
                                let at = here!();
                                return Err(match env.in_flight.take() {
                                    Some(f) => f,
                                    None => {
                                        fault!(Trap::Crash("loom internal: `resume` with nothing unwinding".into()), at)
                                    }
                                });
                            }
                        }
                    }
                }
                continue 'run;
            };

            // Unwinds with `trap`: to this function's landing pad for this
            // instruction, or out through its callers'. An internal error
            // doesn't unwind: nothing about the state can be trusted.
            let at = here!();
            if matches!(trap, Trap::Crash(_) | Trap::Failed(_)) {
                return Err(fault!(trap, at));
            }
            let (trace, skipped) = trace_at(env.image, env.code_base, fp, at);
            env.in_flight = Some(Fault { trap, pc: trace.first().copied().unwrap_or(at_index!(at)), trace, skipped });
            match unwind(env.image, env.code_base, env.stack, fp, at, false) {
                Some((to, frame, end)) => {
                    pc = to;
                    fp = frame;
                    env.seg_end = end;
                }
                None => return Err(env.in_flight.take().expect("set just above")),
            }
        }
    }
}

/// The start of the instruction the word before `pc` is part of: its
/// opcode word, or its extension word. Decoded from the start of its
/// function (verified, so every word there is where an instruction or its
/// extension is).
#[cold]
#[inline(never)]
fn instruction_at(image: &Image, code_base: *const u64, pc: *const u64) -> *const u64 {
    let word = (pc as usize - code_base as usize) / 8 - 1;
    let Some(f) = image.func_at(word as u32).map(|f| &image.funcs[f]) else { return pc };
    let mut i = f.entry as usize;
    loop {
        let words = Op::from_u16(image.code[i] as u16).map_or(1, |op| op.form().words());
        if word < i + words {
            // SAFETY: inside the code, at an instruction.
            return unsafe { code_base.add(i) };
        }
        i += words;
    }
}

/// A new segment for `callee`'s frame, the arguments in `window` copied to
/// its start: its frame pointer and end. Out of the dispatch loop's way.
///
/// # Safety
///
/// `window` holds `callee.params` registers.
#[cold]
#[inline(never)]
unsafe fn enter_segment(stack: &mut Stack, window: *mut u64, callee: &Callee) -> Result<(*mut u64, *mut u64), Trap> {
    let (start, end) = stack.grow(LINK_SLOTS + callee.frame)?;
    // SAFETY: the new segment holds the linkage and the frame; the arguments
    // are in the caller's frame, in another allocation.
    unsafe {
        let fp = start.add(LINK_SLOTS);
        std::ptr::copy_nonoverlapping(window, fp, callee.params);
        Ok((fp, end))
    }
}

/// Returns from a frame that started a segment: its `n` results go to the
/// caller's window at `dst`, in the segment before. The end of that one.
///
/// # Safety
///
/// `fp` and `dst` each hold `n` registers.
#[cold]
#[inline(never)]
unsafe fn leave_segment(stack: &mut Stack, fp: *mut u64, dst: *mut u64, n: usize) -> *mut u64 {
    // SAFETY: as the caller promises; they're in different segments.
    unsafe { std::ptr::copy_nonoverlapping(fp, dst, n) };
    stack.shrink()
}

#[cold]
#[inline(never)]
fn no_function(index: usize) -> Trap {
    Trap::Crash(format!("loom internal: no function {index}"))
}

/// Where an unwind at `at` in the frame at `fp` goes: the landing pad for
/// it, first in this function and then out through its callers, each at
/// the call it's in. With `pop`, this function's pads already ran. `None`
/// when no function has one: the unwind reaches the host.
fn unwind(
    image: &Image,
    code_base: *const u64,
    stack: &mut Stack,
    mut fp: *mut u64,
    at: *const u64,
    mut pop: bool,
) -> Option<(*const u64, *mut u64, *mut u64)> {
    let mut index = (at as usize - code_base as usize) / 8;
    loop {
        if !pop {
            let f = &image.funcs[image.func_at(index as u32)?];
            if let Some(pad) = f.pad_at(index as u32 - f.entry) {
                // SAFETY: the verifier checked that the pad is an
                // instruction of this function.
                let to = unsafe { code_base.add((f.entry + pad) as usize) };
                return Some((to, fp, stack.bounds().1));
            }
        }
        pop = false;
        // SAFETY: every frame has its linkage just below it (see `call`).
        let (back, caller, dst) = unsafe {
            let link = fp.sub(LINK_SLOTS);
            (*link as *const u64, *link.add(1) as *mut u64, *link.add(2) as *mut u64)
        };
        if dst != fp {
            stack.shrink();
        }
        if back.is_null() {
            return None;
        }
        // The call is the word before where it returns to.
        index = (back as usize - code_base as usize) / 8 - 1;
        fp = caller;
    }
}

/// Where a trap at `at`, in the frame at `fp`, happened, frame by frame,
/// innermost first: the instruction, then each call it's inside. Code with
/// no locations (std's methods the compiler writes, glue) is left out, so a
/// trap there is reported at the call into it, as Lua's `error(message,
/// level)` reports at a caller. (Empty when nothing has a location.)
///
/// Only the first and the last [`TRACE_ENDS`] frames are kept, and how many
/// are left out between them: a stack overflow is millions of frames deep.
fn trace_at(image: &Image, code_base: *const u64, mut fp: *mut u64, at: *const u64) -> (Vec<usize>, usize) {
    let mut index = (at as usize - code_base as usize) / 8;
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut skipped = 0;
    let finish = |head: Vec<usize>, tail: std::collections::VecDeque<usize>, skipped| {
        let mut out = head;
        out.extend(tail);
        (out, skipped)
    };
    loop {
        if image.line_at(index as u32).is_some() {
            if head.len() < TRACE_ENDS {
                head.push(index);
            } else {
                if tail.len() == TRACE_ENDS {
                    tail.pop_front();
                    skipped += 1;
                }
                tail.push_back(index);
            }
        }
        // SAFETY: every frame has its linkage just below it (see `call`).
        let (back, caller) = unsafe {
            let link = fp.sub(LINK_SLOTS);
            (*link as *const u64, *link.add(1) as *mut u64)
        };
        if back.is_null() {
            return finish(head, tail, skipped);
        }
        // The call is the word before where it returns to.
        index = (back as usize - code_base as usize) / 8 - 1;
        fp = caller;
    }
}

/// Whether `len` bytes at `addr` are all memory the program may touch: a
/// live object, the stack in use, the image's text or the program's data.
fn valid(heap: &Heap, stack: &Stack, rodata: (u64, u64), data: (u64, u64), addr: u64, len: u64) -> bool {
    let Some(end) = addr.checked_add(len) else { return false };
    let inside = |(start, stop): (u64, u64)| addr >= start && end <= stop;
    len == 0 || heap.holds(addr, len) || stack.holds(addr, len) || inside(rodata) || inside(data)
}
