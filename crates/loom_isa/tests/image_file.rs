//! Image files: an image survives a trip through `encode` and `decode`, and
//! a damaged file is an error, never a panic.

use loom_isa::{
    Align, FmtItem, Func, HostImport, Image, ImageError, IntSpec, Line, Site, Spec, ValueKind, decode, encode,
};

fn every_spec() -> Vec<Spec> {
    let empty =
        Spec { fill: None, align: None, plus: false, alt: false, zero: false, width: None, precision: None, ty: None };
    let full = Spec {
        fill: Some('★'),
        align: Some(Align::Center),
        plus: true,
        alt: true,
        zero: true,
        width: Some(12),
        precision: Some(3),
        ty: Some('x'),
    };
    vec![
        empty,
        full,
        Spec { align: Some(Align::Left), ..empty },
        Spec { align: Some(Align::Right), width: Some(u16::MAX), ..empty },
    ]
}

fn sample() -> Image {
    let kinds = [
        ValueKind::Int(IntSpec::new(8, true)),
        ValueKind::Int(IntSpec { bits: 64, signed: false, size: true }),
        ValueKind::F32,
        ValueKind::F64,
        ValueKind::Bool,
        ValueKind::Char,
        ValueKind::Str,
        ValueKind::Unit,
    ];
    let mut fmts = Vec::new();
    for kind in kinds {
        for debug in [false, true] {
            fmts.push(FmtItem { kind, debug, spec: None });
        }
        for spec in every_spec() {
            fmts.push(FmtItem { kind, debug: false, spec: Some(spec) });
        }
    }
    let func = |name: &str, entry: u32, with_site: bool| Func {
        name: name.into(),
        entry,
        len: 4,
        frame: 9,
        params: 2,
        results: 1,
        lines: vec![
            Line { pc: 0, file: 0, line: 3, column: 5, site: loom_isa::NO_SITE },
            Line { pc: 2, file: 1, line: 40, column: 1, site: 0 },
        ],
        pads: vec![loom_isa::image::Pad { start: 0, end: 2, pad: 3 }],
        sites: if with_site {
            vec![
                Site { name: "inlined".into(), call: Some((0, 3, 9)), parent: loom_isa::NO_SITE },
                Site { name: "deeper".into(), call: None, parent: 0 },
            ]
        } else {
            Vec::new()
        },
    };
    Image {
        funcs: vec![func("main", 0, true), func("héllo", 4, false)],
        code: vec![0, 1, u64::MAX, 0x0123_4567_89AB_CDEF, 5, 6, 7, 8],
        strings: vec![b"hello".to_vec(), Vec::new(), vec![0, 255, 7]],
        fmts,
        natives: vec!["print".into(), "str_len".into()],
        host_imports: vec![HostImport { name: "host.add".into(), args: 2, results: 1 }],
        files: vec!["a.loom".into(), "ünï/b.loom".into()],
        data: vec![1, 2, 3, u64::MAX],
        relocs: vec![0, 8, 16],
        entry: Some(0),
    }
}

#[test]
fn round_trips_every_variant() {
    for image in [sample(), Image::default(), Image { entry: None, ..sample() }] {
        assert_eq!(decode(&encode(&image)), Ok(image));
    }
}

#[test]
fn encoding_is_deterministic() {
    assert_eq!(encode(&sample()), encode(&sample()));
}

const HEADER: usize = 28;

#[test]
fn any_flipped_payload_byte_is_a_hash_mismatch() {
    let bytes = encode(&sample());
    for i in HEADER..bytes.len() {
        let mut bad = bytes.clone();
        bad[i] ^= 0x01;
        assert_eq!(decode(&bad), Err(ImageError::HashMismatch), "byte {i}");
    }
}

#[test]
fn the_header_is_checked() {
    let bytes = encode(&sample());
    let mut bad = bytes.clone();
    bad[0] ^= 0xFF;
    assert_eq!(decode(&bad), Err(ImageError::BadMagic));
    let mut bad = bytes.clone();
    bad[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(decode(&bad), Err(ImageError::BadFormat(2)));
    let mut bad = bytes.clone();
    bad[20] ^= 0x01;
    assert_eq!(decode(&bad), Err(ImageError::HashMismatch));
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(decode(&longer), Err(ImageError::Malformed(_))));
}

#[test]
fn a_truncated_file_is_an_error() {
    let bytes = encode(&sample());
    for k in 0..10 {
        let len = bytes.len() * k / 10;
        let result = decode(&bytes[..len]);
        assert!(matches!(result, Err(ImageError::Truncated | ImageError::HashMismatch)), "{len}: {result:?}");
    }
    let result = decode(&bytes[..bytes.len() - 1]);
    assert!(matches!(result, Err(ImageError::Truncated | ImageError::HashMismatch)), "{result:?}");
}
