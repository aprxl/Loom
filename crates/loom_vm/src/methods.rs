//! Natives behind std's methods of text, integers and floats: the leaves
//! the compiler's `#builtin` bodies call (design/vm.md D9). Text is UTF-8,
//! as an address and a length; a `String` is a header of three words (its
//! buffer, length and capacity) at an address the compiler gives.
//!
//! What they do matches `runtime/rt/core.lua`, case by case: `trim` and
//! `is_whitespace` mean ASCII whitespace, `to_upper` changes ASCII letters.

#![allow(unsafe_code)]

use crate::Trap;
use crate::ints;
use crate::natives::{Ctx, NativeFn};

/// The natives of this module, by name.
pub(crate) fn resolve(name: &str) -> Option<NativeFn> {
    Some(match name {
        "str.char_count" => char_count,
        "str.find" => find,
        "str.starts_with" => starts_with,
        "str.ends_with" => ends_with,
        "str.trim" => trim,
        "str.case" => case,
        "str.replace" => replace,
        "str.repeat" => repeat,
        "str.split" => split,
        "str.parse_int" => parse_int,
        "str.parse_float" => parse_float,
        "string.of" => string_of,
        "string.push_char" => push_char,
        "string.push_str" => push_str,
        "string.pop" => pop,
        "char.digit" => digit,
        "str.next_char" => next_char,
        "str.hash" => str_hash,
        "str.concat2" | "str.concat3" | "str.concat4" => concat,
        "map.find_str" => map_find_str,
        "map.find_int" => map_find_int,
        "str.next_line" => next_line,
        "str.next_word" => next_word,
        "int.bits" => int_bits,
        "int.rotate" => int_rotate,
        "float.math" => float_math,
        _ => return None,
    })
}

/// The text at `ptr`, `len` bytes, which the compiler passes as a text value.
fn text<'c>(ctx: &'c Ctx, ptr: u64, len: u64) -> Result<&'c [u8], Trap> {
    // SAFETY: every caller passes a `str` the compiler built.
    unsafe { ctx.text(ptr, len) }
}

fn is_space(b: u8) -> bool {
    b == b' ' || (9..=13).contains(&b)
}

