//! String, template, byte-string and char literals.

use super::{Interp, Lexer, StrCtx};
use crate::diag::{Diagnostic, codes};
use crate::token::TokenKind;

/// What an escape sequence may produce.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeMode {
    /// Strings and chars: `\x` is limited to ASCII, `\u{…}` is allowed.
    Text,
    /// Byte strings: `\x` covers 00–FF, `\u{…}` isn't allowed.
    Bytes,
}

impl Lexer<'_> {
    /// `"` or `"""` at `start`.
    pub(super) fn string(&mut self, start: usize) {
        if self.src[start..].starts_with("\"\"\"") {
            self.multi_string(start);
        } else {
            self.pos = start + 1;
            self.string_body(StrCtx::Single, start, false);
        }
    }

    fn multi_string(&mut self, start: usize) {
        let open_end = start + 3;
        let bytes = self.src.as_bytes();

        // Find the closing `"""`, skipping escaped characters.
        let mut i = open_end;
        let close = loop {
            match bytes.get(i) {
                None => {
                    self.report(
                        Diagnostic::error(codes::S0102, "unterminated multi-line string")
                            .primary(self.span(start, open_end), "this string is never closed")
                            .help("close it with `\"\"\"`"),
                    );
                    self.pos = self.src.len();
                    self.push(TokenKind::Error, start, self.pos);
                    return;
                }
                Some(b'\\') => i += 2,
                Some(b'"') if bytes[i..].starts_with(b"\"\"\"") => break i,
                Some(_) => i += 1,
            }
        };

        // The line holding the opening quotes and the line holding the closing
        // quotes are dropped when they're blank, and the indentation shared by
        // all non-blank lines is stripped.
        let body = &self.src[open_end..close];
        let is_blank = |s: &str| s.trim_matches([' ', '\t']).is_empty();
        let content_start = match body.find('\n') {
            Some(nl) if is_blank(&body[..nl]) => open_end + nl + 1,
            _ => open_end,
        };
        let content_end = match body.rfind('\n') {
            Some(nl) if is_blank(&body[nl + 1..]) => (open_end + nl).max(content_start),
            _ => close,
        };
        let indent = self.src[content_start..content_end]
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.chars().take_while(|&c| c == ' ' || c == '\t').count())
            .min()
            .unwrap_or(0);

        self.pos = content_start;
        self.skip_indent(indent, content_end);
        self.string_body(StrCtx::Multi { indent, end: content_end, close }, start, false);
    }

    fn skip_indent(&mut self, indent: usize, end: usize) {
        for _ in 0..indent {
            match self.byte_at(self.pos) {
                Some(b' ' | b'\t') if self.pos < end => self.pos += 1,
                _ => break,
            }
        }
    }

    /// Scans string text from `pos` until the string ends or an interpolation
    /// opens. `in_template` is true when resuming after an interpolation.
    fn string_body(&mut self, ctx: StrCtx, start: usize, in_template: bool) {
        let mut text = String::new();
        let text_start = self.pos;
        loop {
            if let StrCtx::Multi { end, close, indent } = ctx {
                if self.pos >= end {
                    self.pos = close + 3;
                    self.end_string(start, in_template, text, text_start, close);
                    return;
                }
                if self.byte_at(self.pos) == Some(b'\n') {
                    self.pos += 1;
                    text.push('\n');
                    self.skip_indent(indent, end);
                    continue;
                }
            }

            let Some(c) = self.peek() else {
                self.unterminated_string(start, "the file ends before the closing `\"`");
                let pos = self.pos;
                self.end_string(start, in_template, text, text_start, pos);
                return;
            };
            match c {
                '"' if matches!(ctx, StrCtx::Single) => {
                    let quote = self.pos;
                    self.pos += 1;
                    self.end_string(start, in_template, text, text_start, quote);
                    return;
                }
                '\n' => {
                    // Only single-line strings get here.
                    self.unterminated_string(start, "the line ends before the closing `\"`");
                    let pos = self.pos;
                    self.end_string(start, in_template, text, text_start, pos);
                    return;
                }
                '\\' => {
                    if let Some(v) = self.escape(EscapeMode::Text) {
                        text.push(char::from_u32(v).unwrap_or(char::REPLACEMENT_CHARACTER));
                    }
                }
                '{' if self.byte_at(self.pos + 1) == Some(b'{') => {
                    text.push('{');
                    self.pos += 2;
                }
                '}' if self.byte_at(self.pos + 1) == Some(b'}') => {
                    text.push('}');
                    self.pos += 2;
                }
                '}' => {
                    self.report(
                        Diagnostic::error(codes::S0109, "unmatched `}` in a string")
                            .primary(self.span(self.pos, self.pos + 1), "no `{` opened here")
                            .help("write `}}` for a literal brace"),
                    );
                    text.push('}');
                    self.pos += 1;
                }
                '{' => {
                    if !in_template {
                        self.push(TokenKind::TemplateStart, start, start + ctx.quote_len());
                    }
                    if !text.is_empty() {
                        self.push_inner(TokenKind::TemplateText(text), text_start, self.pos);
                    }
                    self.push_inner(TokenKind::InterpOpen, self.pos, self.pos + 1);
                    self.pos += 1;
                    self.interps.push(Interp { depth: 0, ctx, start });
                    return;
                }
                c => {
                    text.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    /// Emits the end of a string: a `Str` token for a plain string, or the
    /// trailing text and `TemplateEnd` for a template. `close` is where the
    /// closing quotes start; `pos` is already past them.
    fn end_string(&mut self, start: usize, in_template: bool, text: String, text_start: usize, close: usize) {
        if in_template {
            if !text.is_empty() {
                self.push_inner(TokenKind::TemplateText(text), text_start, close);
            }
            self.push_inner(TokenKind::TemplateEnd, close, self.pos);
        } else {
            self.push(TokenKind::Str(text), start, self.pos);
        }
    }

    fn unterminated_string(&mut self, start: usize, why: &str) {
        let mut diag = Diagnostic::error(codes::S0102, "unterminated string literal")
            .primary(self.span(start, start + 1), "this string is never closed")
            .secondary(self.span(self.pos, self.pos), why);
        if why.contains("line") {
            diag = diag.help("strings can't span lines; use `\"\"\"…\"\"\"` for multi-line text");
        }
        self.report(diag);
    }

    /// `}` at depth 0 inside an interpolation: close it and resume the string.
    pub(super) fn close_interp(&mut self) {
        let interp = self.interps.pop().expect("close_interp called outside an interpolation");
        self.push_inner(TokenKind::InterpClose, self.pos, self.pos + 1);
        self.pos += 1;
        self.string_body(interp.ctx, interp.start, true);
    }

    /// `:` at depth 0 inside an interpolation: the rest up to `}` is a format spec.
    pub(super) fn interp_spec(&mut self) {
        let colon = self.pos;
        let spec_start = colon + 1;
        match self.src[spec_start..].find(['}', '\n']).map(|len| spec_start + len) {
            Some(end) if self.byte_at(end) == Some(b'}') => {
                let spec = self.src[spec_start..end].to_string();
                self.push_inner(TokenKind::InterpSpec(spec), colon, end);
                self.pos = end;
                self.close_interp();
            }
            found => {
                self.pos = found.unwrap_or(self.src.len());
                let start = self.interps.last().map_or(colon, |i| i.start);
                self.report(
                    Diagnostic::error(codes::S0111, "unterminated interpolation")
                        .primary(self.span(start, start + 1), "this string")
                        .secondary(self.span(colon, self.pos), "this format spec is never closed with `}`"),
                );
                self.abandon_interps();
            }
        }
    }

    /// `r"…"`: no escapes, no interpolation, single line.
    pub(super) fn raw_string(&mut self, start: usize) {
        self.pos = start + 2;
        let text_start = self.pos;
        match self.src[text_start..].find(['"', '\n']) {
            Some(len) if self.byte_at(text_start + len) == Some(b'"') => {
                let text = self.src[text_start..text_start + len].to_string();
                self.pos = text_start + len + 1;
                self.push(TokenKind::Str(text), start, self.pos);
            }
            found => {
                self.pos = found.map_or(self.src.len(), |len| text_start + len);
                self.unterminated_string(start, "the line ends before the closing `\"`");
                let text = self.src[text_start..self.pos].to_string();
                self.push(TokenKind::Str(text), start, self.pos);
            }
        }
    }

    /// `b"…"`: ASCII and escapes only, no interpolation.
    pub(super) fn byte_string(&mut self, start: usize) {
        self.pos = start + 2;
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                None | Some('\n') => {
                    self.unterminated_string(start, "the line ends before the closing `\"`");
                    break;
                }
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    if let Some(v) = self.escape(EscapeMode::Bytes) {
                        bytes.push(v as u8);
                    }
                }
                Some(c) if c.is_ascii() => {
                    bytes.push(c as u8);
                    self.pos += 1;
                }
                Some(c) => {
                    let lo = self.pos;
                    self.pos += c.len_utf8();
                    self.report(
                        Diagnostic::error(codes::S0110, format!("non-ASCII character `{c}` in a byte string"))
                            .primary(self.span(lo, self.pos), "byte strings only hold ASCII")
                            .help("write the bytes with `\\x` escapes"),
                    );
                }
            }
        }
        self.push(TokenKind::Bytes(bytes), start, self.pos);
    }

    /// `'x'` at `start`.
    pub(super) fn char_literal(&mut self, start: usize) {
        self.pos = start + 1;
        // `Err` means an invalid escape that was already reported.
        let value: Result<char, ()> = match self.peek() {
            None | Some('\n') => {
                self.unterminated_char(start);
                return;
            }
            Some('\'') => {
                self.pos += 1;
                self.report(
                    Diagnostic::error(codes::S0107, "empty character literal")
                        .primary(self.span(start, self.pos), "a char holds exactly one character"),
                );
                self.push(TokenKind::Error, start, self.pos);
                return;
            }
            Some('\\') => self.escape(EscapeMode::Text).and_then(char::from_u32).ok_or(()),
            Some(c) => {
                self.pos += c.len_utf8();
                Ok(c)
            }
        };

        if self.byte_at(self.pos) == Some(b'\'') {
            self.pos += 1;
            self.push(value.map_or(TokenKind::Error, TokenKind::Char), start, self.pos);
            return;
        }

        // More than one character before the closing quote, or no quote at all.
        let line_end = self.src[self.pos..].find('\n').map_or(self.src.len(), |i| self.pos + i);
        match self.src[self.pos..line_end].find('\'') {
            Some(i) => {
                self.pos += i + 1;
                self.report(
                    Diagnostic::error(codes::S0107, "character literal holds more than one character")
                        .primary(self.span(start, self.pos), "a char holds exactly one character")
                        .help("use double quotes for a string"),
                );
                self.push(TokenKind::Error, start, self.pos);
            }
            None => self.unterminated_char(start),
        }
    }

    fn unterminated_char(&mut self, start: usize) {
        self.report(
            Diagnostic::error(codes::S0107, "unterminated character literal")
                .primary(self.span(start, start + 1), "this char is never closed"),
        );
        self.push(TokenKind::Error, start, self.pos);
    }

    /// Reads an escape sequence at a backslash. Returns the code point, or
    /// `None` after reporting S0108 (the lexer has moved past it either way).
    fn escape(&mut self, mode: EscapeMode) -> Option<u32> {
        let start = self.pos;
        self.pos += 1;
        let Some(c) = self.bump() else {
            self.report(
                Diagnostic::error(codes::S0108, "unfinished escape sequence")
                    .primary(self.span(start, self.pos), "the file ends here"),
            );
            return None;
        };
        let simple = match c {
            'n' => Some(0x0A),
            'r' => Some(0x0D),
            't' => Some(0x09),
            '0' => Some(0x00),
            '\\' => Some(0x5C),
            '\'' => Some(0x27),
            '"' => Some(0x22),
            _ => None,
        };
        if simple.is_some() {
            return simple;
        }
        match c {
            'x' => {
                let digits = self.src.get(self.pos..self.pos + 2).filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()));
                let Some(digits) = digits else {
                    self.report(
                        Diagnostic::error(codes::S0108, "`\\x` needs exactly two hex digits")
                            .primary(self.span(start, self.pos), "e.g. `\\x1b`"),
                    );
                    return None;
                };
                self.pos += 2;
                let v = u32::from_str_radix(digits, 16).ok()?;
                if mode == EscapeMode::Text && v > 0x7F {
                    self.report(
                        Diagnostic::error(codes::S0108, "`\\x` escapes in text must be ASCII (at most `\\x7F`)")
                            .primary(self.span(start, self.pos), "out of range")
                            .help(format!("use `\\u{{{v:X}}}` for the character U+{v:04X}")),
                    );
                    return None;
                }
                Some(v)
            }
            'u' => {
                let body_start = self.pos;
                let close = if self.byte_at(self.pos) == Some(b'{') {
                    self.src[self.pos..].find(['}', '"', '\'', '\n']).map(|i| self.pos + i)
                } else {
                    None
                };
                let Some(close) = close.filter(|&i| self.byte_at(i) == Some(b'}')) else {
                    self.report(
                        Diagnostic::error(codes::S0108, "`\\u` needs braces around its hex digits")
                            .primary(self.span(start, self.pos), "e.g. `\\u{1F9F6}`"),
                    );
                    return None;
                };
                self.pos = close + 1;
                let digits = &self.src[body_start + 1..close];
                let span = self.span(start, self.pos);
                if mode == EscapeMode::Bytes {
                    self.report(
                        Diagnostic::error(codes::S0108, "byte strings can't contain `\\u` escapes")
                            .primary(span, "not a byte"),
                    );
                    return None;
                }
                let valid_len = (1..=6).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit());
                match u32::from_str_radix(digits, 16).ok().filter(|_| valid_len).and_then(char::from_u32) {
                    Some(ch) => Some(ch as u32),
                    None => {
                        self.report(
                            Diagnostic::error(codes::S0108, format!("`\\u{{{digits}}}` isn't a Unicode scalar value"))
                                .primary(span, "use 1 to 6 hex digits naming a valid character"),
                        );
                        None
                    }
                }
            }
            other => {
                self.report(
                    Diagnostic::error(codes::S0108, format!("unknown escape `\\{other}`"))
                        .primary(self.span(start, self.pos), "not a valid escape")
                        .help("valid escapes: \\n \\r \\t \\0 \\\\ \\' \\\" \\xHH \\u{…}"),
                );
                None
            }
        }
    }
}
