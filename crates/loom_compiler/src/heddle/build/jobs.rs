//! Compile-time jobs (design/vm.md §17, V4): the Heddle of what has to run
//! while compiling, which `consteval` runs on Treadle.
//!
//! A job is one of:
//!
//! - computing a constant with a type (`const LIMIT: u32 = …`): its init
//!   function (see `consts.rs`), called with the address of its value;
//! - computing a `const` expression in a body (`const fib(20)`): the same;
//! - checking a `const assert`: a function of its own that gives 0, or 1
//!   with the message in the message sink when the condition is false.
//!
//! The jobs are found without building the program: typeck says which
//! bodies have `const` expressions (`Results::const_bodies`), and each is
//! walked for them. Only what the jobs reach is built, so a program with
//! little compile-time code costs little, and the rest of the program never
//! meets the builder (a construct Treadle can't do yet is reported only if
//! compile-time code needs it).

use std::collections::HashMap;

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::consts::ConstKey;
use super::scan::{Node, children};
use super::{Builder, FnKey};
use crate::heddle::{FuncId, Module};
use crate::hir::{self, DefId, DefKind, ExprKind, HirId, Program, StmtKind};
use crate::typeck::{BodyId, Typed, assert_args};

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Computing constant `def`. Its function takes the address of its
    /// value, 8 bytes past `slot`, the offset of its flag word in the data.
    Const { def: DefId, slot: u32 },
    /// Computing a `const` expression: the same.
    Value { slot: u32 },
    /// A `const assert`, whose condition is at `cond`. Its function takes
    /// nothing, and gives 0, or 1 when it's false, with its message (empty
    /// when it has none) in the message sink.
    Assert { cond: Span },
}

/// Something to run while compiling.
#[derive(Clone, Debug)]
pub struct Job {
    pub kind: JobKind,
    /// Where it is: the constant's name, the `const` expression.
    pub span: Span,
    /// The instance it's in, which may be a generic one.
    pub body: BodyId,
    pub func: FuncId,
}

/// A constant's place in the data, for saying which one a cycle is in.
#[derive(Clone, Debug)]
pub struct ConstSlot {
    /// The offset of its flag word.
    pub offset: u32,
    /// `` `LIMIT` `` or "this `const` expression".
    pub name: String,
    pub span: Span,
}

/// The module that runs a program's compile-time code, with its jobs in the
/// order they're reported in.
#[derive(Debug)]
pub struct Jobs {
    /// Every job's function is a root. There's no `main`.
    pub module: Module,
    pub jobs: Vec<Job>,
    pub slots: Vec<ConstSlot>,
}

