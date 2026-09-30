//! Instruction words, and the integer types that ride in extension words.
//!
//! ```text
//!  63         48 47         32 31         16 15          0
//! ┌─────────────┬─────────────┬─────────────┬─────────────┐
//! │      c      │      b      │      a      │   opcode    │
//! └─────────────┴─────────────┴─────────────┴─────────────┘
//! ```

use crate::Op;

/// An instruction word with fields `a`, `b` and `c`.
pub fn word(op: Op, a: u16, b: u16, c: u16) -> u64 {
    op as u64 | (a as u64) << 16 | (b as u64) << 32 | (c as u64) << 48
}

/// An instruction word with field `a` and a 32-bit `b:c`.
pub fn word_i32(op: Op, a: u16, bc: i32) -> u64 {
    op as u64 | (a as u64) << 16 | (bc as u32 as u64) << 32
}

#[inline(always)]
pub fn opcode(w: u64) -> u16 {
    w as u16
}

#[inline(always)]
pub fn a(w: u64) -> usize {
    (w >> 16) as u16 as usize
}

#[inline(always)]
pub fn b(w: u64) -> usize {
    (w >> 32) as u16 as usize
}

#[inline(always)]
pub fn c(w: u64) -> usize {
    (w >> 48) as usize
}

/// `c` as a signed offset.
#[inline(always)]
pub fn c_i16(w: u64) -> isize {
    (w >> 48) as u16 as i16 as isize
}

/// `b:c` as a signed number.
#[inline(always)]
pub fn bc_i32(w: u64) -> i32 {
    (w >> 32) as u32 as i32
}

/// `b:c` as an unsigned number.
#[inline(always)]
pub fn bc_u32(w: u64) -> u32 {
    (w >> 32) as u32
}

/// A Loom integer type: its width and signedness, and whether it's
/// `usize`/`isize` (for messages). Values are kept normalized in registers:
/// sign-extended from `bits` for signed types, zero-extended otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IntSpec {
    pub bits: u8,
    pub signed: bool,
    pub size: bool,
}

impl IntSpec {
    pub const fn new(bits: u8, signed: bool) -> IntSpec {
        IntSpec { bits, signed, size: false }
    }

    /// As 16 bits: the width, then a bit each for signed and size.
    pub fn pack(self) -> u64 {
        self.bits as u64 | (self.signed as u64) << 8 | (self.size as u64) << 9
    }

    /// The inverse of [`pack`](Self::pack), for the low 16 bits of `v`, if
    /// they're a width from 1 to 64.
    pub fn unpack(v: u64) -> Option<IntSpec> {
        let bits = v as u8;
        let rest = (v >> 8) as u8;
        ((1..=64).contains(&bits) && rest & !3 == 0).then_some(IntSpec {
            bits,
            signed: rest & 1 != 0,
            size: rest & 2 != 0,
        })
    }

    /// Its name, as in messages: `u8`, `i24`, `usize`.
    pub fn name(self) -> String {
        match (self.size, self.signed) {
            (true, true) => "isize".into(),
            (true, false) => "usize".into(),
            (false, true) => format!("i{}", self.bits),
            (false, false) => format!("u{}", self.bits),
        }
    }

    pub fn min(self) -> i128 {
        if self.signed { -(1i128 << (self.bits - 1)) } else { 0 }
    }

    pub fn max(self) -> i128 {
        if self.signed { (1i128 << (self.bits - 1)) - 1 } else { (1i128 << self.bits) - 1 }
    }

    /// The value a register holding a value of this type means.
    pub fn value(self, reg: u64) -> i128 {
        if self.signed { reg as i64 as i128 } else { reg as i128 }
    }

    /// The register holding `v`, a value of this type.
    pub fn reg(self, v: i128) -> u64 {
        v as u64
    }
}

/// A cast's extension word: the target type, then the source's.
pub fn cast_spec(to: IntSpec, from: IntSpec) -> u64 {
    to.pack() | from.pack() << 16
}

/// The target and source of a cast's extension word.
pub fn unpack_cast(v: u64) -> Option<(IntSpec, IntSpec)> {
    Some((IntSpec::unpack(v & 0xFFFF)?, IntSpec::unpack((v >> 16) & 0xFFFF)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip() {
        let w = word(Op::AddTI32, 1, 2, 65535);
        assert_eq!((opcode(w), a(w), b(w), c(w)), (Op::AddTI32 as u16, 1, 2, 65535));
        assert_eq!(c_i16(word(Op::JEq, 0, 0, (-3i16) as u16)), -3);
        let w = word_i32(Op::Ldi, 7, -123_456);
        assert_eq!((a(w), bc_i32(w)), (7, -123_456));
    }

    #[test]
    fn specs_pack_and_unpack() {
        for spec in [IntSpec::new(12, false), IntSpec::new(64, true), IntSpec { bits: 64, signed: false, size: true }] {
            assert_eq!(IntSpec::unpack(spec.pack()), Some(spec));
        }
        assert_eq!(IntSpec::unpack(0), None);
        assert_eq!(IntSpec::unpack(65), None);
        let (to, from) = unpack_cast(cast_spec(IntSpec::new(8, false), IntSpec::new(64, false))).expect("valid");
        assert_eq!((to.name(), from.name()), ("u8".into(), "u64".into()));
        assert_eq!(IntSpec { bits: 64, signed: true, size: true }.name(), "isize");
    }
}
