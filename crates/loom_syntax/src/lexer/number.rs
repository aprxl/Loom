//! Numeric literals: decimal, radix (`0x`, `0o`, `0b`), floats, suffixes,
//! and twill patterns.

use super::{Lexer, is_ident_continue};
use crate::diag::{Diagnostic, codes};
use crate::token::{FloatLit, IntLit, PrimType, Punct, Radix, TokenKind};
use crate::twill::{MAX_WIDTH, TwillError, TwillPattern};

/// Parses digits (with `_` separators) into a `u128`. `None` on overflow or a
/// bad digit.
fn parse_digits(digits: &str, radix: Radix) -> Option<u128> {
    let base = u128::from(radix.value());
    let mut value: u128 = 0;
    for c in digits.chars().filter(|&c| c != '_') {
        let d = c.to_digit(radix.value())?;
        value = value.checked_mul(base)?.checked_add(u128::from(d))?;
    }
    Some(value)
}

/// Digits valid in `radix` (either case for hex), with at least one digit.
fn is_radix_digits(text: &str, radix: Radix) -> bool {
    text.chars().any(|c| c != '_') && text.chars().all(|c| c == '_' || c.is_digit(radix.value()))
}

/// Splits `FFu8` into (`FF`, `u8`) when the tail is an integer type suffix.
fn split_int_suffix(body: &str, radix: Radix) -> Option<(&str, PrimType)> {
    body.char_indices().filter(|&(_, c)| c == 'u' || c == 'i').find_map(|(k, _)| {
        let suffix = PrimType::parse(&body[k..]).filter(|p| p.is_int())?;
        is_radix_digits(&body[..k], radix).then(|| (&body[..k], suffix))
    })
}

