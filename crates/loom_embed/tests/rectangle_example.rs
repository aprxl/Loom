//! An embedded Loom instance with a built-in `Rectangle` value warp: a host
//! method, a host constant, Loom source hardcoded into the module, and a `pub`
//! function that Rust calls.

use std::cell::RefCell;
use std::rc::Rc;

use loom_embed::{Embed, Literal, Scalar};
use loom_sys::Host;

#[test]
fn a_rectangle_built_in_is_used_from_loom_and_called_from_rust() {
    let mut embed = Embed::new("shapes");

    let rectangle = embed.warp("Rectangle", &[("w", "f64"), ("h", "f64")]).expect("declares");
    embed
        .warp_method(&rectangle, "area", &[], "f64", move |cx, r, layout| {
            let bytes = layout.bytes(cx, r[0])?;
            let area = layout.read::<f64>(bytes, 0) * layout.read::<f64>(bytes, 1);
            r[0] = area.to_reg();
            Ok(())
        })
        .expect("registers");

    let one = Literal::of(1.0_f64).expect("a literal");
    embed.constant_warp(&rectangle, "UNIT_SQUARE", &[("w", one.clone()), ("h", one)]).expect("declares");

    embed.source("pub fun doubled_area(r: Rectangle) -> f64 { r.area() * 2.0 }").expect("adds");

    let program = embed
        .compile(
            "use shapes.{Rectangle, UNIT_SQUARE, doubled_area}\npub fun report(w: f64, h: f64) -> f64 {\n    Rectangle(w: w, h: h).area() + doubled_area(UNIT_SQUARE) + doubled_area(Rectangle(w: w, h: h))\n}\n",
        )
        .unwrap_or_else(|d| panic!("{d:?}"));

    let host = Host::captured(Rc::new(RefCell::new(Vec::new())));
    let mut run = program.start(host).expect("starts");
    let out = run.call("report", &[3.0_f64.to_reg(), 2.0_f64.to_reg()]).expect("runs");

    // 3 x 2 = 6, the unit square doubled is 2, and the 3 x 2 rectangle doubled is 12.
    assert_eq!(f64::from_reg(out[0]), 20.0);
}
