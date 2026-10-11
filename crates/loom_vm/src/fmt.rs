//! Formatting values for templates and `print`: `{x}`, `{x:?}`, `{x:>8}`.
//!
//! This is the Lua runtime's `rt.show` and `rt.fmt` (runtime/rt/core.lua)
//! for the values Treadle formats natively. Floats print as Rust prints
//! them (shortest round-trip digits, never an exponent), which is what the
//! Lua runtime imitates; design/vm.md P5 makes Rust's the reference.

use loom_isa::{Align, FmtItem, IntSpec, Spec, ValueKind};

/// A value to format: its registers, and for text, its bytes.
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    Reg(u64),
    Text(&'a [u8]),
}

/// Appends `value`, formatted as `item` says, to `out`.
pub fn write(out: &mut Vec<u8>, item: &FmtItem, value: Value) {
    match &item.spec {
        None => show(out, item.kind, value, item.debug),
        Some(spec) => with_spec(out, item.kind, value, spec),
    }
}

fn reg(value: Value) -> u64 {
    match value {
        Value::Reg(r) => r,
        Value::Text(_) => 0,
    }
}

/// A value's own form (`{x}`), or its debug form (`{x:?}`).
pub fn show(out: &mut Vec<u8>, kind: ValueKind, value: Value, debug: bool) {
    let r = reg(value);
    match kind {
        ValueKind::Int(spec) => {
            let mut digits = [0u8; 20];
            let text = if spec.signed { itoa_i64(r as i64, &mut digits) } else { itoa_u64(r, &mut digits) };
            out.extend_from_slice(text);
        }
        ValueKind::F64 => float_into(out, f64::from_bits(r), false, debug),
        ValueKind::F32 => float_into(out, f64::from(f32::from_bits(r as u32)), true, debug),
        ValueKind::Bool => out.extend_from_slice(if r != 0 { b"true" } else { b"false" }),
        ValueKind::Char => {
            let c = char::from_u32(r as u32).unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buf = [0; 4];
            let text = c.encode_utf8(&mut buf).as_bytes();
            if debug {
                out.push(b'\'');
                escape(out, text, b'\'');
                out.push(b'\'');
            } else {
                out.extend_from_slice(text);
            }
        }
        ValueKind::Str => {
            let Value::Text(text) = value else { return };
            if debug {
                out.push(b'"');
                escape(out, text, b'"');
                out.push(b'"');
            } else {
                out.extend_from_slice(text);
            }
        }
        ValueKind::Unit => out.extend_from_slice(b"()"),
    }
}

/// `v` in decimal, in the end of `buf`: the digits' bytes.
fn itoa_u64(v: u64, buf: &mut [u8; 20]) -> &[u8] {
    let at = digits_at(v, buf);
    &buf[at..]
}

/// Writes `v`'s digits at the end of `buf`: where they start.
fn digits_at(mut v: u64, buf: &mut [u8; 20]) -> usize {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            return at;
        }
    }
}

/// `v` in decimal: `-` and the digits, in a buffer of 20 bytes (`i64::MIN`
/// is 19 digits and the sign).
fn itoa_i64(v: i64, buf: &mut [u8; 20]) -> &[u8] {
    let mut at = digits_at(v.unsigned_abs(), buf);
    if v < 0 {
        at -= 1;
        buf[at] = b'-';
    }
    &buf[at..]
}

/// Rust's Display for a float, and in the debug form `.0` after a whole
/// number. `single` is for an `f32`, shown with the digits that read back
/// as the same `f32`.
fn float_into(out: &mut Vec<u8>, v: f64, single: bool, debug: bool) {
    use std::io::Write as _;
    let start = out.len();
    let _ = if single { write!(out, "{}", v as f32) } else { write!(out, "{v}") };
    if debug && !out[start..].iter().any(|&b| b == b'.' || b.is_ascii_alphabetic()) {
        out.extend_from_slice(b".0");
    }
}

