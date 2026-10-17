//! Building Heddle from the typed program (design/vm.md §6.3).
//!
//! Each function instance the program reaches from `main` becomes a
//! [`Func`], and so do the lambdas it makes, the functions it uses as
//! values (through a thunk that takes a function value's environment), and
//! the code it needs for its types: dropping, copying, comparing, showing
//! (*glue*, see `glue.rs`).
//!
//! Values are held as `repr.rs` says: in registers or in memory (see
//! `value.rs`). Loom locals in registers are *variables*; the builder turns
//! them into SSA values as it goes, with Braun et al.'s algorithm ("Simple
//! and Efficient Construction of Static Single Assignment Form", 2013):
//! reading a variable in a block looks back through its predecessors,
//! adding block parameters where paths meet. A block is *sealed* once all
//! its predecessors are known. At the end, parameters every edge passes the
//! same value to are removed, and unreachable blocks dropped.
//!
//! Cleanups (drops and `defer`s) run on every way out of a scope, landing
//! pads included (see `cleanup.rs`).
//!
//! What Treadle can't run yet is reported as U0102, like the Lua backend
//! does.

mod any;
mod bake;
mod builtin;
mod call;
mod cleanup;
mod compare;
mod consts;
mod expr;
mod fallible;
mod fuse;
mod gauge;
mod glue;
mod hash;
mod host;
mod jobs;
mod lambda;
mod map;
mod methods;
mod pat;
mod repr;
mod scan;
mod seq;
mod shuttle;
mod stream;
mod sync;
mod text;
mod thread;
mod value;
mod warp;

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use indexmap::IndexMap;
use loom_isa::{FmtItem, IntSpec, Spec, ValueKind};
use loom_syntax::Diagnostic;
use loom_syntax::ast::ParamMode;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{Block, BlockId, Edge, Func, FuncId, Inst, InstKind, Module, Term, VTy, Value};
use crate::hir::{DefKind, FnRet, HirId, LocalId, Program};
use crate::ty::layout::Layouts;
use crate::ty::{FloatTy, IntTy, Ty, TyKind};
use crate::typeck::{Body, BodyId, Instance, Typed};
use repr::{Part, Repr};
use value::{Storage, Val};

pub use consts::{COMPUTING, CYCLE, ConstKey, DONE, FAILED, GAVE_UP};
pub use jobs::{ConstSlot, Job, JobKind, Jobs, build_jobs};

/// A Loom value in registers: one Heddle value per scalar part.
type Vals = Vec<Value>;

/// A Loom local (or a part of one), before SSA construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Var(u32);

/// A function to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum FnKey {
    /// A function instance of the program.
    Body(BodyId),
    /// What runs `main ! E` (see `main_entry`).
    Main(BodyId),
    /// A lambda, made in that body, by its expression.
    Lambda(BodyId, crate::hir::HirId),
    /// A thread, made in that body, by its expression: runs as a task, with
    /// `(result address, environment)` (see `thread.rs`).
    Thread(BodyId, crate::hir::HirId),
    /// A task that runs a thread's function for a run of the items of a
    /// `loom for` (see `thread.rs`).
    Chunk(BodyId, crate::hir::HirId),
    /// Drops what a thread's environment owns (a thread that never ran).
    EnvDrop(BodyId, crate::hir::HirId),
    /// A function instance as a value: takes an environment it ignores.
    Thunk(BodyId),
    /// The step function of a kind of stream (see `stream.rs`).
    Step(stream::Step),
    /// Computes a constant with a type (see `consts.rs`).
    ConstInit(consts::ConstKey),
    /// Checks a `const assert`, by its body and expression (see `jobs.rs`).
    Assert(BodyId, crate::hir::HirId),
    /// Drops what the constants own, when `main` is done.
    ConstExit,
    /// Shows a value of a type in an `any`, given its address.
    AnyShow(Ty, bool),
    /// A weft method instance as an `any`'s vtable calls it.
    Dyn(BodyId),
    /// Whether a value is in a gauge.
    GaugeCheck(crate::hir::DefId),
    /// Code for the values of a type.
    Glue(Glue, Ty),
    /// The step function of a shuttle function instance (see
    /// `heddle::frame`).
    ShuttleStep(BodyId),
    /// Runs a burst to its next item (see `shuttle.rs`).
    ShuttleDrive,
    /// Lets go of a shuttle's box (see `shuttle.rs`).
    ShuttleRelease,
}

/// What glue does for a type's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Glue {
    /// Frees what a value owns: `drop(value's address)`.
    Drop,
    /// A deep copy: `copy(destination, source)`, both addresses.
    Copy,
    /// Writes `{x}` (or `{x:?}`) to a sink: `show(x, sink)`, `x` as a
    /// read parameter is passed.
    Show { debug: bool },
    /// Whether two values are equal: `eq(a, b) -> bool`, each as a read
    /// parameter is passed.
    Eq,
    /// How two values order: `cmp(a, b)` is -1, 0 or 1.
    Cmp,
    /// A value's hash: `hash(value's address)`.
    Hash,
}

/// How a function takes its parameters and gives its result (its ABI).
#[derive(Clone, Debug)]
struct Abi {
    /// The result's type, when it lives in memory: the caller passes the
    /// address to write it at, first.
    pub sret: Option<Ty>,
    /// A function value's environment comes next.
    pub env: bool,
    pub params: Vec<ParamAbi>,
    /// The result's parts, when it lives in registers.
    pub results: Vec<VTy>,
}

/// How one parameter is passed.
#[derive(Clone, Debug)]
enum ParamAbi {
    /// Its parts, in registers.
    Regs(Rc<[Part]>),
    /// Its address: a value in memory (read, the caller's; taken, the
    /// callee's to drop), or any `mut` parameter (the caller's place).
    Addr,
}

/// A loop being built: where `break` and `continue` go.
#[derive(Debug)]
struct LoopCtx {
    id: HirId,
    brk: BlockId,
    cont: BlockId,
    /// Where `break value` puts its value: variables, or a slot.
    result: LoopResult,
    /// How many scopes were open outside the loop: `break` and `continue`
    /// clean up the ones inside it.
    scopes: usize,
    /// The same for `break`, when it leaves more than `continue` does: a
    /// loop over a generator built in place (see `shuttle.rs`) closes it.
    brk_scopes: usize,
}

/// A `mend` being built: where its `?`s go.
#[derive(Debug)]
struct MendCtx {
    id: HirId,
    /// Its handler.
    fail: BlockId,
    /// The error `mend e =>` binds: its type, and where it's put.
    err: Option<(Ty, ErrStore)>,
    /// How many scopes were open outside it.
    scopes: usize,
}

#[derive(Clone, Debug)]
enum ErrStore {
    Vars(Vec<Var>),
    Slot(Value),
}

#[derive(Clone, Debug)]
enum LoopResult {
    None,
    Vars(Vec<Var>),
    Slot(Value),
}

