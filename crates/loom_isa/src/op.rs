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
//! (`add`) unravel on overflow, `%` ops wrap and `|` ops saturate. Ops
//! ending in `k` take a small constant in place of a register.

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
    /// `a`, `b`: registers; `c`: an `i16` constant.
    ABK,
    /// `a`, `b`: registers; `c`: a `u16` constant (zero-extended).
    ABU,
    /// `a`: a register; `b`: an `i16` constant; `c`: an `i16` offset to jump
    /// by.
    AKJ,
    /// `b:c`: an `i32` offset to jump by.
    J,
    /// `a`: a register; `b:c`: an `i32` offset to jump by.
    AJ,
    /// `a`: where the callee's frame starts, at least
    /// [`LINK_SLOTS`](crate::LINK_SLOTS) registers in; `b:c`: the function's
    /// index.
    Call,
    /// `a`: where the arguments are; `b`: the native's index; `c`: its immediate.
    Native,
    /// `a`: the first result; `b`: how many.
    Ret,
    /// `a`: a 16-bit constant.
    K,
    /// `a`, `b`: registers; `c`: an `i16` byte offset. A load writes `a`
    /// from the address in `b` plus the offset; a store writes `b` to the
    /// address in `a` plus the offset.
    ABO,
    /// `a`: a register; `b:c`: a byte offset from the frame pointer, which
    /// the access (its width given by the op) stays inside the frame for.
    AF,
    /// `a`: a register; `b:c`: a byte offset into the image's data.
    AG,
    /// `a`, `b`: registers; an extension word: a byte count.
    ABZ,
    /// `a`: a register; an extension word: a byte count.
    AZ,
    /// `a`, `b`, `c`: registers; an extension word: a scale.
    ABCZ,
    /// `a`, `b`: registers; an extension word: a byte offset from the frame
    /// pointer (high half) and a scale (low half).
    ABFZ,
    /// `a`: the register loaded or stored; `b`: an index register; `c`: a
    /// byte offset into the element; an extension word: the byte offset of
    /// a list's header from the frame pointer (high half) and the scale
    /// (low half).
    FE,
    /// `a`: the register loaded or stored; `b`: the register holding the
    /// address of a list's header; `c`: an index register; an extension
    /// word: a byte offset into the element (high half) and the scale (low
    /// half).
    RE,
    /// `a`: the log2 of an alignment; an extension word: the byte offset
    /// of a list's header from the frame pointer (high half) and an element
    /// size (low half).
    FB,
    /// `a`: the register holding the address of a list's header; `b`: the
    /// log2 of an alignment; an extension word: an element size.
    RB,
    /// `a`: a register; `b:c`: a function's index.
    AFn,
    /// `a`: where the callee's frame starts, as for [`Form::Call`]; `b`: the
    /// register holding the function's index; `c`: how many parameters
    /// (high byte) and results (low byte) the callee must have.
    CallR,
    /// No operands.
    Z,
}

impl Form {
    /// How many words an instruction of this form takes.
    pub fn words(self) -> usize {
        match self {
            Form::AK
            | Form::ABX
            | Form::ABCX
            | Form::ABZ
            | Form::AZ
            | Form::ABCZ
            | Form::ABFZ
            | Form::FE
            | Form::RE
            | Form::FB
            | Form::RB => 2,
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
        matches!(self, Op::Jmp | Op::Loop | Op::Ret | Op::Unravel | Op::Crash | Op::Resume)
    }

    /// How many bytes a load or a store moves, if it's one.
    pub fn access_bytes(self) -> Option<u32> {
        Some(match self {
            Op::Ld8S | Op::Ld8U | Op::St8 | Op::LdF8S | Op::LdF8U | Op::StF8 => 1,
            Op::Ld16S | Op::Ld16U | Op::St16 | Op::LdF16S | Op::LdF16U | Op::StF16 => 2,
            Op::Ld32S | Op::Ld32U | Op::St32 | Op::LdF32S | Op::LdF32U | Op::StF32 => 4,
            Op::Ld64 | Op::St64 | Op::LdF64 | Op::StF64 => 8,
            _ => return None,
        })
    }
}

/// A check an op makes before it does its work (counted by the stats
/// build, to see how many the optimizer removes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Check {
    /// An index against a length.
    Bounds,
    /// Integer arithmetic or a conversion that traps on overflow.
    Overflow,
}