/// Text inside quotes in the debug form: the quote, `\` and control
/// characters are escaped.
fn escape(out: &mut Vec<u8>, text: &[u8], quote: u8) {
    for &b in text {
        match b {
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0 => out.extend_from_slice(b"\\0"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b if b == quote => {
                out.push(b'\\');
                out.push(b);
            }
            b if b < 0x20 || b == 0x7F => out.extend_from_slice(format!("\\u{{{b:x}}}").as_bytes()),
            b => out.push(b),
        }
    }
}

/// `{x:spec}`.
fn with_spec(out: &mut Vec<u8>, kind: ValueKind, value: Value, spec: &Spec) {
    let r = reg(value);
    let numeric = matches!(kind, ValueKind::Int(_) | ValueKind::F32 | ValueKind::F64);
    let float = match kind {
        ValueKind::F64 => Some(f64::from_bits(r)),
        ValueKind::F32 => Some(f64::from(f32::from_bits(r as u32))),
        _ => None,
    };
    let mut prefix = "";
    let mut body = Vec::new();
    match (kind, spec.ty, float, spec.precision) {
        (ValueKind::Int(int), Some(ty), _, _) if ty != '?' => {
            let (text, alt) = radix(r, int, ty);
            body.extend_from_slice(text.as_bytes());
            if spec.alt {
                prefix = alt;
            }
        }
        (_, _, Some(f), Some(p)) if f.is_finite() => {
            body.extend_from_slice(format!("{f:.0$}", usize::from(p)).as_bytes());
        }
        _ => show(&mut body, kind, value, spec.ty == Some('?')),
    }
    let mut sign = "";
    if numeric {
        if body.first() == Some(&b'-') {
            sign = "-";
            body.remove(0);
        } else if spec.plus {
            sign = "+";
        }
    }
    let Some(width) = spec.width.map(usize::from) else {
        out.extend_from_slice(sign.as_bytes());
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(&body);
        return;
    };
    let len = sign.len() + prefix.len() + String::from_utf8_lossy(&body).chars().count();
    if len >= width {
        out.extend_from_slice(sign.as_bytes());
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(&body);
        return;
    }
    let gap = width - len;
    if spec.zero && numeric {
        out.extend_from_slice(sign.as_bytes());
        out.extend_from_slice(prefix.as_bytes());
        out.extend(std::iter::repeat_n(b'0', gap));
        out.extend_from_slice(&body);
        return;
    }
    let mut fill = [0; 4];
    let fill = spec.fill.unwrap_or(' ').encode_utf8(&mut fill).as_bytes().to_vec();
    let align = spec.align.unwrap_or(if numeric { Align::Right } else { Align::Left });
    let (left, right) = match align {
        Align::Left => (0, gap),
        Align::Right => (gap, 0),
        Align::Center => (gap / 2, gap - gap / 2),
    };
    for _ in 0..left {
        out.extend_from_slice(&fill);
    }
    out.extend_from_slice(sign.as_bytes());
    out.extend_from_slice(prefix.as_bytes());
    out.extend_from_slice(&body);
    for _ in 0..right {
        out.extend_from_slice(&fill);
    }
}

/// `text`, already written, padded as `spec` says (as text: aligned left
/// unless it says otherwise).
pub fn pad(out: &mut Vec<u8>, text: &[u8], spec: &Spec) {
    let plain = Spec { ty: None, precision: None, plus: false, alt: false, zero: false, ..*spec };
    with_spec(out, ValueKind::Str, Value::Text(text), &plain);
}

/// The bits of `r` (two's complement, within the type's width) in radix
/// `ty`, and the prefix `#` adds.
fn radix(r: u64, int: IntSpec, ty: char) -> (String, &'static str) {
    let bits = if int.bits >= 64 { r } else { r & ((1u64 << int.bits) - 1) };
    match ty {
        'x' => (format!("{bits:x}"), "0x"),
        'X' => (format!("{bits:X}"), "0x"),
        'o' => (format!("{bits:o}"), "0o"),
        _ => (format!("{bits:b}"), "0b"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(kind: ValueKind, value: Value, spec: &str) -> String {
        let item = FmtItem { kind, debug: false, spec: Spec::parse(spec) };
        let item = if spec.is_empty() { FmtItem { spec: None, ..item } } else { item };
        let mut out = Vec::new();
        write(&mut out, &item, value);
        String::from_utf8(out).expect("utf-8")
    }

    fn f64v(v: f64) -> Value<'static> {
        Value::Reg(v.to_bits())
    }

    #[test]
    fn floats_print_like_rust() {
        // The Lua runtime's `floats_print_like_rust` cases (rt_int.rs).
        for (v, want) in [
            (17.0, "17"),
            (-3.0, "-3"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (1e20, "100000000000000000000"),
            (1.25e-7, "0.000000125"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (-0.0, "-0"),
        ] {
            assert_eq!(fmt(ValueKind::F64, f64v(v), ""), want, "{v}");
        }
        let item = FmtItem { kind: ValueKind::F64, debug: true, spec: None };
        let mut out = Vec::new();
        write(&mut out, &item, f64v(17.0));
        assert_eq!(out, b"17.0");
        assert_eq!(fmt(ValueKind::F64, f64v(1.23456), ".2"), "1.23");
        assert_eq!(fmt(ValueKind::F32, Value::Reg(u64::from(0.1f32.to_bits())), ""), "0.1");
    }

    #[test]
    fn specs_match_the_lua_runtime() {
        let int = |bits, signed| ValueKind::Int(IntSpec::new(bits, signed));
        assert_eq!(fmt(int(8, false), Value::Reg(255), "08b"), "11111111");
        assert_eq!(fmt(int(16, false), Value::Reg(224), "04X"), "00E0");
        assert_eq!(fmt(int(8, true), Value::Reg(-1i64 as u64), "x"), "ff");
        assert_eq!(fmt(int(32, true), Value::Reg(42), ">6"), "    42");
        assert_eq!(fmt(ValueKind::Str, Value::Text(b"ab"), "*^6"), "**ab**");
        assert_eq!(fmt(int(32, true), Value::Reg(-5i64 as u64), "+05"), "-0005");
        assert_eq!(fmt(ValueKind::Str, Value::Text(b"a\"b\n"), "?"), "\"a\\\"b\\n\"");
        assert_eq!(fmt(int(64, false), Value::Reg(u64::MAX), ""), "18446744073709551615");
        assert_eq!(fmt(int(32, true), Value::Reg(7), "#x"), "0x7");
        assert_eq!(fmt(int(32, true), Value::Reg(7), "+"), "+7");
        assert_eq!(fmt(ValueKind::Char, Value::Reg('\'' as u64), "?"), "'\\''");
        assert_eq!(fmt(ValueKind::Char, Value::Reg('é' as u64), "-^5"), "--é--");
        assert_eq!(fmt(ValueKind::Bool, Value::Reg(1), "<6"), "true  ");
        assert_eq!(fmt(ValueKind::Str, Value::Text(b"\x01"), "?"), "\"\\u{1}\"");
    }
}
