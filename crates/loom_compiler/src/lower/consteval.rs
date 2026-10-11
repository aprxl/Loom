//! Compile-time code, run in the sandbox (see `loom_host::consteval`).
//!
//! Before the real bundle is built, the program is lowered once more into
//! a bundle that returns a list of jobs rather than `main`: one per
//! constant, per `const` expression and per `const assert`. The host runs
//! them under a step budget with no I/O, and each failure becomes a K
//! diagnostic here.
//!
//! Constants live in the bundle's `C` table, which computes each one the
//! first time it's read (`rt.lazy`): a `const` expression in a function is
//! a constant of its own, so `const fib(20)` in a loop is computed once.
//! The real bundle computes them again when the program reads them: the
//! sandbox proved they can't fail, and compile-time code can't depend on
//! anything but itself, so they get the values they had while compiling.
//! Writing those values into the bundle instead is an optimization, left
//! for Loom's own interpreter (see `design/lua-backend.md`).

use loom_host::consteval::{Budget, JobResult, STEP_BUDGET, TIME_LIMIT, run_jobs};
use loom_host::{Location, Outcome};
use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::{SourceDb, Span};

use super::lir::{Expr, Stmt, StmtKind};
use super::{Dest, Entry, Lowerer};
use crate::hir::{self, DefId, ExprKind, Program};
use crate::typeck::{ArgSource, BodyId, Typed, assert_args};

/// Something the sandbox runs.
#[derive(Clone, Debug)]
pub(super) struct Job {
    kind: JobKind,
    span: Span,
    /// The body it's in, which may be an instance: `Ring[u32, 4]`.
    body: BodyId,
}

#[derive(Clone, Debug)]
enum JobKind {
    /// Computing a constant.
    Const(DefId),
    /// Computing a `const` expression.
    Value,
    /// Checking a `const assert`, whose condition is at this span.
    Assert(Span),
}