/// The position of `needle` in `hay`, if it's there.
fn position(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `str.char_count(s)`.
fn char_count(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    args[0] = s.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u64;
    Ok(())
}

/// `str.find(s, part)`: (where, whether found).
fn find(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let at = position(text(ctx, args[0], args[1])?, text(ctx, args[2], args[3])?);
    args[0] = at.unwrap_or(0) as u64;
    args[1] = u64::from(at.is_some());
    Ok(())
}

fn starts_with(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let yes = text(ctx, args[0], args[1])?.starts_with(text(ctx, args[2], args[3])?);
    args[0] = u64::from(yes);
    Ok(())
}

fn ends_with(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let yes = text(ctx, args[0], args[1])?.ends_with(text(ctx, args[2], args[3])?);
    args[0] = u64::from(yes);
    Ok(())
}

/// `str.trim(s)` with `imm` 0 (both ends), 1 (the start) or 2 (the end):
/// the text left, as an address and a length.
fn trim(ctx: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let mut start = 0;
    let mut end = s.len();
    if imm != 2 {
        while start < end && is_space(s[start]) {
            start += 1;
        }
    }
    if imm != 1 {
        while end > start && is_space(s[end - 1]) {
            end -= 1;
        }
    }
    args[0] += start as u64;
    args[1] = (end - start) as u64;
    Ok(())
}

/// `str.case(s, dst)`: a `String` of `s` with its ASCII letters in upper
/// case (`imm` 0) or lower case (1).
fn case(ctx: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    let mut s = text(ctx, args[0], args[1])?.to_vec();
    if imm == 0 {
        s.make_ascii_uppercase();
    } else {
        s.make_ascii_lowercase();
    }
    ctx.new_string(&s, args[2])
}

/// `str.replace(s, from, to, dst)`: every `from` replaced.
fn replace(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let from = text(ctx, args[2], args[3])?;
    let to = text(ctx, args[4], args[5])?;
    let mut out = Vec::with_capacity(s.len());
    if from.is_empty() {
        out.extend_from_slice(s);
    } else {
        let mut rest = s;
        while let Some(at) = position(rest, from) {
            out.extend_from_slice(&rest[..at]);
            out.extend_from_slice(to);
            rest = &rest[at + from.len()..];
        }
        out.extend_from_slice(rest);
    }
    ctx.new_string(&out, args[6])
}

/// `str.repeat(s, n, dst)`.
fn repeat(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let n = args[2];
    let len = s.len() as u64;
    if len > 0 && n > crate::heap::LIMIT / len {
        return Err(Trap::Unravel(format!("out of memory: the string would be {n} times {len} bytes")));
    }
    let out = s.repeat(n as usize);
    ctx.new_string(&out, args[3])
}

/// `str.split(s, sep, dst)`: a `List[String]` of the pieces between each
/// `sep` (or of each character, for an empty one).
fn split(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?.to_vec();
    let sep = text(ctx, args[2], args[3])?.to_vec();
    let pieces: Vec<&[u8]> = if sep.is_empty() {
        let mut out = Vec::new();
        let mut start = 0;
        for i in 1..=s.len() {
            if i == s.len() || s[i] & 0xC0 != 0x80 {
                out.push(&s[start..i]);
                start = i;
            }
        }
        out
    } else {
        let mut out = Vec::new();
        let mut rest = &s[..];
        while let Some(at) = position(rest, &sep) {
            out.push(&rest[..at]);
            rest = &rest[at + sep.len()..];
        }
        out.push(rest);
        out
    };
    let n = pieces.len() as u64;
    let buf = ctx.heap.alloc(n * 24, 8)?;
    for (i, piece) in pieces.iter().enumerate() {
        ctx.new_string(piece, buf + 24 * i as u64)?;
    }
    write_header(args[4], buf, n, n);
    Ok(())
}

/// Writes a list's (or a string's) header at `dst`.
fn write_header(dst: u64, buf: u64, len: u64, cap: u64) {
    // SAFETY: `dst` is three words the compiler gave for the header.
    unsafe {
        let d = dst as *mut u64;
        d.write(buf);
        d.add(1).write(len);
        d.add(2).write(cap);
    }
}

/// The header (buffer, length, capacity) at `dst`.
fn read_header(dst: u64) -> (u64, u64, u64) {
    // SAFETY: `dst` is the header of a list or a string the program owns.
    unsafe {
        let d = dst as *const u64;
        (d.read(), d.add(1).read(), d.add(2).read())
    }
}

/// `NumError`'s cases, by tag.
const EMPTY: u64 = 0;
const INVALID: u64 = 1;
const OVERFLOW: u64 = 2;
/// No error.
const FINE: u64 = 3;

/// `str.parse_int(s, type)`: (value, `FINE` or the `NumError` tag).
fn parse_int(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let spec = ints::spec(args[2]);
    let (v, e) = parse_int_text(s, spec);
    args[0] = v;
    args[1] = e;
    Ok(())
}

fn parse_int_text(s: &[u8], spec: loom_isa::IntSpec) -> (u64, u64) {
    if s.is_empty() {
        return (0, EMPTY);
    }
    let (neg, digits) = match s[0] {
        b'+' | b'-' if s.len() == 1 => return (0, INVALID),
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if neg && !spec.signed {
        return (0, INVALID);
    }
    let mut mag: u64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return (0, INVALID);
        }
        match mag.checked_mul(10).and_then(|m| m.checked_add(u64::from(b - b'0'))) {
            Some(m) => mag = m,
            None => return (0, OVERFLOW),
        }
    }
    let bits = u32::from(spec.bits.min(64));
    let limit: u64 = match (spec.signed, neg) {
        (true, true) => 1u64 << (bits - 1),
        (true, false) => (1u64 << (bits - 1)) - 1,
        (false, _) if bits >= 64 => u64::MAX,
        (false, _) => (1u64 << bits) - 1,
    };
    if mag > limit {
        return (0, OVERFLOW);
    }
    (if neg { mag.wrapping_neg() } else { mag }, FINE)
}

/// `str.parse_float(s, is_f32)`: (bits, `FINE` or the `NumError` tag).
fn parse_float(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let f32_ = args[2] != 0;
    let (v, e) = match parse_float_text(s) {
        Ok(v) => (v, FINE),
        Err(e) => (0.0, e),
    };
    args[0] = if f32_ { u64::from((v as f32).to_bits()) } else { v.to_bits() };
    args[1] = e;
    Ok(())
}

