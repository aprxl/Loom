//! The registry of diagnostic codes. Never invent a code inline: add it here.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Code {
    pub id: &'static str,
    pub summary: &'static str,
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id)
    }
}

macro_rules! codes {
    ($($name:ident = $summary:literal;)*) => {
        $(pub const $name: Code = Code { id: stringify!($name), summary: $summary };)*

        /// Every registered code, in declaration order.
        pub const ALL: &[Code] = &[$($name),*];
    };
}

codes! {
    // S01xx: lexer
    S0100 = "unexpected character";
    S0101 = "unterminated block comment";
    S0102 = "unterminated string literal";
    S0103 = "integer literal is too large";
    S0104 = "invalid suffix on a numeric literal";
    S0105 = "invalid digit in a numeric literal";
    S0106 = "invalid twill pattern";
    S0107 = "invalid character literal";
    S0108 = "invalid escape sequence";
    S0109 = "unmatched brace in a string";
    S0110 = "non-ASCII character in a byte string";
    S0111 = "unterminated interpolation";

    // S02xx: parser
    S0200 = "unexpected token";
    S0201 = "unclosed delimiter";
    S0202 = "missing statement separator";
    S0203 = "comparison chain mixes directions";
    S0204 = "twill pattern outside a pattern";
    S0205 = "assignment used as an expression";
    S0206 = "hex digit used as a twill capture";
    S0207 = "name has the wrong case";
    S0208 = "parameter mode after the colon";
    S0209 = "`pub(get)` outside a field";
    S0210 = "missing initializer";
    S0211 = "nesting is too deep";

    // U01xx: features the v1 compiler doesn't support (see design/lua-backend.md)
    U0101 = "anonymous warp types aren't supported yet";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL {
            assert!(seen.insert(code.id), "duplicate code {}", code.id);
            assert_eq!(code.id.len(), 5, "{} should be a letter plus four digits", code.id);
            assert!(code.id.as_bytes()[0].is_ascii_uppercase());
            assert!(code.id[1..].bytes().all(|b| b.is_ascii_digit()));
        }
    }
}
