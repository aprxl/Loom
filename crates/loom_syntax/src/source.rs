//! Source files, byte spans, and line/column lookup.

use std::ops::Range;

/// Identifies a file inside a [`SourceDb`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

/// A byte range `lo..hi` inside one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn new(file: FileId, lo: u32, hi: u32) -> Self {
        debug_assert!(lo <= hi, "span lo {lo} > hi {hi}");
        Self { file, lo, hi }
    }

    pub fn len(self) -> u32 {
        self.hi - self.lo
    }

    pub fn is_empty(self) -> bool {
        self.lo == self.hi
    }

    /// The smallest span covering both `self` and `other` (same file).
    pub fn to(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span::new(self.file, self.lo.min(other.lo), self.hi.max(other.hi))
    }

    pub fn range(self) -> Range<usize> {
        self.lo as usize..self.hi as usize
    }
}

/// A 1-based line and 1-based column. Columns count Unicode scalar values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// One loaded source file. The text is normalized on load: a leading BOM is
/// removed and `\r\n` becomes `\n`, so every offset in the compiler refers to
/// this normalized text.
#[derive(Debug)]
pub struct SourceFile {
    name: String,
    text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    fn new(name: String, text: String) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).replace("\r\n", "\n");
        assert!(
            u32::try_from(text.len()).is_ok(),
            "source file `{name}` is larger than 4 GiB"
        );
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i as u32 + 1));
        Self {
            name,
            text,
            line_starts,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The 0-based line containing `byte`.
    pub fn line_index(&self, byte: u32) -> usize {
        match self.line_starts.binary_search(&byte) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    /// The byte range of 0-based `line`, excluding its newline.
    pub fn line_range(&self, line: usize) -> Option<Range<usize>> {
        let start = *self.line_starts.get(line)? as usize;
        let end = match self.line_starts.get(line + 1) {
            Some(&next) => next as usize - 1,
            None => self.text.len(),
        };
        Some(start..end)
    }

    pub fn line_col(&self, byte: u32) -> LineCol {
        let line = self.line_index(byte);
        let start = self.line_starts[line] as usize;
        let byte = (byte as usize).min(self.text.len());
        let col = self.text[start..byte].chars().count() + 1;
        LineCol {
            line: line as u32 + 1,
            col: col as u32,
        }
    }
}

/// All source files of one compilation.
#[derive(Debug, Default)]
pub struct SourceDb {
    files: Vec<SourceFile>,
}

impl SourceDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> FileId {
        let id = FileId(u32::try_from(self.files.len()).expect("too many source files"));
        self.files.push(SourceFile::new(name.into(), text.into()));
        id
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn slice(&self, span: Span) -> &str {
        &self.file(span.file).text()[span.range()]
    }

    pub fn line_col(&self, span: Span) -> LineCol {
        self.file(span.file).line_col(span.lo)
    }
}

impl<'a> codespan_reporting::files::Files<'a> for SourceDb {
    type FileId = FileId;
    type Name = &'a str;
    type Source = &'a str;

    fn name(&'a self, id: FileId) -> Result<&'a str, codespan_reporting::files::Error> {
        self.files
            .get(id.0 as usize)
            .map(|f| f.name.as_str())
            .ok_or(codespan_reporting::files::Error::FileMissing)
    }

    fn source(&'a self, id: FileId) -> Result<&'a str, codespan_reporting::files::Error> {
        self.files
            .get(id.0 as usize)
            .map(|f| f.text.as_str())
            .ok_or(codespan_reporting::files::Error::FileMissing)
    }

    fn line_index(&'a self, id: FileId, byte_index: usize) -> Result<usize, codespan_reporting::files::Error> {
        let file = self
            .files
            .get(id.0 as usize)
            .ok_or(codespan_reporting::files::Error::FileMissing)?;
        if byte_index > file.text.len() {
            return Err(codespan_reporting::files::Error::IndexTooLarge {
                given: byte_index,
                max: file.text.len(),
            });
        }
        Ok(file.line_index(byte_index as u32))
    }

    fn line_range(&'a self, id: FileId, line_index: usize) -> Result<Range<usize>, codespan_reporting::files::Error> {
        let file = self
            .files
            .get(id.0 as usize)
            .ok_or(codespan_reporting::files::Error::FileMissing)?;
        // codespan expects the range to include the line's newline.
        let range = file
            .line_range(line_index)
            .ok_or(codespan_reporting::files::Error::LineTooLarge {
                given: line_index,
                max: file.line_count() - 1,
            })?;
        let end = if range.end < file.text.len() {
            range.end + 1
        } else {
            range.end
        };
        Ok(range.start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_crlf_and_bom() {
        let mut db = SourceDb::new();
        let id = db.add("a.loom", "\u{feff}let a\r\nlet b\r\n");
        assert_eq!(db.file(id).text(), "let a\nlet b\n");
    }

    #[test]
    fn line_col_counts_chars() {
        let mut db = SourceDb::new();
        let id = db.add("a.loom", "ab\n×yz\n");
        let file = db.file(id);
        assert_eq!(file.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(file.line_col(3), LineCol { line: 2, col: 1 });
        // '×' takes bytes 3..5, so byte 5 is 'y', the second character of line 2.
        assert_eq!(file.line_col(5), LineCol { line: 2, col: 2 });
        assert_eq!(file.line_range(1), Some(3..7));
    }
}
