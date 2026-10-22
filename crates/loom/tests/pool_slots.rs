//! The Lua bundle sizes a `Pool`'s slots as Treadle does (`loom_vm::slot_size`),
//! so both backends run out of memory at the same slot count.

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{lower, typeck};
use loom_syntax::SourceDb;

#[test]
fn lua_pool_slots_are_treadles() {
    let source = "use std.io.print\nuse std.mem.Pool\n\nwarp Trio {\n    a: u64\n    b: u64\n    c: u64\n}\n\nfn main() {\n    spool a = Pool[u8](1)\n    spool b = Pool[u64](1)\n    spool c = Pool[Trio](1)\n    print(\"x\")\n}\n";
    let path = std::env::temp_dir().join(format!("loom-pool-slots-{}.loom", std::process::id()));
    std::fs::write(&path, source).expect("a temporary file");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), source.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(&path));
    let _ = std::fs::remove_file(&path);
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::LUA);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    let bundle = lower::lower(&db, &resolved.program, &typed).expect("lowers");
    // A 1-byte value, an 8-byte one and a 24-byte warp.
    for (size, slot) in [(1, 16), (8, 16), (24, 32)] {
        assert_eq!(loom_vm::slot_size(size), slot, "Treadle's slot for {size} bytes");
        assert!(bundle.lua.contains(&format!("rt.pool_new({slot}, 1)")), "a {size}-byte pool's slot is {slot}");
    }
}
