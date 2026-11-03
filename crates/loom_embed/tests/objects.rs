use std::cell::RefCell;
use std::rc::Rc;

use loom_embed::{Embed, Literal, Program, Scalar};
use loom_sys::Host;

fn host() -> Host {
    Host::captured(Rc::new(RefCell::new(Vec::new())))
}

fn compile(embed: Embed, user: &str) -> Program {
    embed.compile(user).unwrap_or_else(|d| panic!("{d:?}"))
}

fn call(program: &Program, name: &str, args: &[u64]) -> Vec<u64> {
    program.start(host()).expect("starts").call(name, args).expect("runs")
}

#[test]
fn a_host_method_reads_a_warp_literal() {
    let mut embed = Embed::new("m");
    let point = embed.warp("Point", &[("x", "f64"), ("y", "f64")]).expect("declares");
    embed
        .warp_method(&point, "norm", &[], "f64", move |cx, r, l| {
            let bytes = l.bytes(cx, r[0])?;
            let (x, y): (f64, f64) = (l.read(bytes, 0), l.read(bytes, 1));
            r[0] = (x * x + y * y).sqrt().to_reg();
            Ok(())
        })
        .expect("registers");
    let program = compile(
        embed,
        "use m.{Point}
pub fun go(x: f64) -> f64 { Point(x: x, y: 4.0).norm() }\n",
    );
    assert_eq!(f64::from_reg(call(&program, "go", &[3.0_f64.to_reg()])[0]), 5.0);
}

#[test]
fn a_host_method_returns_a_warp_through_the_out_slot() {
    let mut embed = Embed::new("m");
    let point = embed.warp("Point", &[("x", "f64"), ("y", "f64")]).expect("declares");
    embed
        .warp_method(&point, "scale", &["f64"], "Point", move |cx, r, l| {
            let k = f64::from_reg(r[2]);
            let source = l.bytes(cx, r[1])?;
            let (x, y): (f64, f64) = (l.read(source, 0), l.read(source, 1));
            let out = l.bytes_mut(cx, r[0])?;
            l.write(out, 0, x * k);
            l.write(out, 1, y * k);
            Ok(())
        })
        .expect("registers");
    let program = compile(
        embed,
        "use m.{Point}
pub fun go(x: f64) -> f64 {\n    let q = Point(x: 1.0, y: 2.0).scale(x)\n    q.x + q.y\n}\n",
    );
    assert_eq!(f64::from_reg(call(&program, "go", &[2.0_f64.to_reg()])[0]), 6.0);
}