/// What building one function needs.
#[derive(Debug, Default)]
struct FnState<'a> {
    /// Whose tables expressions are read from.
    body: BodyId,
    blocks: Vec<Block>,
    types: Vec<VTy>,
    /// The block instructions go into; `None` after control has left
    /// (`return`, `break`, an unravel).
    cur: Option<BlockId>,
    preds: Vec<Vec<BlockId>>,
    sealed: Vec<bool>,
    defs: HashMap<(Var, BlockId), Value>,
    /// Parameters added to unsealed blocks, filled in when they're sealed.
    incomplete: HashMap<BlockId, Vec<(Var, Value)>>,
    var_ty: Vec<VTy>,
    locals: HashMap<LocalId, Storage>,
    loops: Vec<LoopCtx>,
    mends: Vec<MendCtx>,
    /// The Loom statement being built, for instructions' locations.
    span: Option<Span>,
    /// Frame slots: memory for values that live in memory.
    slots: Vec<super::Slot>,
    /// Values that are slots' addresses, by value.
    slot_of: HashMap<Value, u32>,
    /// What `scan` found about the body.
    scan: scan::Scan,
    /// The function's result type, and where it goes when it's in memory.
    ret: Option<Ty>,
    sret: Option<Value>,
    /// The cleanup stack (see `cleanup.rs`), where each open scope starts
    /// on it, and the landing pads made so far, by the live cleanups' ids.
    cleanups: Vec<cleanup::Cleanup<'a>>,
    scopes: Vec<usize>,
    pads: HashMap<Vec<u32>, BlockId>,
    next_cleanup: u32,
    /// The depth of the scope of the statement being built: a `let`'s
    /// local goes under its temporaries.
    stmt_start: Option<usize>,
    /// Temporaries in memory that own something, by address: their
    /// cleanups.
    temps: HashMap<Value, u32>,
    /// Addresses a pattern's binding is a view of: moving out of the
    /// binding zeroes what's there, and leaves the temporary there (if it
    /// is one) to be dropped, which a branch that moved it must not stop.
    viewed: std::collections::HashSet<Value>,
    /// While a landing pad is built.
    in_pad: u32,
    /// While a scope's cleanups are emitted at its normal end (falling off
    /// it), rather than for leaving early.
    closing: bool,
    /// The slots drops are registered for, with their types: zeroed when
    /// the function starts.
    owned_slots: Vec<(u32, Ty)>,
    /// Each fallback `defer`'s flag, for the local it cleans up: slots
    /// holding a byte.
    defer_flags: HashMap<LocalId, Vec<u32>>,
    /// Dot-blocks being built: their subjects' places.
    subjects: HashMap<HirId, (value::Place, Ty)>,
    /// Variables a `mut` argument was given in memory of their own for:
    /// written back after the call.
    writebacks: Vec<(Vec<Var>, Ty, Value)>,
    /// A shuttle's step being built.
    shuttle: Option<shuttle::ShuttleBuild>,
    /// Generators being built in place, in the loops over them (see
    /// `shuttle.rs`), innermost last.
    inline_gens: Vec<shuttle::InlineGen<'a>>,
}

pub(super) struct Builder<'a> {
    p: &'a Program,
    typed: &'a Typed,
    diags: Vec<Diagnostic>,
    reported: HashSet<String>,
    funcs: Vec<Option<Func>>,
    ids: IndexMap<FnKey, FuncId>,
    queue: VecDeque<FnKey>,
    strings: IndexMap<Vec<u8>, u32>,
    fmts: IndexMap<FmtItem, u32>,
    layouts: Layouts<'a>,
    reprs: HashMap<Ty, Option<Repr>>,
    drops: HashMap<Ty, bool>,
    /// Where `main` is: glue, which no one line of the program made, is
    /// said to be there.
    main_span: Option<Span>,
    /// Each constant read, by where it is in the data and its type.
    globals: IndexMap<consts::ConstKey, (u32, Ty)>,
    /// The `const` expressions read, by their body and id.
    const_exprs: HashMap<(BodyId, HirId), &'a crate::hir::Expr>,
    /// The `const assert` calls found as jobs, by their body and the id of
    /// the `const` expression.
    asserts: HashMap<(BodyId, HirId), &'a crate::hir::Expr>,
    /// The program's data as it starts: constants (zero until computed, or
    /// baked in) and vtables.
    data: Vec<u64>,
    /// What compile-time code computed, to write into the data (see
    /// `bake.rs`).
    baked: Option<&'a crate::baked::Baked>,
    /// The constants that were, which are never computed or freed.
    baked_keys: HashSet<consts::ConstKey>,
    /// Words of `data` that hold a pointer into it.
    relocs: Vec<u32>,
    /// Word ranges of `data` that are baked values.
    plain: Vec<(u32, u32)>,
    /// Each weft method called through `any`: its slot in every vtable.
    dyn_slots: IndexMap<crate::hir::DefId, u32>,
    /// The number of each type made into an `any`.
    type_ids: IndexMap<Ty, u64>,
    /// Each vtable made, by its type and wefts: where it is in the data.
    vtable_at: HashMap<(Ty, Vec<crate::hir::DefId>), u32>,
    /// The lambdas made so far, by the body that made them.
    lambdas: HashMap<(BodyId, crate::hir::HirId), lambda::LambdaInfo<'a>>,
    /// The threads made so far, by the body that made them.
    threads: HashMap<(BodyId, crate::hir::HirId), thread::ThreadInfo<'a>>,
    /// The environments of the chunk tasks made so far, by their thread.
    chunks: HashMap<(BodyId, crate::hir::HirId), thread::ChunkEnv>,
    /// Whether the program has a `thread` anywhere: only then can a thread
    /// be cancelled, so only then are cancellation points emitted.
    has_threads: bool,
    /// Each shuttle function's frame, once its step is built.
    frames: HashMap<BodyId, super::frame::FrameLayout>,
    f: FnState<'a>,
}

/// Builds the Heddle of the program that starts at `main`. `typed` must be
/// checked for Treadle's target and have no errors.
pub fn build(p: &Program, typed: &Typed) -> Result<Module, Vec<Diagnostic>> {
    build_with(p, typed, &crate::baked::Baked::default())
}

/// [`build`], with the constants compile-time code computed written into
/// the data rather than computed when the program reads them.
pub fn build_with(p: &Program, typed: &Typed, baked: &crate::baked::Baked) -> Result<Module, Vec<Diagnostic>> {
    let mut b = Builder::new(p, typed);
    b.baked = Some(baked);
    let Some(main) = b.find_main() else { return Err(b.diags) };
    b.main_span = Some(p.def(typed.results.instance(main).def).span);
    let fails = typed.results.sigs.get(&main).is_some_and(|s| matches!(typed.tcx.kind(s.ret), TyKind::Result(..)));
    let mut entry = b.func_id(if fails { FnKey::Main(main) } else { FnKey::Body(main) });
    b.drain();
    // Constants that own memory: dropped once `main` is done, by an entry
    // that calls it and then them.
    let consts: Vec<Ty> = b.globals.iter().filter(|(k, _)| !b.is_baked(k)).map(|(_, &(_, t))| t).collect();
    let owning = consts.into_iter().any(|t| b.const_owns(t));
    if owning && !fails {
        entry = b.func_id(FnKey::Main(main));
        b.drain();
    }
    b.finish(entry)
}

/// What `f.Frame.size` and `f.Frame.align` say, for each shuttle function
/// instance of `typed` they're asked about: its frame's size and
/// alignment. The frame is Treadle's (see `frame.rs`), so it's worked out
/// on the program checked for Treadle, which `typed` may not be.
pub fn frames(p: &Program, typed: &Typed) -> Result<HashMap<BodyId, (u64, u64)>, Vec<Diagnostic>> {
    let mut asked: Vec<BodyId> =
        typed.results.bodies.iter().flat_map(|b| b.frames.values().map(|q| q.shuttle)).collect();
    asked.sort();
    asked.dedup();
    if asked.is_empty() {
        return Ok(HashMap::new());
    }
    let vm_typed;
    let vm: &Typed = if typed.tcx.target == crate::target::Target::VM {
        typed
    } else {
        vm_typed = crate::typeck::check_for(p, crate::target::Target::VM);
        if vm_typed.has_errors() {
            return Err(vm_typed.diagnostics);
        }
        &vm_typed
    };
    let mut b = Builder::new(p, vm);
    let mut out = HashMap::new();
    for id in asked {
        let inst = typed.results.instance(id);
        let span = p.def(inst.def).span;
        b.main_span.get_or_insert(span);
        let Some(vm_id) = vm.results.find(inst) else { continue };
        if let Some(layout) = b.frame_layout(vm_id) {
            out.insert(id, (layout.size, layout.align));
        }
    }
    if !b.diags.is_empty() {
        return Err(b.diags);
    }
    Ok(out)
}

