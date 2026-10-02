//! The interpreter loop (design/vm.md §9.1).
//!
//! The state that matters lives in locals so it stays in machine registers:
//! `pc` (the next instruction), `fp` (the current frame's first register),
//! `seg_end` (where the current stack segment ends) and `ticks` (loop
//! iterations and calls until the next check for Ctrl-C). Just below a
//! frame's first register, [`LINK_SLOTS`] slots of its caller's frame say
//! where to return: the return address, the caller's frame, and where the
//! results go (the frame itself, unless the frame started a new stack
//! segment and the caller's window is in the one before).
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use loom_isa::word::{a, b, bc_i32, bc_u32, c, c_i16, unpack_cast};
use loom_isa::{Image, LINK_SLOTS, Op};

use crate::heap::Heap;
use loom_sys::Host;

use crate::ints::{self, IntErr};
use crate::natives::{Ctx, NativeFn, resolve};
use crate::stack::Stack;
use crate::{Config, Fault, Trap, crash_message};

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
    interrupt: Option<Arc<AtomicBool>>,
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
            ctx: Ctx::new(image, host),
            stack_limit: config.stack_limit,
            interrupt: config.interrupt.clone(),
            stack: None,
        })
    }

    /// Calls function `entry` with `args` (one per parameter register) and
    /// gives its results.
    pub fn call(&mut self, entry: usize, args: &[u64]) -> Result<Vec<u64>, Fault> {
        let Some(start) = self.funcs.get(entry) else {
            return Err(Fault { trap: Trap::Crash(format!("there's no function {entry}")), pc: 0, trace: Vec::new() });
        };
        if args.len() != start.params {
            let trap = Trap::Crash(format!("{} arguments for {} parameters", args.len(), start.params));
            return Err(Fault {
                trap,
                pc: (start.entry as usize - self.code.as_ptr() as usize) / 8,
                trace: Vec::new(),
            });
        }
        let need = LINK_SLOTS + start.frame;
        let mut stack = match self.stack.take() {
            Some(mut stack) if stack.first_holds(need) => {
                stack.reset();
                stack
            }
            _ => Stack::new(self.stack_limit, need).map_err(|trap| Fault { trap, pc: 0, trace: Vec::new() })?,
        };
        let result = self.execute(&mut stack, entry, args);
        self.stack = Some(stack);
        result
    }

    fn execute(&mut self, stack: &mut Stack, entry: usize, args: &[u64]) -> Result<Vec<u64>, Fault> {
        let code_base = self.code.as_ptr();
        let at_index = |at: *const u64| (at as usize - code_base as usize) / 8;
        let start = &self.funcs[entry];
        let (base, mut seg_end) = stack.bounds();
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
        let entry_results = start.results;
        let mut pc = start.entry;
        let mut ticks = TICKS;
        let funcs = self.funcs.as_ptr();
        let nfuncs = self.funcs.len();
        let natives = &self.natives;
        let ctx = &mut self.ctx;
        let interrupt = self.interrupt.clone();
        let image = self.image;
        let data_range = {
            let r = self.data.as_mut_ptr_range();
            (r.start as u64, r.end as u64)
        };
        let fault = |trap: Trap, at: *const u64| Fault { trap, pc: at_index(at), trace: Vec::new() };
        // What's unwinding the stack, while landing pads run.
        let mut in_flight: Option<Fault> = None;

        'run: loop {
            // SAFETY: `pc` is always at an instruction of a verified
            // function: every jump, call and return lands on one, and no
            // function runs off its end.
            let w = unsafe { *pc };
            let at = pc;
            pc = unsafe { pc.add(1) };
            if cfg!(feature = "stats") {
                ctx.dispatched += 1;
            }
            let op = if cfg!(feature = "checked") {
                Op::from_u16(w as u16).expect("the verifier checked every opcode")
            } else {
                // SAFETY: the verifier checked that every opcode is an `Op`
                // discriminant, and `Op` is `repr(u16)`.
                unsafe { std::mem::transmute::<u16, Op>(w as u16) }
            };

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
                        debug_assert!(unsafe { fp.add(i) } < seg_end);
                        // SAFETY: see above.
                        unsafe { *fp.add(i) }
                    }};
                }
                macro_rules! set {
                    ($i:expr, $v:expr) => {{
                        let (i, v) = ($i, $v);
                        // SAFETY: see above.
                        debug_assert!(unsafe { fp.add(i) } < seg_end);
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
                        pc = unsafe { at.offset($off) };
                    }};
                }
                macro_rules! tick {
                    () => {{
                        ticks -= 1;
                        if ticks < 0 {
                            ticks = TICKS;
                            if interrupt.as_ref().is_some_and(|i| i.load(Ordering::Relaxed)) {
                                throw!(Trap::Interrupt);
                            }
                        }
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
                            if !valid(&ctx.heap, stack, ctx.rodata_range(), data_range, addr, len) {
                                return Err(fault(
                                    Trap::Crash(format!(
                                        "loom internal: a bad memory access: {len} bytes at {addr:#x}"
                                    )),
                                    at,
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
                            jump!(c_i16(w));
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
                macro_rules! branch {
                    ($f:expr) => {{
                        if $f(r!(a(w)), r!(b(w))) {
                            jump!(c_i16(w));
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
                    Op::Ldstr => set!(a(w), ctx.string_ptr(bc_u32(w) as usize)),
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

                    Op::Jmp => jump!(bc_i32(w) as isize),
                    Op::Loop => {
                        jump!(bc_i32(w) as isize);
                        tick!();
                    }
                    Op::Jz => {
                        if r!(a(w)) == 0 {
                            jump!(bc_i32(w) as isize);
                        }
                    }
                    Op::Jnz => {
                        if r!(a(w)) != 0 {
                            jump!(bc_i32(w) as isize);
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
                        let callee = unsafe { &*funcs.add(bc_u32(w) as usize) };
                        // SAFETY: the window is inside this frame (verified).
                        let window = unsafe { fp.add(a(w)) };
                        let mut new_fp = window;
                        // The callee's registers must fit the segment. (Its
                        // linkage goes in this frame, just below the window.)
                        if (seg_end as usize - window as usize) / 8 < callee.frame {
                            match stack.grow(LINK_SLOTS + callee.frame) {
                                Ok((start, end)) => {
                                    // SAFETY: the new segment holds the linkage
                                    // and the frame; the arguments are in this
                                    // frame, in another allocation.
                                    unsafe {
                                        new_fp = start.add(LINK_SLOTS);
                                        std::ptr::copy_nonoverlapping(window, new_fp, callee.params);
                                    }
                                    seg_end = end;
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
                        tick!();
                    }
                    Op::CallN => {
                        let (native, regs) = natives[b(w)];
                        // SAFETY: the verifier checked that the native's
                        // registers are inside the frame.
                        let args = unsafe { std::slice::from_raw_parts_mut(fp.add(a(w)), regs) };
                        if cfg!(feature = "checked") {
                            ctx.stack = &*stack;
                        }
                        if let Err(trap) = native(ctx, args, c(w) as u16) {
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
                                for i in 0..n {
                                    *fp.add(i) = *fp.add(src + i);
                                }
                            }
                            let link = fp.sub(LINK_SLOTS);
                            let back = *link as *const u64;
                            let caller = *link.add(1) as *mut u64;
                            let dst = *link.add(2) as *mut u64;
                            if dst != fp {
                                // This frame started a segment: the results go
                                // back to the caller's window in the one before.
                                std::ptr::copy_nonoverlapping(fp, dst, n);
                                seg_end = stack.shrink();
                            }
                            if back.is_null() {
                                // Back to the host: the entry's results are in
                                // its first registers.
                                return Ok(std::slice::from_raw_parts(fp, entry_results).to_vec());
                            }
                            pc = back;
                            fp = caller;
                        }
                    }

                    Op::Unravel => {
                        let (ptr, len) = (r!(a(w)), r!(b(w)));
                        // SAFETY: `unravel` is given a text value.
                        let text = match unsafe { ctx.text(ptr, len) } {
                            Ok(t) => String::from_utf8_lossy(t).into_owned(),
                            Err(trap) => throw!(trap),
                        };
                        throw!(Trap::Unravel(text));
                    }
                    Op::Crash => return Err(fault(Trap::Crash(crash_message(a(w))), at)),

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
                    Op::Gaddr => set!(a(w), data_range.0 + u64::from(bc_u32(w))),
                    Op::Idx => {
                        let scale = ext!();
                        let v = r!(b(w)).wrapping_add(r!(c(w)).wrapping_mul(scale));
                        set!(a(w), v)
                    }
                    Op::Elem | Op::ElemF => {
                        let ext = ext!();
                        let (header, index, scale) = if op == Op::Elem {
                            (r!(b(w)), r!(c(w)), ext)
                        } else {
                            ((fp as u64).wrapping_add(ext >> 32), r!(b(w)), ext & 0xFFFF_FFFF)
                        };
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
                        set!(a(w), buf.wrapping_add(index.wrapping_mul(scale)))
                    }
                    Op::Copy => {
                        let n = ext!();
                        let (dst, src) = (r!(a(w)), r!(b(w)));
                        mem_check!(dst, n);
                        mem_check!(src, n);
                        // SAFETY: both are memory the program owns, `n` bytes
                        // each (checked in the checked build); they may overlap.
                        unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, n as usize) }
                    }
                    Op::Zero => {
                        let n = ext!();
                        let dst = r!(a(w));
                        mem_check!(dst, n);
                        // SAFETY: as for `copy`.
                        unsafe { std::ptr::write_bytes(dst as *mut u8, 0, n as usize) }
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
                        if index >= nfuncs {
                            return Err(fault(Trap::Crash(format!("loom internal: no function {index}")), at));
                        }
                        // SAFETY: just checked.
                        let callee = unsafe { &*funcs.add(index) };
                        if callee.params != c(w) >> 8 || callee.results != c(w) & 0xFF {
                            return Err(fault(Trap::Crash(crash_message(2)), at));
                        }
                        // SAFETY: the window is inside this frame (verified).
                        let window = unsafe { fp.add(a(w)) };
                        let mut new_fp = window;
                        if (seg_end as usize - window as usize) / 8 < callee.frame {
                            match stack.grow(LINK_SLOTS + callee.frame) {
                                Ok((start, end)) => {
                                    // SAFETY: as for `call`.
                                    unsafe {
                                        new_fp = start.add(LINK_SLOTS);
                                        std::ptr::copy_nonoverlapping(window, new_fp, callee.params);
                                    }
                                    seg_end = end;
                                }
                                Err(trap) => throw!(trap),
                            }
                        }
                        // SAFETY: as for `call`.
                        unsafe {
                            let link = new_fp.sub(LINK_SLOTS);
                            *link = pc as u64;
                            *link.add(1) = fp as u64;
                            *link.add(2) = window as u64;
                        }
                        fp = new_fp;
                        pc = callee.entry;
                        tick!();
                    }
                    Op::Resume => {
                        // This frame's pads are done: on to the caller's.
                        match unwind(image, code_base, stack, fp, at, true) {
                            Some((to, frame, end)) => {
                                pc = to;
                                fp = frame;
                                seg_end = end;
                            }
                            None => {
                                let f = in_flight.take();
                                return Err(f.unwrap_or_else(|| {
                                    fault(Trap::Crash("loom internal: `resume` with nothing unwinding".into()), at)
                                }));
                            }
                        }
                    }
                }
                continue 'run;
            };

            // Unwinds with `trap`: to this function's landing pad for this
            // instruction, or out through its callers'. An internal error
            // doesn't unwind: nothing about the state can be trusted.
            if matches!(trap, Trap::Crash(_) | Trap::Failed(_)) {
                return Err(fault(trap, at));
            }
            let trace = trace_at(image, code_base, fp, at);
            in_flight = Some(Fault { trap, pc: trace.first().copied().unwrap_or(at_index(at)), trace });
            match unwind(image, code_base, stack, fp, at, false) {
                Some((to, frame, end)) => {
                    pc = to;
                    fp = frame;
                    seg_end = end;
                }
                None => return Err(in_flight.take().expect("set just above")),
            }
        }
    }
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
fn trace_at(image: &Image, code_base: *const u64, mut fp: *mut u64, at: *const u64) -> Vec<usize> {
    let mut index = (at as usize - code_base as usize) / 8;
    let mut out = Vec::new();
    loop {
        if image.line_at(index as u32).is_some() {
            out.push(index);
        }
        // SAFETY: every frame has its linkage just below it (see `call`).
        let (back, caller) = unsafe {
            let link = fp.sub(LINK_SLOTS);
            (*link as *const u64, *link.add(1) as *mut u64)
        };
        if back.is_null() {
            return out;
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
