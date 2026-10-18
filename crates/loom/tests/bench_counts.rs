//! The deterministic speed gate (design/vm.md §16): how many instructions
//! Treadle dispatches for each benchmark in `bench/`, at the small size
//! `bench/counts.txt` gives, must stay within 2% of the count written
//! there. A change that means to move a count updates the file (run with
//! `LOOM_BLESS=1`); one that doesn't mean to is caught here, where wall-clock
//! time would be too noisy.
//!
//! Only with Treadle's `stats` feature: `cargo test -p loom --features
//! vm-stats`.

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

/// Compiles `bench/NAME.loom` with `SIZE` set to `size` and gives how many
/// instructions it dispatches.
fn dispatched(root: &Path, name: &str, size: u64) -> u64 {
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
    let compiled = treadle::compile(&db, &resolved.program, &typed).expect("compiles");
    let host = Host::captured(Rc::new(RefCell::new(Vec::new())));
    let mut vm = loom_vm::Vm::new(&compiled.image, host, &loom_vm::Config::default()).expect("loads");
    vm.call(compiled.image.entry.expect("an entry"), &[]).expect("runs");
    vm.dispatched().expect("built with `stats`")
}

#[test]
fn dispatch_counts_hold() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let counts_path = root.join("bench/counts.txt");
    let text = std::fs::read_to_string(&counts_path).expect("bench/counts.txt");
    let bless = std::env::var_os("LOOM_BLESS").is_some();
    let mut out = String::new();
    let mut failures = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [name, size, count] = fields.as_slice() else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let (Ok(size), Ok(want)) = (size.parse::<u64>(), count.parse::<u64>()) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let got = dispatched(&root, name, size);
        out.push_str(&format!("{name:<12} {size:<6} {got}\n"));
        let drift = got.abs_diff(want) as f64 / want.max(1) as f64;
        if drift > 0.02 {
            failures.push(format!(
                "{name}: {got} instructions, was {want} ({:+.1}%)",
                100.0 * (got as f64 / want as f64 - 1.0)
            ));
        }
    }
    if bless {
        std::fs::write(&counts_path, out).expect("writable");
        return;
    }
    assert!(failures.is_empty(), "dispatch counts moved (LOOM_BLESS=1 to accept):\n{}", failures.join("\n"));
}

/// Ten times the input costs about ten times the instructions: the deque
/// does constant work per operation, and sorting does `n log n`.
#[test]
fn std_collections_scale() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (name, small, large, limit) in [("deque", 50_000, 500_000, 10.5), ("list_ops", 20_000, 200_000, 12.0)] {
        let ratio = dispatched(&root, name, large) as f64 / dispatched(&root, name, small) as f64;
        assert!(ratio <= limit, "{name}: 10x the input cost {ratio:.2}x the instructions (limit {limit}x)");
    }
}
