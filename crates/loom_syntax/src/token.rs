//! Tokens produced by the lexer.

use std::fmt;

use crate::source::{SourceDb, Span};
use crate::twill::TwillPattern;

/// One token. `nl_before` and `line_indent` let the parser apply Loom's
/// continuation rule: a line starting with `.`, `?` or a binary operator
/// continues the statement only when it's indented deeper than the line the
/// statement began on.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// A newline (or a comment containing one) separates this token from the
    /// previous one. Always false for tokens inside string templates.
    pub nl_before: bool,
    /// Indentation of the line this token starts on, in columns (tab = 4).
    pub line_indent: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    /// `snake_case` names: values, functions, fields, modules. Contextual
    /// keywords (`take`, `from`, `get`, `once`) are lexed as identifiers.
    Ident,
    /// `PascalCase` names (or a single uppercase letter): types, wefts, cases.
    TypeIdent,
    /// `SCREAMING_CASE` names: constants.
    ConstIdent,
    Keyword(Keyword),
    Prim(PrimType),
    /// A lone `_`.
    Underscore,

    Int(IntLit),
    Float(FloatLit),
    /// A radix literal with capture letters or `?` digits, e.g. `0x8xy4`.
    /// Only valid in patterns; the parser rejects it anywhere else.
    Twill(Box<TwillPattern>),
    Char(char),
    /// A string with no interpolation (plain, raw, or multi-line), cooked.
    Str(String),
    /// A byte string `b"…"`.
    Bytes(Vec<u8>),

    /// An interpolated string is a flat run of tokens:
    /// `TemplateStart (TemplateText | InterpOpen expr-tokens [InterpSpec] InterpClose)* TemplateEnd`.
    TemplateStart,
    TemplateText(String),
    InterpOpen,
    /// The format spec after a top-level `:` inside `{…}`, without the colon.
    InterpSpec(String),
    InterpClose,
    TemplateEnd,

    /// `/// text` (the text after `///`, minus one leading space).
    DocComment(String),
    Punct(Punct),
    /// Something the lexer couldn't make sense of; a diagnostic was reported.
    Error,
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Radix {
    Bin,
    Oct,
    Dec,
    Hex,
}

impl Radix {
    pub fn value(self) -> u32 {
        match self {
            Radix::Bin => 2,
            Radix::Oct => 8,
            Radix::Dec => 10,
            Radix::Hex => 16,
        }
    }

    /// Bits per digit, for the power-of-two radixes.
    pub fn bits_per_digit(self) -> Option<u32> {
        match self {
            Radix::Bin => Some(1),
            Radix::Oct => Some(3),
            Radix::Hex => Some(4),
            Radix::Dec => None,
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Radix::Bin => "0b",
            Radix::Oct => "0o",
            Radix::Dec => "",
            Radix::Hex => "0x",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Radix::Bin => "binary",
            Radix::Oct => "octal",
            Radix::Dec => "decimal",
            Radix::Hex => "hexadecimal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntLit {
    pub value: u128,
    pub radix: Radix,
    /// An integer type suffix such as `u8` in `255u8`.
    pub suffix: Option<PrimType>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloatLit {
    pub value: f64,
    /// `f16`, `f32` or `f64`.
    pub suffix: Option<PrimType>,
}

/// Primitive type names, which are keywords.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrimType {
    /// `iN` / `uN` for N in 1..=128.
    Int {
        signed: bool,
        bits: u8,
    },
    Isize,
    Usize,
    F16,
    F32,
    F64,
    Bool,
    Char,
    Str,
    Never,
}

impl PrimType {
    pub fn parse(text: &str) -> Option<PrimType> {
        Some(match text {
            "isize" => PrimType::Isize,
            "usize" => PrimType::Usize,
            "f16" => PrimType::F16,
            "f32" => PrimType::F32,
            "f64" => PrimType::F64,
            "bool" => PrimType::Bool,
            "char" => PrimType::Char,
            "str" => PrimType::Str,
            "never" => PrimType::Never,
            _ => {
                let signed = match text.as_bytes().first()? {
                    b'i' => true,
                    b'u' => false,
                    _ => return None,
                };
                let digits = &text[1..];
                // No sign, no leading zeros: `u8` yes, `u08` and `u0` no.
                if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let bits: u8 = digits.parse().ok()?;
                if !(1..=128).contains(&bits) {
                    return None;
                }
                PrimType::Int { signed, bits }
            }
        })
    }

    pub fn is_int(self) -> bool {
        matches!(self, PrimType::Int { .. } | PrimType::Isize | PrimType::Usize)
    }

    pub fn is_float(self) -> bool {
        matches!(self, PrimType::F16 | PrimType::F32 | PrimType::F64)
    }
}

impl fmt::Display for PrimType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrimType::Int { signed: true, bits } => write!(f, "i{bits}"),
            PrimType::Int { signed: false, bits } => write!(f, "u{bits}"),
            PrimType::Isize => f.write_str("isize"),
            PrimType::Usize => f.write_str("usize"),
            PrimType::F16 => f.write_str("f16"),
            PrimType::F32 => f.write_str("f32"),
            PrimType::F64 => f.write_str("f64"),
            PrimType::Bool => f.write_str("bool"),
            PrimType::Char => f.write_str("char"),
            PrimType::Str => f.write_str("str"),
            PrimType::Never => f.write_str("never"),
        }
    }
}

