//! Images on disk: `encode` writes one, `decode` reads it back.
//!
//! ```text
//! b"LOOMIMG\0"  FORMAT: u32  payload_len: u64  hash: u64  payload
//! ```
//!
//! All little-endian. The payload holds every field of [`Image`] in
//! declaration order. `decode` checks the magic, the format, the length and
//! the hash before it parses anything, and every read after that is bounds
//! checked: a damaged file is an [`ImageError`], never a panic. The hash
//! tells a damaged file from a good one, it is not a signature: whoever can
//! write an image can write a hash for it.

use std::fmt;

use crate::{Align, FmtItem, Func, HostImport, Image, IntSpec, Line, Site, Spec, ValueKind, image::Pad};

const MAGIC: &[u8; 8] = b"LOOMIMG\0";
const FORMAT: u32 = 1;
const HEADER: usize = 8 + 4 + 8 + 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    BadMagic,
    BadFormat(u32),
    Truncated,
    HashMismatch,
    Malformed(&'static str),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::BadMagic => write!(f, "not a Loom image"),
            ImageError::BadFormat(v) => write!(f, "image format {v} is not supported (this is format {FORMAT})"),
            ImageError::Truncated => write!(f, "the image is cut short"),
            ImageError::HashMismatch => write!(f, "the image is damaged (its hash doesn't match)"),
            ImageError::Malformed(what) => write!(f, "the image is malformed: {what}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// The 64-bit hash of `bytes`: 8-byte words, the tail zero-padded.
fn hash64(bytes: &[u8]) -> u64 {
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
    let mut h = 0xCBF2_9CE4_8422_2325 ^ bytes.len() as u64;
    let (words, tail) = bytes.as_chunks::<8>();
    for &w in words {
        h = mix(h, u64::from_le_bytes(w));
    }
    if !tail.is_empty() {
        let mut last = [0u8; 8];
        last[..tail.len()].copy_from_slice(tail);
        h = mix(h, u64::from_le_bytes(last));
    }
    h ^ (h >> 32)
}

/// The bytes of `image`, ready to write to a file.
pub fn encode(image: &Image) -> Vec<u8> {
    let mut w = Writer(Vec::with_capacity(image.code.len() * 8 + image.data.len() * 8 + 4096));
    w.image(image);
    let payload = w.0;
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&hash64(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// The image `bytes` hold.
pub fn decode(bytes: &[u8]) -> Result<Image, ImageError> {
    let header = bytes.get(..HEADER).ok_or(ImageError::Truncated)?;
    if &header[..8] != MAGIC {
        return Err(ImageError::BadMagic);
    }
    let format = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if format != FORMAT {
        return Err(ImageError::BadFormat(format));
    }
    let len = u64::from_le_bytes(header[12..20].try_into().unwrap_or([0; 8]));
    let hash = u64::from_le_bytes(header[20..28].try_into().unwrap_or([0; 8]));
    let body = &bytes[HEADER..];
    let len = usize::try_from(len).map_err(|_| ImageError::Truncated)?;
    if body.len() < len {
        return Err(ImageError::Truncated);
    }
    if body.len() > len {
        return Err(ImageError::Malformed("bytes after the image"));
    }
    if hash64(body) != hash {
        return Err(ImageError::HashMismatch);
    }
    let mut r = Reader { bytes: body, at: 0 };
    let image = r.image()?;
    if r.at != body.len() {
        return Err(ImageError::Malformed("bytes after the image"));
    }
    Ok(image)
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.0.extend_from_slice(v);
    }

    fn string(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    fn vec<T>(&mut self, items: &[T], mut item: impl FnMut(&mut Self, &T)) {
        self.u64(items.len() as u64);
        for i in items {
            item(self, i);
        }
    }

    fn option<T>(&mut self, v: &Option<T>, mut some: impl FnMut(&mut Self, &T)) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                some(self, v);
            }
        }
    }

    fn image(&mut self, image: &Image) {
        self.vec(&image.funcs, |w, f| w.func(f));
        self.u64(image.code.len() as u64);
        for &word in &image.code {
            self.u64(word);
        }
        self.vec(&image.strings, |w, s| w.bytes(s));
        self.vec(&image.fmts, |w, f| w.fmt_item(f));
        self.vec(&image.natives, |w, s| w.string(s));
        self.vec(&image.host_imports, |w, h| {
            w.string(&h.name);
            w.u16(h.args);
            w.u16(h.results);
        });
        self.vec(&image.files, |w, s| w.string(s));
        self.u64(image.data.len() as u64);
        for &word in &image.data {
            self.u64(word);
        }
        self.u64(image.relocs.len() as u64);
        for &r in &image.relocs {
            self.u32(r);
        }
        self.option(&image.entry, |w, e| w.u32(*e));
    }

    fn func(&mut self, f: &Func) {
        self.string(&f.name);
        self.u32(f.entry);
        self.u32(f.len);
        self.u16(f.frame);
        self.u16(f.params);
        self.u16(f.results);
        self.vec(&f.lines, |w, l| {
            w.u32(l.pc);
            w.u32(l.file);
            w.u32(l.line);
            w.u32(l.column);
            w.u32(l.site);
        });
        self.vec(&f.pads, |w, p| {
            w.u32(p.start);
            w.u32(p.end);
            w.u32(p.pad);
        });
        self.vec(&f.sites, |w, s| {
            w.string(&s.name);
            w.option(&s.call, |w, &(file, line, column)| {
                w.u32(file);
                w.u32(line);
                w.u32(column);
            });
            w.u32(s.parent);
        });
    }

    fn int_spec(&mut self, s: IntSpec) {
        self.u8(s.bits);
        self.bool(s.signed);
        self.bool(s.size);
    }

    fn fmt_item(&mut self, f: &FmtItem) {
        match f.kind {
            ValueKind::Int(s) => {
                self.u8(0);
                self.int_spec(s);
            }
            ValueKind::F32 => self.u8(1),
            ValueKind::F64 => self.u8(2),
            ValueKind::Bool => self.u8(3),
            ValueKind::Char => self.u8(4),
            ValueKind::Str => self.u8(5),
            ValueKind::Unit => self.u8(6),
        }
        self.bool(f.debug);
        self.option(&f.spec, |w, s| w.spec(s));
    }

    fn spec(&mut self, s: &Spec) {
        self.option(&s.fill, |w, c| w.u32(*c as u32));
        self.option(&s.align, |w, a| {
            w.u8(match a {
                Align::Left => 0,
                Align::Right => 1,
                Align::Center => 2,
            })
        });
        self.bool(s.plus);
        self.bool(s.alt);
        self.bool(s.zero);
        self.option(&s.width, |w, v| w.u16(*v));
        self.option(&s.precision, |w, v| w.u16(*v));
        self.option(&s.ty, |w, c| w.u32(*c as u32));
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

type Read<T> = Result<T, ImageError>;

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Read<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(ImageError::Truncated)?;
        let out = self.bytes.get(self.at..end).ok_or(ImageError::Truncated)?;
        self.at = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Read<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Read<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn bool(&mut self) -> Read<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ImageError::Malformed("a flag is neither 0 nor 1")),
        }
    }

    fn u16(&mut self) -> Read<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Read<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Read<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn char(&mut self) -> Read<char> {
        char::from_u32(self.u32()?).ok_or(ImageError::Malformed("not a character"))
    }

    /// A count of items that each take at least one byte.
    fn count(&mut self) -> Read<usize> {
        let n = usize::try_from(self.u64()?).map_err(|_| ImageError::Truncated)?;
        if n > self.bytes.len() - self.at {
            return Err(ImageError::Truncated);
        }
        Ok(n)
    }

    fn bytes(&mut self) -> Read<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }

    fn string(&mut self) -> Read<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| ImageError::Malformed("text is not UTF-8"))
    }

    fn vec<T>(&mut self, mut item: impl FnMut(&mut Self) -> Read<T>) -> Read<Vec<T>> {
        let n = self.count()?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(item(self)?);
        }
        Ok(out)
    }

    /// A count of items of `size` bytes, read as one slice.
    fn run(&mut self, size: usize) -> Read<&'a [u8]> {
        let n = usize::try_from(self.u64()?).map_err(|_| ImageError::Truncated)?;
        self.take(n.checked_mul(size).ok_or(ImageError::Truncated)?)
    }

    fn option<T>(&mut self, mut some: impl FnMut(&mut Self) -> Read<T>) -> Read<Option<T>> {
        match self.u8()? {
            0 => Ok(None),
            1 => some(self).map(Some),
            _ => Err(ImageError::Malformed("an option's tag is neither 0 nor 1")),
        }
    }

    fn image(&mut self) -> Read<Image> {
        let funcs = self.vec(|r| r.func())?;
        let code = self.run(8)?.as_chunks::<8>().0.iter().map(|&w| u64::from_le_bytes(w)).collect();
        let strings = self.vec(|r| Ok(r.bytes()?.to_vec()))?;
        let fmts = self.vec(|r| r.fmt_item())?;
        let natives = self.vec(|r| r.string())?;
        let host_imports = self.vec(|r| Ok(HostImport { name: r.string()?, args: r.u16()?, results: r.u16()? }))?;
        let files = self.vec(|r| r.string())?;
        let data = self.run(8)?.as_chunks::<8>().0.iter().map(|&w| u64::from_le_bytes(w)).collect();
        let relocs = self.run(4)?.as_chunks::<4>().0.iter().map(|&w| u32::from_le_bytes(w)).collect();
        let entry = self.option(|r| r.u32())?;
        Ok(Image { funcs, code, strings, fmts, natives, host_imports, files, data, relocs, entry })
    }

    fn func(&mut self) -> Read<Func> {
        Ok(Func {
            name: self.string()?,
            entry: self.u32()?,
            len: self.u32()?,
            frame: self.u16()?,
            params: self.u16()?,
            results: self.u16()?,
            lines: self
                .vec(|r| Ok(Line { pc: r.u32()?, file: r.u32()?, line: r.u32()?, column: r.u32()?, site: r.u32()? }))?,
            pads: self.vec(|r| Ok(Pad { start: r.u32()?, end: r.u32()?, pad: r.u32()? }))?,
            sites: self.vec(|r| {
                Ok(Site {
                    name: r.string()?,
                    call: r.option(|r| Ok((r.u32()?, r.u32()?, r.u32()?)))?,
                    parent: r.u32()?,
                })
            })?,
        })
    }

    fn fmt_item(&mut self) -> Read<FmtItem> {
        let kind = match self.u8()? {
            0 => ValueKind::Int(IntSpec { bits: self.u8()?, signed: self.bool()?, size: self.bool()? }),
            1 => ValueKind::F32,
            2 => ValueKind::F64,
            3 => ValueKind::Bool,
            4 => ValueKind::Char,
            5 => ValueKind::Str,
            6 => ValueKind::Unit,
            _ => return Err(ImageError::Malformed("unknown value kind")),
        };
        Ok(FmtItem { kind, debug: self.bool()?, spec: self.option(|r| r.spec())? })
    }

    fn spec(&mut self) -> Read<Spec> {
        Ok(Spec {
            fill: self.option(|r| r.char())?,
            align: self.option(|r| match r.u8()? {
                0 => Ok(Align::Left),
                1 => Ok(Align::Right),
                2 => Ok(Align::Center),
                _ => Err(ImageError::Malformed("unknown alignment")),
            })?,
            plus: self.bool()?,
            alt: self.bool()?,
            zero: self.bool()?,
            width: self.option(|r| r.u16())?,
            precision: self.option(|r| r.u16())?,
            ty: self.option(|r| r.char())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file with `payload` as it is and the hash that goes with it.
    fn file_of(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&hash64(payload).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_payload_with_a_good_hash_but_bad_contents_is_an_error() {
        let image = Image {
            funcs: vec![Func {
                name: "main".into(),
                entry: 0,
                len: 1,
                frame: 1,
                params: 0,
                results: 0,
                lines: vec![Line { pc: 0, file: 0, line: 1, column: 1, site: crate::NO_SITE }],
                pads: Vec::new(),
                sites: Vec::new(),
            }],
            code: vec![0],
            strings: vec![b"x".to_vec()],
            fmts: vec![FmtItem { kind: ValueKind::Str, debug: false, spec: Spec::parse(">8") }],
            files: vec!["a".into()],
            entry: Some(0),
            ..Image::default()
        };
        let good = encode(&image);
        let payload = &good[HEADER..];
        for i in 0..payload.len() {
            for value in [0u8, 1, 2, 0x7F, 0xFF] {
                let mut bad = payload.to_vec();
                bad[i] = value;
                // Anything but a panic: it may even be another valid image.
                let _ = decode(&file_of(&bad));
            }
        }
        for len in 0..payload.len() {
            assert!(decode(&file_of(&payload[..len])).is_err(), "{len}");
        }
        assert_eq!(decode(&file_of(payload)), Ok(image));
        // Huge counts don't allocate.
        assert_eq!(decode(&file_of(&u64::MAX.to_le_bytes())), Err(ImageError::Truncated));
    }
}