/// Builds the Heddle of the tests of the program's root file (`test name()
/// { … }`): a module with an entry for each, which runs it as `main` runs,
/// and the tests' names with their entries, in order.
#[allow(clippy::type_complexity)]
pub fn build_tests(p: &Program, typed: &Typed) -> Result<(Module, Vec<(String, FuncId)>), Vec<Diagnostic>> {
    build_tests_with(p, typed, &crate::baked::Baked::default())
}

/// [`build_tests`], with baked constants (see [`build_with`]).
#[allow(clippy::type_complexity)]
pub fn build_tests_with(
    p: &Program,
    typed: &Typed,
    baked: &crate::baked::Baked,
) -> Result<(Module, Vec<(String, FuncId)>), Vec<Diagnostic>> {
    let mut b = Builder::new(p, typed);
    b.baked = Some(baked);
    let tests = p.root.map(|root| p.module(root).tests.clone()).unwrap_or_default();
    let mut entries = Vec::new();
    for def in tests {
        let Some(id) = typed.results.find(&Instance::of(def)) else { continue };
        b.main_span.get_or_insert(p.def(def).span);
        entries.push((p.def(def).name.clone(), b.func_id(FnKey::Main(id))));
    }
    b.drain();
    let Some(&(_, entry)) = entries.first() else {
        return Ok((
            Module {
                funcs: Vec::new(),
                strings: Vec::new(),
                fmts: Vec::new(),
                data: Vec::new(),
                relocs: Vec::new(),
                plain: Vec::new(),
                entry: FuncId(0),
                roots: Vec::new(),
            },
            entries,
        ));
    };
    let mut module = b.finish(entry)?;
    module.roots = entries.iter().map(|&(_, id)| id).collect();
    Ok((module, entries))
}

impl<'a> Builder<'a> {
    fn new(p: &'a Program, typed: &'a Typed) -> Self {
        Builder {
            p,
            typed,
            diags: Vec::new(),
            reported: HashSet::new(),
            funcs: Vec::new(),
            ids: IndexMap::new(),
            queue: VecDeque::new(),
            strings: IndexMap::new(),
            fmts: IndexMap::new(),
            layouts: Layouts::new(&typed.tcx, p, &typed.results),
            reprs: HashMap::new(),
            drops: HashMap::new(),
            main_span: None,
            lambdas: HashMap::new(),
            threads: HashMap::new(),
            chunks: HashMap::new(),
            has_threads: typed.results.bodies.iter().any(|b| !b.thread_copies.is_empty()),
            globals: IndexMap::new(),
            const_exprs: HashMap::new(),
            asserts: HashMap::new(),
            data: Vec::new(),
            baked: None,
            baked_keys: HashSet::new(),
            relocs: Vec::new(),
            plain: Vec::new(),
            dyn_slots: IndexMap::new(),
            type_ids: IndexMap::new(),
            vtable_at: HashMap::new(),
            frames: HashMap::new(),
            f: FnState::default(),
        }
    }

    /// Builds what's left (the constants' drops, built once every
    /// constant is known) and puts the module together.
    fn finish(mut self, entry: FuncId) -> Result<Module, Vec<Diagnostic>> {
        if let Some(&exit) = self.ids.get(&FnKey::ConstExit)
            && self.funcs[exit.0 as usize].is_none()
        {
            self.build_func(FnKey::ConstExit);
            self.drain();
        }
        if self.fmts.len() > usize::from(u16::MAX) + 1 {
            self.diags.push(Diagnostic::error(
                codes::U0103,
                format!("{} different formats in templates: Treadle's limit is 65 536", self.fmts.len()),
            ));
        }
        if !self.diags.is_empty() {
            return Err(self.diags);
        }
        let funcs = self.funcs.into_iter().map(|f| f.expect("every function queued is built")).collect();
        let module = Module {
            funcs,
            strings: self.strings.into_keys().collect(),
            fmts: self.fmts.into_keys().collect(),
            data: self.data,
            relocs: self.relocs,
            plain: self.plain,
            entry,
            roots: Vec::new(),
        };
        if let Err(e) = super::verify(&module) {
            let diag = Diagnostic::error(codes::U0102, format!("internal error: invalid Heddle: {e}"));
            return Err(vec![diag]);
        }
        Ok(module)
    }
}

impl<'a> Builder<'a> {
    // ------------------------------------------------------------ reporting

    fn unsupported<T>(&mut self, span: Span, what: &str) -> Option<T> {
        if self.reported.insert(what.to_string()) {
            self.diags.push(
                Diagnostic::error(codes::U0102, format!("not implemented yet: {what}"))
                    .primary(span, "Treadle can't run this yet"),
            );
        }
        None
    }

    /// Builds every function queued, and those they queue.
    fn drain(&mut self) {
        while let Some(key) = self.queue.pop_front() {
            // A shuttle's step can be built early, for its frame.
            if self.funcs[self.ids[&key].0 as usize].is_some() {
                continue;
            }
            self.build_func(key);
        }
    }

    /// The function that drops the constants: built last, once every
    /// constant is known, so not queued.
    fn const_exit_id(&mut self) -> FuncId {
        if let Some(&f) = self.ids.get(&FnKey::ConstExit) {
            return f;
        }
        let f = FuncId(self.funcs.len() as u32);
        self.funcs.push(None);
        self.ids.insert(FnKey::ConstExit, f);
        f
    }

    fn find_main(&mut self) -> Option<BodyId> {
        let p = self.p;
        let root = p.root?;
        let main = p.module(root).items.get("main").copied();
        let Some(def) = main.filter(|&d| matches!(p.def(d).kind, DefKind::Fn(_))) else {
            let mut diag = Diagnostic::error(codes::N0116, "there's no `main` function to run")
                .help("add `fn main() { … }`, where the program starts");
            if !p.module(root).tests.is_empty() {
                diag = diag.help("`loom test` runs this file's tests");
            }
            if let Some(file) = p.module(root).file {
                diag = diag.primary(Span::new(file, 0, 0), "in this file");
            }
            self.diags.push(diag);
            return None;
        };
        let DefKind::Fn(f) = &p.def(def).kind else { return None };
        let span = p.def(def).span;
        let id = self.typed.results.find(&Instance::of(def))?;
        let ret = self.typed.results.sigs.get(&id).map(|s| s.ret);
        let fails = ret.is_some_and(|r| matches!(self.tcx().kind(r), TyKind::Result(ok, _) if *ok == self.tcx().unit));
        if !f.params.is_empty() || (!matches!(f.ret, FnRet::Unit) && !fails) {
            return self.unsupported(span, "`main` with parameters, or a return type other than `! E`");
        }
        Some(id)
    }