impl Lexer<'_> {
    pub(super) fn number(&mut self, start: usize) {
        if self.byte_at(start) == Some(b'0') {
            let radix = match self.byte_at(start + 1) {
                Some(b'x') => Some(Radix::Hex),
                Some(b'o') => Some(Radix::Oct),
                Some(b'b') => Some(Radix::Bin),
                _ => None,
            };
            if let Some(radix) = radix {
                self.radix_number(start, radix);
                return;
            }
        }
        self.decimal_number(start);
    }

    fn radix_number(&mut self, start: usize, radix: Radix) {
        let body_start = start + 2;
        let mut end = body_start;
        while let Some(b) = self.byte_at(end) {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'?' {
                end += 1;
            } else {
                break;
            }
        }
        self.pos = end;
        let src = self.src;
        let body = &src[body_start..end];

        // 1. Plain digits, possibly with an integer suffix: `0xFF`, `0b1010u8`.
        let int = if is_radix_digits(body, radix) {
            Some((body, None))
        } else {
            split_int_suffix(body, radix).map(|(digits, suffix)| (digits, Some(suffix)))
        };
        if let Some((digits, suffix)) = int {
            self.push_int(start, digits, radix, suffix);
            return;
        }

        // 2. A twill pattern: it has `?` digits or capture letters.
        let non_digit_lowercase =
            |b: u8| b.is_ascii_lowercase() && !(radix == Radix::Hex && (b'a'..=b'f').contains(&b));
        if body.bytes().any(|b| b == b'?' || non_digit_lowercase(b)) {
            match TwillPattern::parse(body, radix) {
                Ok(pattern) => self.push(TokenKind::Twill(Box::new(pattern)), start, end),
                Err(err) => {
                    self.twill_error(start, body_start, end, radix, err);
                    self.push(TokenKind::Error, start, end);
                }
            }
            return;
        }

        // 3. Invalid.
        let diag = match body.char_indices().find(|&(_, c)| c != '_' && !c.is_digit(radix.value())) {
            Some((offset, bad)) => {
                let lo = body_start + offset;
                let mut diag =
                    Diagnostic::error(codes::S0105, format!("invalid digit `{bad}` in a {} literal", radix.name()))
                        .primary(self.span(lo, lo + bad.len_utf8()), "not a digit in this base");
                if radix == Radix::Hex && bad.is_ascii_uppercase() {
                    diag = diag.help("capture letters in a twill pattern are lowercase; hex digits are 0-9 and A-F");
                }
                diag
            }
            None => Diagnostic::error(codes::S0105, format!("`{}` needs at least one digit", radix.prefix()))
                .primary(self.span(start, end), "no digits"),
        };
        self.report(diag);
        self.push(TokenKind::Error, start, end);
    }

    fn twill_error(&mut self, start: usize, body_start: usize, end: usize, radix: Radix, err: TwillError) {
        let diag = match err {
            TwillError::Empty | TwillError::Decimal => {
                Diagnostic::error(codes::S0106, "twill pattern has no digits").primary(self.span(start, end), "empty")
            }
            TwillError::TooWide(width) => {
                Diagnostic::error(codes::S0106, format!("twill pattern is {width} bits wide; the limit is {MAX_WIDTH}"))
                    .primary(self.span(start, end), "too wide")
            }
            TwillError::BadChar { ch, offset } => {
                let lo = body_start + offset;
                let mut diag =
                    Diagnostic::error(codes::S0106, format!("`{ch}` can't appear in a {} twill pattern", radix.name()))
                        .primary(self.span(lo, lo + ch.len_utf8()), "not a digit, `?`, `_` or capture letter");
                if ch.is_ascii_uppercase() {
                    diag = diag.help(if radix == Radix::Hex {
                        "capture letters are lowercase; hex digits are 0-9 and A-F"
                    } else {
                        "capture letters are lowercase"
                    });
                }
                diag
            }
        };
        self.report(diag);
    }

    fn decimal_number(&mut self, start: usize) {
        // After `.`, a number is a tuple index: `t.0.1` is `t . 0 . 1`, not `t . 0.1`.
        let tuple_index = matches!(self.tokens.last(), Some(t) if t.kind == TokenKind::Punct(Punct::Dot));
        let digit_or_sep = |b: u8| b.is_ascii_digit() || b == b'_';

        while self.byte_at(self.pos).is_some_and(digit_or_sep) {
            self.pos += 1;
        }
        let mut is_float = false;
        if !tuple_index {
            // A fraction needs a digit right after the dot, so `1..2` and `64.kib()` stay integers.
            if self.byte_at(self.pos) == Some(b'.') && self.byte_at(self.pos + 1).is_some_and(|b| b.is_ascii_digit()) {
                is_float = true;
                self.pos += 1;
                while self.byte_at(self.pos).is_some_and(digit_or_sep) {
                    self.pos += 1;
                }
            }
            if matches!(self.byte_at(self.pos), Some(b'e' | b'E')) {
                let signed = matches!(self.byte_at(self.pos + 1), Some(b'+' | b'-'));
                let digit_at = self.pos + 1 + usize::from(signed);
                if self.byte_at(digit_at).is_some_and(|b| b.is_ascii_digit()) {
                    is_float = true;
                    self.pos = digit_at;
                    while self.byte_at(self.pos).is_some_and(digit_or_sep) {
                        self.pos += 1;
                    }
                }
            }
        }
        let number_end = self.pos;

        let mut suffix = None;
        if self.peek().is_some_and(|c| c.is_alphabetic()) {
            while self.peek().is_some_and(is_ident_continue) {
                self.bump();
            }
            let src = self.src;
            let text = &src[number_end..self.pos];
            match PrimType::parse(text).filter(|p| p.is_int() || p.is_float()) {
                Some(p) if p.is_int() && is_float => {
                    self.report(
                        Diagnostic::error(
                            codes::S0104,
                            format!("a float literal can't have the integer suffix `{text}`"),
                        )
                        .primary(self.span(number_end, self.pos), "integer suffix"),
                    );
                    self.push(TokenKind::Error, start, self.pos);
                    return;
                }
                Some(p) => suffix = Some(p),
                None => {
                    self.report(
                        Diagnostic::error(codes::S0104, format!("invalid suffix `{text}` on a numeric literal"))
                            .primary(self.span(number_end, self.pos), "not a numeric type")
                            .help("suffixes are integer or float types, like `u8`, `usize` or `f32`"),
                    );
                    self.push(TokenKind::Error, start, self.pos);
                    return;
                }
            }
        }

        let digits: String = self.src[start..number_end].chars().filter(|&c| c != '_').collect();
        if is_float || suffix.is_some_and(PrimType::is_float) {
            // Only digits, one dot and an exponent are left, which Rust's parser accepts.
            let value = digits.parse::<f64>().unwrap_or(f64::NAN);
            self.push(TokenKind::Float(FloatLit { value, suffix }), start, self.pos);
        } else {
            let src = self.src;
            self.push_int(start, &src[start..number_end], Radix::Dec, suffix);
        }
    }

    fn push_int(&mut self, start: usize, digits: &str, radix: Radix, suffix: Option<PrimType>) {
        match parse_digits(digits, radix) {
            Some(value) => self.push(TokenKind::Int(IntLit { value, radix, suffix }), start, self.pos),
            None => {
                self.report(
                    Diagnostic::error(codes::S0103, "integer literal is too large")
                        .primary(self.span(start, self.pos), "doesn't fit in 128 bits"),
                );
                self.push(TokenKind::Error, start, self.pos);
            }
        }
    }
}
