//! Integer semantics, on registers.
//!
//! A register holds an integer normalized to its type: sign-extended from
//! its width for signed types, zero-extended for unsigned ones. Every
//! function here takes and returns normalized values, and agrees with the
//! compiler's oracle (`ty::int`): trapping ops fail when the exact result
//! doesn't fit, `%` ops wrap, `|` ops saturate.
//!
//! They're `#[inline(always)]`: the interpreter calls them with constant
//! [`IntSpec`]s for the common widths, and the width checks fold away.

use loom_isa::IntSpec;

/// Why an integer operation gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntErr {
    Overflow,
    DivZero,
    ShiftRange,
    /// A cast's value doesn't fit the target.
    Range,
}

pub type IntResult = Result<u64, IntErr>;

pub const I8: IntSpec = IntSpec::new(8, true);
pub const I16: IntSpec = IntSpec::new(16, true);
pub const I32: IntSpec = IntSpec::new(32, true);
pub const I64: IntSpec = IntSpec::new(64, true);
pub const U8: IntSpec = IntSpec::new(8, false);
pub const U16: IntSpec = IntSpec::new(16, false);
pub const U32: IntSpec = IntSpec::new(32, false);
pub const U64: IntSpec = IntSpec::new(64, false);

/// An extension word's [`IntSpec`], which the verifier has checked.
#[inline(always)]
pub fn spec(ext: u64) -> IntSpec {
    IntSpec { bits: ext as u8, signed: ext & 0x100 != 0, size: ext & 0x200 != 0 }
}

#[inline(always)]
fn min_s(bits: u8) -> i64 {
    if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) }
}

#[inline(always)]
fn max_s(bits: u8) -> i64 {
    if bits >= 64 { i64::MAX } else { (1i64 << (bits - 1)) - 1 }
}

#[inline(always)]
fn max_u(bits: u8) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

#[inline(always)]
fn fits_s(r: i64, bits: u8) -> bool {
    bits >= 64 || {
        let sh = 64 - u32::from(bits);
        (r << sh) >> sh == r
    }
}

#[inline(always)]
fn fits_u(r: u64, bits: u8) -> bool {
    bits >= 64 || r >> bits == 0
}

/// Any 64-bit pattern, wrapped into the type: its low `bits` bits,
/// normalized.
#[inline(always)]
pub fn wrap(r: u64, s: IntSpec) -> u64 {
    if s.bits >= 64 {
        r
    } else if s.signed {
        let sh = 64 - u32::from(s.bits);
        (((r << sh) as i64) >> sh) as u64
    } else {
        r & max_u(s.bits)
    }
}

#[inline(always)]
fn check_s(r: Option<i64>, s: IntSpec) -> IntResult {
    match r {
        Some(r) if fits_s(r, s.bits) => Ok(r as u64),
        _ => Err(IntErr::Overflow),
    }
}

#[inline(always)]
fn check_u(r: Option<u64>, s: IntSpec) -> IntResult {
    match r {
        Some(r) if fits_u(r, s.bits) => Ok(r),
        _ => Err(IntErr::Overflow),
    }
}

#[inline(always)]
fn clamp_s(r: i64, s: IntSpec) -> u64 {
    r.clamp(min_s(s.bits), max_s(s.bits)) as u64
}

#[inline(always)]
pub fn add_t(x: u64, y: u64, s: IntSpec) -> IntResult {
    if s.signed { check_s((x as i64).checked_add(y as i64), s) } else { check_u(x.checked_add(y), s) }
}

#[inline(always)]
pub fn sub_t(x: u64, y: u64, s: IntSpec) -> IntResult {
    if s.signed { check_s((x as i64).checked_sub(y as i64), s) } else { check_u(x.checked_sub(y), s) }
}

#[inline(always)]
pub fn mul_t(x: u64, y: u64, s: IntSpec) -> IntResult {
    if s.signed { check_s((x as i64).checked_mul(y as i64), s) } else { check_u(x.checked_mul(y), s) }
}

#[inline(always)]
pub fn add_w(x: u64, y: u64, s: IntSpec) -> u64 {
    wrap(x.wrapping_add(y), s)
}

#[inline(always)]
pub fn sub_w(x: u64, y: u64, s: IntSpec) -> u64 {
    wrap(x.wrapping_sub(y), s)
}

#[inline(always)]
pub fn mul_w(x: u64, y: u64, s: IntSpec) -> u64 {
    wrap(x.wrapping_mul(y), s)
}

#[inline(always)]
pub fn add_s(x: u64, y: u64, s: IntSpec) -> u64 {
    if s.signed { clamp_s((x as i64).saturating_add(y as i64), s) } else { x.saturating_add(y).min(max_u(s.bits)) }
}

#[inline(always)]
pub fn sub_s(x: u64, y: u64, s: IntSpec) -> u64 {
    if s.signed { clamp_s((x as i64).saturating_sub(y as i64), s) } else { x.saturating_sub(y) }
}

#[inline(always)]
pub fn mul_s(x: u64, y: u64, s: IntSpec) -> u64 {
    if s.signed { clamp_s((x as i64).saturating_mul(y as i64), s) } else { x.saturating_mul(y).min(max_u(s.bits)) }
}

/// Division, truncating toward zero.
#[inline(always)]
pub fn div(x: u64, y: u64, s: IntSpec) -> IntResult {
    if y == 0 {
        return Err(IntErr::DivZero);
    }
    if s.signed {
        let (x, y) = (x as i64, y as i64);
        if y == -1 && x == min_s(s.bits) {
            return Err(IntErr::Overflow);
        }
        Ok((x / y) as u64)
    } else {
        Ok(x / y)
    }
}

