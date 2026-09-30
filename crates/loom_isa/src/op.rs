//! The op table: every Treadle instruction, declared once.
//!
//! Each op has a name (its mnemonic in listings) and a [`Form`], which says
//! what its operand fields hold. The interpreter matches on [`Op`], so Rust
//! checks that it handles every one; the verifier and the disassembler read
//! the forms.
//!
//! Integer ops come in families specialized by width: `add.i32` for the
//! common widths, and `add.gen`, whose extension word describes any other
//! width (`u12`, `i24`) as an [`IntSpec`](crate::IntSpec). Trapping ops
//! (`add`) unravel on overflow, `%` ops wrap and `|` ops saturate.

/// What an instruction's operand fields hold. Registers are counted in
/// 8-byte slots from the frame pointer; offsets in words from the
/// instruction itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `a`, `b`: registers.
    AB,
    /// `a`, `b`, `c`: registers.
    ABC,
    /// `a`: a register; `b:c`: an `i32`.
    AI,
    /// `a`: a register; an extension word: a 64-bit constant.
    AK,
    /// `a`: a register; `b:c`: an index into the image's strings.
    AS,
    /// `a`, `b`: registers; an extension word: an `IntSpec` (or a cast's).
    ABX,
    /// `a`, `b`, `c`: registers; an extension word: an `IntSpec`.
    ABCX,
    /// `a`, `b`: registers; `c`: an `i16` offset to jump by.
    ABJ,
    /// `b:c`: an `i32` offset to jump by.
    J,
    /// `a`: a register; `b:c`: an `i32` offset to jump by.
    AJ,
    /// `a`: where the callee's frame starts; `b:c`: the function's index.
    Call,
    /// `a`: where the arguments are; `b`: the native's index; `c`: its immediate.
    Native,
    /// `a`: the first result; `b`: how many; `c`: the function's frame size.
    Ret,
    /// `a`: a 16-bit constant.
    K,
}

impl Form {
    /// How many words an instruction of this form takes.
    pub fn words(self) -> usize {
        match self {
            Form::AK | Form::ABX | Form::ABCX => 2,
            _ => 1,
        }
    }
}

macro_rules! ops {
    ($($name:ident = $mnemonic:literal, $form:ident;)*) => {
        /// A Treadle instruction. Discriminants are consecutive from 0.
        #[repr(u16)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Op {
            $($name,)*
        }

        impl Op {
            /// Every op, in discriminant order.
            pub const ALL: &'static [Op] = &[$(Op::$name,)*];

            /// Its mnemonic.
            pub fn name(self) -> &'static str {
                match self {
                    $(Op::$name => $mnemonic,)*
                }
            }

            pub fn form(self) -> Form {
                match self {
                    $(Op::$name => Form::$form,)*
                }
            }
        }
    };
}

impl Op {
    /// The op with discriminant `n`, if there is one.
    pub fn from_u16(n: u16) -> Option<Op> {
        Op::ALL.get(n as usize).copied()
    }

    /// Whether control never goes on to the next instruction.
    pub fn ends_flow(self) -> bool {
        matches!(self, Op::Jmp | Op::Loop | Op::Ret | Op::Unravel | Op::Crash)
    }
}

