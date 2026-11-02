use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use loom_isa::word::word;
use loom_isa::{Func, HostImport, Image, Op};
use loom_sys::{Host, Outcome};
use loom_vm::{Config, HostTable, Vm};

fn import(name: &str, args: u16, results: u16) -> HostImport {
    HostImport { name: name.into(), args, results }
}

/// An image of one function, `f(x)`, that does `host.call` of import `c`
/// over its registers and returns `r0`, and the image's imports. The frame
/// holds 4 registers.
fn image(calls: &[u16], imports: Vec<HostImport>) -> Image {
    let mut code: Vec<u64> = calls.iter().map(|&c| word(Op::CallN, 0, 0, c)).collect();
    code.push(word(Op::Ret, 0, 1, 0));
    let func = Func {
        name: "f".into(),
        entry: 0,
        len: code.len() as u32,
        frame: 4,
        params: 1,
        results: 1,
        lines: vec![],
        pads: vec![],
        sites: vec![],
    };
    Image {
        funcs: vec![func],
        code,
        natives: vec!["host.call".into()],
        host_imports: imports,
        entry: Some(0),
        ..Image::default()
    }
}

fn host() -> Host {
    Host::captured(Rc::new(RefCell::new(Vec::new())))
}

fn vm<'i>(img: &'i Image, table: HostTable, config: &Config) -> Result<Vm<'i>, Outcome> {
    Vm::new_hosted(img, host(), config, table)
}

#[test]
fn a_host_function_reads_and_writes_the_registers() {
    let img = image(&[0], vec![import("app.increment", 1, 1)]);
    let mut table = HostTable::default();
    table
        .register(
            "app.increment",
            Box::new(|r| {
                r[0] += 1;
                Ok(())
            }),
        )
        .expect("registers");
    let mut vm = vm(&img, table, &Config::default()).expect("loads");
    assert_eq!(vm.call(0, &[5]).expect("runs"), vec![6]);
}

#[test]
fn each_import_gets_its_own_function_and_window() {
    let img = image(&[0, 1], vec![import("app.one", 1, 1), import("app.two", 2, 1)]);
    let mut table = HostTable::default();
    table
        .register(
            "app.one",
            Box::new(|r| {
                if r.len() != 1 {
                    return Err(format!("window of {}", r.len()));
                }
                r[0] += 1;
                Ok(())
            }),
        )
        .expect("registers");
    table
        .register(
            "app.two",
            Box::new(|r| {
                if r.len() != 2 {
                    return Err(format!("window of {}", r.len()));
                }
                r[0] += 10;
                Ok(())
            }),
        )
        .expect("registers");
    let mut vm = vm(&img, table, &Config::default()).expect("loads");
    assert_eq!(vm.call(0, &[5]).expect("runs"), vec![16]);
}

#[test]
fn an_error_from_the_host_is_an_unravel() {
    let img = image(&[0], vec![import("app.fail", 1, 1)]);
    let mut table = HostTable::default();
    table.register("app.fail", Box::new(|_| Err("boom".into()))).expect("registers");
    let mut vm = vm(&img, table, &Config::default()).expect("loads");
    let Err(Outcome::Unravel { message, .. }) = vm.call(0, &[5]) else { panic!("expected an unravel") };
    assert_eq!(message, "boom");
}

#[test]
fn an_unregistered_import_does_not_load() {
    let img = image(&[0], vec![import("app.missing", 1, 1)]);
    let Err(Outcome::Crash { message, .. }) = vm(&img, HostTable::default(), &Config::default()) else {
        panic!("expected a crash")
    };
    assert!(message.contains("unbound host import"), "{message}");
}

#[test]
fn a_name_registers_once() {
    let mut table = HostTable::default();
    table.register("app.f", Box::new(|_| Ok(()))).expect("registers");
    assert!(table.register("app.f", Box::new(|_| Ok(()))).is_err());
}

#[test]
fn compile_time_code_cannot_call_the_host() {
    let img = image(&[0], vec![import("app.increment", 1, 1)]);
    let called = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&called);
    let mut table = HostTable::default();
    table
        .register(
            "app.increment",
            Box::new(move |_| {
                seen.store(true, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("registers");
    let config = Config { refuse_host: true, ..Config::default() };
    let mut vm = vm(&img, table, &config).expect("loads");
    let Err(Outcome::Refused { native, .. }) = vm.call(0, &[5]) else { panic!("expected a refusal") };
    assert_eq!(native, "host.call");
    assert!(!called.load(Ordering::Relaxed));
}
