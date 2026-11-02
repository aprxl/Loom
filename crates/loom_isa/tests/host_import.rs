use loom_isa::word::word;
use loom_isa::{Func, HostImport, Image, Op, verify};

fn import(name: &str, args: u16, results: u16) -> HostImport {
    HostImport { name: name.into(), args, results }
}

/// An image of one function that does `calln host.call` with immediate `c`,
/// in a frame of 4 registers.
fn image(c: u16, imports: Vec<HostImport>) -> Image {
    let code = vec![word(Op::CallN, 0, 0, c), word(Op::Ret, 0, 1, 0)];
    let func = Func {
        name: "f".into(),
        entry: 0,
        len: code.len() as u32,
        frame: 4,
        params: 1,
        results: 1,
        lines: vec![],
        pads: vec![],
        sites: vec![],
    };
    Image {
        funcs: vec![func],
        code,
        natives: vec!["host.call".into()],
        host_imports: imports,
        entry: Some(0),
        ..Image::default()
    }
}

fn rejects(img: &Image, what: &str) {
    let Err(e) = verify(img) else { panic!("accepted, expected `{what}`") };
    assert!(e.what.contains(what), "{e}");
}

#[test]
fn a_call_of_an_import_that_exists() {
    verify(&image(0, vec![import("app.increment", 1, 1)])).expect("valid");
}

#[test]
fn a_call_of_an_import_that_does_not_exist() {
    rejects(&image(1, vec![import("app.increment", 1, 1)]), "no such host import");
}

#[test]
fn an_import_must_fit_the_frame() {
    rejects(&image(0, vec![import("app.big", 5, 1)]), "don't fit the frame");
    rejects(&image(0, vec![import("app.big", 1, 5)]), "don't fit the frame");
}

#[test]
fn import_names_are_unique() {
    rejects(&image(0, vec![import("app.f", 1, 1), import("app.f", 0, 0)]), "duplicate");
}

#[test]
fn an_import_has_a_name() {
    rejects(&image(0, vec![import("", 1, 1)]), "empty");
}
