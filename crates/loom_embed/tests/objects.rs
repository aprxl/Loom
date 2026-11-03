use std::cell::RefCell;
use std::rc::Rc;

use loom_embed::{Embed, Program, Scalar};
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
