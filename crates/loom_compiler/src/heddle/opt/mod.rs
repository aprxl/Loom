//! The optimizer (design/vm.md §7): passes over Heddle, between building it
//! and generating code.
//!
//! At `-O0` nothing runs: the code is what the builder made, which compiles
//! fastest and keeps every call (so every frame of a backtrace). `-O2`, the
//! default, runs the passes in order. Each pass keeps what Heddle promises
//! (`verify`), which the optimizer's tests check after every pass.

mod alias;
mod cfg;
mod fold;
mod gvn;
mod inline;
mod ipcp;
mod licm;
mod modref;
mod range;
mod sccp;
mod tidy;

use super::{Block, Func, InstKind, Module, Term, Value};

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
const PASSES: &[Pass] = &[
    ("inline", inline::inline),
    // What inlining used up the last call of isn't worth the rest's time.
    ("prune", prune),
    ("sccp", sccp::sccp),
    ("gvn", gvn::gvn),
    // What GVN found (a load of memory a `zero` wrote) may decide a branch.
    ("sccp", sccp::sccp),
    // Again: what GVN found to call (a lambda passed to a method that was
    // inlined) can be inlined now, and numbered with the rest.
    ("inline", inline::inline),
    ("prune", prune),
    ("gvn", gvn::gvn),
    ("sccp", sccp::sccp),
    // A parameter all calls give one constant is that constant: GVN has
    // just found what the calls give.
    ("ipcp", ipcp::ipcp),
    ("ranges", range::ranges),
    // What ranges decided about a comparison decides its branch.
    ("sccp", sccp::sccp),
    ("licm", licm::licm),
    ("cleanup", tidy::cleanup),
];

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
    // `LOOM_PASS_TIMES=1` prints how long each pass took, to find what makes
    // compiling slow.
    let timing = std::env::var_os("LOOM_PASS_TIMES").is_some();
    for name in names {
        let Some((name, pass)) = PASSES.iter().find(|(n, _)| n == name) else {
            return Err(format!("no pass is called `{name}`"));
        };
        let start = timing.then(std::time::Instant::now);
        pass(&mut module);
        if let Some(start) = start {
            eprintln!("pass {name:8} {:>9.2?}", start.elapsed());
        }
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

/// Functions nothing reaches have their bodies replaced by a stub, so no
/// later pass and no code generation spends time on them (their numbers stay:
/// a call, a function value and a vtable name a function by it). What
/// reaches a function: the entry and the module's roots, a call or a
/// function value in one that's reached, and (a vtable holding functions'
/// numbers among the program's data, which isn't typed) any word of the data
/// that is one.
fn prune(module: &mut Module) {
    let n = module.funcs.len();
    let mut live = vec![false; n];
    let mut work: Vec<usize> = Vec::new();
    let reach = |f: usize, live: &mut Vec<bool>, work: &mut Vec<usize>| {
        if f < n && !live[f] {
            live[f] = true;
            work.push(f);
        }
    };
    reach(module.entry.0 as usize, &mut live, &mut work);
    for r in &module.roots {
        reach(r.0 as usize, &mut live, &mut work);
    }
    for &w in &module.data {
        if let Ok(w) = usize::try_from(w) {
            reach(w, &mut live, &mut work);
        }
    }
    while let Some(f) = work.pop() {
        for inst in module.funcs[f].blocks.iter().flat_map(|b| &b.insts) {
            match inst.kind {
                InstKind::Call { func, .. } | InstKind::FuncRef(func) => reach(func.0 as usize, &mut live, &mut work),
                _ => {}
            }
        }
    }
    for (f, live) in module.funcs.iter_mut().zip(live) {
        if !live && !is_stub(f) {
            *f = Func {
                name: std::mem::take(&mut f.name),
                blocks: vec![Block { term: Some(Term::Crash(0)), ..Block::default() }],
                results: Vec::new(),
                types: Vec::new(),
                slots: Vec::new(),
                sites: Vec::new(),
            };
        }
    }
}

fn is_stub(f: &Func) -> bool {
    matches!(f.blocks.as_slice(), [b] if b.insts.is_empty() && matches!(b.term, Some(Term::Crash(0))) && f.types.is_empty())
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