#[test]
fn field_offsets_come_from_the_layout_for_mixed_sizes() {
    let mut embed = Embed::new("m");
    let mixed = embed.warp("Mixed", &[("a", "i8"), ("b", "f64"), ("c", "i16")]).expect("declares");
    embed
        .warp_method(&mixed, "b_twice", &[], "f64", move |cx, r, l| {
            let b: f64 = l.read(l.bytes(cx, r[0])?, 1);
            r[0] = (2.0 * b).to_reg();
            Ok(())
        })
        .expect("registers");
    embed
        .warp_method(&mixed, "c_value", &[], "i64", move |cx, r, l| {
            let c: i16 = l.read(l.bytes(cx, r[0])?, 2);
            r[0] = i64::from(c).to_reg();
            Ok(())
        })
        .expect("registers");
    let program = compile(
        embed,
        "use m.{Mixed}
pub fun twice(x: f64) -> f64 { Mixed(a: -3, b: x, c: -7).b_twice() }\npub fun c_of() -> i64 { Mixed(a: -3, b: 2.5, c: -7).c_value() }\n",
    );
    assert_eq!(f64::from_reg(call(&program, "twice", &[2.5_f64.to_reg()])[0]), 5.0);
    assert_eq!(i64::from_reg(call(&program, "c_of", &[])[0]), -7);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Light {
    Red,
    Green,
    Off,
}

impl Scalar for Light {
    const LOOM: &'static str = "Light";
    fn to_reg(self) -> u64 {
        self as u64
    }
    fn from_reg(reg: u64) -> Self {
        match reg {
            0 => Light::Red,
            1 => Light::Green,
            _ => Light::Off,
        }
    }
    fn literal(self) -> Option<String> {
        None
    }
}

#[test]
fn a_unit_enum_crosses_as_its_index() {
    let mut embed = Embed::new("m");
    embed.unit_enum("Light", &["Red", "Green", "Off"]).expect("declares");
    embed
        .fun("next", |l: Light| match l {
            Light::Red => Light::Green,
            Light::Green => Light::Off,
            Light::Off => Light::Red,
        })
        .expect("registers");
    let program = compile(
        embed,
        "use m.{next}\npub fun go() -> i64 {\n    match next(next(.Red)) {\n        .Off => 1\n        _ => 0\n    }\n}\n",
    );
    assert_eq!(call(&program, "go", &[]), [1]);
}

#[test]
fn a_constant_warp_is_a_loom_constant() {
    let mut embed = Embed::new("m");
    let point = embed.warp("Point", &[("x", "f64"), ("y", "f64")]).expect("declares");
    let x = Literal::of(1.5_f64).expect("a literal");
    let y = Literal::of(2.0_f64).expect("a literal");
    embed.constant_warp(&point, "ORIGIN", &[("y", y), ("x", x)]).expect("declares");
    let program = compile(embed, "use m.{Point, ORIGIN}\npub fun go() -> f64 { ORIGIN.x + ORIGIN.y }\n");
    assert_eq!(f64::from_reg(call(&program, "go", &[])[0]), 3.5);
}

#[test]
fn a_constant_warp_checks_its_fields() {
    let mut embed = Embed::new("m");
    let point = embed.warp("Point", &[("x", "f64"), ("y", "f64")]).expect("declares");
    let int = Literal::of(1_i64).expect("a literal");
    let float = Literal::of(2.0_f64).expect("a literal");
    assert!(embed.constant_warp(&point, "A", &[("x", int), ("y", float.clone())]).is_err(), "an i64 is not an f64");
    assert!(embed.constant_warp(&point, "B", &[("x", float)]).is_err(), "y is missing");
    assert!(Literal::of(f64::NAN).is_err(), "NaN has no literal");
}

#[test]
fn a_host_method_implements_a_weft_and_any_dispatches_to_it() {
    let mut embed = Embed::new("m");
    let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
    embed.weft("Describe", &[("describe", &[], "String"), ("scale", &["f64"], "f64")]).expect("declares");
    embed.implement(&circle, "Describe").expect("implements");
    embed
        .warp_method(&circle, "describe", &[], "String", move |cx, r, l| {
            let radius: f64 = l.read(l.bytes(cx, r[1])?, 0);
            cx.new_string(format!("circle r={radius}").as_bytes(), r[0])
        })
        .expect("registers");
    embed
        .warp_method(&circle, "scale", &["f64"], "f64", move |cx, r, l| {
            let radius: f64 = l.read(l.bytes(cx, r[0])?, 0);
            r[0] = (radius * f64::from_reg(r[1])).to_reg();
            Ok(())
        })
        .expect("registers");
    let program = compile(
        embed,
        "use m.{Circle, Describe}\npub fun text_len(r: f64) -> usize {\n    let d: any Describe = Circle(r: r)\n    d.describe().len()\n}\npub fun scaled(r: f64) -> f64 {\n    let d: any Describe = Circle(r: r)\n    d.scale(2.0)\n}\n",
    );
    assert_eq!(call(&program, "text_len", &[2.0_f64.to_reg()]), [10]);
    assert_eq!(f64::from_reg(call(&program, "scaled", &[3.0_f64.to_reg()])[0]), 6.0);
}

#[test]
fn a_warp_missing_a_weft_method_is_refused() {
    let mut embed = Embed::new("m");
    let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
    embed.weft("Area", &[("area", &[], "f64")]).expect("declares");
    embed.implement(&circle, "Area").expect("implements");
    assert!(embed.compile("use m.{Circle}\npub fun go() -> f64 { 0.0 }\n").is_err());
}

#[test]
fn implementing_an_unknown_weft_or_twice_is_an_error() {
    let mut embed = Embed::new("m");
    let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
    assert!(embed.implement(&circle, "Nope").is_err(), "no such weft");
    embed.weft("Area", &[("area", &[], "f64")]).expect("declares");
    embed.implement(&circle, "Area").expect("implements");
    assert!(embed.implement(&circle, "Area").is_err(), "already implemented");
}

#[test]
fn a_warp_shows_through_a_host_method() {
    let mut embed = Embed::new("m");
    let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
    embed
        .warp_method(&circle, "label", &[], "String", move |cx, r, l| {
            let radius: f64 = l.read(l.bytes(cx, r[1])?, 0);
            cx.new_string(format!("circle r={radius}").as_bytes(), r[0])
        })
        .expect("registers");
    embed.show_via(&circle, "label").expect("shows");
    let program = compile(
        embed,
        "use m.{Circle}\nuse std.fmt.Show\npub fun shown_len(r: f64) -> usize {\n    \"{Circle(r: r)}\".len()\n}\npub fun any_len(r: f64) -> usize {\n    let s: any Show = Circle(r: r)\n    \"{s}\".len()\n}\n",
    );
    assert_eq!(call(&program, "shown_len", &[2.0_f64.to_reg()]), [10]);
    assert_eq!(call(&program, "any_len", &[2.0_f64.to_reg()]), [10]);
}

#[test]
fn a_payload_enum_crosses_with_its_tag_and_payload() {
    let mut embed = Embed::new("m");
    let shape = embed.enum_warp("Shape", &[("Dot", &[]), ("Circle", &[("r", "f64")])]).expect("declares");
    embed
        .warp_method(&shape, "area", &[], "f64", move |cx, r, l| {
            let bytes = l.bytes(cx, r[0])?;
            let area = if l.case(bytes) == 1 {
                let radius: f64 = l.read_case(bytes, 1, 0);
                3.0 * radius * radius
            } else {
                0.0
            };
            r[0] = area.to_reg();
            Ok(())
        })
        .expect("registers");
    embed
        .warp_method(&shape, "grown", &["f64"], "Shape", move |cx, r, l| {
            let by = f64::from_reg(r[2]);
            let radius: f64 = l.read_case(l.bytes(cx, r[1])?, 1, 0);
            let out = l.bytes_mut(cx, r[0])?;
            l.set_case(out, 1);
            l.write_case(out, 1, 0, radius + by);
            Ok(())
        })
        .expect("registers");
    let program = compile(
        embed,
        "use m.{Shape}\npub fun area_of(r: f64) -> f64 { Shape.Circle(r: r).area() }\npub fun dot_area() -> f64 { Shape.Dot.area() }\npub fun grown_area(r: f64, by: f64) -> f64 { Shape.Circle(r: r).grown(by).area() }\n",
    );
    assert_eq!(f64::from_reg(call(&program, "area_of", &[2.0_f64.to_reg()])[0]), 12.0);
    assert_eq!(f64::from_reg(call(&program, "dot_area", &[])[0]), 0.0);
    assert_eq!(f64::from_reg(call(&program, "grown_area", &[1.0_f64.to_reg(), 2.0_f64.to_reg()])[0]), 27.0);
}

#[test]
fn a_host_declared_weave_maps_weft_methods_to_host_methods() {
    let mut embed = Embed::new("m");
    let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
    embed.weft("Labeled", &[("label", &[], "String"), ("twice", &["f64"], "f64")]).expect("declares");
    embed
        .warp_method(&circle, "text", &[], "String", move |cx, r, l| {
            let radius: f64 = l.read(l.bytes(cx, r[1])?, 0);
            cx.new_string(format!("circle r={radius}").as_bytes(), r[0])
        })
        .expect("registers");
    embed
        .warp_method(&circle, "double", &["f64"], "f64", move |cx, r, l| {
            let radius: f64 = l.read(l.bytes(cx, r[0])?, 0);
            r[0] = (radius * f64::from_reg(r[1])).to_reg();
            Ok(())
        })
        .expect("registers");
    embed.weave(&circle, "Labeled", &[("label", "text"), ("twice", "double")]).expect("weaves");
    let program = compile(
        embed,
        "use m.{Circle, Labeled}\npub fun label_len(r: f64) -> usize {\n    let d: any Labeled = Circle(r: r)\n    d.label().len()\n}\npub fun twice(r: f64) -> f64 {\n    let d: any Labeled = Circle(r: r)\n    d.twice(4.0)\n}\n",
    );
    assert_eq!(call(&program, "label_len", &[2.0_f64.to_reg()]), [10]);
    assert_eq!(f64::from_reg(call(&program, "twice", &[2.0_f64.to_reg()])[0]), 8.0);
}

#[test]
fn a_weave_needs_a_host_method_for_each_weft_method() {
    let mut embed = Embed::new("m");
    let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
    embed.weft("Labeled", &[("label", &[], "String"), ("twice", &["f64"], "f64")]).expect("declares");
    assert!(embed.weave(&circle, "Labeled", &[("label", "text")]).is_err(), "twice has no host method");
    assert!(embed.weave(&circle, "Nope", &[]).is_err(), "no such weft");
}