/// Builds the jobs of the program `typed` was checked from; `tests`: the
/// tests' too. `None` when there's no compile-time code. `typed` may be
/// checked for either target: the jobs run with its `usize`.
pub fn build_jobs(p: &Program, typed: &Typed, tests: bool) -> Result<Option<Jobs>, Vec<Diagnostic>> {
    let results = &typed.results;
    let is_test =
        |def: DefId| matches!(&p.def(def).kind, DefKind::Fn(f) if f.qualifier == loom_syntax::ast::FnQualifier::Test);
    let mut b = Builder::new(p, typed);
    let mut jobs: Vec<Job> = Vec::new();
    let mut spans: HashMap<ConstKey, Span> = HashMap::new();

    // Constants with a type: each one, read or not.
    for &id in &results.reachable {
        let def = results.instance(id).def;
        if !tests && is_test(def) {
            continue;
        }
        if !matches!(p.def(def).kind, DefKind::Const(_))
            || results.untyped_consts.contains_key(&def)
            || results.untyped_floats.contains_key(&def)
        {
            continue;
        }
        let span = p.def(def).span;
        b.main_span.get_or_insert(span);
        let key = ConstKey::Def(def);
        let Some((slot, _)) = b.const_global(key, span) else { continue };
        spans.insert(key, span);
        let func = b.func_id(FnKey::ConstInit(key));
        jobs.push(Job { kind: JobKind::Const { def, slot }, span, body: id, func });
    }

    // `const` expressions and `const assert`s: those typeck checked, each by
    // the body it was checked in. Typeck gives ids, and finding what they
    // are means looking at the code that holds them: first the bodies they
    // were checked in, and if one's not there (a field's or a parameter's
    // default is checked in the body that used it), at everything.
    let mut wanted: Vec<(BodyId, HirId)> = Vec::new();
    for &(body, id) in &results.const_exprs {
        if !wanted.contains(&(body, id)) {
            wanted.push((body, id));
        }
    }
    let mut index: HashMap<HirId, &hir::Expr> = HashMap::new();
    let mut walked: Vec<DefId> = Vec::new();
    for &(body, _) in &wanted {
        let def = results.instance(body).def;
        if !walked.contains(&def) {
            walked.push(def);
            index_consts(&p.def(def).kind, &mut index);
        }
    }
    if wanted.iter().any(|(_, id)| !index.contains_key(id)) {
        for def in &p.defs {
            index_consts(&def.kind, &mut index);
        }
    }
    let mut found: Vec<(BodyId, &hir::Expr)> = Vec::new();
    for (body, id) in wanted {
        let def = results.instance(body).def;
        if !tests && is_test(def) {
            continue;
        }
        match index.get(&id) {
            Some(&e) => found.push((body, e)),
            None => b.diags.push(
                Diagnostic::error(codes::U0102, "internal error: compile-time code was checked but can't be found")
                    .primary(p.def(def).span, "while looking for what runs while compiling in this")
                    .note("this is a bug in Loom, not in your program"),
            ),
        }
    }
    // `const` items at the top of a module or in a warp.
    for &id in &results.const_blocks {
        let def = results.instance(id).def;
        if let DefKind::ConstBlock(e) = &p.def(def).kind {
            found.push((id, e));
        }
    }
    let mut done: std::collections::HashSet<(BodyId, HirId)> = std::collections::HashSet::new();
    for (id, e) in found {
        if !done.insert((id, e.id)) {
            continue;
        }
        let ExprKind::Const(inner) = &e.kind else { continue };
        b.main_span.get_or_insert(e.span);
        if let Some(args) = assert_args(p, inner) {
            let Some(cond) = args.first() else { continue };
            b.asserts.insert((id, e.id), inner);
            let func = b.func_id(FnKey::Assert(id, e.id));
            jobs.push(Job { kind: JobKind::Assert { cond: cond.value.span }, span: e.span, body: id, func });
        } else {
            b.const_exprs.insert((id, e.id), inner);
            let key = ConstKey::Expr(id, e.id);
            let Some((slot, _)) = b.const_global(key, e.span) else { continue };
            spans.insert(key, e.span);
            let func = b.func_id(FnKey::ConstInit(key));
            jobs.push(Job { kind: JobKind::Value { slot }, span: e.span, body: id, func });
        }
    }
    if jobs.is_empty() {
        return if b.diags.is_empty() { Ok(None) } else { Err(b.diags) };
    }
    b.drain();
    if !b.diags.is_empty() {
        return Err(b.diags);
    }

    // The order the Lua backend ran them in: by where the instance was
    // reached, then where in it (`const` items last).
    let reached: HashMap<BodyId, usize> = results.reachable.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    jobs.sort_by_key(|j| (reached.get(&j.body).copied().unwrap_or(usize::MAX), j.span.file, j.span.lo));

    let slots = b
        .globals
        .iter()
        .map(|(key, &(offset, _))| {
            let span = spans.get(key).copied().unwrap_or_else(|| b.main_span.expect("set above"));
            let name = match key {
                ConstKey::Def(def) => format!("`{}`", p.def(*def).name),
                ConstKey::Expr(..) => "this `const` expression".to_string(),
            };
            ConstSlot { offset, name, span }
        })
        .collect();
    let roots: Vec<FuncId> = jobs.iter().map(|j| j.func).collect();
    let mut module = b.finish(roots[0])?;
    module.roots = roots;
    Ok(Some(Jobs { module, jobs, slots }))
}

/// Every `const` expression in the code `kind` holds, by its id: a
/// function's body and parameters' defaults, a constant's value, a
/// `const` block, a field's default, a gauge's predicate.
fn index_consts<'e>(kind: &'e DefKind, index: &mut HashMap<HirId, &'e hir::Expr>) {
    let mut roots: Vec<&'e hir::Expr> = Vec::new();
    match kind {
        DefKind::Fn(f) => {
            roots.extend(f.body.as_ref());
            roots.extend(f.params.iter().filter_map(|param| param.default.as_ref()));
        }
        DefKind::Const(c) => roots.push(&c.value),
        DefKind::ConstBlock(e) => roots.push(e),
        DefKind::Field(f) => roots.extend(f.default.as_ref()),
        DefKind::Type(t) => roots.extend(t.gauge.as_ref()),
        _ => {}
    }
    for root in roots {
        consts_in(root, &mut |c| {
            index.insert(c.id, c);
        });
    }
}

/// Calls `found` with each `const` expression in `e`, outermost first and
/// in source order, those inside another one included.
fn consts_in<'e>(e: &'e hir::Expr, found: &mut dyn FnMut(&'e hir::Expr)) {
    if matches!(e.kind, ExprKind::Const(_)) {
        found(e);
    }
    for node in children(e) {
        match node {
            Node::Expr(e) => consts_in(e, found),
            Node::Stmt(s) => match &s.kind {
                StmtKind::Let { init, .. } => consts_in(init, found),
                StmtKind::Given { value, .. } => consts_in(value, found),
                StmtKind::Spool { init, .. } => consts_in(init, found),
                StmtKind::Defer { body, .. } => consts_in(body, found),
                StmtKind::Expr(e) => consts_in(e, found),
                StmtKind::Item(_) => {}
            },
            Node::Pat(_) => {}
        }
    }
}