    /// What runs `main ! E`: calls it, and when it fails, shows the error
    /// and ends the run with it (`exit.fail`), the way the Lua host does.
    fn main_entry(&mut self, main: BodyId) -> Option<Vec<VTy>> {
        let span = self.here();
        let ret = self.typed.results.sigs.get(&main)?.ret;
        let func = self.func_id(FnKey::Body(main));
        let exit = self.const_exit_id();
        let TyKind::Result(_, err) = *self.tcx().kind(ret) else {
            // `main` with no result: then the constants' drops.
            self.emit(InstKind::Call { func, args: Vec::new() }, &[]);
            self.emit(InstKind::Call { func: exit, args: Vec::new() }, &[]);
            self.terminate(Term::Ret(Vec::new()));
            return Some(Vec::new());
        };
        let r = self.temp(ret, span)?;
        self.emit(InstKind::Call { func, args: vec![r] }, &[]);
        let v = Val::Mem(r);
        let tag = self.tag_value(&v, ret, span)?;
        let zero = self.iconst(0);
        let ok = self.emit1(InstKind::ICmp { cmp: super::Cmp::Eq, unsigned: true, a: tag, b: zero }, VTy::Int);
        let (yes, no) = (self.new_block(), self.new_block());
        self.br(ok, yes, no);
        self.enter(no);
        let e = self.payload(&v, ret, err, span)?;
        let msg = self.msg_sink();
        self.write_value(msg, e, err, None, false, span)?;
        self.drop_at(r, ret);
        self.emit(InstKind::Call { func: exit, args: Vec::new() }, &[]);
        self.emit(InstKind::Native { name: "exit.fail", imm: 0, args: Vec::new() }, &[]);
        self.terminate(Term::Crash(1));
        self.enter(yes);
        self.drop_at(r, ret);
        self.emit(InstKind::Call { func: exit, args: Vec::new() }, &[]);
        self.terminate(Term::Ret(Vec::new()));
        Some(Vec::new())
    }

    // ------------------------------------------------------------ functions

    /// The function `key` becomes, queuing it to be built.
    fn func_id(&mut self, key: FnKey) -> FuncId {
        if let Some(&f) = self.ids.get(&key) {
            return f;
        }
        let f = FuncId(self.funcs.len() as u32);
        self.funcs.push(None);
        self.ids.insert(key, f);
        self.queue.push_back(key);
        f
    }

    fn body_name(&self, id: BodyId) -> String {
        let inst = self.typed.results.instance(id);
        let mut name = self.p.def(inst.def).name.clone();
        if let Some(parent) = self.p.def(inst.def).parent.filter(|&w| !self.p.def(w).name.is_empty()) {
            name = format!("{}.{name}", self.p.def(parent).name);
        }
        if inst.self_ty.is_some() || !inst.args.is_empty() || inst.forward {
            name = format!("{name}#{}", id.0);
        }
        name
    }

    fn key_name(&self, key: FnKey) -> String {
        match key {
            FnKey::Body(id) => self.body_name(id),
            FnKey::Main(id) => format!("{}.entry", self.body_name(id)),
            FnKey::Lambda(id, _) => format!("{}.{{lambda}}", self.body_name(id)),
            FnKey::Thread(id, _) => format!("{}.{{thread}}", self.body_name(id)),
            FnKey::Chunk(id, _) => format!("{}.{{thread}}.chunk", self.body_name(id)),
            FnKey::EnvDrop(id, _) => format!("{}.{{thread}}.drop", self.body_name(id)),
            FnKey::Thunk(id) => format!("{}.value", self.body_name(id)),
            FnKey::Step(step) => format!("stream.{}", self.step_name(step)),
            FnKey::ConstInit(consts::ConstKey::Def(def)) => format!("{}.init", self.p.def_path(def)),
            FnKey::ConstInit(consts::ConstKey::Expr(body, e)) => format!("{}.const{}", self.body_name(body), e.0),
            FnKey::Assert(body, e) => format!("{}.assert{}", self.body_name(body), e.0),
            FnKey::ConstExit => "consts.drop".to_string(),
            FnKey::AnyShow(ty, debug) => {
                format!("{}[{}].any", if debug { "debug" } else { "show" }, self.tcx().display(self.p, ty))
            }
            FnKey::Dyn(id) => format!("{}.dyn", self.body_name(id)),
            FnKey::GaugeCheck(def) => format!("{}.check", self.p.def_path(def)),
            FnKey::ShuttleStep(id) => format!("{}.step", self.body_name(id)),
            FnKey::ShuttleDrive => "shuttle.drive".to_string(),
            FnKey::ShuttleRelease => "shuttle.release".to_string(),
            FnKey::Glue(g, ty) => {
                let what = match g {
                    Glue::Drop => "drop",
                    Glue::Copy => "copy",
                    Glue::Show { debug: false } => "show",
                    Glue::Show { debug: true } => "debug",
                    Glue::Eq => "eq",
                    Glue::Cmp => "cmp",
                    Glue::Hash => "hash",
                };
                format!("{what}[{}]", self.tcx().display(self.p, ty))
            }
        }
    }

    /// The glue function `g` for `ty`.
    fn glue(&mut self, g: Glue, ty: Ty) -> FuncId {
        let ty = self.tcx().degauge(ty);
        self.func_id(FnKey::Glue(g, ty))
    }

    fn build_func(&mut self, key: FnKey) {
        let fid = self.ids[&key];
        let name = self.key_name(key);
        let body = match key {
            FnKey::Body(id)
            | FnKey::Main(id)
            | FnKey::Lambda(id, _)
            | FnKey::Thread(id, _)
            | FnKey::Chunk(id, _)
            | FnKey::EnvDrop(id, _)
            | FnKey::Thunk(id) => id,
            FnKey::Glue(..) | FnKey::Step(_) | FnKey::ConstInit(_) | FnKey::ConstExit | FnKey::AnyShow(..) => BodyId(0),
            FnKey::Dyn(id) | FnKey::ShuttleStep(id) | FnKey::Assert(id, _) => id,
            FnKey::GaugeCheck(_) | FnKey::ShuttleDrive | FnKey::ShuttleRelease => BodyId(0),
        };
        self.f = FnState { body, ..FnState::default() };
        let entry = self.new_block();
        self.f.sealed[entry.0 as usize] = true;
        self.f.cur = Some(entry);
        let reported = self.diags.len();
        let span = self.key_span(key);
        self.f.span = Some(span);
        if let FnKey::Glue(..) | FnKey::ShuttleDrive | FnKey::ShuttleRelease = key {
            // Glue has no locations: what goes wrong in it is reported
            // where it was called from.
            self.f.span = None;
        }
        let results = match key {
            FnKey::Body(id) => self.body_func(id),
            FnKey::Main(id) => self.main_entry(id),
            FnKey::Lambda(id, e) => self.lambda_func(id, e),
            FnKey::Thread(id, e) => self.thread_func(id, e),
            FnKey::Chunk(id, e) => self.chunk_func(id, e),
            FnKey::EnvDrop(id, e) => self.env_drop_func(id, e),
            FnKey::Thunk(id) => self.thunk_func(id, span),
            FnKey::Step(step) => self.step_func(step),
            FnKey::ConstInit(def) => self.const_init(def),
            FnKey::Assert(id, e) => self.assert_job(id, e),
            FnKey::ConstExit => self.const_exit(),
            FnKey::AnyShow(ty, debug) => self.any_show_func(ty, debug),
            FnKey::Dyn(id) => self.dyn_func(id, span),
            FnKey::GaugeCheck(def) => self.gauge_check_func(def),
            FnKey::Glue(g, ty) => self.glue_func(g, ty),
            FnKey::ShuttleStep(id) => self.shuttle_step(id),
            FnKey::ShuttleDrive => self.shuttle_drive_func(),
            FnKey::ShuttleRelease => self.shuttle_release_func(),
        };
        // A step's frame starts zeroed, and its slots are zeroed where
        // their values start (see `heddle::frame`).
        let frame_input = self.frame_input();
        if frame_input.is_none() {
            self.zero_owned_slots();
        }
        let state = std::mem::take(&mut self.f);
        let (mut func, complete) = finish(name, state, results.unwrap_or_default());
        if let (FnKey::ShuttleStep(id), Some(input)) = (key, frame_input)
            && complete
            && self.diags.len() == reported
        {
            match super::frame::lay_out(&mut func, &input) {
                Ok(layout) => {
                    self.frames.insert(id, layout);
                }
                Err(e) => {
                    self.diags.push(
                        Diagnostic::error(codes::U0102, format!("internal error: {e}"))
                            .primary(span, "while building this shuttle for Treadle")
                            .note("this is a bug in Loom, not in your program"),
                    );
                }
            }
        }
        // Code that can run but doesn't end is a builder bug, unless
        // something was reported (and nothing runs).
        if !complete && self.diags.len() == reported {
            self.diags.push(
                Diagnostic::error(codes::U0102, format!("internal error: Treadle lost track of `{}`", func.name))
                    .primary(span, "while building this")
                    .note("this is a bug in Loom, not in your program"),
            );
        }
        self.funcs[fid.0 as usize] = Some(func);
    }

