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

#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use loom_isa::word::{a, b, bc_i32, bc_u32, c, c_i16, unpack_cast};
use loom_isa::{Image, LINK_SLOTS, Op};
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
    code: &'i [u64],
    funcs: Vec<Callee>,
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
            code: &image.code,
            funcs,
            natives,
            ctx: Ctx::new(image, host.output),
            stack_limit: config.stack_limit,
            interrupt: config.interrupt.clone(),
            stack: None,
        })
    }

    /// Calls function `entry` with `args` (one per parameter register) and
    /// gives its results.
    pub fn call(&mut self, entry: usize, args: &[u64]) -> Result<Vec<u64>, Fault> {
        let Some(start) = self.funcs.get(entry) else {
            return Err(Fault { trap: Trap::Crash(format!("there's no function {entry}")), pc: 0 });
        };
        if args.len() != start.params {
            let trap = Trap::Crash(format!("{} arguments for {} parameters", args.len(), start.params));
            return Err(Fault { trap, pc: (start.entry as usize - self.code.as_ptr() as usize) / 8 });
        }
        let need = LINK_SLOTS + start.frame;
        let mut stack = match self.stack.take() {
            Some(mut stack) if stack.first_holds(need) => {
                stack.reset();
                stack
            }
            _ => Stack::new(self.stack_limit, need).map_err(|trap| Fault { trap, pc: 0 })?,
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
        let natives = &self.natives;
        let ctx = &mut self.ctx;
        let interrupt = self.interrupt.clone();
        let fault = |trap: Trap, at: *const u64| Fault { trap, pc: at_index(at) };

        loop {
            // SAFETY: `pc` is always at an instruction of a verified
            // function: every jump, call and return lands on one, and no
            // function runs off its end.
            let w = unsafe { *pc };
            let at = pc;
            pc = unsafe { pc.add(1) };
            let op = if cfg!(feature = "checked") {
                Op::from_u16(w as u16).expect("the verifier checked every opcode")
            } else {
                // SAFETY: the verifier checked that every opcode is an `Op`
                // discriminant, and `Op` is `repr(u16)`.
                unsafe { std::mem::transmute::<u16, Op>(w as u16) }
            };

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
                            return Err(fault(Trap::Interrupt, at));
                        }
                    }
                }};
            }
            macro_rules! int_trap {
                ($e:expr) => {
                    return Err(fault(
                        match $e {
                            IntErr::Overflow | IntErr::Range => Trap::Overflow,
                            IntErr::DivZero => Trap::DivZero,
                            IntErr::ShiftRange => Trap::ShiftRange,
                        },
                        at,
                    ))
                };
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
                    bin!(|x: u64, y: u64| u64::from($f(f32::from_bits(x as u32), f32::from_bits(y as u32)).to_bits()))
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
                    branch!(|x, y| not_below(f32::from_bits(x as u32).partial_cmp(&f32::from_bits(y as u32)), false))
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
                        Err(_) => return Err(fault(Trap::IntRange(to), at)),
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
                        Err(_) => return Err(fault(Trap::FloatRange(to), at)),
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
                        Err(_) => return Err(fault(Trap::BadChar, at)),
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
                            Err(trap) => return Err(fault(trap, at)),
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
                    if let Err(trap) = native(ctx, args, c(w) as u16) {
                        return Err(fault(trap, at));
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
                        Err(trap) => return Err(fault(trap, at)),
                    };
                    return Err(fault(Trap::Unravel(text), at));
                }
                Op::Crash => return Err(fault(Trap::Crash(crash_message(a(w))), at)),
            }
        }
    }
}
