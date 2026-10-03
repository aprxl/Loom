//! The optimizer (design/vm.md §7): passes over Heddle, between building it
//! and generating code.
//!
//! At `-O0` nothing runs: the code is what the builder made, which compiles
//! fastest and keeps every call (so every frame of a backtrace). `-O2`, the
//! default, runs the passes in order. Each pass keeps what Heddle promises
//! (`verify`), which the optimizer's tests check after every pass.

mod alias;
mod cfg;
mod gvn;
mod inline;
mod licm;
mod tidy;

use super::{Module, Term, Value};

/// How hard to optimize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Level {
    /// Nothing: the code as built.
    O0,
    /// Every pass.
    #[default]
    O2,
}

/// A pass: what it's called, and what it does to a module.
type Pass = (&'static str, fn(&mut Module));

/// The passes `-O2` runs, in order.
const PASSES: &[Pass] =
    &[("inline", inline::inline), ("gvn", gvn::gvn), ("licm", licm::licm), ("cleanup", tidy::cleanup)];

/// The names of the passes, in the order `-O2` runs them.
pub fn passes() -> impl Iterator<Item = &'static str> {
    PASSES.iter().map(|(name, _)| *name)
}

/// `module`, optimized at `level`; or what's wrong with what a pass made
/// (a bug in Loom).
pub fn optimize(module: Module, level: Level) -> Result<Module, String> {
    match level {
        Level::O0 => Ok(module),
        // `LOOM_PASSES=inline,cleanup` runs only those (to see what one
        // does).
        Level::O2 => match std::env::var("LOOM_PASSES") {
            Ok(names) => run(module, &names.split(',').filter(|n| !n.is_empty()).collect::<Vec<_>>()),
            Err(_) => run(module, &passes().collect::<Vec<_>>()),
        },
    }
}

/// `module` after the passes named, in that order (for the optimizer's
/// tests, which look at what one pass does).
pub fn run(mut module: Module, names: &[&str]) -> Result<Module, String> {
    for name in names {
        let Some((name, pass)) = PASSES.iter().find(|(n, _)| n == name) else {
            return Err(format!("no pass is called `{name}`"));
        };
        pass(&mut module);
        // Every pass in a debug build (the tests), once at the end otherwise.
        // `LOOM_DUMP_BAD=1` prints what a pass got wrong.
        if cfg!(debug_assertions)
            && let Err(e) = super::verify(&module)
        {
            if std::env::var_os("LOOM_DUMP_BAD").is_some() {
                eprintln!("{}", super::print(&module));
            }
            return Err(format!("after {name}: {e}"));
        }
    }
    if !cfg!(debug_assertions) {
        super::verify(&module)?;
    }
    Ok(module)
}

/// The values a terminator reads, its edges' arguments included.
pub(crate) fn term_uses(t: &Term) -> Vec<Value> {
    let mut out = match t {
        Term::Br { cond, .. } => vec![*cond],
        Term::Ret(vs) => vs.clone(),
        Term::Unravel { ptr, len } => vec![*ptr, *len],
        Term::Jump(_) | Term::UnravelMsg | Term::Crash(_) | Term::Resume | Term::Suspend { .. } => Vec::new(),
    };
    for e in t.edges() {
        out.extend(&e.args);
    }
    out
}

/// Replaces every value `t` reads (its edges' arguments included) with
/// `f` of it.
pub(crate) fn rewrite_term(t: &mut Term, mut f: impl FnMut(Value) -> Value) {
    match t {
        Term::Br { cond, .. } => *cond = f(*cond),
        Term::Ret(vs) => vs.iter_mut().for_each(|v| *v = f(*v)),
        Term::Unravel { ptr, len } => {
            *ptr = f(*ptr);
            *len = f(*len);
        }
        Term::Jump(_) | Term::UnravelMsg | Term::Crash(_) | Term::Resume | Term::Suspend { .. } => {}
    }
    for e in t.edges_mut() {
        e.args.iter_mut().for_each(|a| *a = f(*a));
    }
}
