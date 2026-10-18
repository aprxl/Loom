//! V3's check-removal gate (design/vm.md §17, item 4): over the benchmarks
//! at the small sizes of `bench/counts.txt`, at least 80% of the bounds
//! checks and 60% of the overflow checks that run at `-O0` don't run at
//! `-O2`. The stats build counts them.
//!
//! Only with Treadle's `stats` feature: `cargo test -p loom --features
//! vm-stats`.

#![cfg(feature = "vm-stats")]

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_host::Host;
use loom_isa::Check;
use loom_syntax::SourceDb;

/// The bounds and overflow checks `bench/NAME.loom` runs with `SIZE` set to
/// `size`, compiled at `level`.
fn checks(root: &Path, name: &str, size: u64, level: Level) -> (u64, u64) {
    let path = root.join("bench").join(format!("{name}.loom"));
    let text = std::fs::read_to_string(&path).expect("readable benchmark");
    let line = text.lines().find(|l| l.starts_with("const SIZE = ")).expect("a benchmark has `const SIZE`");
    let text = text.replacen(line, &format!("const SIZE = {size}"), 1);
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text);
    let resolved = resolve(&mut db, file, &FsLoader::new(&path));
    assert!(!resolved.has_errors(), "{name}: {:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{name}: {:?}", typed.diagnostics);
    let compiled = treadle::compile_at(&db, &resolved.program, &typed, level).expect("compiles");
    let host = Host::captured(Rc::new(RefCell::new(Vec::new())));
    let mut vm = loom_vm::Vm::new(&compiled.image, host, &loom_vm::Config::default()).expect("loads");
    vm.call(compiled.image.entry.expect("an entry"), &[]).expect("runs");
    let counts = vm.op_counts().expect("built with `stats`");
    let of = |c: Check| counts.iter().filter(|(op, _)| op.check() == Some(c)).map(|(_, n)| n).sum::<u64>();
    (of(Check::Bounds), of(Check::Overflow))
}

#[test]
fn most_checks_are_gone_at_o2() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join("bench/counts.txt")).expect("bench/counts.txt");
    let mut o0 = (0u64, 0u64);
    let mut o2 = (0u64, 0u64);
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [name, size, _] = fields.as_slice() else { continue };
        let Ok(size) = size.parse::<u64>() else { continue };
        // These measure the standard library's collections (see
        // `std_collections_scale`), not the optimizer's loops.
        if matches!(*name, "deque" | "list_ops" | "heap") {
            continue;
        }
        let (b0, v0) = checks(&root, name, size, Level::O0);
        let (b2, v2) = checks(&root, name, size, Level::O2);
        o0 = (o0.0 + b0, o0.1 + v0);
        o2 = (o2.0 + b2, o2.1 + v2);
    }
    let gone = |before: u64, after: u64| 100.0 * (1.0 - after as f64 / before.max(1) as f64);
    let bounds = gone(o0.0, o2.0);
    let overflow = gone(o0.1, o2.1);
    assert!(bounds >= 80.0, "bounds checks: {bounds:.1}% gone at -O2 ({} of {} left), V3 wants 80%", o2.0, o0.0);
    assert!(overflow >= 60.0, "overflow checks: {overflow:.1}% gone at -O2 ({} of {} left), V3 wants 60%", o2.1, o0.1);
}
