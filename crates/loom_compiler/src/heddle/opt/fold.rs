//! Working out an instruction whose operands are constants, as Treadle
//! would: integers through the oracle (`ty::int`), so a fold can't disagree
//! with run time, and floats with the same IEEE operations. An instruction
//! that would trap isn't folded: it stays, and traps when it runs.
//!
//! Values are 64-bit patterns, as in registers: an integer normalized to its
//! type (sign-extended or zero-extended), a `bool` 0 or 1, an `f32`'s bits
//! in the low 32.

use crate::heddle::{BitOp, Cmp, FOp, InstKind, IntOp, Policy};
use crate::ty::{FloatTy, IntTy};

/// The value of `bits`, a register of type `t`.
fn value(t: IntTy, bits: u64) -> i128 {
    if t.signed { bits as i64 as i128 } else { i128::from(bits) }
}

/// The register holding `v`, a value of type `t`.
fn reg(v: i128) -> u64 {
    v as u64
}

/// `v` wrapped into `t`'s range.
fn wrapped(t: IntTy, v: i128) -> u64 {
    reg(t.wrap(v))
}

fn f64_(bits: u64) -> f64 {
    f64::from_bits(bits)
}

fn f32_(bits: u64) -> f32 {
    f32::from_bits(bits as u32)
}

/// What `k` gives when its operands are the constants `c` gives for them;
/// `None` when an operand isn't constant, or the instruction would trap, or
/// isn't one this works out.
pub fn fold(k: &InstKind, c: impl Fn(crate::heddle::Value) -> Option<u64>) -> Option<u64> {
    use InstKind as K;
    match k {
        K::Iconst(bits) | K::Fconst(bits) => Some(*bits),
        K::Arith { op, policy, ty, a, b } => {
            let (x, y) = (value(*ty, c(*a)?), value(*ty, c(*b)?));
            let exact = match op {
                IntOp::Add => x.checked_add(y)?,
                IntOp::Sub => x.checked_sub(y)?,
                IntOp::Mul => x.checked_mul(y)?,
            };
            match policy {
                Policy::Trap => ty.checked(exact).map(reg),
                Policy::Wrap => Some(wrapped(*ty, exact)),
                Policy::Sat => Some(reg(ty.saturate(exact))),
            }
        }
        K::Div { ty, a, b } => {
            let (x, y) = (value(*ty, c(*a)?), value(*ty, c(*b)?));
            if y == 0 {
                return None;
            }
            // Truncating toward zero; `MIN / -1` overflows.
            ty.checked(x / y).map(reg)
        }
        K::Rem { ty, a, b } => {
            let (x, y) = (value(*ty, c(*a)?), value(*ty, c(*b)?));
            (y != 0).then(|| reg(x % y))
        }
        K::Shl { ty, a, b } => {
            let (x, n) = (c(*a)?, c(*b)?);
            (n < u64::from(ty.bits)).then(|| wrapped(*ty, i128::from(x << n)))
        }
        K::Shr { ty, a, b } => {
            let (x, n) = (c(*a)?, c(*b)?);
            (n < u64::from(ty.bits)).then(|| if ty.signed { ((x as i64) >> n) as u64 } else { x >> n })
        }
        K::Bit { op, a, b } => {
            let (x, y) = (c(*a)?, c(*b)?);
            Some(match op {
                BitOp::And => x & y,
                BitOp::Or => x | y,
                BitOp::Xor => x ^ y,
            })
        }
        K::Neg { ty, a } => ty.checked(-value(*ty, c(*a)?)).map(reg),
        K::BitNot { ty, a } => Some(wrapped(*ty, !value(*ty, c(*a)?))),
        K::Not(a) => Some(u64::from(c(*a)? == 0)),
        K::ICmp { cmp, unsigned, a, b } => {
            let (x, y) = (c(*a)?, c(*b)?);
            let order = if *unsigned { x.cmp(&y) } else { (x as i64).cmp(&(y as i64)) };
            Some(u64::from(holds(*cmp, order)))
        }
        K::FArith { op, ty, a, b } => {
            let (x, y) = (c(*a)?, c(*b)?);
            Some(match ty {
                FloatTy::F64 => {
                    let (x, y) = (f64_(x), f64_(y));
                    match op {
                        FOp::Add => x + y,
                        FOp::Sub => x - y,
                        FOp::Mul => x * y,
                        FOp::Div => x / y,
                        FOp::Rem => x % y,
                    }
                    .to_bits()
                }
                FloatTy::F32 => {
                    let (x, y) = (f32_(x), f32_(y));
                    u64::from(
                        match op {
                            FOp::Add => x + y,
                            FOp::Sub => x - y,
                            FOp::Mul => x * y,
                            FOp::Div => x / y,
                            FOp::Rem => x % y,
                        }
                        .to_bits(),
                    )
                }
            })
        }
        K::FNeg { ty, a } => {
            let x = c(*a)?;
            Some(match ty {
                FloatTy::F64 => (-f64_(x)).to_bits(),
                FloatTy::F32 => u64::from((-f32_(x)).to_bits()),
            })
        }
        K::FCmp { cmp, ty, a, b } => {
            let (x, y) = (c(*a)?, c(*b)?);
            let order = match ty {
                FloatTy::F64 => f64_(x).partial_cmp(&f64_(y)),
                FloatTy::F32 => f32_(x).partial_cmp(&f32_(y)),
            };
            // NaN: only `!=` holds.
            Some(u64::from(match order {
                Some(o) => holds(*cmp, o),
                None => *cmp == Cmp::Ne,
            }))
        }
        K::IntCast { policy, to, from, a } => {
            let v = value(*from, c(*a)?);
            match policy {
                Policy::Trap => to.checked(v).map(reg),
                Policy::Wrap => Some(wrapped(*to, v)),
                Policy::Sat => Some(reg(to.saturate(v))),
            }
        }
        K::IntToFloat { from, to, a } => {
            let x = c(*a)?;
            // As Treadle does it: a `u64` from its bits, anything else as
            // the `i64` its register holds.
            let unsigned = !from.signed && from.bits == 64;
            Some(match (to, unsigned) {
                (FloatTy::F64, false) => (x as i64 as f64).to_bits(),
                (FloatTy::F64, true) => (x as f64).to_bits(),
                (FloatTy::F32, false) => u64::from((x as i64 as f32).to_bits()),
                (FloatTy::F32, true) => u64::from((x as f32).to_bits()),
            })
        }
        K::FloatCast { to, a } => {
            let x = c(*a)?;
            Some(match to {
                FloatTy::F32 => u64::from((f64_(x) as f32).to_bits()),
                FloatTy::F64 => f64::from(f32_(x)).to_bits(),
            })
        }
        _ => None,
    }
}

fn holds(cmp: Cmp, order: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match cmp {
        Cmp::Eq => order == Equal,
        Cmp::Ne => order != Equal,
        Cmp::Lt => order == Less,
        Cmp::Le => order != Greater,
        Cmp::Gt => order == Greater,
        Cmp::Ge => order != Less,
    }
}