    fn key_span(&self, key: FnKey) -> Span {
        match key {
            FnKey::Body(id) | FnKey::Main(id) | FnKey::Thunk(id) | FnKey::Dyn(id) => {
                self.p.def(self.typed.results.instance(id).def).span
            }
            FnKey::Lambda(id, e) => match self.lambdas.get(&(id, e)) {
                Some(info) => info.expr.span,
                None => self.p.def(self.typed.results.instance(id).def).span,
            },
            FnKey::Thread(id, e) | FnKey::Chunk(id, e) | FnKey::EnvDrop(id, e) => match self.threads.get(&(id, e)) {
                Some(info) => info.expr.span,
                None => self.p.def(self.typed.results.instance(id).def).span,
            },
            FnKey::ShuttleStep(id) => self.p.def(self.typed.results.instance(id).def).span,
            FnKey::Glue(..)
            | FnKey::Step(_)
            | FnKey::ConstExit
            | FnKey::AnyShow(..)
            | FnKey::ShuttleDrive
            | FnKey::ShuttleRelease => self.main_span.expect("set before anything is built"),
            FnKey::ConstInit(consts::ConstKey::Def(def)) | FnKey::GaugeCheck(def) => self.p.def(def).span,
            FnKey::ConstInit(consts::ConstKey::Expr(body, e)) => match self.const_exprs.get(&(body, e)) {
                Some(inner) => inner.span,
                None => self.main_span.expect("set before anything is built"),
            },
            FnKey::Assert(body, e) => match self.asserts.get(&(body, e)) {
                Some(call) => call.span,
                None => self.main_span.expect("set before anything is built"),
            },
        }
    }

    /// How a function with these parameters and result is called.
    fn abi(&mut self, params: &[(ParamMode, Ty)], ret: Ty, env: bool, span: Span) -> Option<Abi> {
        let sret = match self.repr_of(ret, span)? {
            Repr::Regs(_) => None,
            Repr::Mem { .. } => Some(ret),
        };
        let results = match self.repr_of(ret, span)? {
            Repr::Regs(parts) => parts.iter().map(|p| p.vty).collect(),
            Repr::Mem { .. } => Vec::new(),
        };
        let mut out = Vec::with_capacity(params.len());
        for &(mode, ty) in params {
            out.push(match (mode, self.repr_of(ty, span)?) {
                (ParamMode::Mut, _) | (_, Repr::Mem { .. }) => ParamAbi::Addr,
                (_, Repr::Regs(parts)) => ParamAbi::Regs(parts),
            });
        }
        Some(Abi { sret, env, params: out, results })
    }

    /// The ABI of function instance `id`.
    fn body_abi(&mut self, id: BodyId, span: Span) -> Option<Abi> {
        let sig = self.typed.results.sigs.get(&id)?.clone();
        let params: Vec<(ParamMode, Ty)> = sig.params.iter().map(|p| (p.mode, self.plain_ty(p.ty))).collect();
        let ret = self.plain_ty(sig.ret);
        self.abi(&params, ret, false, span)
    }

    /// A function instance of the program.
    fn body_func(&mut self, id: BodyId) -> Option<Vec<VTy>> {
        let p = self.p;
        let inst = self.typed.results.instance(id);
        let def = p.def(inst.def);
        match &def.kind {
            DefKind::Fn(f) if f.abi.is_none() && f.body.is_some() && !inst.forward => self.fn_body(id, f, def.span),
            DefKind::Fn(_) if inst.forward => self.forward_body(id, def.span),
            // A weft method std leaves to the compiler, for a type that
            // doesn't write its own: derived.
            DefKind::Fn(f) if f.body.is_none() && inst.self_ty.is_some() => self.derived_method(id, def.span),
            DefKind::Fn(f) if f.body.is_none() && builtin::writes_body(&p.def_path(inst.def)) => {
                self.builtin_body(id, def.span)
            }
            _ => self.unsupported(def.span, &format!("calling `{}`", p.def_path(inst.def))),
        }
    }

    /// A weft method a `through` field gives its warp: calls the field's,
    /// with the rest of the arguments as they came.
    fn forward_body(&mut self, id: BodyId, span: Span) -> Option<Vec<VTy>> {
        self.f.span = None;
        let fwd = self.typed.results.forwards.get(&id)?.clone();
        let sig = self.typed.results.sigs.get(&id)?.clone();
        let abi = self.body_abi(id, span)?;
        let target = self.body_abi(fwd.target, span)?;
        let (sret, _, params) = self.entry_params(&abi);
        let mut ty = self.plain_ty(sig.params.first()?.ty);
        let mut args: Vec<Value> = sret.into_iter().collect();
        match &abi.params[0] {
            ParamAbi::Addr => {
                let mut place = value::Place::At { ptr: params[0][0], off: 0 };
                for key in &fwd.path {
                    let pos = self.field_pos(ty, None, key, span)?;
                    place = self.field_place_of(&place, &pos, span)?;
                    ty = pos.ty;
                }
                match &target.params[0] {
                    ParamAbi::Addr => args.push(self.place_addr(&place, span)?),
                    ParamAbi::Regs(_) => {
                        let v = self.read_place(&place, ty, span)?;
                        args.extend(self.regs(v, ty, span)?);
                    }
                }
            }
            ParamAbi::Regs(_) => {
                let mut v = Val::Regs(params[0].clone());
                for key in &fwd.path {
                    let pos = self.field_pos(ty, None, key, span)?;
                    v = self.field_of(&v, &pos, span)?;
                    ty = pos.ty;
                }
                match &target.params[0] {
                    ParamAbi::Addr => args.push(self.addr_of(v, ty, span)?),
                    ParamAbi::Regs(_) => args.extend(self.regs(v, ty, span)?),
                }
            }
        }
        for vals in params.into_iter().skip(1) {
            args.extend(vals);
        }
        let func = self.func_id(FnKey::Body(fwd.target));
        let results = self.emit(InstKind::Call { func, args }, &target.results);
        self.terminate(Term::Ret(results));
        Some(abi.results)
    }

