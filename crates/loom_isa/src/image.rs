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

/// A function's code and what calling it takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Func {
    pub name: String,
    /// Where its code starts in [`Image::code`], and how many words it has.
    pub entry: u32,
    pub len: u32,
    /// How many registers it uses, its call windows included. Its caller's
    /// return address is kept just past them.
    pub frame: u16,
    /// Its arguments arrive in `r0..params`; its results leave in
    /// `r0..results`.
    pub params: u16,
    pub results: u16,
    /// Sorted by `pc`.
    pub lines: Vec<Line>,
}

/// What an internal-error instruction (`crash k`) says.
pub const CRASHES: &[&str] = &["loom internal: no match arm matched", "loom internal: unreachable code was reached"];

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
