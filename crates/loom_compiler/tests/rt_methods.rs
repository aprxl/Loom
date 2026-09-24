//! Every method std declares on a warp the runtime implements (`List`,
//! `Str`, `Int`, …) has an implementation in `rt.ext`. A missing one would
//! only show up as a crash in a program that calls it.

use loom_compiler::hir::{DefKind, resolve};
use loom_compiler::lower::RUNTIME_WARPS;
use loom_compiler::modules::MemoryLoader;
use loom_compiler::syntax::source::SourceDb;
use loom_host::Vm;
use mlua::{Table, Value};

#[test]
fn every_runtime_method_is_implemented() {
    let mut db = SourceDb::new();
    let root = db.add("test.loom", "fn main() {}");
    let resolved = resolve(&mut db, root, &MemoryLoader::default());
    let p = &resolved.program;
    let vm = Vm::new().expect("vm");
    let rt: Table = vm.rt().expect("rt table");
    let ext: Table = rt.get("ext").expect("rt.ext");
    let mut missing = Vec::new();
    let mut checked = 0;
    for (i, d) in p.defs.iter().enumerate() {
        let DefKind::Fn(f) = &d.kind else { continue };
        let Some(parent) = d.parent else { continue };
        let warp = p.def_path(parent);
        if f.body.is_some() || !RUNTIME_WARPS.contains(&warp.as_str()) {
            continue;
        }
        let path = p.def_path(loom_compiler::hir::DefId(i as u32));
        checked += 1;
        if matches!(ext.get::<Value>(path.as_str()), Ok(Value::Nil) | Err(_)) {
            missing.push(path);
        }
    }
    assert!(checked > 50, "only {checked} methods found: is std loaded?");
    assert!(missing.is_empty(), "no `rt.ext` entry for:\n{}", missing.join("\n"));
}
