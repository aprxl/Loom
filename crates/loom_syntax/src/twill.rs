//! Twill patterns: radix literals with capture letters, like `0x8xy4`.
//!
//! In a twill pattern, digits must match exactly, `?` is a don't-care digit,
//! `_` is a separator, and every lowercase letter is a capture. All digits
//! sharing a letter are concatenated, most significant first, into one
//! unsigned integer exactly as wide as the bits they cover. In hex twill,
//! digits A–F must be uppercase: lowercase letters are always captures.
//!
//! The lexer produces [`TokenKind::Twill`](crate::token::TokenKind::Twill) for
//! literals that can only be twill (they contain a non-digit lowercase letter
//! or `?`). The parser also calls [`TwillPattern::parse`] on plain radix
//! literals that appear in pattern position, since `0x00e0` there captures `e`.

use std::fmt::Write as _;

use crate::token::Radix;

/// Twill patterns are at most 128 bits wide.
pub const MAX_WIDTH: u32 = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwillPattern {
    pub radix: Radix,
    /// Total width in bits: digits × bits per digit.
    pub width: u32,
    /// Bits that must match (`1` where a digit is fixed).
    pub mask: u128,
    /// The required value of the masked bits.
    pub value: u128,
    /// Captures in order of first appearance.
    pub captures: Vec<TwillCapture>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwillCapture {
    pub name: char,
    /// Bit positions (0 = least significant), most significant first. The
    /// captured value is these bits concatenated in this order.
    pub bits: Vec<u32>,
}

impl TwillCapture {
    pub fn width(&self) -> u32 {
        self.bits.len() as u32
    }

    /// `Some((shift, width))` when the bits form one contiguous run, so the
    /// capture is simply `(x >> shift) & ((1 << width) - 1)`.
    pub fn contiguous(&self) -> Option<(u32, u32)> {
        let lowest = *self.bits.last()?;
        let run = self.bits.iter().rev().enumerate().all(|(i, &b)| b == lowest + i as u32);
        run.then(|| (lowest, self.width()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TwillError {
    /// No digits at all (`0x`, `0b__`).
    Empty,
    /// Wider than [`MAX_WIDTH`] bits.
    TooWide(u32),
    /// A character that isn't a valid digit, `?`, `_`, or lowercase letter.
    /// The offset is in bytes from the start of the body (after the prefix).
    BadChar { ch: char, offset: usize },
    /// Decimal literals can't be twill patterns.
    Decimal,
}

impl TwillPattern {
    /// Parses a twill body (the text after `0x` / `0o` / `0b`).
    pub fn parse(body: &str, radix: Radix) -> Result<TwillPattern, TwillError> {
        let bits_per_digit = radix.bits_per_digit().ok_or(TwillError::Decimal)?;
        let digits: Vec<(usize, char)> = body.char_indices().filter(|&(_, c)| c != '_').collect();
        if digits.is_empty() {
            return Err(TwillError::Empty);
        }
        let width = digits.len() as u32 * bits_per_digit;
        if width > MAX_WIDTH {
            return Err(TwillError::TooWide(width));
        }

        let mut pattern = TwillPattern {
            radix,
            width,
            mask: 0,
            value: 0,
            captures: Vec::new(),
        };
        for (index, &(offset, ch)) in digits.iter().enumerate() {
            // This digit covers bits [low, low + bits_per_digit).
            let low = width - (index as u32 + 1) * bits_per_digit;
            let digit_mask = ((1u128 << bits_per_digit) - 1) << low;
            if ch == '?' {
                continue;
            }
            if ch.is_ascii_lowercase() {
                let capture = match pattern.captures.iter_mut().find(|c| c.name == ch) {
                    Some(c) => c,
                    None => {
                        pattern.captures.push(TwillCapture {
                            name: ch,
                            bits: Vec::new(),
                        });
                        pattern.captures.last_mut().expect("just pushed")
                    }
                };
                capture.bits.extend((low..low + bits_per_digit).rev());
                continue;
            }
            let digit = twill_digit(ch, radix).ok_or(TwillError::BadChar { ch, offset })?;
            pattern.mask |= digit_mask;
            pattern.value |= u128::from(digit) << low;
        }
        Ok(pattern)
    }

    /// Capture letters that are also hex digits (`a`–`f`) in a hex pattern.
    /// `0x00e0` in a pattern captures `e`, which is almost always a typo for
    /// `0x00E0`, so the parser warns about these.
    pub fn hex_digit_captures(&self) -> Vec<char> {
        if self.radix != Radix::Hex {
            return Vec::new();
        }
        self.captures
            .iter()
            .map(|c| c.name)
            .filter(|c| ('a'..='f').contains(c))
            .collect()
    }

    /// Whether this value matches the pattern.
    pub fn matches(&self, value: u128) -> bool {
        value & self.mask == self.value
    }

    /// Extracts every capture from a value, in [`TwillPattern::captures`] order.
    pub fn extract(&self, value: u128) -> Vec<u128> {
        self.captures
            .iter()
            .map(|c| c.bits.iter().fold(0u128, |acc, &bit| (acc << 1) | ((value >> bit) & 1)))
            .collect()
    }

    /// A compact description for token dumps, e.g. `w16 mask=0xF00F value=0x8004 x:u4 y:u4`.
    pub fn summary(&self) -> String {
        let mut s = format!("w{} mask={:#X} value={:#X}", self.width, self.mask, self.value);
        for c in &self.captures {
            let _ = write!(s, " {}:u{}", c.name, c.width());
        }
        s
    }
}

/// The value of a fixed digit in a twill pattern. Hex letters must be uppercase.
fn twill_digit(ch: char, radix: Radix) -> Option<u32> {
    let d = match ch {
        '0'..='9' => ch as u32 - '0' as u32,
        'A'..='F' if radix == Radix::Hex => ch as u32 - 'A' as u32 + 10,
        _ => return None,
    };
    (d < radix.value()).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(p: &TwillPattern, name: char) -> &TwillCapture {
        p.captures.iter().find(|c| c.name == name).expect("capture")
    }

    #[test]
    fn chip8_add() {
        let p = TwillPattern::parse("8xy4", Radix::Hex).expect("valid");
        assert_eq!(p.width, 16);
        assert_eq!(p.mask, 0xF00F);
        assert_eq!(p.value, 0x8004);
        assert_eq!(cap(&p, 'x').contiguous(), Some((8, 4)));
        assert_eq!(cap(&p, 'y').contiguous(), Some((4, 4)));
        assert!(p.matches(0x8124));
        assert!(!p.matches(0x8125));
        assert_eq!(p.extract(0x8124), vec![1, 2]);
    }

    #[test]
    fn address_capture_is_twelve_bits() {
        let p = TwillPattern::parse("1nnn", Radix::Hex).expect("valid");
        assert_eq!(cap(&p, 'n').width(), 12);
        assert_eq!(p.extract(0x1ABC), vec![0xABC]);
    }

    #[test]
    fn nine_bit_carry_split() {
        let p = TwillPattern::parse("c_ssss_ssss", Radix::Bin).expect("valid");
        assert_eq!((p.width, p.mask), (9, 0));
        assert_eq!(p.extract(300), vec![1, 44]);
    }

    #[test]
    fn dont_care_digits() {
        let p = TwillPattern::parse("10??_????", Radix::Bin).expect("valid");
        assert_eq!((p.mask, p.value), (0b1100_0000, 0b1000_0000));
        assert!(p.captures.is_empty());
    }

    #[test]
    fn scattered_capture_concatenates_msb_first() {
        let p = TwillPattern::parse("a1a0", Radix::Bin).expect("valid");
        let a = cap(&p, 'a');
        assert_eq!(a.bits, vec![3, 1]);
        assert_eq!(a.contiguous(), None);
        assert_eq!(p.extract(0b1010), vec![0b11]);
        assert_eq!(p.extract(0b0010), vec![0b01]);
    }

    #[test]
    fn lowercase_hex_letters_are_captures() {
        let p = TwillPattern::parse("00e0", Radix::Hex).expect("valid");
        assert_eq!(p.hex_digit_captures(), vec!['e']);
        assert_eq!(p.mask, 0xFF0F);
    }

    #[test]
    fn errors() {
        assert_eq!(TwillPattern::parse("__", Radix::Hex), Err(TwillError::Empty));
        assert_eq!(
            TwillPattern::parse("1G", Radix::Hex),
            Err(TwillError::BadChar { ch: 'G', offset: 1 })
        );
        assert_eq!(
            TwillPattern::parse("12", Radix::Bin),
            Err(TwillError::BadChar { ch: '2', offset: 1 })
        );
        assert_eq!(
            TwillPattern::parse("A", Radix::Oct),
            Err(TwillError::BadChar { ch: 'A', offset: 0 })
        );
        assert_eq!(
            TwillPattern::parse(&"f".repeat(33), Radix::Hex),
            Err(TwillError::TooWide(132))
        );
        assert_eq!(TwillPattern::parse("1x", Radix::Dec), Err(TwillError::Decimal));
    }
}
