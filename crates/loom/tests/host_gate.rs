//! The speed gate of host functions (embedding): calling a function the host
//! registers costs at most 25% more than calling the same function written
//! in Loom, that is `loom_time / host_time >= 0.8`, on a call that does
//! nothing (the overhead) and on one that works for a while.
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

fn gate(name: &str, calls: u32, loom_step: &str, guarded_step: &str, main: &str, host: fn() -> HostFn) {
    let with_host = format!("use std.io.print\n\nextern \"host\" {{\n    pub fun step(n: u64) -> u64\n}}\n\n{main}");
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
    println!("{name}: ratio = {ratio:.2}, ratio_guarded = {ratio_guarded:.2} (at least {MIN_RATIO})");
    assert!(ratio_guarded >= MIN_RATIO, "{name}: ratio_guarded {ratio_guarded:.2} is below {MIN_RATIO}");
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_host_call_costs_what_a_loom_call_does() {
    gate(
        "overhead",
        10_000_000,
        "fun step(n: u64) -> u64 = n + 1",
        "fun step(x: u64) -> u64 {\n    if x == u64.MAX { step(x) } else { x + 1 }\n}",
        "fun main() {\n    var x = 0u64\n    for _ in 0..10000000 { x = step(x) }\n    print(x)\n}\n",
        || {
            Box::new(|r| {
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
        100_000,
        "fun step(n: u64) -> u64 {\n    var s = 0u64\n    for k in 1..=n { s += k }\n    s\n}",
        "fun step(n: u64) -> u64 {\n    if n == u64.MAX {\n        step(n)\n    } else {\n        var s = 0u64\n        for k in 1..=n { s += k }\n        s\n    }\n}",
        "fun main() {\n    var total = 0u64\n    for _ in 0..100000 { total += step(1000) }\n    print(total)\n}\n",
        || {
            Box::new(|r| {
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
