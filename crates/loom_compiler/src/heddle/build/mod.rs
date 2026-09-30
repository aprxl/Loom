//! Building Heddle from the typed program (design/vm.md §6.3).
//!
//! Each function instance the program reaches from `main` becomes a
//! [`Func`]. Loom locals are *variables*; the builder turns them into SSA
//! values as it goes, with Braun et al.'s algorithm ("Simple and Efficient
//! Construction of Static Single Assignment Form", 2013): reading a
//! variable in a block looks back through its predecessors, adding block
//! parameters where paths meet. A block is *sealed* once all its
//! predecessors are known. At the end, parameters every edge passes the
//! same value to are removed, and unreachable blocks dropped.
//!
//! What Treadle can't run yet is reported as U0102, like the Lua backend
//! does.

mod expr;
mod pat;
mod text;

use std::collections::{HashMap, HashSet, VecDeque};

use indexmap::IndexMap;
use loom_isa::{FmtItem, IntSpec, Spec, ValueKind};
use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{Block, BlockId, Edge, Func, FuncId, Inst, InstKind, Module, Term, VTy, Value};
use crate::hir::{DefKind, FnRet, HirId, LocalId, Program};
use crate::ty::{FloatTy, IntTy, Ty, TyKind};
use crate::typeck::{Body, BodyId, Instance, Typed};

/// A Loom value: one Heddle value per scalar part.
type Vals = Vec<Value>;

/// A Loom local (or a part of one), before SSA construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Var(u32);

/// A loop being built: where `break` and `continue` go.
#[derive(Debug)]
struct LoopCtx {
    id: HirId,
    brk: BlockId,
    cont: BlockId,
    /// Where `break value` puts its value.
    result: Vec<Var>,
}

/// What building one function needs.
#[derive(Debug, Default)]
struct FnState {
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
    locals: HashMap<LocalId, Vec<Var>>,
    loops: Vec<LoopCtx>,
    /// The Loom statement being built, for instructions' locations.
    span: Option<Span>,
    /// The function's `mut` scalar parameters, returned after its results.
    outs: Vec<Var>,
}

pub(super) struct Builder<'a> {
    p: &'a Program,
    typed: &'a Typed,
    diags: Vec<Diagnostic>,
    reported: HashSet<String>,
    funcs: Vec<Option<Func>>,
    ids: IndexMap<BodyId, FuncId>,
    queue: VecDeque<BodyId>,
    strings: IndexMap<Vec<u8>, u32>,
    fmts: IndexMap<FmtItem, u32>,
    f: FnState,
}

