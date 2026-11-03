//! The speed gate of host functions (embedding): calling a function the host
//! registers costs at most 25% more than calling the same function written
//! in Loom, that is `loom_guarded_time / host_time >= 0.8`. The cases are a
//! call that does nothing (the overhead), one that works for a while, and two
//! that pass text: one that reads a `str` and one that builds a `String`.
//!
//! `#[ignore]`d for wall-clock noise: `cargo test --release -p loom --test
//! host_gate -- --ignored --nocapture`.

use std::cell::RefCell;
use std::hint::black_box;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_isa::Image;
use loom_syntax::SourceDb;
use loom_sys::Host;
use loom_vm::{Config, HostFn, HostTable, Vm};

const RUNS: usize = 5;
const MIN_RATIO: f64 = 0.8;

fn compile(text: &str) -> Image {
    let path = Path::new("host_gate.loom");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    treadle::compile(&db, &resolved.program, &typed).expect("compiles").image
}

/// The best time of `RUNS` calls of `main`, and what it printed.
fn best(image: &Image, host: Option<fn() -> HostFn>) -> (Duration, String) {
    let mut best = Duration::MAX;
    let mut printed = String::new();
    for _ in 0..RUNS {
        let mut table = HostTable::default();
        if let Some(make) = host {
            table.register(&image.host_imports[0].name, make()).expect("registers");
        }
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut vm =
            Vm::new_hosted(image, Host::captured(Rc::clone(&output)), &Config::default(), table).expect("loads");
        let start = Instant::now();
        vm.call(image.entry.expect("an entry"), &[]).expect("runs");
        best = best.min(start.elapsed());
        printed = String::from_utf8_lossy(&output.borrow()).into_owned();
    }
    (best, printed)
}

