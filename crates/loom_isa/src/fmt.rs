//! What a formatting instruction formats: a value's kind, and the format
//! spec after `:` in `{x:>8}`, parsed once while compiling.

use crate::IntSpec;

/// The kind of value a formatting native writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Int(IntSpec),
    F32,
    F64,
    Bool,
    Char,
    /// Text: a pointer and a length, in two registers.
    Str,
    Unit,
}

impl ValueKind {
    /// How many registers a value of this kind takes.
    pub fn regs(self) -> u16 {
        match self {
            ValueKind::Str => 2,
            ValueKind::Unit => 0,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// `[[fill]align][+][#][0][width][.precision][type]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    pub fill: Option<char>,
    pub align: Option<Align>,
    pub plus: bool,
    pub alt: bool,
    pub zero: bool,
    pub width: Option<u16>,
    pub precision: Option<u16>,
    /// `?`, `x`, `X`, `o` or `b`.
    pub ty: Option<char>,
}

impl Spec {
    /// Parses a spec the type checker has accepted. A `.` with no digits is
    /// a precision of 0.
    pub fn parse(s: &str) -> Option<Spec> {
        let chars: Vec<char> = s.chars().collect();
        let align = |c: char| match c {
            '<' => Some(Align::Left),
            '>' => Some(Align::Right),
            '^' => Some(Align::Center),
            _ => None,
        };
        let mut spec = Spec {
            fill: None,
            align: None,
            plus: false,
            alt: false,
            zero: false,
            width: None,
            precision: None,
            ty: None,
        };
        let mut i = 0;
        if let Some(a) = chars.get(1).and_then(|&c| align(c)) {
            spec.fill = Some(chars[0]);
            spec.align = Some(a);
            i = 2;
        } else if let Some(a) = chars.first().and_then(|&c| align(c)) {
            spec.align = Some(a);
            i = 1;
        }
        for (flag, set) in [('+', &mut spec.plus), ('#', &mut spec.alt), ('0', &mut spec.zero)] {
            if chars.get(i) == Some(&flag) {
                *set = true;
                i += 1;
            }
        }
        let number = |i: &mut usize| -> Option<Option<u16>> {
            let start = *i;
            while chars.get(*i).is_some_and(char::is_ascii_digit) {
                *i += 1;
            }
            if *i == start {
                return Some(None);
            }
            chars[start..*i].iter().collect::<String>().parse().ok().map(Some)
        };
        spec.width = number(&mut i)?;
        if chars.get(i) == Some(&'.') {
            i += 1;
            spec.precision = Some(number(&mut i)?.unwrap_or(0));
        }
        if let Some(&c) = chars.get(i).filter(|c| matches!(c, '?' | 'x' | 'X' | 'o' | 'b')) {
            spec.ty = Some(c);
            i += 1;
        }
        (i == chars.len()).then_some(spec)
    }
}

/// One thing a formatting native writes: a value of `kind`, in its debug
/// form or not, with a spec or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmtItem {
    pub kind: ValueKind,
    pub debug: bool,
    pub spec: Option<Spec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_part() {
        let s = Spec::parse("*^+#012.3x").expect("valid");
        assert_eq!(
            s,
            Spec {
                fill: Some('*'),
                align: Some(Align::Center),
                plus: true,
                alt: true,
                zero: true,
                width: Some(12),
                precision: Some(3),
                ty: Some('x')
            }
        );
        assert_eq!(Spec::parse(">8").and_then(|s| s.width), Some(8));
        assert_eq!(Spec::parse("★<5").and_then(|s| s.fill), Some('★'));
        assert_eq!(Spec::parse(".").and_then(|s| s.precision), Some(0));
        assert_eq!(Spec::parse("?").and_then(|s| s.ty), Some('?'));
        assert_eq!(Spec::parse("08b").map(|s| (s.zero, s.width, s.ty)), Some((true, Some(8), Some('b'))));
        assert_eq!(Spec::parse("q"), None);
        assert_eq!(Spec::parse("99999"), None);
    }
}