    /// A derived weft method (`Show.show`), for its instance's `Self`.
    fn derived_method(&mut self, id: BodyId, span: Span) -> Option<Vec<VTy>> {
        let inst = self.typed.results.instance(id);
        let path = self.p.def_path(inst.def);
        let self_ty = self.tcx().degauge(inst.self_ty?);
        let abi = self.body_abi(id, span)?;
        let (sret, _, params) = self.entry_params(&abi);
        let arg = |i: usize| match &abi.params[i] {
            ParamAbi::Regs(_) => Val::Regs(params[i].clone()),
            ParamAbi::Addr => Val::Mem(params[i][0]),
        };
        let this = arg(0);
        match path.as_str() {
            "std.ops.Eq.eq" => {
                let eq = self.eq_vals(this, arg(1), self_ty, span)?;
                self.terminate(Term::Ret(vec![eq]));
            }
            "std.ops.Hash.hash" => {
                let h = self.hash_val(this, self_ty, span)?;
                self.terminate(Term::Ret(vec![h]));
            }
            "std.ops.Ord.lt" => {
                let lt = self.compare_vals(loom_syntax::ast::BinaryOp::Lt, this, arg(1), self_ty, span)?;
                self.terminate(Term::Ret(vec![lt]));
            }
            // What `{x}` writes.
            "std.error.Error.message" => {
                let buf = self.new_sink();
                self.show(buf, this, self_ty, false, span)?;
                let dst = sret?;
                self.emit(InstKind::Native { name: "sink.string", imm: 0, args: vec![buf.0, dst] }, &[]);
                self.terminate(Term::Ret(Vec::new()));
            }
            "std.fmt.Show.show" => {
                let f = params[1][0];
                let h = self.load(super::Mem::W64, f, 0, VTy::Int);
                self.show(text::Sink(h), this, self_ty, false, span)?;
                self.terminate(Term::Ret(Vec::new()));
            }
            _ => return self.unsupported(span, &format!("deriving `{path}`")),
        }
        Some(abi.results)
    }

    /// Makes the entry block's parameters for `abi`: the result's address,
    /// the environment, then each parameter's values.
    fn entry_params(&mut self, abi: &Abi) -> (Option<Value>, Option<Value>, Vec<Vals>) {
        let entry = BlockId(0);
        let new = |this: &mut Self, ty: VTy| {
            let v = this.new_value(ty);
            this.f.blocks[entry.0 as usize].params.push(v);
            v
        };
        let sret = abi.sret.map(|_| new(self, VTy::Ptr));
        let env = abi.env.then(|| new(self, VTy::Ptr));
        let params = abi
            .params
            .iter()
            .map(|p| match p {
                ParamAbi::Regs(parts) => parts.iter().map(|part| new(self, part.vty)).collect(),
                ParamAbi::Addr => vec![new(self, VTy::Ptr)],
            })
            .collect();
        (sret, env, params)
    }

    /// Binds parameter local `l` (of `mode`, passed as `abi`) to its entry
    /// values.
    fn bind_param(&mut self, l: LocalId, mode: ParamMode, abi: &ParamAbi, vals: Vals, span: Span) -> Option<()> {
        let ty = self.local_ty(l);
        match abi {
            ParamAbi::Addr => {
                let ptr = vals[0];
                self.f.locals.insert(l, Storage::At(ptr));
                // A value in memory it takes is its own to drop.
                if mode == ParamMode::Take && self.needs_drop(ty) {
                    self.add_cleanup(cleanup::CleanupKind::Drop { at: cleanup::Stable::Param(ptr), ty });
                }
            }
            ParamAbi::Regs(_) => {
                let place = self.local_place(l, span)?;
                self.write_place(&place, ty, Val::Regs(vals), span)?;
            }
        }
        Some(())
    }

    /// Builds a function's body; gives its result types.
    fn fn_body(&mut self, id: BodyId, f: &'a crate::hir::FnDef, span: Span) -> Option<Vec<VTy>> {
        let sig = self.typed.results.sigs.get(&id)?.clone();
        if matches!(f.ret, FnRet::Shuttle { .. }) {
            return self.shuttle_maker(id, f, span);
        }
        let body = f.body.as_ref().expect("checked by the caller");
        let abi = self.body_abi(id, span)?;
        self.f.scan = self.scan(id, body);
        let (sret, _, params) = self.entry_params(&abi);
        self.f.ret = Some(sig.ret);
        self.f.sret = sret;
        let depth = self.open_scope();
        for (((param, sp), pabi), vals) in f.params.iter().zip(&sig.params).zip(&abi.params).zip(params) {
            self.bind_param(param.local, sp.mode, pabi, vals, param.span)?;
        }
        let val = self.expr(body);
        if self.live() {
            let val = match val {
                Some(v) => v,
                None if self.live() => return None,
                None => Val::Regs(Vec::new()),
            };
            self.ret_val(val, span)?;
        }
        self.close_scope(depth);
        Some(abi.results)
    }

    /// Returns `val` from the function: into its result's memory, or in
    /// registers, after every cleanup of the function runs.
    fn ret_val(&mut self, val: Val, span: Span) -> Option<()> {
        let ty = self.f.ret.unwrap_or(self.tcx().unit);
        if self.inline_return()? {
            return Some(());
        }
        if self.f.shuttle.is_some() {
            // A shuttle's step: finished.
            self.run_cleanups(0);
            if self.live() {
                let k = self.iconst(super::frame::FINISHED);
                self.terminate(Term::Ret(vec![k]));
            }
            return Some(());
        }
        let vals = match self.f.sret {
            Some(sret) => {
                self.move_to(sret, 0, val, ty, span)?;
                Vec::new()
            }
            None => self.regs(val, ty, span)?,
        };
        self.run_cleanups(0);
        if self.live() {
            self.terminate(Term::Ret(vals));
        }
        Some(())
    }

    // ------------------------------------------------------------ types

