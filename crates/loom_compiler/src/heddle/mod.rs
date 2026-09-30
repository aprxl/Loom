//! Heddle: Treadle's typed mid-level IR (design/vm.md §6).
//!
//! A function is a control-flow graph of blocks in SSA form. Blocks take
//! parameters instead of having phi nodes, and each edge passes arguments
//! for them. Values have machine types ([`VTy`]): integers of every width,
//! `bool`s and `char`s are all 64-bit integers there, kept normalized, so
//! widening one is free. Integer instructions carry the Loom integer type
//! and the overflow policy, so checks are exact.
//! A Loom value of a compound type (a tuple, a `str`) is several values,
//! one per scalar part.
//!
//! [`build`] makes Heddle from the typed program; `codegen` turns it into
//! Treadle bytecode. [`verify`] checks the SSA properties both rely on.

mod build;
mod print;
mod verify;

pub use build::build;
pub use print::print;
pub use verify::verify;

use loom_isa::FmtItem;
use loom_syntax::source::Span;

use crate::ty::{FloatTy, IntTy};

/// A value: an instruction's result or a block parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Value(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncId(pub u32);

/// The machine type of a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VTy {
    /// An integer of any width, a `bool` (0 or 1) or a `char`, normalized
    /// to its type in 64 bits.
    Int,
    F32,
    F64,
    /// An address.
    Ptr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Policy {
    /// Unravel when the result doesn't fit.
    Trap,
    Wrap,
    Sat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BitOp {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InstKind {
    /// A value nothing reads before something else is written (a variable
    /// read on a path where it has no value, which typing rules out).
    Undef,
    /// A 64-bit pattern: an integer normalized to its type, a `bool` (0 or
    /// 1), or a `char`.
    Iconst(u64),
    /// A float's bits (an `f32`'s in the low 32).
    Fconst(u64),
    /// The address of text literal `n` of the module.
    Str(u32),
    Arith {
        op: IntOp,
        policy: Policy,
        ty: IntTy,
        a: Value,
        b: Value,
    },
    Div {
        ty: IntTy,
        a: Value,
        b: Value,
    },
    Rem {
        ty: IntTy,
        a: Value,
        b: Value,
    },
    Shl {
        ty: IntTy,
        a: Value,
        b: Value,
    },
    Shr {
        ty: IntTy,
        a: Value,
        b: Value,
    },
    Bit {
        op: BitOp,
        a: Value,
        b: Value,
    },
    Neg {
        ty: IntTy,
        a: Value,
    },
    BitNot {
        ty: IntTy,
        a: Value,
    },
    /// `not` on a `bool`.
    Not(Value),
    /// Integers, `bool`s and `char`s; `unsigned` for unsigned types.
    ICmp {
        cmp: Cmp,
        unsigned: bool,
        a: Value,
        b: Value,
    },
    FArith {
        op: FOp,
        ty: FloatTy,
        a: Value,
        b: Value,
    },
    FNeg {
        ty: FloatTy,
        a: Value,
    },
    FCmp {
        cmp: Cmp,
        ty: FloatTy,
        a: Value,
        b: Value,
    },
    /// `as`, `as%` and `as|` between integer types.
    IntCast {
        policy: Policy,
        to: IntTy,
        from: IntTy,
        a: Value,
    },
    IntToFloat {
        from: IntTy,
        to: FloatTy,
        a: Value,
    },
    /// `as` (unravels on NaN or out of range) or `as|` (`sat`).
    FloatToInt {
        sat: bool,
        from: FloatTy,
        to: IntTy,
        a: Value,
    },
    FloatCast {
        to: FloatTy,
        a: Value,
    },
    IntToChar {
        from: IntTy,
        a: Value,
    },
    Call {
        func: FuncId,
        args: Vec<Value>,
    },
    /// A native (see `loom_isa::natives`), with its immediate.
    Native {
        name: &'static str,
        imm: u16,
        args: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inst {
    pub kind: InstKind,
    pub results: Vec<Value>,
    /// The Loom statement it came from.
    pub span: Option<Span>,
}

/// An edge to a block, with an argument for each of its parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub block: BlockId,
    pub args: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    Jump(Edge),
    Br {
        cond: Value,
        then: Edge,
        else_: Edge,
    },
    Ret(Vec<Value>),
    /// Unravel with a text value as the message.
    Unravel {
        ptr: Value,
        len: Value,
    },
    /// Unravel with the message the `msg.*` natives built.
    UnravelMsg,
    /// An internal error: `loom_isa::CRASHES[n]`.
    Crash(u16),
}

impl Term {
    pub fn edges(&self) -> Vec<&Edge> {
        match self {
            Term::Jump(e) => vec![e],
            Term::Br { then, else_, .. } => vec![then, else_],
            _ => Vec::new(),
        }
    }

    pub fn edges_mut(&mut self) -> Vec<&mut Edge> {
        match self {
            Term::Jump(e) => vec![e],
            Term::Br { then, else_, .. } => vec![then, else_],
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    pub params: Vec<Value>,
    pub insts: Vec<Inst>,
    /// `None` only while building.
    pub term: Option<Term>,
    pub term_span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Func {
    pub name: String,
    /// The entry block is `blocks[0]`; its parameters are the function's.
    pub blocks: Vec<Block>,
    pub results: Vec<VTy>,
    /// The type of each value.
    pub types: Vec<VTy>,
}

impl Func {
    pub fn params(&self) -> &[Value] {
        &self.blocks[0].params
    }

    pub fn ty(&self, v: Value) -> VTy {
        self.types[v.0 as usize]
    }
}

/// A whole program in Heddle.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Module {
    pub funcs: Vec<Func>,
    /// Text literals, by [`InstKind::Str`] number.
    pub strings: Vec<Vec<u8>>,
    /// What the formatting natives format, by immediate.
    pub fmts: Vec<FmtItem>,
    /// The function to run.
    pub entry: FuncId,
}

impl Module {
    pub fn func(&self, id: FuncId) -> &Func {
        &self.funcs[id.0 as usize]
    }
}