/// The remainder of truncating division: it has the sign of `x`.
#[inline(always)]
pub fn rem_s(x: u64, y: u64) -> IntResult {
    if y == 0 {
        return Err(IntErr::DivZero);
    }
    Ok((x as i64).wrapping_rem(y as i64) as u64)
}

#[inline(always)]
pub fn rem_u(x: u64, y: u64) -> IntResult {
    if y == 0 {
        return Err(IntErr::DivZero);
    }
    Ok(x % y)
}

#[inline(always)]
pub fn neg(x: u64, s: IntSpec) -> IntResult {
    if x as i64 == min_s(s.bits) { Err(IntErr::Overflow) } else { Ok((x as i64).wrapping_neg() as u64) }
}

#[inline(always)]
pub fn bnot(x: u64, s: IntSpec) -> u64 {
    if s.signed { !x } else { !x & max_u(s.bits) }
}

/// `x << n`, truncated to the type's width. `n` is the shift amount's
/// register: a negative amount is a huge one.
#[inline(always)]
pub fn shl(x: u64, n: u64, s: IntSpec) -> IntResult {
    if n >= u64::from(s.bits) {
        return Err(IntErr::ShiftRange);
    }
    Ok(wrap(x << n, s))
}

/// `x >> n`: arithmetic for signed types, logical for unsigned ones.
#[inline(always)]
pub fn shr(x: u64, n: u64, s: IntSpec) -> IntResult {
    if n >= u64::from(s.bits) {
        return Err(IntErr::ShiftRange);
    }
    Ok(if s.signed { ((x as i64) >> n) as u64 } else { x >> n })
}

/// `v as to`, where `v` is a value of `from`: unravels when it doesn't fit.
pub fn cast_t(v: u64, to: IntSpec, from: IntSpec) -> IntResult {
    let value = from.value(v);
    if to.min() <= value && value <= to.max() { Ok(to.reg(value)) } else { Err(IntErr::Range) }
}

/// `v as% to`: wraps.
pub fn cast_w(v: u64, to: IntSpec) -> u64 {
    wrap(v, to)
}

/// `v as| to`: saturates.
pub fn cast_s(v: u64, to: IntSpec, from: IntSpec) -> u64 {
    to.reg(from.value(v).clamp(to.min(), to.max()))
}

/// `f as to`: truncates, and fails on NaN and out of range.
pub fn ftoi_t(f: f64, to: IntSpec) -> IntResult {
    if f.is_nan() {
        return Err(IntErr::Range);
    }
    let t = f.trunc();
    // Both bounds are powers of two, so exactly representable.
    let (lo, hi) = if to.signed {
        (-((1u128 << (to.bits - 1)) as f64), (1u128 << (to.bits - 1)) as f64)
    } else {
        (0.0, (1u128 << to.bits) as f64)
    };
    if t >= lo && t < hi { Ok(if to.signed { t as i64 as u64 } else { t as u64 }) } else { Err(IntErr::Range) }
}

/// `f as| to`: truncates and saturates; NaN is 0.
pub fn ftoi_s(f: f64, to: IntSpec) -> u64 {
    if f.is_nan() {
        return 0;
    }
    if to.signed { clamp_s(f as i64, to) } else { (f as u64).min(max_u(to.bits)) }
}

/// `v as char`, where `v` is a value of `from`.
pub fn to_char(v: u64, from: IntSpec) -> IntResult {
    let value = from.value(v);
    let valid = (0..=0x10FFFF).contains(&value) && !(0xD800..=0xDFFF).contains(&value);
    if valid { Ok(value as u64) } else { Err(IntErr::Range) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_docs_examples_hold() {
        // 01-syntax: `200u8 + 100`.
        assert_eq!(add_t(200, 100, U8), Err(IntErr::Overflow));
        assert_eq!(add_w(200, 100, U8), 44);
        assert_eq!(add_s(200, 100, U8), 255);
        assert_eq!(cast_w(300, U8), 44);
        assert_eq!(cast_s(300, U8, I32), 255);
        assert_eq!(cast_t(300, U8, I32), Err(IntErr::Range));
    }

    #[test]
    fn edges() {
        let m = i64::MIN as u64;
        assert_eq!(div(m, -1i64 as u64, I64), Err(IntErr::Overflow));
        assert_eq!(div(-128i64 as u64, -1i64 as u64, I8), Err(IntErr::Overflow));
        assert_eq!(rem_s(m, -1i64 as u64), Ok(0));
        assert_eq!(neg(-128i64 as u64, I8), Err(IntErr::Overflow));
        assert_eq!(shl(1, 8, U8), Err(IntErr::ShiftRange));
        assert_eq!(shl(0x81, 1, U8), Ok(2));
        assert_eq!(shr(-8i64 as u64, 1, I8), Ok(-4i64 as u64));
        assert_eq!(shl(1, -1i64 as u64, U64), Err(IntErr::ShiftRange));
        assert_eq!(ftoi_t(9.223_372_036_854_776e18, I64), Err(IntErr::Range));
        assert_eq!(ftoi_t(-0.5, U8), Ok(0));
        assert_eq!(ftoi_t(f64::NAN, U8), Err(IntErr::Range));
        assert_eq!(ftoi_s(f64::NAN, U8), 0);
        assert_eq!(ftoi_s(-1e30, I16), -32768i64 as u64);
        assert_eq!(ftoi_s(1e30, U64), u64::MAX);
        assert_eq!(to_char(0xD800, U32), Err(IntErr::Range));
    }
}
