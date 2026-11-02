//! A program that calls a function the host registers: it runs on Treadle
//! with the host's closure bound to the import.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_syntax::SourceDb;
use loom_sys::Host;
use loom_vm::{Config, HostTable, Vm};

#[test]
fn a_registered_host_function_runs() {
    let text = "use std.io.print\n\nextern \"host\" {\n    pub fun add(x: u64, y: u64) -> u64\n}\n\nfun main() {\n    print(add(2, 3))\n}\n";
    let path = Path::new("host_run.loom");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    let image = treadle::compile(&db, &resolved.program, &typed).expect("compiles").image;
    let name = image.host_imports[0].name.clone();
    let mut table = HostTable::default();
    table
        .register(
            &name,
            Box::new(|r| {
                r[0] += r[1];
                Ok(())
            }),
        )
        .expect("registers");
    let output = Rc::new(RefCell::new(Vec::new()));
    let host = Host::captured(Rc::clone(&output));
    let mut vm = Vm::new_hosted(&image, host, &Config::default(), table).expect("loads");
    vm.call(image.entry.expect("an entry"), &[]).expect("runs");
    assert_eq!(String::from_utf8_lossy(&output.borrow()), "5\n");
}