    fn tables(&self) -> &'a Body {
        self.typed.results.body(self.f.body)
    }

    fn tcx(&self) -> &'a crate::ty::TyCtx {
        &self.typed.tcx
    }

    /// The type of `e` as checked, before its coercion; a gauge is its base.
    fn ty_of(&self, e: &crate::hir::Expr) -> Ty {
        let ty = self.tables().expr_ty.get(&e.id).copied().unwrap_or(self.tcx().error);
        self.tcx().degauge(ty)
    }

    fn int_of(&self, ty: Ty) -> Option<IntTy> {
        self.tcx().as_int(ty)
    }

    fn float_of(&self, ty: Ty) -> Option<FloatTy> {
        match self.tcx().kind(self.tcx().degauge(ty)) {
            TyKind::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// How many parts each item of tuple type `ty` has.
    fn tuple_parts(&mut self, ty: Ty, span: Span) -> Option<Vec<(Ty, usize)>> {
        let TyKind::Tuple(items) = self.tcx().kind(ty) else { return None };
        let items = items.clone();
        items.into_iter().map(|t| Some((t, self.repr_of(t, span)?.parts().len()))).collect()
    }

    // ------------------------------------------------------------ blocks and values

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.f.blocks.len() as u32);
        self.f.blocks.push(Block::default());
        self.f.preds.push(Vec::new());
        self.f.sealed.push(false);
        id
    }

    fn new_value(&mut self, ty: VTy) -> Value {
        let v = Value(self.f.types.len() as u32);
        self.f.types.push(ty);
        v
    }

    fn switch_to(&mut self, b: BlockId) {
        self.f.cur = Some(b);
    }

    /// The block being built, making a dead one if control has left.
    fn cur_block(&mut self) -> BlockId {
        match self.f.cur {
            Some(b) => b,
            None => {
                let b = self.new_block();
                self.f.sealed[b.0 as usize] = true;
                self.f.cur = Some(b);
                b
            }
        }
    }

    /// Whether code being built can run.
    fn live(&self) -> bool {
        self.f.cur.is_some()
    }

    /// Adds an instruction to the current block; its results. One that can
    /// unravel gets the landing pad for the cleanups live here.
    fn emit(&mut self, kind: InstKind, tys: &[VTy]) -> Vals {
        let unwind = if kind.can_unwind() { self.pad() } else { None };
        let b = self.cur_block();
        let results: Vals = tys.iter().map(|&t| self.new_value(t)).collect();
        let span = self.f.span;
        self.f.blocks[b.0 as usize].insts.push(Inst { kind, results: results.clone(), span, unwind, site: None });
        results
    }

    fn emit1(&mut self, kind: InstKind, ty: VTy) -> Value {
        self.emit(kind, &[ty])[0]
    }

    fn iconst(&mut self, bits: u64) -> Value {
        self.emit1(InstKind::Iconst(bits), VTy::Int)
    }

    /// Ends the current block. Control has left it; code built next is dead
    /// until [`switch_to`](Self::switch_to).
    fn terminate(&mut self, term: Term) {
        // A jump may be a loop's back edge, where an interrupt is noticed:
        // it unwinds as an unravel would.
        let unwind =
            if matches!(term, Term::Unravel { .. } | Term::UnravelMsg | Term::Jump(_)) { self.pad() } else { None };
        let b = self.cur_block();
        for e in term.edges() {
            self.f.preds[e.block.0 as usize].push(b);
        }
        let block = &mut self.f.blocks[b.0 as usize];
        block.term = Some(term);
        block.term_span = self.f.span;
        block.term_unwind = unwind;
        self.f.cur = None;
    }

    fn jump(&mut self, to: BlockId) {
        self.terminate(Term::Jump(Edge { block: to, args: Vec::new() }));
    }

    fn br(&mut self, cond: Value, then: BlockId, else_: BlockId) {
        let then = Edge { block: then, args: Vec::new() };
        let else_ = Edge { block: else_, args: Vec::new() };
        self.terminate(Term::Br { cond, then, else_ });
    }

    /// Marks `b`'s predecessors as all known, filling in the parameters
    /// reads added before.
    fn seal(&mut self, b: BlockId) {
        let i = b.0 as usize;
        if self.f.sealed[i] {
            return;
        }
        self.f.sealed[i] = true;
        for (var, param) in self.f.incomplete.remove(&b).unwrap_or_default() {
            self.add_args(var, b, param);
        }
    }

    /// Switches to `b` once all its predecessors are known; control has
    /// left if there are none.
    fn enter(&mut self, b: BlockId) {
        self.seal(b);
        if self.f.preds[b.0 as usize].is_empty() {
            self.f.cur = None;
        } else {
            self.switch_to(b);
        }
    }

    // ------------------------------------------------------------ variables

    fn new_var(&mut self, ty: VTy) -> Var {
        let v = Var(self.f.var_ty.len() as u32);
        self.f.var_ty.push(ty);
        v
    }

    fn new_vars(&mut self, tys: &[VTy]) -> Vec<Var> {
        tys.iter().map(|&t| self.new_var(t)).collect()
    }

    fn write(&mut self, var: Var, v: Value) {
        let b = self.cur_block();
        self.f.defs.insert((var, b), v);
        self.note_width(var, v);
    }

    fn write_all(&mut self, vars: &[Var], vals: &[Value]) {
        for (&var, &v) in vars.iter().zip(vals) {
            self.write(var, v);
        }
    }

    fn read(&mut self, var: Var) -> Value {
        let b = self.cur_block();
        self.read_in(var, b)
    }

    fn read_all(&mut self, vars: &[Var]) -> Vals {
        vars.iter().map(|&v| self.read(v)).collect()
    }

    fn read_in(&mut self, var: Var, b: BlockId) -> Value {
        if let Some(&v) = self.f.defs.get(&(var, b)) {
            return v;
        }
        let ty = self.f.var_ty[var.0 as usize];
        let i = b.0 as usize;
        let v = if !self.f.sealed[i] {
            let param = self.new_value(ty);
            self.f.blocks[i].params.push(param);
            self.f.incomplete.entry(b).or_default().push((var, param));
            self.note_width(var, param);
            param
        } else {
            let preds = self.f.preds[i].clone();
            match preds.as_slice() {
                [] => {
                    // Nothing wrote it on any path here: the entry block,
                    // or dead code. Typing rules out reading it.
                    let v = self.new_value(ty);
                    let span = self.f.span;
                    self.f.blocks[i]
                        .insts
                        .insert(0, Inst { kind: InstKind::Undef, results: vec![v], span, unwind: None, site: None });
                    v
                }
                [one] => self.read_in(var, *one),
                _ => {
                    let param = self.new_value(ty);
                    self.f.blocks[i].params.push(param);
                    self.f.defs.insert((var, b), param);
                    self.note_width(var, param);
                    self.add_args(var, b, param);
                    param
                }
            }
        };
        self.f.defs.insert((var, b), v);
        v
    }

    /// Passes `var`'s value along every edge into `b`, for its new
    /// parameter `param`.
    fn add_args(&mut self, var: Var, b: BlockId, param: Value) {
        let _ = param;
        let mut preds = self.f.preds[b.0 as usize].clone();
        preds.dedup();
        let mut done = HashSet::new();
        for pred in preds {
            if !done.insert(pred) {
                continue;
            }
            let v = self.read_in(var, pred);
            let term = self.f.blocks[pred.0 as usize].term.as_mut().expect("a predecessor has jumped");
            for e in term.edges_mut() {
                if e.block == b {
                    e.args.push(v);
                }
            }
        }
    }

    // ------------------------------------------------------------ module data

    fn string(&mut self, s: &[u8]) -> u32 {
        let next = self.strings.len() as u32;
        *self.strings.entry(s.to_vec()).or_insert(next)
    }

    fn fmt_item(&mut self, item: FmtItem) -> u16 {
        let next = self.fmts.len() as u32;
        let n = *self.fmts.entry(item).or_insert(next);
        // An instruction's field numbers them in 16 bits: `finish` reports
        // a program with more.
        n as u16
    }

    /// A text literal's parts: its address and its length.
    fn str_lit(&mut self, s: &[u8]) -> Vals {
        let n = self.string(s);
        let ptr = self.emit1(InstKind::Str(n), VTy::Ptr);
        let len = self.iconst(s.len() as u64);
        vec![ptr, len]
    }
}

/// An integer type as the instruction set describes it.
pub(super) fn int_spec(t: IntTy) -> IntSpec {
    IntSpec { bits: t.bits, signed: t.signed, size: t.size }
}

/// What a formatting native formats for a scalar of type `ty`.
fn value_kind(tcx: &crate::ty::TyCtx, ty: Ty) -> Option<ValueKind> {
    Some(match tcx.kind(tcx.degauge(ty)) {
        TyKind::Int(i) => ValueKind::Int(int_spec(*i)),
        TyKind::Float(FloatTy::F32) => ValueKind::F32,
        TyKind::Float(FloatTy::F64) => ValueKind::F64,
        TyKind::Bool => ValueKind::Bool,
        TyKind::Char => ValueKind::Char,
        TyKind::Str => ValueKind::Str,
        TyKind::Unit => ValueKind::Unit,
        _ => return None,
    })
}

fn parse_spec(s: &str) -> Option<Spec> {
    Spec::parse(s)
}

