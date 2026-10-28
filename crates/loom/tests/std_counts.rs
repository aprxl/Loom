//! The standard library's methods written in Loom cost about what the loop
//! written by hand costs: how many instructions Treadle dispatches for a
//! method, set against the same work done with a hand-written loop, both
//! with the cost of building the input taken away.
//!
//! The plan (design/stdlib-plan.md, section 4.3) asks for 1.1x for list
//! methods and 1.25x for stream adaptors in front of `sum`. Where a method
//! misses, `GUARD` holds it at what it costs now, so it can't get worse
//! unnoticed, and `PLAN` says what it should reach.
//!
//! Only with Treadle's `stats` feature: `cargo test -p loom --features
//! vm-stats`. `LOOM_COUNTS_VERBOSE=1` prints every ratio.

#![cfg(feature = "vm-stats")]

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_host::Host;
use loom_syntax::SourceDb;

const N: u64 = 20_000;

/// The instructions a program dispatches, and what it prints: `setup` builds
/// `xs`, then `work` runs.
fn run(setup: &str, work: &str) -> (u64, String) {
    let text = format!("use std.io.print\n\nfn main() {{\n{setup}\n{work}\n}}\n");
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/counts_probe.loom");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.clone());
    let resolved = resolve(&mut db, file, &FsLoader::new(&path));
    assert!(!resolved.has_errors(), "{text}\n{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{text}\n{:?}", typed.diagnostics);
    let compiled = treadle::compile(&db, &resolved.program, &typed).expect("compiles");
    let buffer = Rc::new(RefCell::new(Vec::new()));
    let mut vm =
        loom_vm::Vm::new(&compiled.image, Host::captured(buffer.clone()), &loom_vm::Config::default()).expect("loads");
    vm.call(compiled.image.entry.expect("an entry"), &[]).expect("runs");
    let dispatched = vm.dispatched().expect("built with `stats`");
    (dispatched, String::from_utf8_lossy(&buffer.borrow()).into_owned())
}

struct Case {
    name: &'static str,
    /// Builds `xs`, a list of `u64`.
    setup: &'static str,
    std: &'static str,
    hand: &'static str,
    /// What the plan asks for.
    plan: f64,
    /// What it costs now, with some room.
    guard: f64,
}

const COUNTING: &str = "    var xs: List[u64] = []\n    for i in 0..20000u64 { xs.push(i * 7 % 1000) }\n";
const SORTED: &str = "    var xs: List[u64] = []\n    for i in 0..20000u64 { xs.push(i / 3) }\n";

const CASES: &[Case] = &[
    Case {
        name: "retain",
        setup: COUNTING,
        std: "    xs.retain(_ % 3 != 0)\n    print(xs.len())",
        hand: "    var w = 0\n    for i in 0..xs.len() {\n        if xs[i] % 3 != 0 {\n            if w != i { xs.swap(w, i) }\n            w += 1\n        }\n    }\n    xs.truncate(w)\n    print(xs.len())",
        plan: 1.1,
        guard: 1.5,
    },
    Case {
        name: "dedup",
        setup: SORTED,
        std: "    xs.dedup()\n    print(xs.len())",
        hand: "    var w = 1\n    for i in 1..xs.len() {\n        if xs[i] != xs[w - 1] {\n            if w != i { xs.swap(w, i) }\n            w += 1\n        }\n    }\n    xs.truncate(w)\n    print(xs.len())",
        plan: 1.1,
        guard: 1.5,
    },
    Case {
        name: "swap_remove",
        setup: COUNTING,
        std: "    var s = 0u64\n    while xs.len() > 0 { s += xs.swap_remove(0) }\n    print(s)",
        hand: "    var s = 0u64\n    while xs.len() > 0 {\n        xs.swap(0, xs.len() - 1)\n        s += xs.pop() mend 0\n    }\n    print(s)",
        plan: 1.1,
        guard: 1.6,
    },
    Case {
        name: "resize",
        setup: COUNTING,
        std: "    xs.resize(60000, 7)\n    print(xs.len())",
        hand: "    for _ in 0..40000 { xs.push(7) }\n    print(xs.len())",
        plan: 1.1,
        guard: 2.6,
    },
    Case {
        name: "step_by.sum",
        setup: COUNTING,
        std: "    print(xs.step_by(3).sum())",
        hand: "    var s = 0u64\n    var i = 0\n    while i < xs.len() {\n        s += xs[i]\n        i += 3\n    }\n    print(s)",
        plan: 1.25,
        guard: 2.4,
    },
    Case {
        name: "filter_map.sum",
        setup: COUNTING,
        std: "    print(xs.filter_map(x => if x % 2 == 0 { some(x / 2) } else { none }).sum())",
        hand: "    var s = 0u64\n    for x in xs {\n        if x % 2 == 0 { s += x / 2 }\n    }\n    print(s)",
        plan: 1.25,
        guard: 2.4,
    },
    Case {
        name: "min_by_key",
        setup: COUNTING,
        std: "    print(xs.min_by_key(x => x % 17))",
        hand: "    var best = 0u64\n    var best_key = 1000u64\n    for x in xs {\n        if x % 17 < best_key {\n            best = x\n            best_key = x % 17\n        }\n    }\n    print(best)",
        plan: 1.25,
        guard: 7.5,
    },
    Case {
        name: "counts",
        setup: COUNTING,
        std: "    print(xs.counts().len())",
        hand: "    var m: Map[u64, usize] = Map()\n    for x in xs { m.entry(x, default: 0) += 1 }\n    print(m.len())",
        plan: 1.1,
        guard: 2.5,
    },
    Case {
        name: "sort_by vs sort",
        setup: COUNTING,
        std: "    xs.sort_by(_ < _)\n    print(xs[0])",
        hand: "    xs.sort()\n    print(xs[0])",
        plan: 3.0,
        guard: 40.0,
    },
];

#[test]
fn std_methods_match_hand_loops() {
    let verbose = std::env::var_os("LOOM_COUNTS_VERBOSE").is_some();
    for setup in [COUNTING, SORTED] {
        let (_, len) = run(setup, "    print(xs.len())");
        assert_eq!(len.trim(), N.to_string(), "the setup builds N items");
    }
    let mut failures = Vec::new();
    for case in CASES {
        let base = run(case.setup, "    print(xs.len())").0;
        let std = run(case.setup, case.std).0.saturating_sub(base).max(1);
        let hand = run(case.setup, case.hand).0.saturating_sub(base).max(1);
        let ratio = std as f64 / hand as f64;
        if verbose {
            eprintln!(
                "{:<18} std {std:>9} hand {hand:>9} ratio {ratio:.2} (plan {}, guard {})",
                case.name, case.plan, case.guard
            );
        }
        if ratio > case.guard {
            failures
                .push(format!("{}: {ratio:.2}x the hand loop (guard {}x, plan {}x)", case.name, case.guard, case.plan));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