/// Runs the program's compile-time code. `tests`: the tests' too.
pub(super) fn evaluate(db: &SourceDb, p: &Program, typed: &Typed, tests: bool) -> Result<(), Vec<Diagnostic>> {
    let mut l = Lowerer::new(p, typed);
    l.eval = true;
    l.lower_all(tests);
    for &id in &typed.results.const_blocks {
        l.const_block(id);
    }
    if !l.diags.is_empty() {
        return Err(l.diags);
    }
    if l.jobs.is_empty() {
        return Ok(());
    }
    let jobs: Vec<Job> = l.jobs.iter().map(|(job, _)| job.clone()).collect();
    let slots = l.slots.clone();
    let bundle = l.assemble(db, Entry::ConstEval);
    let results = match run_jobs(&bundle.lua, Some(&bundle.map)) {
        Ok(results) => results,
        Err(outcome) => {
            let span = jobs[0].span;
            return Err(vec![internal(span, &outcome)]);
        }
    };
    let mut diags = Vec::new();
    for (job, result) in jobs.iter().zip(results) {
        let what = match job.kind {
            JobKind::Const(def) => format!("while computing `{}`", p.def(def).name),
            JobKind::Value | JobKind::Assert(_) => "while running this".to_string(),
        };
        let diag = match result {
            JobResult::Ok | JobResult::Skipped => continue,
            JobResult::AssertFailed(message) => {
                let JobKind::Assert(cond) = job.kind else { continue };
                let title = match message.as_str() {
                    "" => "const assertion failed".to_string(),
                    m => format!("const assertion failed: {m}"),
                };
                Diagnostic::error(codes::K0101, title).primary(cond, "this is false")
            }
            JobResult::Failed(Outcome::Unravel { message, location }) => with_location(
                Diagnostic::error(codes::K0103, format!("compile-time code unravelled: {message}"))
                    .primary(job.span, what),
                "it unravelled at",
                location,
            )
            .help("unravelling while compiling is a compile error"),
            JobResult::Failed(outcome) => internal(job.span, &outcome),
            JobResult::NotConst { path, location } => with_location(
                Diagnostic::error(codes::K0104, format!("`{path}` can't run at compile time")).primary(job.span, what),
                "called at",
                location,
            )
            .note("compile-time code computes; it can't read or write anything outside the compiler"),
            JobResult::Cycle { key, location } => {
                let (span, name) = slots.get(&key).cloned().unwrap_or((job.span, "this constant".into()));
                with_location(
                    Diagnostic::error(codes::K0107, format!("{name} needs its own value"))
                        .primary(span, "computing this reads it again"),
                    "read again at",
                    location,
                )
            }
            JobResult::OutOfBudget { budget, location } => {
                let (title, note) = match budget {
                    Budget::Memory => (
                        "compile-time code ran out of memory".to_string(),
                        "it may build something that never ends".into(),
                    ),
                    Budget::Steps => (
                        "compile-time code ran out of steps".to_string(),
                        format!("compile-time code may take {STEP_BUDGET} Lua steps in all; this may never stop"),
                    ),
                    Budget::Time => (
                        "compile-time code ran out of time".to_string(),
                        format!(
                            "compile-time code may run for {} seconds in all; this may never stop",
                            TIME_LIMIT.as_secs()
                        ),
                    ),
                };
                with_location(Diagnostic::error(codes::K0102, title).primary(job.span, what), "stopped at", location)
                    .note(note)
            }
        };
        let diag = match typed.results.instance_names.get(&job.body) {
            Some(name) => diag.note(format!("in {name}")),
            None => diag,
        };
        diags.push(diag);
    }
    // In source order, rather than the order the jobs ran in.
    diags.sort_by_key(|d| d.labels.first().map(|l| (l.span.file, l.span.lo)));
    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

fn with_location(diag: Diagnostic, what: &str, location: Option<Location>) -> Diagnostic {
    match location {
        Some(at) => diag.note(format!("{what} {}:{}:{}", at.file, at.line, at.column)),
        None => diag,
    }
}

/// A failure of the compile-time bundle itself: a bug in Loom.
fn internal(span: Span, outcome: &Outcome) -> Diagnostic {
    let message = match outcome {
        Outcome::Unravel { message, .. } | Outcome::Crash { message, .. } | Outcome::Failed { message } => {
            message.clone()
        }
        Outcome::Finished => "it finished".into(),
        Outcome::Exit(code) => format!("it called `process.exit({code})`"),
        Outcome::Interrupted => "it was interrupted".into(),
        // Only a Treadle run with limits gives these (V4).
        Outcome::OverBudget { .. } | Outcome::Refused { .. } => "it was stopped".into(),
    };
    Diagnostic::error(codes::K0103, format!("compile-time code failed inside Loom: {message}"))
        .primary(span, "while running this")
        .note("this is a bug in Loom, not in your program")
}

impl Lowerer<'_> {
    /// `const inner`, the expression `e`: its value, read from its constant.
    /// A `const assert` has none, and nothing of it is left to run.
    pub(super) fn lower_const(&mut self, e: &hir::Expr, inner: &hir::Expr) -> Expr {
        if let Some(args) = assert_args(self.p, inner) {
            if self.eval {
                self.assert_job(e, inner, args);
            }
            return Expr::Nil;
        }
        let key = format!("K{}_{}", self.cur.0, e.id.0);
        if !self.slots.contains_key(&key) {
            self.slots.insert(key.clone(), (e.span, "this `const` expression".into()));
            let body = self.body(e.span, |this| this.lower_into(inner, &Dest::Return));
            self.const_init(&key, body, e.span);
            if self.eval {
                self.read_job(&key, Job { kind: JobKind::Value, span: e.span, body: self.cur });
            }
        }
        Expr::field(Expr::name("C"), &key)
    }

    /// `CI.key = function() … end`: how constant `key` is computed.
    pub(super) fn const_init(&mut self, key: &str, body: Vec<Stmt>, span: Span) {
        let target = Expr::field(Expr::name("CI"), key);
        let init = Expr::Function(Vec::new(), body);
        self.consts.push(Stmt { kind: StmtKind::Assign(vec![target], vec![init]), span: Some(span) });
    }

    /// A job that reads constant `key`, computing it.
    pub(super) fn read_job(&mut self, key: &str, job: Job) {
        let read = Stmt {
            kind: StmtKind::Local(vec!["_".into()], vec![Expr::field(Expr::name("C"), key)]),
            span: Some(job.span),
        };
        self.jobs.push((job, Expr::Function(Vec::new(), vec![read])));
    }

    /// The job for constant `def`, which is `key` in `C`.
    pub(super) fn const_job(&mut self, def: DefId, key: &str) {
        let span = self.p.def(def).span;
        self.slots.insert(key.to_string(), (span, format!("`{}`", self.p.def(def).name)));
        if self.eval {
            self.read_job(key, Job { kind: JobKind::Const(def), span, body: self.cur });
        }
    }

    /// `const assert(cond, message)`: a job that returns the message when
    /// `cond` is false (`""` for std's default one).
    fn assert_job(&mut self, e: &hir::Expr, call: &hir::Expr, args: &[hir::Arg]) {
        let Some(info) = self.tables().calls.get(&call.id).cloned() else { return };
        let arg = |k: usize| match info.args.get(k) {
            Some(ArgSource::Arg(i)) => args.get(*i).map(|a| &a.value),
            _ => None,
        };
        let Some(cond) = arg(0) else { return };
        let message = arg(1);
        let body = self.body(e.span, |this| {
            let c = this.lower_expr(cond);
            let fail = this.nested(|this| match message {
                Some(m) => this.lower_into(m, &Dest::Return),
                None => this.push(StmtKind::Return(Some(Expr::Str(Vec::new())))),
            });
            this.push(StmtKind::If(vec![(Expr::Unary("not", Box::new(c)), fail)], None));
        });
        let job = Job { kind: JobKind::Assert(cond.span), span: e.span, body: self.cur };
        self.jobs.push((job, Expr::Function(Vec::new(), body)));
    }

    /// A `const` item: `const assert(…)` at the top of a module or in a warp.
    fn const_block(&mut self, id: BodyId) {
        let def = self.typed.results.instance(id).def;
        let crate::hir::DefKind::ConstBlock(e) = &self.p.def(def).kind else { return };
        let ExprKind::Const(inner) = &e.kind else { return };
        self.cur = id;
        // Its value, if it has one, is thrown away: computing it is the point.
        self.lower_const(e, inner);
    }
}
