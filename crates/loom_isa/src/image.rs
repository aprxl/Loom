//! Images: a whole compiled program, ready for Treadle to load.

use crate::FmtItem;

/// Where an instruction came from in Loom source: every instruction from
/// `pc` (counted from the function's start) up to the next entry's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub pc: u32,
    /// An index into [`Image::files`].
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

/// Where an unwind that starts in `start..end` (in words from the
/// function's start) goes: the landing pad at `pad`, which runs the
/// `defer`s and drops of that stretch of code and ends by going on to the
/// next pad out, or with `resume`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pad {
    pub start: u32,
    pub end: u32,
    pub pad: u32,
}

/// A function's code and what calling it takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Func {
    pub name: String,
    /// Where its code starts in [`Image::code`], and how many words it has.
    pub entry: u32,
    pub len: u32,
    /// How many registers it uses, its call windows included (and the
    /// [`LINK_SLOTS`](crate::LINK_SLOTS) before each window).
    pub frame: u16,
    /// Its arguments arrive in `r0..params`; its results leave in
    /// `r0..results`.
    pub params: u16,
    pub results: u16,
    /// Sorted by `pc`.
    pub lines: Vec<Line>,
    /// Landing pads, sorted by `start`; ranges don't overlap.
    pub pads: Vec<Pad>,
}

impl Func {
    /// The landing pad for an unwind at `pc` (in words from the start).
    pub fn pad_at(&self, pc: u32) -> Option<u32> {
        let i = self.pads.partition_point(|p| p.start <= pc);
        let p = self.pads.get(i.checked_sub(1)?)?;
        (pc < p.end).then_some(p.pad)
    }
}

/// What an internal-error instruction (`crash k`) says.
pub const CRASHES: &[&str] = &[
    "loom internal: no match arm matched",
    "loom internal: unreachable code was reached",
    "loom internal: a function value called with the wrong number of arguments",
];

/// A compiled program.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Image {
    pub funcs: Vec<Func>,
    pub code: Vec<u64>,
    /// Text literals, which `ldstr` points at.
    pub strings: Vec<Vec<u8>>,
    pub fmts: Vec<FmtItem>,
    /// The natives it calls, by name (see [`crate::natives`]).
    pub natives: Vec<String>,
    /// Source file names, for [`Line::file`].
    pub files: Vec<String>,
    /// What the program's data starts as, in 8-byte words: vtables,
    /// constants and their flags. `gaddr` gives addresses in it.
    pub data: Vec<u64>,
    /// The function to run: `main`.
    pub entry: Option<u32>,
}

impl Image {
    /// The function whose code holds word `pc`.
    pub fn func_at(&self, pc: u32) -> Option<usize> {
        let i = self.funcs.partition_point(|f| f.entry <= pc);
        let f = self.funcs.get(i.checked_sub(1)?)?;
        (pc < f.entry + f.len).then_some(i - 1)
    }

    /// The Loom file, line and column of the instruction at `pc`.
    pub fn line_at(&self, pc: u32) -> Option<(&str, u32, u32)> {
        let f = &self.funcs[self.func_at(pc)?];
        let offset = pc - f.entry;
        let i = f.lines.partition_point(|l| l.pc <= offset);
        let l = f.lines.get(i.checked_sub(1)?)?;
        Some((self.files.get(l.file as usize)?.as_str(), l.line, l.column))
    }
}