/// Builds the Heddle of the program that starts at `main`. `typed` must be
/// checked for Treadle's target and have no errors.
pub fn build(p: &Program, typed: &Typed) -> Result<Module, Vec<Diagnostic>> {
    let mut b = Builder {
        p,
        typed,
        diags: Vec::new(),
        reported: HashSet::new(),
        funcs: Vec::new(),
        ids: IndexMap::new(),
        queue: VecDeque::new(),
        strings: IndexMap::new(),
        fmts: IndexMap::new(),
        f: FnState::default(),
    };
    let Some(main) = b.find_main() else { return Err(b.diags) };
    let entry = b.func_id(main);
    while let Some(id) = b.queue.pop_front() {
        b.build_func(id);
    }
    if !b.diags.is_empty() {
        return Err(b.diags);
    }
    let funcs = b.funcs.into_iter().map(|f| f.expect("every function queued is built")).collect();
    let module = Module { funcs, strings: b.strings.into_keys().collect(), fmts: b.fmts.into_keys().collect(), entry };
    if let Err(e) = super::verify(&module) {
        let diag = Diagnostic::error(codes::U0102, format!("internal error: invalid Heddle: {e}"));
        return Err(vec![diag]);
    }
    Ok(module)
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
        if !f.params.is_empty() || !matches!(f.ret, FnRet::Unit) {
            return self.unsupported(span, "`main` with parameters or a result");
        }
        self.typed.results.find(&Instance::of(def))
    }

    // ------------------------------------------------------------ functions

    /// The function instance `id` becomes, queuing it to be built.
    fn func_id(&mut self, id: BodyId) -> FuncId {
        if let Some(&f) = self.ids.get(&id) {
            return f;
        }
        let f = FuncId(self.funcs.len() as u32);
        self.funcs.push(None);
        self.ids.insert(id, f);
        self.queue.push_back(id);
        f
    }

    fn func_name(&self, id: BodyId) -> String {
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

    fn build_func(&mut self, id: BodyId) {
        let p = self.p;
        let inst = self.typed.results.instance(id);
        let def = p.def(inst.def);
        let fid = self.ids[&id];
        let name = self.func_name(id);
        self.f = FnState { body: id, ..FnState::default() };
        let entry = self.new_block();
        self.f.sealed[entry.0 as usize] = true;
        self.f.cur = Some(entry);
        let reported = self.diags.len();
        let func = match &def.kind {
            DefKind::Fn(f) if f.abi.is_none() && f.body.is_some() && !inst.forward => self.fn_body(id, f, def.span),
            _ => self.unsupported(def.span, &format!("calling `{}`", p.def_path(inst.def))),
        };
        let results = func.unwrap_or_default();
        let state = std::mem::take(&mut self.f);
        let (func, complete) = finish(name, state, results);
        // Code that can run but doesn't end is a builder bug, unless
        // something was reported (and nothing runs).
        if !complete && self.diags.len() == reported {
            self.diags.push(
                Diagnostic::error(codes::U0102, format!("internal error: Treadle lost track of `{}`", func.name))
                    .primary(def.span, "while building this")
                    .note("this is a bug in Loom, not in your program"),
            );
        }
        self.funcs[fid.0 as usize] = Some(func);
    }

    /// Builds a function's body; gives its result types.
    fn fn_body(&mut self, id: BodyId, f: &'a crate::hir::FnDef, span: Span) -> Option<Vec<VTy>> {
        let sig = self.typed.results.sigs.get(&id)?.clone();
        if matches!(f.ret, FnRet::Shuttle { .. }) {
            return self.unsupported(span, "shuttles");
        }
        let entry = BlockId(0);
        for (param, sp) in f.params.iter().zip(&sig.params) {
            let tys = self.flat(sp.ty, param.span)?;
            let vars = self.local_vars(param.local, param.span)?;
            for (&var, &ty) in vars.iter().zip(&tys) {
                let v = self.new_value(ty);
                self.f.blocks[entry.0 as usize].params.push(v);
                self.write(var, v);
            }
            if sp.mode == loom_syntax::ast::ParamMode::Mut {
                self.f.outs.extend(vars);
            }
        }
        let ret_tys = if matches!(f.ret, FnRet::Unit) { Vec::new() } else { self.flat(sig.ret, span)? };
        let mut results = ret_tys.clone();
        results.extend(self.f.outs.iter().map(|v| self.f.var_ty[v.0 as usize]));
        self.f.span = Some(span);
        let body = f.body.as_ref().expect("checked by the caller");
        let vals = self.expr(body);
        if self.f.cur.is_some() {
            let mut vals = if ret_tys.is_empty() { Vec::new() } else { vals? };
            vals.extend(self.read_outs());
            self.terminate(Term::Ret(vals));
        }
        Some(results)
    }

    fn read_outs(&mut self) -> Vals {
        let outs = self.f.outs.clone();
        outs.into_iter().map(|v| self.read(v)).collect()
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

    /// The machine types of the parts of a value of type `ty`, or `None`
    /// (reported) for types Treadle can't hold yet.
    fn flat(&mut self, ty: Ty, span: Span) -> Option<Vec<VTy>> {
        let mut out = Vec::new();
        self.flat_into(ty, span, &mut out)?;
        Some(out)
    }

    fn flat_into(&mut self, ty: Ty, span: Span, out: &mut Vec<VTy>) -> Option<()> {
        let tcx = self.tcx();
        match tcx.kind(tcx.degauge(ty)) {
            TyKind::Int(_) | TyKind::Bool | TyKind::Char => out.push(VTy::Int),
            TyKind::Float(FloatTy::F32) => out.push(VTy::F32),
            TyKind::Float(FloatTy::F64) => out.push(VTy::F64),
            TyKind::Str => out.extend([VTy::Ptr, VTy::Int]),
            TyKind::Unit | TyKind::Never => {}
            TyKind::Tuple(items) => {
                for &item in items {
                    self.flat_into(item, span, out)?;
                }
            }
            _ => {
                let shown = tcx.display(self.p, ty);
                return self.unsupported(span, &format!("values of type `{shown}`"));
            }
        }
        Some(())
    }

    /// How many parts each item of tuple type `ty` has.
    fn tuple_parts(&mut self, ty: Ty, span: Span) -> Option<Vec<(Ty, usize)>> {
        let TyKind::Tuple(items) = self.tcx().kind(ty) else { return None };
        let items = items.clone();
        items.into_iter().map(|t| Some((t, self.flat(t, span)?.len()))).collect()
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

    fn emit(&mut self, kind: InstKind, tys: &[VTy]) -> Vals {
        let b = self.cur_block();
        let results: Vals = tys.iter().map(|&t| self.new_value(t)).collect();
        let span = self.f.span;
        self.f.blocks[b.0 as usize].insts.push(Inst { kind, results: results.clone(), span });
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
        let b = self.cur_block();
        for e in term.edges() {
            self.f.preds[e.block.0 as usize].push(b);
        }
        let block = &mut self.f.blocks[b.0 as usize];
        block.term = Some(term);
        block.term_span = self.f.span;
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

    /// The variables of local `l`, made on first use.
    fn local_vars(&mut self, l: LocalId, span: Span) -> Option<Vec<Var>> {
        if let Some(vars) = self.f.locals.get(&l) {
            return Some(vars.clone());
        }
        let ty = self.tables().local_ty.get(&l).copied().unwrap_or(self.tcx().error);
        let tys = self.flat(ty, span)?;
        let vars = self.new_vars(&tys);
        self.f.locals.insert(l, vars.clone());
        Some(vars)
    }

    fn write(&mut self, var: Var, v: Value) {
        let b = self.cur_block();
        self.f.defs.insert((var, b), v);
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
            param
        } else {
            let preds = self.f.preds[i].clone();
            match preds.as_slice() {
                [] => {
                    // Nothing wrote it on any path here: the entry block,
                    // or dead code. Typing rules out reading it.
                    let v = self.new_value(ty);
                    let span = self.f.span;
                    self.f.blocks[i].insts.insert(0, Inst { kind: InstKind::Undef, results: vec![v], span });
                    v
                }
                [one] => self.read_in(var, *one),
                _ => {
                    let param = self.new_value(ty);
                    self.f.blocks[i].params.push(param);
                    self.f.defs.insert((var, b), param);
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
        *self.fmts.entry(item).or_insert(next) as u16
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
fn finish(name: String, state: FnState, results: Vec<VTy>) -> (Func, bool) {
    let FnState { blocks, types, .. } = state;
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
    (Func { name, blocks, results, types }, complete)
}

/// The blocks the entry reaches, renumbered in order.
fn reachable(mut blocks: Vec<Block>) -> Vec<Block> {
    let mut keep = vec![false; blocks.len()];
    let mut stack = vec![0usize];
    keep[0] = true;
    while let Some(b) = stack.pop() {
        for e in blocks[b].term.iter().flat_map(|t| t.edges()) {
            let t = e.block.0 as usize;
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
    for b in &mut kept {
        for e in b.term.iter_mut().flat_map(|t| t.edges_mut()) {
            e.block = BlockId(renumber[e.block.0 as usize]);
        }
    }
    kept
}

/// Sends edges into a block that only jumps on (no parameters, no
/// instructions) straight to where it jumps, with its arguments. Values the
/// jump passes are defined where they reach the block, and so where they
/// reach its predecessors.
fn thread_jumps(blocks: &mut [Block]) {
    let forward = |blocks: &[Block], b: usize| -> Option<Edge> {
        let block = &blocks[b];
        match &block.term {
            Some(Term::Jump(e)) if b != 0 && block.params.is_empty() && block.insts.is_empty() => Some(e.clone()),
            _ => None,
        }
    };
    for i in 0..blocks.len() {
        let Some(mut term) = blocks[i].term.take() else { continue };
        for e in term.edges_mut() {
            // Follow a chain of them, stopping at a cycle.
            let mut seen = HashSet::new();
            while let Some(next) = forward(blocks, e.block.0 as usize) {
                if !seen.insert(e.block) || next.block == e.block || !e.args.is_empty() {
                    break;
                }
                *e = next;
            }
        }
        blocks[i].term = Some(term);
    }
}

/// Replaces every value `k` reads with `f` of it.
fn rewrite(k: &mut InstKind, f: impl Fn(Value) -> Value) {
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
    }
}