macro_rules! keywords {
    ($($variant:ident = $text:literal,)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Keyword {
            $($variant,)*
        }

        impl Keyword {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Keyword::$variant => $text,)*
                }
            }

            /// Looks up a keyword spelled exactly like `text`. The glued forms
            /// `as%`, `as|` and `as?` are produced by the lexer, not by this.
            pub fn from_ident(text: &str) -> Option<Keyword> {
                match text {
                    $($text => Some(Keyword::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

keywords! {
    Fn = "fn",
    Let = "let",
    Var = "var",
    Const = "const",
    Type = "type",
    Warp = "warp",
    Weft = "weft",
    Weave = "weave",
    Into = "into",
    Through = "through",
    Case = "case",
    Pub = "pub",
    If = "if",
    Else = "else",
    Match = "match",
    For = "for",
    In = "in",
    While = "while",
    Loop = "loop",
    Break = "break",
    Continue = "continue",
    Return = "return",
    Fail = "fail",
    Is = "is",
    Not = "not",
    And = "and",
    Or = "or",
    Mend = "mend",
    Defer = "defer",
    Ref = "ref",
    Spool = "spool",
    Given = "given",
    Loom = "loom",
    Thread = "thread",
    Yield = "yield",
    Pull = "pull",
    Unsafe = "unsafe",
    Extern = "extern",
    Use = "use",
    As = "as",
    AsWrap = "as%",
    AsSat = "as|",
    AsChecked = "as?",
    Any = "any",
    SelfValue = "self",
    SelfType = "Self",
    Mut = "mut",
    True = "true",
    False = "false",
    Some = "some",
    None = "none",
    Ok = "ok",
    Err = "err",
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

macro_rules! puncts {
    ($($variant:ident = $text:literal,)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Punct {
            $($variant,)*
        }

        impl Punct {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Punct::$variant => $text,)*
                }
            }

            /// Every punctuation token, longest spellings first, which is
            /// the order the lexer tries them in (maximal munch).
            pub const BY_LENGTH: &'static [Punct] = &{
                let mut all = [$(Punct::$variant,)*];
                // Sort by descending length at compile time (insertion sort).
                let mut i = 1;
                while i < all.len() {
                    let mut j = i;
                    while j > 0 && all[j - 1].len() < all[j].len() {
                        let tmp = all[j - 1];
                        all[j - 1] = all[j];
                        all[j] = tmp;
                        j -= 1;
                    }
                    i += 1;
                }
                all
            };

            const fn len(self) -> usize {
                match self {
                    $(Punct::$variant => $text.len(),)*
                }
            }
        }
    };
}

puncts! {
    // three characters
    DotDotEq = "..=",
    PlusPercentEq = "+%=",
    MinusPercentEq = "-%=",
    StarPercentEq = "*%=",
    PlusPipeEq = "+|=",
    MinusPipeEq = "-|=",
    StarPipeEq = "*|=",
    ShlEq = "<<=",
    ShrEq = ">>=",
    // two characters
    DotDot = "..",
    EqEq = "==",
    NotEq = "!=",
    LtEq = "<=",
    GtEq = ">=",
    Shl = "<<",
    Shr = ">>",
    FatArrow = "=>",
    Arrow = "->",
    YieldArrow = "~>",
    PullArrow = "<~",
    PlusEq = "+=",
    MinusEq = "-=",
    StarEq = "*=",
    SlashEq = "/=",
    PercentEq = "%=",
    AmpEq = "&=",
    PipeEq = "|=",
    TildeEq = "~=",
    PlusPercent = "+%",
    MinusPercent = "-%",
    StarPercent = "*%",
    PlusPipe = "+|",
    MinusPipe = "-|",
    StarPipe = "*|",
    PlusQuestion = "+?",
    MinusQuestion = "-?",
    StarQuestion = "*?",
    // one character
    LParen = "(",
    RParen = ")",
    LBracket = "[",
    RBracket = "]",
    LBrace = "{",
    RBrace = "}",
    Comma = ",",
    Semi = ";",
    Colon = ":",
    Dot = ".",
    Question = "?",
    Caret = "^",
    At = "@",
    Hash = "#",
    Eq = "=",
    Lt = "<",
    Gt = ">",
    Plus = "+",
    Minus = "-",
    Star = "*",
    Slash = "/",
    Percent = "%",
    Amp = "&",
    Pipe = "|",
    Tilde = "~",
    Bang = "!",
}

impl fmt::Display for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A human-readable, one-line-per-token dump, used by `loom build --emit tokens`
/// and by tests. Format: `line:col  [nl+indent]  description`.
pub fn dump_tokens(db: &SourceDb, tokens: &[Token]) -> String {
    let mut out = String::new();
    for tok in tokens {
        let lc = db.line_col(tok.span);
        let nl = if tok.nl_before {
            format!("nl+{}", tok.line_indent)
        } else {
            String::new()
        };
        out.push_str(&format!("{:>4}:{:<4}{:<7}{}\n", lc.line, lc.col, nl, describe(db, tok)));
    }
    out
}

/// A short description of one token, e.g. `ident main` or `int 255 (u8)`.
pub fn describe(db: &SourceDb, tok: &Token) -> String {
    let text = db.slice(tok.span);
    match &tok.kind {
        TokenKind::Ident => format!("ident {text}"),
        TokenKind::TypeIdent => format!("type {text}"),
        TokenKind::ConstIdent => format!("const {text}"),
        TokenKind::Keyword(k) => format!("keyword {k}"),
        TokenKind::Prim(p) => format!("prim {p}"),
        TokenKind::Underscore => "underscore".into(),
        TokenKind::Int(i) => match i.suffix {
            Some(s) => format!("int {} ({s})", i.value),
            None => format!("int {}", i.value),
        },
        TokenKind::Float(x) => match x.suffix {
            Some(s) => format!("float {:?} ({s})", x.value),
            None => format!("float {:?}", x.value),
        },
        TokenKind::Twill(t) => format!("twill {text} {}", t.summary()),
        TokenKind::Char(c) => format!("char {c:?}"),
        TokenKind::Str(s) => format!("str {s:?}"),
        TokenKind::Bytes(b) => {
            let escaped: String = b
                .iter()
                .flat_map(|&c| std::ascii::escape_default(c))
                .map(char::from)
                .collect();
            format!("bytes b\"{escaped}\"")
        }
        TokenKind::TemplateStart => "template-start".into(),
        TokenKind::TemplateText(s) => format!("text {s:?}"),
        TokenKind::InterpOpen => "interp-open".into(),
        TokenKind::InterpSpec(s) => format!("spec {s:?}"),
        TokenKind::InterpClose => "interp-close".into(),
        TokenKind::TemplateEnd => "template-end".into(),
        TokenKind::DocComment(s) => format!("doc {s:?}"),
        TokenKind::Punct(p) => format!("punct {p}"),
        TokenKind::Error => format!("error {text:?}"),
        TokenKind::Eof => "eof".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prim_types_parse_strictly() {
        assert_eq!(PrimType::parse("u8"), Some(PrimType::Int { signed: false, bits: 8 }));
        assert_eq!(
            PrimType::parse("i128"),
            Some(PrimType::Int {
                signed: true,
                bits: 128
            })
        );
        assert_eq!(
            PrimType::parse("u12"),
            Some(PrimType::Int {
                signed: false,
                bits: 12
            })
        );
        assert_eq!(PrimType::parse("usize"), Some(PrimType::Usize));
        for not_prim in ["u0", "u129", "u08", "u", "i-1", "uint", "f128"] {
            assert_eq!(PrimType::parse(not_prim), None, "{not_prim}");
        }
    }

    #[test]
    fn puncts_are_ordered_longest_first() {
        let lens: Vec<usize> = Punct::BY_LENGTH.iter().map(|p| p.as_str().len()).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "{lens:?}");
    }

    #[test]
    fn keywords_round_trip() {
        for kw in [
            Keyword::Fn,
            Keyword::Defer,
            Keyword::Yield,
            Keyword::SelfType,
            Keyword::Unsafe,
        ] {
            assert_eq!(Keyword::from_ident(kw.as_str()), Some(kw));
        }
        assert_eq!(Keyword::from_ident("once"), None, "`once` is contextual");
        assert_eq!(Keyword::from_ident("take"), None, "`take` is contextual");
    }
}
