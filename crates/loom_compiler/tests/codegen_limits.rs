//! What codegen says when a function is over one of Treadle's limits: a
//! `CodegenError::Limit`, which the compiler reports as U0103, not a
//! program it "can't run yet" (U0102) and not a bug.

use loom_compiler::codegen::{self, CodegenError};
use loom_compiler::heddle::{Block, Func, FuncId, Inst, InstKind, Module, Slot, Term, VTy, Value};
use loom_syntax::SourceDb;

fn generate(func: Func) -> Result<loom_isa::Image, CodegenError> {
    let module = Module {
        funcs: vec![func],
        strings: Vec::new(),
        fmts: Vec::new(),
        data: Vec::new(),
        entry: FuncId(0),
        roots: Vec::new(),
        relocs: Vec::new(),
        plain: Vec::new(),
    };
    loom_compiler::heddle::verify(&module).expect("valid Heddle");
    codegen::generate(&SourceDb::new(), &module)
}

/// A function whose one block is `insts`, returning `ret`.
fn func(params: u32, types: Vec<VTy>, insts: Vec<Inst>, ret: Vec<Value>, slots: Vec<Slot>) -> Func {
    let block = Block {
        params: (0..params).map(Value).collect(),
        insts,
        term: Some(Term::Ret(ret.clone())),
        term_span: None,
        term_unwind: None,
        term_site: None,
    };
    Func { name: "f".into(), blocks: vec![block], results: vec![VTy::Int; ret.len()], types, slots, sites: Vec::new() }
}

fn inst(kind: InstKind, results: Vec<Value>) -> Inst {
    Inst { kind, results, span: None, unwind: None, site: None }
}

#[test]
fn a_call_through_a_function_value_takes_at_most_255_arguments() {
    let args: Vec<Value> = (1..=256).map(Value).collect();
    let r = Value(257);
    let f = func(
        257,
        vec![VTy::Int; 258],
        vec![inst(InstKind::CallInd { callee: Value(0), args }, vec![r])],
        vec![r],
        Vec::new(),
    );
    match generate(f) {
        Err(CodegenError::Limit(message)) => assert!(message.contains("255"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_frame_with_more_than_65535_registers_is_a_limit_not_a_bug() {
    let n = 70_000u32;
    let values: Vec<Value> = (0..n).map(Value).collect();
    let insts = values.iter().map(|&v| inst(InstKind::Iconst(u64::from(v.0)), vec![v])).collect();
    let f = func(0, vec![VTy::Int; n as usize], insts, values, Vec::new());
    match generate(f) {
        Err(CodegenError::Limit(message)) => assert!(message.contains("65 535"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_big_slot_lives_on_the_heap_and_the_frame_stays_small() {
    // 600 000 bytes: far over what 16-bit register numbers could reach in a
    // frame.
    let at = Value(0);
    let f = func(
        0,
        vec![VTy::Ptr, VTy::Int],
        vec![inst(InstKind::Slot(0), vec![at]), inst(InstKind::Iconst(7), vec![Value(1)])],
        vec![Value(1)],
        vec![Slot { size: 600_000, align: 8 }],
    );
    let image = generate(f).expect("compiles");
    assert!(image.funcs[0].frame < 100, "{}", image.funcs[0].frame);
    assert!(image.natives.iter().any(|n| n == "mem.alloc"), "{:?}", image.natives);
}