/// A function once its body is built: dead blocks dropped, parameters
/// every edge passes the same value to replaced by that value. `false`
/// when a block that can run has no terminator.
fn finish(name: String, state: FnState<'_>, results: Vec<VTy>) -> (Func, bool) {
    let FnState { blocks, types, slots, .. } = state;
    let mut blocks = reachable(blocks);
    let complete = blocks.iter().all(|b| b.term.is_some());
    for b in &mut blocks {
        b.term.get_or_insert(Term::Crash(1));
    }
    let mut alias: HashMap<Value, Value> = HashMap::new();
    let resolve = |alias: &HashMap<Value, Value>, mut v: Value| {
        while let Some(&next) = alias.get(&v) {
            v = next;
        }
        v
    };
    // Parameters every incoming edge gives the same value (or itself).
    loop {
        let mut changed = false;
        for b in 1..blocks.len() {
            let mut i = 0;
            while i < blocks[b].params.len() {
                let param = blocks[b].params[i];
                let mut same: Option<Value> = None;
                let mut trivial = true;
                for pred in &blocks {
                    for e in pred.term.iter().flat_map(|t| t.edges()) {
                        if e.block.0 as usize != b {
                            continue;
                        }
                        let arg = resolve(&alias, e.args[i]);
                        if arg == param || same == Some(arg) {
                            continue;
                        }
                        if same.is_none() {
                            same = Some(arg);
                        } else {
                            trivial = false;
                        }
                    }
                }
                match same {
                    Some(v) if trivial => {
                        alias.insert(param, v);
                        blocks[b].params.remove(i);
                        for pred in &mut blocks {
                            for e in pred.term.iter_mut().flat_map(|t| t.edges_mut()) {
                                if e.block.0 as usize == b {
                                    e.args.remove(i);
                                }
                            }
                        }
                        changed = true;
                    }
                    _ => i += 1,
                }
            }
        }
        if !changed {
            break;
        }
    }
    if !alias.is_empty() {
        for b in &mut blocks {
            for inst in &mut b.insts {
                rewrite(&mut inst.kind, |v| resolve(&alias, v));
            }
            if let Some(term) = &mut b.term {
                match term {
                    Term::Br { cond, .. } => *cond = resolve(&alias, *cond),
                    Term::Ret(vs) => vs.iter_mut().for_each(|v| *v = resolve(&alias, *v)),
                    Term::Unravel { ptr, len } => {
                        *ptr = resolve(&alias, *ptr);
                        *len = resolve(&alias, *len);
                    }
                    _ => {}
                }
                for e in term.edges_mut() {
                    e.args.iter_mut().for_each(|v| *v = resolve(&alias, *v));
                }
            }
        }
    }
    thread_jumps(&mut blocks);
    let blocks = reachable(blocks);
    (Func { name, blocks, results, types, slots, sites: Vec::new() }, complete)
}

/// The blocks the entry reaches, by jumping or by unwinding to a landing
/// pad, renumbered in order.
fn reachable(mut blocks: Vec<Block>) -> Vec<Block> {
    let mut keep = vec![false; blocks.len()];
    let mut stack = vec![0usize];
    keep[0] = true;
    while let Some(b) = stack.pop() {
        let edges = blocks[b].term.iter().flat_map(|t| t.edges()).map(|e| e.block);
        for t in edges.chain(blocks[b].pads()) {
            let t = t.0 as usize;
            if !keep[t] {
                keep[t] = true;
                stack.push(t);
            }
        }
    }
    let mut renumber = vec![u32::MAX; blocks.len()];
    let mut next = 0;
    for (i, &k) in keep.iter().enumerate() {
        if k {
            renumber[i] = next;
            next += 1;
        }
    }
    let mut kept: Vec<Block> = blocks.drain(..).zip(&keep).filter(|(_, k)| **k).map(|(b, _)| b).collect();
    let renumbered = |b: BlockId| BlockId(renumber[b.0 as usize]);
    for b in &mut kept {
        for e in b.term.iter_mut().flat_map(|t| t.edges_mut()) {
            e.block = renumbered(e.block);
        }
        for inst in &mut b.insts {
            inst.unwind = inst.unwind.map(renumbered);
        }
        b.term_unwind = b.term_unwind.map(renumbered);
    }
    kept
}

/// Sends edges into a block that only jumps on (no parameters, no
/// instructions) straight to where it jumps, with its arguments. Values the
/// jump passes are defined where they reach the block, and so where they
/// reach its predecessors.
fn thread_jumps(blocks: &mut [Block]) {
    let forward = |blocks: &[Block], b: usize| -> Option<(Edge, Option<BlockId>)> {
        let block = &blocks[b];
        match &block.term {
            Some(Term::Jump(e)) if b != 0 && block.params.is_empty() && block.insts.is_empty() => {
                Some((e.clone(), block.term_unwind))
            }
            _ => None,
        }
    };
    for i in 0..blocks.len() {
        let Some(mut term) = blocks[i].term.take() else { continue };
        let mut unwind = blocks[i].term_unwind;
        for e in term.edges_mut() {
            // Follow a chain of them, stopping at a cycle.
            let mut seen = HashSet::new();
            while let Some((next, pad)) = forward(blocks, e.block.0 as usize) {
                if !seen.insert(e.block) || next.block == e.block || !e.args.is_empty() {
                    break;
                }
                // A jump skipped may be a loop's back edge, where an
                // interrupt is noticed: its landing pad comes along, unless
                // this block's terminator has another.
                if pad.is_some() {
                    if unwind.is_some() && unwind != pad {
                        break;
                    }
                    unwind = pad;
                }
                *e = next;
            }
        }
        blocks[i].term = Some(term);
        blocks[i].term_unwind = unwind;
    }
}

/// Replaces every value `k` reads with `f` of it.
pub(crate) fn rewrite(k: &mut InstKind, mut f: impl FnMut(Value) -> Value) {
    use InstKind as K;
    match k {
        K::Undef | K::Iconst(_) | K::Fconst(_) | K::Str(_) => {}
        K::Arith { a, b, .. }
        | K::Div { a, b, .. }
        | K::Rem { a, b, .. }
        | K::Shl { a, b, .. }
        | K::Shr { a, b, .. }
        | K::Bit { a, b, .. }
        | K::ICmp { a, b, .. }
        | K::FArith { a, b, .. }
        | K::FCmp { a, b, .. } => {
            *a = f(*a);
            *b = f(*b);
        }
        K::Neg { a, .. }
        | K::BitNot { a, .. }
        | K::Not(a)
        | K::FNeg { a, .. }
        | K::IntCast { a, .. }
        | K::IntToFloat { a, .. }
        | K::FloatToInt { a, .. }
        | K::FloatCast { a, .. }
        | K::IntToChar { a, .. } => *a = f(*a),
        K::Call { args, .. } | K::Native { args, .. } => args.iter_mut().for_each(|v| *v = f(*v)),
        K::Slot(_) | K::Global(_) | K::FuncRef(_) | K::Life { .. } => {}
        K::Load { ptr, .. } | K::Offset { ptr, .. } => *ptr = f(*ptr),
        K::Store { ptr, val, .. } => {
            *ptr = f(*ptr);
            *val = f(*val);
        }
        K::Copy { dst, src, .. } => {
            *dst = f(*dst);
            *src = f(*src);
        }
        K::Zero { dst, .. } => *dst = f(*dst),
        K::FreeBuf { header, .. } => *header = f(*header),
        K::Elem { header, index, .. } => {
            *header = f(*header);
            *index = f(*index);
        }
        K::Index { base, index, .. } => {
            *base = f(*base);
            *index = f(*index);
        }
        K::Bounds { index, len } => {
            *index = f(*index);
            *len = f(*len);
        }
        K::CallInd { callee, args } => {
            *callee = f(*callee);
            args.iter_mut().for_each(|v| *v = f(*v));
        }
    }
}
