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
    S0212 = "test inside a function";
    S0213 = "function without a body";

    // N01xx: names and modules
    N0101 = "unknown name";
    N0102 = "unknown module";
    N0103 = "item is private";
    N0104 = "name defined twice";
    N0105 = "not found in module";
    N0106 = "`self` outside a method";
    N0107 = "`Self` outside a warp, weft or weave";
    N0108 = "`break` or `continue` outside a loop";
    N0109 = "`_` placeholder outside a call argument";
    N0110 = "import conflicts with another name";
    N0111 = "expected a type";
    N0112 = "expected a value";
    N0113 = "unknown region";
    N0114 = "`yield` or `pull` outside a shuttle";
    N0115 = "or-pattern alternatives bind different names";
    N0116 = "no `main` function";

    // T01xx: types
    T0101 = "mismatched types";
    T0102 = "literal out of range";
    T0103 = "operator doesn't apply to this type";
    T0104 = "wrong number of arguments";
    T0105 = "bad named argument";
    T0106 = "not callable";
    T0107 = "can't assign here";
    T0108 = "type doesn't weave a required weft";
    T0109 = "cast not allowed";
    T0110 = "can't infer a type";
    T0111 = "not a value of this kind";
    T0112 = "`mut` argument doesn't match its parameter";
    T0113 = "weave is missing a method";
    T0114 = "method doesn't match its weft";
    T0115 = "weave not allowed";
    T0116 = "ambiguous method";
    T0117 = "not a format spec";
    T0118 = "control flow can't leave a `defer`";
    T0119 = "a test's signature isn't `test name()` or `test name() ! E`";
    T0120 = "`yield` or `pull` doesn't fit the shuttle";
    T0121 = "shuttle calls itself";

    // G02xx: gauges
    G0201 = "value outside a gauge";

    // K01xx: compile-time code
    K0101 = "a `const assert` failed";
    K0102 = "compile-time code ran out of its budget";
    K0103 = "compile-time code unravelled";
    K0104 = "compile-time code can't do I/O";
    K0105 = "a runtime value in compile-time code";
    K0106 = "control flow can't leave compile-time code";
    K0107 = "a constant needs its own value";

    // L03xx: `once` values
    L0301 = "loose end: a `once` value isn't used up";

    // M01xx: moves and borrows
    M0101 = "use of a moved value";
    M0102 = "move out of a borrow";
    M0103 = "move out of an element";
    M0104 = "`mut` access overlaps another use";
    M0105 = "a borrow is used after what it borrows changed";

    // P01xx: patterns
    P0101 = "match isn't exhaustive";
    P0102 = "refutable pattern in a binding";

    // U01xx: features the v1 compiler doesn't support (see design/lua-backend.md)
    U0101 = "anonymous warp types aren't supported yet";
    U0102 = "not implemented yet";
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
