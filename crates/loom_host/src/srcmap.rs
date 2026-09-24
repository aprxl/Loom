//! Source maps: which Loom line each line of a bundle came from.
//!
//! The compiler writes one next to every bundle (`out.lua.map`), and the
//! host reads it to turn a Lua error position (`bundle.lua:123:`) into a
//! Loom location. The format is plain text, one record per line:
//!
//! ```text
//! loom-map 1
//! file 0 examples/tour.loom
//! 123 0 45 9
//! ```
//!
//! A mapping line is `lua_line file_index loom_line loom_column`, all
//! 1-based, sorted by Lua line.

use std::fmt::Write as _;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceMap {
    pub files: Vec<String>,
    pub lines: Vec<Mapping>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub lua_line: u32,
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

/// A Loom source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

impl SourceMap {
    pub fn to_text(&self) -> String {
        let mut out = String::from("loom-map 1\n");
        for (i, file) in self.files.iter().enumerate() {
            let _ = writeln!(out, "file {i} {file}");
        }
        for m in &self.lines {
            let _ = writeln!(out, "{} {} {} {}", m.lua_line, m.file, m.line, m.column);
        }
        out
    }

    pub fn parse(text: &str) -> Option<SourceMap> {
        let mut lines = text.lines();
        if lines.next()? != "loom-map 1" {
            return None;
        }
        let mut map = SourceMap::default();
        for line in lines {
            if let Some(rest) = line.strip_prefix("file ") {
                let (_, name) = rest.split_once(' ')?;
                map.files.push(name.to_string());
                continue;
            }
            let mut nums = line.split(' ').map(|n| n.parse::<u32>().ok());
            let (lua_line, file, line, column) = (nums.next()??, nums.next()??, nums.next()??, nums.next()??);
            map.lines.push(Mapping { lua_line, file, line, column });
        }
        Some(map)
    }

    /// The Loom location of the statement that covers `lua_line`: the
    /// closest mapped line at or before it.
    pub fn locate(&self, lua_line: u32) -> Option<Location> {
        let i = self.lines.partition_point(|m| m.lua_line <= lua_line);
        let m = self.lines.get(i.checked_sub(1)?)?;
        Some(Location { file: self.files.get(m.file as usize)?.clone(), line: m.line, column: m.column })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_locates() {
        let map = SourceMap {
            files: vec!["a.loom".into()],
            lines: vec![
                Mapping { lua_line: 10, file: 0, line: 3, column: 5 },
                Mapping { lua_line: 14, file: 0, line: 4, column: 1 },
            ],
        };
        let parsed = SourceMap::parse(&map.to_text()).expect("parses");
        assert_eq!(parsed, map);
        assert_eq!(map.locate(9), None);
        assert_eq!(map.locate(12), Some(Location { file: "a.loom".into(), line: 3, column: 5 }));
        assert_eq!(map.locate(14).map(|l| l.line), Some(4));
    }
}