fn parse_float_text(s: &[u8]) -> Result<f64, u64> {
    if s.is_empty() {
        return Err(EMPTY);
    }
    let body = match s[0] {
        b'+' | b'-' => &s[1..],
        _ => s,
    };
    let lower = body.to_ascii_lowercase();
    let v = if lower == b"inf" || lower == b"infinity" {
        f64::INFINITY
    } else if lower == b"nan" {
        f64::NAN
    } else {
        let split = body.iter().position(|&b| !(b.is_ascii_digit() || b == b'.')).unwrap_or(body.len());
        let (mantissa, exponent) = body.split_at(split);
        let ok = !mantissa.is_empty()
            && mantissa.iter().any(u8::is_ascii_digit)
            && mantissa.iter().filter(|&&b| b == b'.').count() <= 1
            && (exponent.is_empty() || {
                let e = &exponent[1..];
                matches!(exponent[0], b'e' | b'E') && {
                    let digits = match e.first() {
                        Some(b'+' | b'-') => &e[1..],
                        _ => e,
                    };
                    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
                }
            });
        if !ok {
            return Err(INVALID);
        }
        std::str::from_utf8(body).ok().and_then(|t| t.parse::<f64>().ok()).ok_or(INVALID)?
    };
    Ok(if s[0] == b'-' { -v } else { v })
}

/// `string.of(chars, n, dst)`: a `String` of `n` `char`s.
fn string_of(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let n = args[1] as usize;
    let mut out = String::new();
    for i in 0..n {
        // SAFETY: the compiler passes the elements of a `[char]`, 4 bytes each.
        let c = unsafe { (args[0] as *const u32).add(i).read() };
        out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
    }
    ctx.new_string(out.as_bytes(), args[2])
}

/// Adds `more` to the end of the `String` at `header`.
fn append(ctx: &mut Ctx, header: u64, more: &[u8]) -> Result<(), Trap> {
    let (buf, len, cap) = read_header(header);
    let want = len + more.len() as u64;
    let (buf, cap) = if want > cap {
        let new_cap = want.max(cap.saturating_mul(2)).max(8);
        if new_cap > crate::heap::LIMIT {
            return Err(Trap::OutOfMemory);
        }
        (ctx.heap.realloc(buf, cap, new_cap, 1)?, new_cap)
    } else {
        (buf, cap)
    };
    // SAFETY: the buffer has room for `want` bytes now.
    unsafe { std::ptr::copy_nonoverlapping(more.as_ptr(), (buf + len) as *mut u8, more.len()) };
    write_header(header, buf, want, cap);
    Ok(())
}

/// `string.push_char(header, c)`.
fn push_char(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let c = char::from_u32(args[1] as u32).unwrap_or('\u{FFFD}');
    let mut b = [0; 4];
    append(ctx, args[0], c.encode_utf8(&mut b).as_bytes())
}

/// `string.push_str(header, s)`.
fn push_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let more = text(ctx, args[1], args[2])?.to_vec();
    append(ctx, args[0], &more)
}