/// One case: the host function `decl` declares, against the Loom function
/// `loom_step` and the same function with a guard, `guarded_step`, both run
/// by `main` `calls` times.
/// `gated`: whether the ratio is asserted; an ungated row is only reported.
#[allow(clippy::too_many_arguments)]
fn gate(
    name: &str,
    gated: bool,
    calls: u32,
    decl: &str,
    loom_step: &str,
    guarded_step: &str,
    main: &str,
    host: fn() -> HostFn,
) {
    let with_host = format!("use std.io.print\n\nextern \"host\" {{\n    {decl}\n}}\n\n{main}");
    let in_loom = format!("use std.io.print\n\n{loom_step}\n\n{main}");
    let guarded = format!("use std.io.print\n\n{guarded_step}\n\n{main}");
    let (host_time, host_out) = best(&compile(&with_host), Some(host));
    let (loom_time, loom_out) = best(&compile(&in_loom), None);
    let (guarded_time, guarded_out) = best(&compile(&guarded), None);
    assert_eq!(host_out, loom_out, "{name}: the two programs disagree");
    assert_eq!(host_out, guarded_out, "{name}: the guarded program disagrees");
    let per = |t: Duration| t.as_secs_f64() * 1e9 / f64::from(calls);
    let ratio = loom_time.as_secs_f64() / host_time.as_secs_f64();
    let ratio_guarded = guarded_time.as_secs_f64() / host_time.as_secs_f64();
    println!(
        "{name}: host {:.2} ns/call, loom {:.2} ns/call, loom_guarded {:.2} ns/call",
        per(host_time),
        per(loom_time),
        per(guarded_time)
    );
    let tag = if gated { "gated" } else { "ungated" };
    println!("{name} ({tag}): ratio = {ratio:.2}, ratio_guarded = {ratio_guarded:.2} (at least {MIN_RATIO})");
    if gated {
        assert!(ratio_guarded >= MIN_RATIO, "{name}: ratio_guarded {ratio_guarded:.2} is below {MIN_RATIO}");
    }
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_costs_what_a_loom_call_does() {
    println!("overhead: the guarded reference adds dispatches (jeqk, jmp), not a call");
    gate(
        "overhead",
        true,
        10_000_000,
        "pub fun step(n: u64) -> u64",
        "fun step(n: u64) -> u64 = n + 1",
        "fun step(x: u64) -> u64 {\n    if x == u64.MAX { step(x) } else { x + 1 }\n}",
        "fun main() {\n    var x = 0u64\n    for _ in 0..10000000 { x = step(x) }\n    print(x)\n}\n",
        || {
            Box::new(|_, r| {
                r[0] += 1;
                Ok(())
            })
        },
    );
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_that_works_costs_what_a_loom_call_does() {
    gate(
        "work",
        true,
        100_000,
        "pub fun step(n: u64) -> u64",
        "fun step(n: u64) -> u64 {\n    var s = 0u64\n    for k in 1..=n { s += k }\n    s\n}",
        "fun step(n: u64) -> u64 {\n    if n == u64.MAX {\n        step(n)\n    } else {\n        var s = 0u64\n        for k in 1..=n { s += k }\n        s\n    }\n}",
        "fun main() {\n    var total = 0u64\n    for _ in 0..100000 { total += step(1000) }\n    print(total)\n}\n",
        || {
            Box::new(|_, r| {
                let mut s = 0u64;
                for k in 1..=black_box(r[0]) {
                    s = s.wrapping_add(black_box(k));
                }
                r[0] = s;
                Ok(())
            })
        },
    );
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_length_call_is_reported_ungated() {
    gate(
        "len_of",
        false,
        10_000_000,
        "pub fun len_of(s: str) -> u64",
        "fun len_of(s: str) -> u64 = s.len() as u64",
        "fun len_of(s: str) -> u64 {\n    let n = s.len() as u64;\n    if n == u64.MAX { len_of(s) } else { n }\n}",
        "fun main() {\n    var total = 0u64\n    var k = 0u64\n    for _ in 0..10000000 {\n        let s = if k % 2 == 0 { \"héllo\" } else { \"héllo!\" }\n        total += len_of(s)\n        k += 1\n    }\n    print(total)\n}\n",
        || {
            Box::new(|_, r| {
                r[0] = r[1];
                Ok(())
            })
        },
    );
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_that_reads_every_byte_costs_what_a_loom_call_does() {
    // Two texts of 64 bytes, with 20 and 32 spaces, picked by parity.
    let (a, b) = ("ab cd ".repeat(10) + "efgh", "a b c d e f g h ".repeat(4));
    assert_eq!((a.len(), b.len()), (64, 64));
    let main = format!(
        "fun main() {{\n    var total = 0u64\n    var k = 0u64\n    for _ in 0..1000000 {{\n        let s = if k % 2 == 0 {{ \"{a}\" }} else {{ \"{b}\" }}\n        total += count_spaces(s)\n        k += 1\n    }}\n    print(total)\n}}\n"
    );
    gate(
        "count_spaces",
        true,
        1_000_000,
        "pub fun count_spaces(s: str) -> u64",
        "fun count_spaces(s: str) -> u64 {\n    var n = 0u64\n    for c in s.bytes() {\n        if c == 32u8 { n += 1 }\n    }\n    n\n}",
        "fun count_spaces(s: str) -> u64 {\n    var n = 0u64\n    for c in s.bytes() {\n        if c == 32u8 { n += 1 }\n    }\n    if n == u64.MAX { count_spaces(s) } else { n }\n}",
        &main,
        || {
            Box::new(|cx, r| {
                let n = cx.text(r[0], r[1])?.iter().filter(|&&c| c == b' ').count() as u64;
                r[0] = n;
                Ok(())
            })
        },
    );
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_that_builds_a_string_costs_what_a_loom_call_does() {
    gate(
        "to_text",
        true,
        1_000_000,
        "pub fun to_text(n: u64) -> String",
        "fun to_text(n: u64) -> String = \"{n}\"",
        "fun to_text(n: u64) -> String {\n    if n == u64.MAX { to_text(n) } else { \"{n}\" }\n}",
        "fun main() {\n    var total = 0usize\n    for k in 0..1000000 { total += to_text(k).len() }\n    print(total)\n}\n",
        || {
            Box::new(|cx, r| {
                let text = r[1].to_string();
                cx.new_string(text.as_bytes(), r[0])
            })
        },
    );
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_that_counts_words_costs_what_a_loom_call_does() {
    // The texts of the space count, with 20 and 32 words.
    let (a, b) = ("ab cd ".repeat(10) + "efgh", "a b c d e f g h ".repeat(4));
    let main = format!(
        "fun main() {{\n    var total = 0u64\n    var k = 0u64\n    for _ in 0..1000000 {{\n        let s = if k % 2 == 0 {{ \"{a}\" }} else {{ \"{b}\" }}\n        total += count_words(s)\n        k += 1\n    }}\n    print(total)\n}}\n"
    );
    gate(
        "count_words",
        true,
        1_000_000,
        "pub fun count_words(s: str) -> u64",
        "fun count_words(s: str) -> u64 {\n    var n = 0u64\n    var inword = false\n    for c in s.bytes() {\n        if c == 32u8 {\n            inword = false\n        } else if not inword {\n            inword = true\n            n += 1\n        }\n    }\n    n\n}",
        "fun count_words(s: str) -> u64 {\n    var n = 0u64\n    var inword = false\n    for c in s.bytes() {\n        if c == 32u8 {\n            inword = false\n        } else if not inword {\n            inword = true\n            n += 1\n        }\n    }\n    if n == u64.MAX { count_words(s) } else { n }\n}",
        &main,
        || {
            Box::new(|cx, r| {
                let text = cx.text(r[0], r[1])?;
                let (mut n, mut inword) = (0u64, false);
                for &c in text {
                    if c == b' ' {
                        inword = false;
                    } else if !inword {
                        inword = true;
                        n += 1;
                    }
                }
                r[0] = n;
                Ok(())
            })
        },
    );
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_that_evaluates_a_polynomial_costs_what_a_loom_call_does() {
    let horner =
        "((((((((-0.5 * x + 0.75) * x - 0.25) * x + 0.5) * x + 0.0625) * x - 0.125) * x + 0.25) * x + 0.5) * x + 1.0)";
    let main = "fun main() {\n    var total = 0.0f64\n    for k in 0..1000000 {\n        total += poly((k % 7) as f64)\n    }\n    print(total)\n}\n";
    gate(
        "poly",
        true,
        1_000_000,
        "pub fun poly(x: f64) -> f64",
        &format!("fun poly(x: f64) -> f64 = {horner}"),
        &format!("fun poly(x: f64) -> f64 {{\n    let y = {horner}\n    if y != y {{ poly(x) }} else {{ y }}\n}}"),
        main,
        || {
            Box::new(|_, r| {
                let x = f64::from_bits(r[0]);
                let y = (((((((-0.5 * x + 0.75) * x - 0.25) * x + 0.5) * x + 0.0625) * x - 0.125) * x + 0.25) * x
                    + 0.5)
                    * x
                    + 1.0;
                r[0] = y.to_bits();
                Ok(())
            })
        },
    );
}

/// The best time of `RUNS` calls of an exported `run`, and what it printed.
fn best_export(program: &loom_embed::Program) -> (Duration, String) {
    let mut best = Duration::MAX;
    let mut printed = String::new();
    for _ in 0..RUNS {
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut run = program.start(Host::captured(Rc::clone(&output))).expect("starts");
        let start = Instant::now();
        run.call("run", &[]).expect("runs");
        best = best.min(start.elapsed());
        printed = String::from_utf8_lossy(&output.borrow()).into_owned();
    }
    (best, printed)
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_buffer_append_costs_what_a_loom_list_push_costs() {
    const CALLS: u32 = 200_000;
    let host = {
        let mut embed = loom_embed::Embed::new("m");
        let buffer = embed.handle::<Vec<String>>("Buffer").expect("declares");
        embed.constructor0(&buffer, "open", Vec::new).expect("registers");
        embed
            .method(&buffer, "append", |items: &mut Vec<String>, text: &str| {
                items.push(text.to_string());
                items.len() as u64
            })
            .expect("registers");
        embed
            .compile(&format!(
                "use std.io.print
use m.{{open}}

pub fun run() {{
    var total = 0u64
    let b = open()
    var k = 0u64
    for _ in 0..{CALLS} {{
        let s = if k % 2 == 0 {{ \"héllo\" }} else {{ \"héllo!\" }}
        total += b.append(s)
        k += 1
    }}
    print(total)
}}
"
            ))
            .unwrap_or_else(|d| panic!("{d:?}"))
    };
    let loom = loom_embed::Embed::new("m")
        .compile(&format!(
            "use std.io.print

fun append_ref(mut xs: List[String], text: str) -> u64 {{
    xs.push(\"{{text}}\")
    let n = xs.len() as u64
    if n == u64.MAX {{ append_ref(mut xs, text) }} else {{ n }}
}}

pub fun run() {{
    var xs: List[String] = []
    var total = 0u64
    var k = 0u64
    for _ in 0..{CALLS} {{
        let s = if k % 2 == 0 {{ \"héllo\" }} else {{ \"héllo!\" }}
        total += append_ref(mut xs, s)
        k += 1
    }}
    print(total)
}}
"
        ))
        .unwrap_or_else(|d| panic!("{d:?}"));
    let (host_time, host_out) = best_export(&host);
    let (loom_time, loom_out) = best_export(&loom);
    assert_eq!(host_out, loom_out, "append: the two programs disagree");
    let per = |t: Duration| t.as_secs_f64() * 1e9 / f64::from(CALLS);
    let ratio = loom_time.as_secs_f64() / host_time.as_secs_f64();
    println!(
        "append (gated): host {:.2} ns/call, loom_list {:.2} ns/call, ratio_guarded = {ratio:.2} (at least {MIN_RATIO})",
        per(host_time),
        per(loom_time)
    );
    assert!(ratio >= MIN_RATIO, "append: ratio_guarded {ratio:.2} is below {MIN_RATIO}");
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_mat3_product_costs_what_a_loom_method_costs() {
    const CALLS: u32 = 200_000;
    let names: Vec<String> = (0..9).map(|k| format!("m{}{}", k / 3, k % 3)).collect();
    let user = format!(
        "use std.io.print
use m.{{Mat3}}

pub fun run() {{
    var total = 0.0f64
    let b = Mat3(m00: 1.0, m01: 0.5, m02: 0.25, m10: 0.0, m11: 1.0, m12: 0.5, m20: 0.5, m21: 0.0, m22: 1.0)
    for k in 0..{CALLS} {{
        let kf = (k % 7) as f64
        let x = Mat3(m00: kf, m01: 1.0, m02: 2.0, m10: 0.5, m11: kf, m12: 1.0, m20: 2.0, m21: 0.5, m22: kf)
        let r = x.mul(b)
        total += r.m00 + r.m22
    }}
    print(total)
}}
"
    );
    // The product's entry (i, j): a row of `self` times a column of `other`, summed in order.
    let entry =
        |i: usize, j: usize| format!("self.m{i}0 * other.m0{j} + self.m{i}1 * other.m1{j} + self.m{i}2 * other.m2{j}");
    let fields: Vec<String> = names.iter().map(|n| format!("pub {n}: f64")).collect();
    let body: Vec<String> = (0..9).map(|k| format!("m{}{}: {}", k / 3, k % 3, entry(k / 3, k % 3))).collect();
    let reference = format!(
        "pub warp Mat3 {{
    {},
    pub fun mul(self, other: Mat3) -> Mat3 {{
        let r = Mat3({})
        if r.m00 != r.m00 {{ self.mul(other) }} else {{ r }}
    }}
}}
",
        fields.join(
            ",
    "
        ),
        body.join(", ")
    );
    let loom = {
        let mut embed = loom_embed::Embed::new("m");
        embed.source(&reference).unwrap_or_else(|e| panic!("{e}"));
        embed.compile(&user).unwrap_or_else(|d| panic!("{d:?}"))
    };
    let host = {
        let mut embed = loom_embed::Embed::new("m");
        let pairs: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "f64")).collect();
        let mat = embed.warp("Mat3", &pairs).expect("declares");
        embed
            .warp_method(&mat, "mul", &["Mat3"], "Mat3", move |cx, r, l| {
                let (out, a, b) = (r[0], r[1], r[2]);
                let (mut x, mut y) = ([0.0_f64; 9], [0.0_f64; 9]);
                {
                    let xs = l.bytes(cx, a)?;
                    let ys = l.bytes(cx, b)?;
                    for k in 0..9 {
                        x[k] = l.read(xs, k);
                        y[k] = l.read(ys, k);
                    }
                }
                let mut c = [0.0_f64; 9];
                for i in 0..3 {
                    for j in 0..3 {
                        c[i * 3 + j] = x[i * 3] * y[j] + x[i * 3 + 1] * y[3 + j] + x[i * 3 + 2] * y[6 + j];
                    }
                }
                let zs = l.bytes_mut(cx, out)?;
                for (k, value) in c.into_iter().enumerate() {
                    l.write(zs, k, value);
                }
                Ok(())
            })
            .expect("registers");
        embed.compile(&user).unwrap_or_else(|d| panic!("{d:?}"))
    };
    let (host_time, host_out) = best_export(&host);
    let (loom_time, loom_out) = best_export(&loom);
    assert_eq!(host_out, loom_out, "mat3: the two programs disagree");
    let per = |t: Duration| t.as_secs_f64() * 1e9 / f64::from(CALLS);
    let ratio = loom_time.as_secs_f64() / host_time.as_secs_f64();
    println!(
        "mat3 (gated): host {:.2} ns/call, loom_guarded {:.2} ns/call, ratio_guarded = {ratio:.2} (at least {MIN_RATIO})",
        per(host_time),
        per(loom_time)
    );
    assert!(ratio >= MIN_RATIO, "mat3: ratio_guarded {ratio:.2} is below {MIN_RATIO}");
}