impl Op {
    /// The check it makes, if any.
    pub fn check(self) -> Option<Check> {
        let name = self.name();
        let (base, ty) = name.split_once('.').unwrap_or((name, ""));
        match base {
            "elem" | "elemf" | "ldelf" | "stelf" | "ldel" | "stel" | "bounds" | "boundsk" => Some(Check::Bounds),
            "add" | "sub" | "mul" | "addk" | "subk" | "mulk" | "neg" if !ty.starts_with('f') => Some(Check::Overflow),
            "cast" => Some(Check::Overflow),
            _ => None,
        }
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
    // with a 16-bit constant as the second operand
    AddTKI8 = "addk.i8", ABK;
    AddTKI16 = "addk.i16", ABK;
    AddTKI32 = "addk.i32", ABK;
    AddTKI64 = "addk.i64", ABK;
    AddTKU8 = "addk.u8", ABK;
    AddTKU16 = "addk.u16", ABK;
    AddTKU32 = "addk.u32", ABK;
    AddTKU64 = "addk.u64", ABK;
    SubTKI8 = "subk.i8", ABK;
    SubTKI16 = "subk.i16", ABK;
    SubTKI32 = "subk.i32", ABK;
    SubTKI64 = "subk.i64", ABK;
    SubTKU8 = "subk.u8", ABK;
    SubTKU16 = "subk.u16", ABK;
    SubTKU32 = "subk.u32", ABK;
    SubTKU64 = "subk.u64", ABK;
    AddWKI8 = "add%k.i8", ABK;
    AddWKI16 = "add%k.i16", ABK;
    AddWKI32 = "add%k.i32", ABK;
    AddWKI64 = "add%k.i64", ABK;
    AddWKU8 = "add%k.u8", ABK;
    AddWKU16 = "add%k.u16", ABK;
    AddWKU32 = "add%k.u32", ABK;
    AddWKU64 = "add%k.u64", ABK;
    // bitwise ops with a mask: a 16-bit constant, zero-extended
    AndK = "andk", ABU;
    OrK = "ork", ABU;
    XorK = "xork", ABU;
    // shifts by a constant below the width (the verifier checks it)
    ShlKI8 = "shlk.i8", ABK;
    ShlKI16 = "shlk.i16", ABK;
    ShlKI32 = "shlk.i32", ABK;
    ShlKI64 = "shlk.i64", ABK;
    ShlKU8 = "shlk.u8", ABK;
    ShlKU16 = "shlk.u16", ABK;
    ShlKU32 = "shlk.u32", ABK;
    ShlKU64 = "shlk.u64", ABK;
    ShrKS = "shrk.s", ABK;
    ShrKU = "shrk.u", ABK;
    // division and remainder by a constant above zero (checked too), which
    // can't trap
    DivKS = "divk.s", ABK;
    DivKU = "divk.u", ABK;
    RemKS = "remk.s", ABK;
    RemKU = "remk.u", ABK;
    // comparisons with a 16-bit constant that give a `bool`
    EqK = "eqk", ABK;
    NeK = "nek", ABK;
    LtSK = "ltk.s", ABK;
    LeSK = "lek.s", ABK;
    GtSK = "gtk.s", ABK;
    GeSK = "gek.s", ABK;
    LtUK = "ltk.u", ABK;
    LeUK = "lek.u", ABK;
    GtUK = "gtk.u", ABK;
    GeUK = "gek.u", ABK;
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
    // compare with a 16-bit constant and branch: to `c` when it holds
    JEqK = "jeqk", AKJ;
    JNeK = "jnek", AKJ;
    JLtSK = "jltk.s", AKJ;
    JLeSK = "jlek.s", AKJ;
    JGtSK = "jgtk.s", AKJ;
    JGeSK = "jgek.s", AKJ;
    JLtUK = "jltk.u", AKJ;
    JLeUK = "jlek.u", AKJ;
    JGtUK = "jgtk.u", AKJ;
    JGeUK = "jgek.u", AKJ;
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
    // memory: loads (a = dst, b = address), sign- or zero-extending
    Ld8S = "ld.i8", ABO;
    Ld8U = "ld.u8", ABO;
    Ld16S = "ld.i16", ABO;
    Ld16U = "ld.u16", ABO;
    Ld32S = "ld.i32", ABO;
    Ld32U = "ld.u32", ABO;
    Ld64 = "ld.64", ABO;
    // stores (a = address, b = what's stored), of the value's low bytes
    St8 = "st.8", ABO;
    St16 = "st.16", ABO;
    St32 = "st.32", ABO;
    St64 = "st.64", ABO;
    // the same at a byte offset from the frame pointer
    LdF8S = "ldf.i8", AF;
    LdF8U = "ldf.u8", AF;
    LdF16S = "ldf.i16", AF;
    LdF16U = "ldf.u16", AF;
    LdF32S = "ldf.i32", AF;
    LdF32U = "ldf.u32", AF;
    LdF64 = "ldf.64", AF;
    StF8 = "stf.8", AF;
    StF16 = "stf.16", AF;
    StF32 = "stf.32", AF;
    StF64 = "stf.64", AF;
    // addresses: in the frame, in the image's data, and an element's
    Lea = "lea", AF;
    Gaddr = "gaddr", AG;
    Idx = "idx", ABCZ;
    // a list's element: the address of element c (scaled) of the list
    // whose header (buffer, length) is at b's address, or at fp+off;
    // unravels unless c < the length
    Elem = "elem", ABCZ;
    ElemF = "elemf", ABFZ;
    // a load from or a store to a list's element (checked as `elem` does),
    // at a byte offset into it, with the header in the frame (`f`) or at a
    // register's address
    LdElF8S = "ldelf.i8", FE;
    LdElF8U = "ldelf.u8", FE;
    LdElF16S = "ldelf.i16", FE;
    LdElF16U = "ldelf.u16", FE;
    LdElF32S = "ldelf.i32", FE;
    LdElF32U = "ldelf.u32", FE;
    LdElF64 = "ldelf.64", FE;
    StElF8 = "stelf.8", FE;
    StElF16 = "stelf.16", FE;
    StElF32 = "stelf.32", FE;
    StElF64 = "stelf.64", FE;
    LdEl8S = "ldel.i8", RE;
    LdEl8U = "ldel.u8", RE;
    LdEl16S = "ldel.i16", RE;
    LdEl16U = "ldel.u16", RE;
    LdEl32S = "ldel.i32", RE;
    LdEl32U = "ldel.u32", RE;
    LdEl64 = "ldel.64", RE;
    StEl8 = "stel.8", RE;
    StEl16 = "stel.16", RE;
    StEl32 = "stel.32", RE;
    StEl64 = "stel.64", RE;
    // free the buffer of a list (or a `String`) unless its capacity is 0:
    // its header in the frame, or at a register's address
    FreeBufF = "freebuf.f", FB;
    FreeBuf = "freebuf", RB;
    // blocks of memory: copy `ext` bytes from b's address to a's; zero them
    Copy = "copy", ABZ;
    Zero = "zero", AZ;
    // unravel unless a < b (unsigned): an index and a length; or a < the
    // constant in b:c
    Bounds = "bounds", AB;
    BoundsK = "boundsk", AI;
    // function values and calling them
    Ldfn = "ldfn", AFn;
    CallR = "callr", CallR;
    // the end of a landing pad: unwinding goes on in the caller
    Resume = "resume", Z;
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