/// `string.pop(header)`: (the last `char`, whether there was one).
fn pop(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let (buf, len, cap) = read_header(args[0]);
    if len == 0 {
        args[0] = 0;
        args[1] = 0;
        return Ok(());
    }
    // SAFETY: the string's buffer holds `len` bytes of UTF-8.
    let s = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
    let start = (0..s.len()).rev().find(|&i| s[i] & 0xC0 != 0x80).unwrap_or(0);
    let c = std::str::from_utf8(&s[start..]).ok().and_then(|t| t.chars().next()).unwrap_or('\u{FFFD}');
    write_header(args[0], buf, start as u64, cap);
    args[0] = u64::from(c);
    args[1] = 1;
    Ok(())
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// The multiplier of `text_hash`: 2^64 over the golden ratio, odd.
const TEXT_MUL: u64 = 0x9e37_79b9_7f4a_7c15;

/// The bytes of `p` (`n` of them, `0 < n <= 8`) as one word: two
/// overlapping reads, which together cover every byte. Equal for equal
/// bytes, different for different ones of the same length.
///
/// # Safety
///
/// `p` is `n` readable bytes.
#[inline(always)]
unsafe fn short_word(p: *const u8, n: usize) -> u64 {
    // SAFETY: every read is inside the `n` bytes.
    unsafe {
        if n >= 4 {
            let lo = u64::from((p as *const u32).read_unaligned());
            let hi = u64::from((p.add(n - 4) as *const u32).read_unaligned());
            lo | (hi << 32)
        } else {
            u64::from(*p) | (u64::from(*p.add(n / 2)) << 8) | (u64::from(*p.add(n - 1)) << 16)
        }
    }
}

/// Treadle's hash of text (design/vm.md D8): a word at a time, each mixed
/// in by a multiply, so a short key costs one. Words past the first 8
/// bytes are read whole, the last overlapping the one before.
fn text_hash(bytes: &[u8]) -> u64 {
    let n = bytes.len();
    let mut h = FNV_OFFSET ^ (n as u64).wrapping_mul(FNV_PRIME);
    let mut mix = |w: u64| {
        h = (h ^ w).wrapping_mul(TEXT_MUL);
        h ^= h >> 32;
    };
    let p = bytes.as_ptr();
    if n == 0 {
        return h;
    }
    if n <= 8 {
        // SAFETY: `p` is `n` bytes.
        mix(unsafe { short_word(p, n) });
        return h;
    }
    let mut at = 0;
    while at + 8 < n {
        // SAFETY: inside the text.
        mix(unsafe { (p.add(at) as *const u64).read_unaligned() });
        at += 8;
    }
    // SAFETY: `n > 8`, so the last 8 bytes are inside it.
    mix(unsafe { (p.add(n - 8) as *const u64).read_unaligned() });
    h
}

/// Whether the `key.len()` bytes at `p` are `key`'s.
///
/// # Safety
///
/// `p` is `key.len()` readable bytes.
#[inline(always)]
unsafe fn same_text(p: *const u8, key: &[u8]) -> bool {
    let n = key.len();
    let k = key.as_ptr();
    // SAFETY: every read is inside the `n` bytes of each.
    unsafe {
        match n {
            0 => true,
            1..=8 => short_word(p, n) == short_word(k, n),
            9..=16 => {
                let first = (p as *const u64).read_unaligned() == (k as *const u64).read_unaligned();
                first && (p.add(n - 8) as *const u64).read_unaligned() == (k.add(n - 8) as *const u64).read_unaligned()
            }
            _ => std::slice::from_raw_parts(p, n) == key,
        }
    }
}

/// A one-word key's hash, as the compiler's `key_hash` makes it: the word
/// mixed into FNV's offset, then the high half folded into the low.
fn key_hash(word: u64) -> u64 {
    let h = (FNV_OFFSET ^ word).wrapping_mul(FNV_PRIME);
    h ^ (h >> 32)
}

/// Looks for the entry whose key `same` says is it, with hash `h`, in the
/// map whose header is at `header`: its number, or `u64::MAX`.
fn probe(header: u64, h: u64, esize: u64, same: impl Fn(u64) -> bool) -> u64 {
    // SAFETY: the compiler passes a map's header: five words.
    let (ebuf, ibuf, slots) = unsafe {
        let p = header as *const u64;
        (p.read(), p.add(3).read(), p.add(4).read())
    };
    if slots == 0 {
        return u64::MAX;
    }
    let mask = slots - 1;
    let mut i = h & mask;
    loop {
        // SAFETY: the index has `slots` words, each 0 or an entry's number
        // plus one, and the entries `esize` bytes each.
        let s = unsafe { (ibuf as *const u64).add(i as usize).read() };
        if s == 0 {
            return u64::MAX;
        }
        let entry = ebuf + (s - 1) * esize;
        if unsafe { (entry as *const u64).read() } == h && same(entry) {
            return s - 1;
        }
        i = (i + 1) & mask;
    }
}

/// `map.find_str(header, text, layout)`: (entry or `u64::MAX`, hash).
fn map_find_str(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let key = text(ctx, args[1], args[2])?;
    let (koff, esize) = (args[3] & 0xFFFF, args[3] >> 32);
    let h = key_hash(text_hash(key));
    let e = probe(args[0], h, esize, |entry| {
        // SAFETY: an entry's key is text: its address and length first.
        let (p, n) = unsafe {
            let k = (entry + koff) as *const u64;
            (k.read(), k.add(1).read())
        };
        // SAFETY: the key's text: `n` bytes at `p`.
        n as usize == key.len() && unsafe { same_text(p as *const u8, key) }
    });
    args[0] = e;
    args[1] = h;
    Ok(())
}

/// `map.find_int(header, word, layout)`: (entry or `u64::MAX`, hash).
fn map_find_int(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let word = args[1];
    let (koff, bytes, esize) = (args[2] & 0xFFFF, (args[2] >> 16) & 0xFFFF, args[2] >> 32);
    let mask = if bytes >= 8 { u64::MAX } else { (1u64 << (bytes * 8)) - 1 };
    let h = key_hash(word);
    let e = probe(args[0], h, esize, |entry| {
        // SAFETY: an entry's key is `bytes` bytes at its offset.
        let mut v = [0u8; 8];
        unsafe { std::ptr::copy_nonoverlapping((entry + koff) as *const u8, v.as_mut_ptr(), bytes as usize) };
        u64::from_le_bytes(v) == word & mask
    });
    args[0] = e;
    args[1] = h;
    Ok(())
}

/// `str.concatN(text, …, dst)`: a `String` of the texts.
fn concat(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let n = (args.len() - 1) / 2;
    let mut len = 0;
    for k in 0..n {
        len += text(ctx, args[2 * k], args[2 * k + 1])?.len() as u64;
    }
    let buf = ctx.heap.alloc(len, 1)?;
    let mut at = buf;
    for k in 0..n {
        let piece = text(ctx, args[2 * k], args[2 * k + 1])?;
        // SAFETY: `buf` is a new object of `len` bytes, the pieces' total,
        // and no piece is in it.
        unsafe { std::ptr::copy_nonoverlapping(piece.as_ptr(), at as *mut u8, piece.len()) };
        at += piece.len() as u64;
    }
    write_header(args[2 * n], buf, len, len);
    Ok(())
}

/// `str.hash(s)`: `text_hash` of the text.
fn str_hash(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    args[0] = text_hash(text(ctx, args[0], args[1])?);
    Ok(())
}

/// `str.next_char(s, at)`: the `char` at byte `at`, and where the next
/// starts.
fn next_char(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let at = args[2] as usize;
    let rest = &s[at.min(s.len())..];
    let width = match rest.first() {
        Some(&b) if b < 0x80 => 1,
        Some(&b) if b >= 0xF0 => 4,
        Some(&b) if b >= 0xE0 => 3,
        Some(_) => 2,
        None => 0,
    }
    .min(rest.len());
    let c = std::str::from_utf8(&rest[..width]).ok().and_then(|t| t.chars().next()).unwrap_or('\u{FFFD}');
    args[0] = u64::from(c);
    args[1] = (at + width.max(1)) as u64;
    Ok(())
}

/// `str.next_line(s, at)`: the line from byte `at` (its start and length,
/// less a `\r` before its `\n`), and where the next starts.
fn next_line(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let at = args[2] as usize;
    let rest = &s[at.min(s.len())..];
    let (mut len, next) = match rest.iter().position(|&b| b == b'\n') {
        Some(nl) => (nl, at + nl + 1),
        None => (rest.len(), s.len() + 1),
    };
    if len > 0 && rest[len - 1] == b'\r' {
        len -= 1;
    }
    args[0] += at as u64;
    args[1] = len as u64;
    args[2] = next as u64;
    Ok(())
}

/// `str.next_word(s, at)`: the next run of characters that aren't ASCII
/// whitespace from byte `at` (start, length), where the search goes on,
/// and whether there was one.
fn next_word(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let s = text(ctx, args[0], args[1])?;
    let mut i = (args[2] as usize).min(s.len());
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let start = i;
    while i < s.len() && !is_space(s[i]) {
        i += 1;
    }
    args[3] = u64::from(i > start);
    args[0] += start as u64;
    args[1] = (i - start) as u64;
    args[2] = i as u64;
    Ok(())
}

/// `char.digit(c)`: the decimal digit `c` is.
fn digit(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let c = args[0] as u32;
    if !(48..=57).contains(&c) {
        let mut shown = Vec::new();
        crate::fmt::show(&mut shown, loom_isa::ValueKind::Char, crate::fmt::Value::Reg(u64::from(c)), true);
        let shown = String::from_utf8_lossy(&shown);
        return Err(Trap::Unravel(format!("not a decimal digit: {shown}")));
    }
    args[0] = u64::from(c - 48);
    Ok(())
}

/// `int.bits(v, type)` with `imm`: 0 `trailing_zeros`, 1 `leading_zeros`,
/// 2 `count_ones`, 3 `is_power_of_two`, 4 `log2`.
fn int_bits(_: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    let s = ints::spec(args[1]);
    let bits = u32::from(s.bits.min(64));
    let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let v = args[0];
    let x = v & mask;
    let negative = s.signed && (v as i64) < 0;
    args[0] = match imm {
        0 => u64::from(if x == 0 { bits } else { x.trailing_zeros() }),
        1 => u64::from(x.leading_zeros() - (64 - bits)),
        2 => u64::from(x.count_ones()),
        3 => u64::from(!negative && x != 0 && x & (x - 1) == 0),
        _ => {
            if x == 0 || negative {
                let shown = if s.signed { (v as i64).to_string() } else { v.to_string() };
                return Err(Trap::Unravel(format!("log2 of {shown}: it needs a number above zero")));
            }
            u64::from(63 - x.leading_zeros())
        }
    };
    Ok(())
}

/// `int.rotate(v, n, type)`: `imm` 0 left, 1 right.
fn int_rotate(_: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    let s = ints::spec(args[2]);
    let bits = u64::from(s.bits.min(64));
    let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let n = args[1] % bits;
    let n = if imm == 0 { n } else { (bits - n) % bits };
    let x = args[0] & mask;
    let r = if n == 0 { x } else { ((x << n) | (x >> (bits - n))) & mask };
    args[0] = ints::wrap(r, s);
    Ok(())
}

/// `float.math(a, b)`: `imm` is the operation times two, plus one for
/// `f32` (whose bits are the low half of a register).
fn float_math(_: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    let f32_ = imm & 1 == 1;
    let get = |r: u64| if f32_ { f64::from(f32::from_bits(r as u32)) } else { f64::from_bits(r) };
    let (a, b) = (get(args[0]), get(args[1]));
    let r = match imm >> 1 {
        0 => a.abs(),
        1 if a.is_nan() => b,
        1 if b.is_nan() => a,
        1 => {
            if b < a {
                b
            } else {
                a
            }
        }
        2 if a.is_nan() => b,
        2 if b.is_nan() => a,
        2 => {
            if b > a {
                b
            } else {
                a
            }
        }
        3 => a.sqrt(),
        4 => a.floor(),
        5 => a.ceil(),
        6 => a.trunc(),
        // Halves away from zero.
        7 => a.round(),
        8 => a.powf(b),
        _ => {
            args[0] = u64::from(a.is_nan());
            return Ok(());
        }
    };
    args[0] = if f32_ { u64::from((r as f32).to_bits()) } else { r.to_bits() };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_texts_compare_and_hash_as_their_bytes() {
        // Every text of up to 3 bytes over a small alphabet, and texts of
        // every length to 40 that differ in one byte.
        let alphabet = [b'a', b'b', 0, 0xff];
        let mut texts: Vec<Vec<u8>> = vec![Vec::new()];
        for n in 1..=3 {
            let mut all = vec![Vec::new()];
            for _ in 0..n {
                all = all.iter().flat_map(|t| alphabet.iter().map(move |&c| [t.clone(), vec![c]].concat())).collect();
            }
            texts.extend(all);
        }
        for n in 1..=40 {
            let base: Vec<u8> = (0..n).map(|i| b'a' + (i % 26) as u8).collect();
            texts.push(base.clone());
            for at in 0..n {
                let mut t = base.clone();
                t[at] ^= 1;
                texts.push(t);
            }
        }
        let mut hashes = std::collections::HashMap::new();
        for a in &texts {
            for b in &texts {
                if a.len() == b.len() {
                    // SAFETY: `a` is `b.len()` bytes.
                    assert_eq!(unsafe { same_text(a.as_ptr(), b) }, a == b, "{a:?} {b:?}");
                }
            }
            let h = text_hash(a);
            assert_eq!(h, text_hash(&a.clone()));
            hashes.entry(h).or_insert_with(Vec::new).push(a.clone());
        }
        let collisions = hashes.values().filter(|v| {
            let mut v = (*v).clone();
            v.sort();
            v.dedup();
            v.len() > 1
        });
        assert_eq!(collisions.count(), 0);
    }
}
