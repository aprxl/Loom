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
use loom_sys::{Host, Outcome};
use loom_vm::{Config, HostFn, HostTable, Vm};

/// Compiles `text` and runs its entry with the host functions `hosts`
/// bound by import name, giving what it printed.
fn run(text: &str, hosts: Vec<(&str, HostFn)>, config: &Config) -> Result<String, Outcome> {
    let path = Path::new("host_run.loom");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), text.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    let image = treadle::compile(&db, &resolved.program, &typed).expect("compiles").image;
    let mut table = HostTable::default();
    for (name, f) in hosts {
        table.register(name, f).expect("registers");
    }
    let output = Rc::new(RefCell::new(Vec::new()));
    let host = Host::captured(Rc::clone(&output));
    let mut vm = Vm::new_hosted(&image, host, config, table)?;
    vm.call(image.entry.expect("an entry"), &[])?;
    Ok(String::from_utf8_lossy(&output.borrow()).into_owned())
}

#[test]
fn a_registered_host_function_runs() {
    let text = "use std.io.print\n\nextern \"host\" {\n    pub fun add(x: u64, y: u64) -> u64\n}\n\nfun main() {\n    print(add(2, 3))\n}\n";
    let add: HostFn = Box::new(|_, r| {
        r[0] += r[1];
        Ok(())
    });
    assert_eq!(run(text, vec![("host_run.add", add)], &Config::default()).expect("runs"), "5\n");
}

#[test]
fn a_text_argument_is_its_address_and_length() {
    let text = "use std.io.print\n\nextern \"host\" {\n    pub fun len_of(s: str) -> u64\n}\n\nfun main() {\n    print(len_of(\"h\u{e9}llo\"))\n    print(len_of(\"\"))\n}\n";
    let len_of: HostFn = Box::new(|cx, r| {
        let n = cx.text(r[0], r[1])?.len() as u64;
        r[0] = n;
        Ok(())
    });
    assert_eq!(run(text, vec![("host_run.len_of", len_of)], &Config::default()).expect("runs"), "6\n0\n");
}

#[test]
fn a_string_result_is_written_to_its_slot() {
    let text = "use std.io.print\n\nextern \"host\" {\n    pub fun to_text(n: u64) -> String\n    pub fun empty() -> String\n}\n\nfun main() {\n    print(to_text(42))\n    print(empty())\n    print(to_text(7).len())\n}\n";
    let to_text: HostFn = Box::new(|cx, r| {
        let text = r[1].to_string();
        cx.new_string(text.as_bytes(), r[0])
    });
    let empty: HostFn = Box::new(|cx, r| cx.new_string(b"", r[0]));
    let hosts = vec![("host_run.to_text", to_text), ("host_run.empty", empty)];
    assert_eq!(run(text, hosts, &Config::default()).expect("runs"), "42\n\n1\n");
}

#[test]
fn text_functions_are_refused_at_compile_time() {
    let text = "use std.io.print\n\nextern \"host\" {\n    pub fun len_of(s: str) -> u64\n}\n\nfun main() {\n    print(len_of(\"x\"))\n}\n";
    let len_of: HostFn = Box::new(|_, _| panic!("a refused call must not run the host function"));
    let config = Config { refuse_host: true, ..Config::default() };
    let Err(Outcome::Refused { native, .. }) = run(text, vec![("host_run.len_of", len_of)], &config) else {
        panic!("expected a refusal")
    };
    assert_eq!(native, "hostcall");
}