ops! {
    // moves and constants
    Mov = "mov", AB;
    Ldi = "ldi", AI;
    Ldk = "ldk", AK;
    Ldstr = "ldstr", AS;
    // trapping `+`
    AddTI8 = "add.i8", ABC;
    AddTI16 = "add.i16", ABC;
    AddTI32 = "add.i32", ABC;
    AddTI64 = "add.i64", ABC;
    AddTU8 = "add.u8", ABC;
    AddTU16 = "add.u16", ABC;
    AddTU32 = "add.u32", ABC;
    AddTU64 = "add.u64", ABC;
    AddTG = "add.gen", ABCX;
    // wrapping `+%`
    AddWI8 = "add%.i8", ABC;
    AddWI16 = "add%.i16", ABC;
    AddWI32 = "add%.i32", ABC;
    AddWI64 = "add%.i64", ABC;
    AddWU8 = "add%.u8", ABC;
    AddWU16 = "add%.u16", ABC;
    AddWU32 = "add%.u32", ABC;
    AddWU64 = "add%.u64", ABC;
    AddWG = "add%.gen", ABCX;
    // saturating `+|`
    AddSI8 = "add|.i8", ABC;
    AddSI16 = "add|.i16", ABC;
    AddSI32 = "add|.i32", ABC;
    AddSI64 = "add|.i64", ABC;
    AddSU8 = "add|.u8", ABC;
    AddSU16 = "add|.u16", ABC;
    AddSU32 = "add|.u32", ABC;
    AddSU64 = "add|.u64", ABC;
    AddSG = "add|.gen", ABCX;
    // trapping `-`
    SubTI8 = "sub.i8", ABC;
    SubTI16 = "sub.i16", ABC;
    SubTI32 = "sub.i32", ABC;
    SubTI64 = "sub.i64", ABC;
    SubTU8 = "sub.u8", ABC;
    SubTU16 = "sub.u16", ABC;
    SubTU32 = "sub.u32", ABC;
    SubTU64 = "sub.u64", ABC;
    SubTG = "sub.gen", ABCX;
    // wrapping `-%`
    SubWI8 = "sub%.i8", ABC;
    SubWI16 = "sub%.i16", ABC;
    SubWI32 = "sub%.i32", ABC;
    SubWI64 = "sub%.i64", ABC;
    SubWU8 = "sub%.u8", ABC;
    SubWU16 = "sub%.u16", ABC;
    SubWU32 = "sub%.u32", ABC;
    SubWU64 = "sub%.u64", ABC;
    SubWG = "sub%.gen", ABCX;
    // saturating `-|`
    SubSI8 = "sub|.i8", ABC;
    SubSI16 = "sub|.i16", ABC;
    SubSI32 = "sub|.i32", ABC;
    SubSI64 = "sub|.i64", ABC;
    SubSU8 = "sub|.u8", ABC;
    SubSU16 = "sub|.u16", ABC;
    SubSU32 = "sub|.u32", ABC;
    SubSU64 = "sub|.u64", ABC;
    SubSG = "sub|.gen", ABCX;
    // trapping `*`
    MulTI8 = "mul.i8", ABC;
    MulTI16 = "mul.i16", ABC;
    MulTI32 = "mul.i32", ABC;
    MulTI64 = "mul.i64", ABC;
    MulTU8 = "mul.u8", ABC;
    MulTU16 = "mul.u16", ABC;
    MulTU32 = "mul.u32", ABC;
    MulTU64 = "mul.u64", ABC;
    MulTG = "mul.gen", ABCX;
    // wrapping `*%`
    MulWI8 = "mul%.i8", ABC;
    MulWI16 = "mul%.i16", ABC;
    MulWI32 = "mul%.i32", ABC;
    MulWI64 = "mul%.i64", ABC;
    MulWU8 = "mul%.u8", ABC;
    MulWU16 = "mul%.u16", ABC;
    MulWU32 = "mul%.u32", ABC;
    MulWU64 = "mul%.u64", ABC;
    MulWG = "mul%.gen", ABCX;
    // saturating `*|`
    MulSI8 = "mul|.i8", ABC;
    MulSI16 = "mul|.i16", ABC;
    MulSI32 = "mul|.i32", ABC;
    MulSI64 = "mul|.i64", ABC;
    MulSU8 = "mul|.u8", ABC;
    MulSU16 = "mul|.u16", ABC;
    MulSU32 = "mul|.u32", ABC;
    MulSU64 = "mul|.u64", ABC;
    MulSG = "mul|.gen", ABCX;
    // division, remainder, negation, bitwise not, shifts
    DivI8 = "div.i8", ABC;
    DivI16 = "div.i16", ABC;
    DivI32 = "div.i32", ABC;
    DivI64 = "div.i64", ABC;
    DivU8 = "div.u8", ABC;
    DivU16 = "div.u16", ABC;
    DivU32 = "div.u32", ABC;
    DivU64 = "div.u64", ABC;
    DivG = "div.gen", ABCX;
    RemS = "rem.s", ABC;
    RemU = "rem.u", ABC;
    NegI8 = "neg.i8", AB;
    NegI16 = "neg.i16", AB;
    NegI32 = "neg.i32", AB;
    NegI64 = "neg.i64", AB;
    NegG = "neg.gen", ABX;
    BnotS = "bnot.s", AB;
    BnotU8 = "bnot.u8", AB;
    BnotU16 = "bnot.u16", AB;
    BnotU32 = "bnot.u32", AB;
    BnotU64 = "bnot.u64", AB;
    BnotG = "bnot.gen", ABX;
    ShlI8 = "shl.i8", ABC;
    ShlI16 = "shl.i16", ABC;
    ShlI32 = "shl.i32", ABC;
    ShlI64 = "shl.i64", ABC;
    ShlU8 = "shl.u8", ABC;
    ShlU16 = "shl.u16", ABC;
    ShlU32 = "shl.u32", ABC;
    ShlU64 = "shl.u64", ABC;
    ShlG = "shl.gen", ABCX;
    ShrI8 = "shr.i8", ABC;
    ShrI16 = "shr.i16", ABC;
    ShrI32 = "shr.i32", ABC;
    ShrI64 = "shr.i64", ABC;
    ShrU8 = "shr.u8", ABC;
    ShrU16 = "shr.u16", ABC;
    ShrU32 = "shr.u32", ABC;
    ShrU64 = "shr.u64", ABC;
    ShrG = "shr.gen", ABCX;
    And = "and", ABC;
    Or = "or", ABC;
    Xor = "xor", ABC;
    Not = "not", AB;
    // comparisons that give a `bool`
    Eq = "eq", ABC;
    Ne = "ne", ABC;
    LtS = "lt.s", ABC;
    LeS = "le.s", ABC;
    LtU = "lt.u", ABC;
    LeU = "le.u", ABC;
    EqF64 = "eq.f64", ABC;
    NeF64 = "ne.f64", ABC;
    LtF64 = "lt.f64", ABC;
    LeF64 = "le.f64", ABC;
    EqF32 = "eq.f32", ABC;
    NeF32 = "ne.f32", ABC;
    LtF32 = "lt.f32", ABC;
    LeF32 = "le.f32", ABC;
    // compare and branch: to `c` (an i16 offset) when the comparison holds
    JEq = "jeq", ABJ;
    JNe = "jne", ABJ;
    JLtS = "jlt.s", ABJ;
    JLeS = "jle.s", ABJ;
    JLtU = "jlt.u", ABJ;
    JLeU = "jle.u", ABJ;
    JEqF64 = "jeq.f64", ABJ;
    JNeF64 = "jne.f64", ABJ;
    JLtF64 = "jlt.f64", ABJ;
    JLeF64 = "jle.f64", ABJ;
    JNLtF64 = "jnlt.f64", ABJ;
    JNLeF64 = "jnle.f64", ABJ;
    JEqF32 = "jeq.f32", ABJ;
    JNeF32 = "jne.f32", ABJ;
    JLtF32 = "jlt.f32", ABJ;
    JLeF32 = "jle.f32", ABJ;
    JNLtF32 = "jnlt.f32", ABJ;
    JNLeF32 = "jnle.f32", ABJ;
    // jumps
    Jmp = "jmp", J;
    Loop = "loop", J;
    Jz = "jz", AJ;
    Jnz = "jnz", AJ;
    // floats
    AddF64 = "add.f64", ABC;
    SubF64 = "sub.f64", ABC;
    MulF64 = "mul.f64", ABC;
    DivF64 = "div.f64", ABC;
    RemF64 = "rem.f64", ABC;
    NegF64 = "neg.f64", AB;
    AddF32 = "add.f32", ABC;
    SubF32 = "sub.f32", ABC;
    MulF32 = "mul.f32", ABC;
    DivF32 = "div.f32", ABC;
    RemF32 = "rem.f32", ABC;
    NegF32 = "neg.f32", AB;
    // conversions
    CastT = "cast", ABX;
    CastW = "cast%", ABX;
    CastS = "cast|", ABX;
    ItofS64 = "itof.s.f64", AB;
    ItofU64 = "itof.u.f64", AB;
    ItofS32 = "itof.s.f32", AB;
    ItofU32 = "itof.u.f32", AB;
    FtoiT64 = "ftoi.f64", ABX;
    FtoiS64 = "ftoi|.f64", ABX;
    FtoiT32 = "ftoi.f32", ABX;
    FtoiS32 = "ftoi|.f32", ABX;
    F64toF32 = "f64to32", AB;
    F32toF64 = "f32to64", AB;
    Itoc = "itoc", ABX;
    // calls
    Call = "call", Call;
    CallN = "calln", Native;
    Ret = "ret", Ret;
    // giving up
    Unravel = "unravel", AB;
    Crash = "crash", K;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_are_positions() {
        for (i, op) in Op::ALL.iter().enumerate() {
            assert_eq!(*op as usize, i);
            assert_eq!(Op::from_u16(i as u16), Some(*op));
        }
        assert_eq!(Op::from_u16(Op::ALL.len() as u16), None);
    }

    #[test]
    fn mnemonics_are_unique() {
        let mut names: Vec<&str> = Op::ALL.iter().map(|o| o.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }
}
