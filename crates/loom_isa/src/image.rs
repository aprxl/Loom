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
    /// The inlined call the instructions came from: an index into
    /// [`Func::sites`], or [`NO_SITE`].
    pub site: u32,
}

/// A [`Line`] of the function's own code, not an inlined call's.
pub const NO_SITE: u32 = u32::MAX;

/// A call the compiler inlined, which a backtrace shows as a frame of its
/// own: the function called, where the call was (in the caller's frame),
/// and the call the caller was itself inlined from ([`NO_SITE`] if none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub name: String,
    /// The call's file (an index into [`Image::files`]), line and column.
    pub call: Option<(u32, u32, u32)>,
    pub parent: u32,
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
    /// The calls inlined into it, which its [`Line`]s name.
    pub sites: Vec<Site>,
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

/// A place in Loom source: a file's name, a line and a column.
pub type Place<'a> = (&'a str, u32, u32);

/// A host function the program calls: the name the host registers it
/// under, and how many registers it reads and writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostImport {
    pub name: String,
    pub args: u16,
    pub results: u16,
}

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
    /// The host functions it calls, by index (the immediate of `host.call`).
    pub host_imports: Vec<HostImport>,
    /// Source file names, for [`Line::file`].
    pub files: Vec<String>,
    /// What the program's data starts as, in 8-byte words: vtables,
    /// constants and their flags. `gaddr` gives addresses in it.
    pub data: Vec<u64>,
    /// Words of `data` that hold a pointer into `data` itself, as a byte
    /// offset from its start (constants baked in by the compiler: a string's
    /// bytes, a list's buffer). The loader turns each into the address it
    /// is, so none depends on where the data is.
    pub relocs: Vec<u32>,
    /// The function to run: `main`.
    pub entry: Option<u32>,
}

impl Image {
    /// The registers a `host.call` needs: the largest import's window.
    pub fn host_window(&self) -> usize {
        self.host_imports.iter().map(|h| usize::from(h.args.max(h.results))).max().unwrap_or(0)
    }

    /// The function whose code holds word `pc`.
    pub fn func_at(&self, pc: u32) -> Option<usize> {
        let i = self.funcs.partition_point(|f| f.entry <= pc);
        let f = self.funcs.get(i.checked_sub(1)?)?;
        (pc < f.entry + f.len).then_some(i - 1)
    }

    /// The Loom file, line and column of the instruction at `pc`.
    pub fn line_at(&self, pc: u32) -> Option<(&str, u32, u32)> {
        let l = self.line_entry(pc)?.1;
        Some((self.files.get(l.file as usize)?.as_str(), l.line, l.column))
    }

    fn line_entry(&self, pc: u32) -> Option<(&Func, &Line)> {
        let f = &self.funcs[self.func_at(pc)?];
        let offset = pc - f.entry;
        let i = f.lines.partition_point(|l| l.pc <= offset);
        Some((f, f.lines.get(i.checked_sub(1)?)?))
    }

    /// The frames the instruction at `pc` is in, innermost first: each
    /// function's name and where in it (file, line, column). One, unless
    /// the instruction came from a call the compiler inlined, whose frames
    /// come first.
    pub fn frames_at(&self, pc: u32) -> Vec<(&str, Option<Place<'_>>)> {
        let Some(f) = self.func_at(pc).map(|f| &self.funcs[f]) else { return Vec::new() };
        let Some((_, l)) = self.line_entry(pc) else { return vec![(f.name.as_str(), None)] };
        let file = |k: u32| self.files.get(k as usize).map(String::as_str);
        let mut out = Vec::new();
        let mut at = file(l.file).map(|name| (name, l.line, l.column));
        let mut site = l.site;
        // (Each site's parent comes before it, so this ends.)
        while let Some(s) = f.sites.get(site as usize) {
            out.push((s.name.as_str(), at));
            at = s.call.and_then(|(k, line, column)| file(k).map(|name| (name, line, column)));
            site = s.parent;
        }
        out.push((f.name.as_str(), at));
        out
    }
}
