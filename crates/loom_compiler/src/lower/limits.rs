//! Keeping emitted functions within Lua's limit of 200 active locals.
//!
//! A long function (lots of `let`s, and the temporaries that `if` and
//! `match` values need) could go over it, and Lua would refuse to load the
//! bundle. When the locals of one block would, they become the fields of a
//! table local to that block instead: `local _L1 = {}` and `_L1.x_3`. The
//! table is made each time the block runs, just as its locals would be, and
//! a closure that used a local now uses the table, so nothing else changes.

use std::collections::HashSet;

use super::lir::{Expr, Stmt, StmtKind};

/// Locals a block may add before its own go into a table. Below Lua's 200,
/// to leave room for what `for` loops declare behind the scenes.
const MAX_ACTIVE: usize = 180;

/// Fixes every function in `stmt`.
pub fn fix(stmt: &mut Stmt, next: &mut u32) {
    visit_stmt(stmt, &mut |e| fix_functions(e, next));
}

/// Fixes the functions in `e` (and in the functions they contain).
fn fix_functions(e: &mut Expr, next: &mut u32) {
    if let Expr::Function(params, body) = e {
        let params = params.len();
        fix_block(body, params, next);
    } else {
        visit_expr_children(e, &mut |c| fix_functions(c, next));
    }
}

/// Fixes a block that runs with `outer` locals already active.
fn fix_block(stmts: &mut Vec<Stmt>, outer: usize, next: &mut u32) {
    let declared: Vec<String> = stmts
        .iter()
        .flat_map(|s| match &s.kind {
            StmtKind::Local(names, _) => names.clone(),
            _ => Vec::new(),
        })
        .collect();
    let own = if outer + declared.len() > MAX_ACTIVE {
        *next += 1;
        let table = format!("_L{next}");
        let names: HashSet<String> = declared.into_iter().collect();
        let old = std::mem::take(stmts);
        stmts.push(Stmt { kind: StmtKind::Local(vec![table.clone()], vec![Expr::Table(Vec::new())]), span: None });
        for mut stmt in old {
            rename_stmt(&mut stmt, &names, &table);
            match stmt.kind {
                // A table's missing field is already nil.
                StmtKind::Local(_, values) if values.is_empty() => {}
                StmtKind::Local(targets, values) => {
                    let targets = targets.into_iter().map(|n| Expr::field(Expr::name(table.clone()), &n)).collect();
                    stmts.push(Stmt { kind: StmtKind::Assign(targets, values), span: stmt.span });
                }
                kind => stmts.push(Stmt { kind, span: stmt.span }),
            }
        }
        1
    } else {
        declared.len()
    };
    let active = outer + own;
    for stmt in stmts.iter_mut() {
        match &mut stmt.kind {
            StmtKind::If(arms, else_) => {
                for (cond, body) in arms.iter_mut() {
                    fix_functions(cond, next);
                    fix_block(body, active, next);
                }
                if let Some(body) = else_ {
                    fix_block(body, active, next);
                }
            }
            StmtKind::While(cond, body) => {
                fix_functions(cond, next);
                fix_block(body, active, next);
            }
            // A numeric `for` keeps three hidden locals, a generic one four.
            StmtKind::NumFor(_, lo, hi, body) => {
                fix_functions(lo, next);
                fix_functions(hi, next);
                fix_block(body, active + 4, next);
            }
            StmtKind::GenFor(vars, f, body) => {
                fix_functions(f, next);
                let n = vars.len();
                fix_block(body, active + 4 + n, next);
            }
            StmtKind::Do(body) => fix_block(body, active, next),
            _ => visit_stmt_exprs(stmt, &mut |e| fix_functions(e, next)),
        }
    }
}

/// Replaces each use of a local in `names` with the field of `table`.
fn rename_stmt(stmt: &mut Stmt, names: &HashSet<String>, table: &str) {
    visit_stmt(stmt, &mut |e| rename_expr(e, names, table));
}

fn rename_expr(e: &mut Expr, names: &HashSet<String>, table: &str) {
    match e {
        Expr::Name(n) if names.contains(n.as_str()) => {
            let n = std::mem::take(n);
            *e = Expr::field(Expr::name(table), &n);
        }
        Expr::Function(_, body) => {
            for stmt in body {
                rename_stmt(stmt, names, table);
            }
        }
        _ => visit_expr_children(e, &mut |c| rename_expr(c, names, table)),
    }
}

/// Calls `f` on each expression directly in `stmt` and in the blocks nested
/// in it (not inside functions: `f` decides whether to go in).
fn visit_stmt(stmt: &mut Stmt, f: &mut impl FnMut(&mut Expr)) {
    match &mut stmt.kind {
        StmtKind::If(arms, else_) => {
            for (cond, body) in arms {
                f(cond);
                body.iter_mut().for_each(|s| visit_stmt(s, f));
            }
            if let Some(body) = else_ {
                body.iter_mut().for_each(|s| visit_stmt(s, f));
            }
        }
        StmtKind::While(cond, body) => {
            f(cond);
            body.iter_mut().for_each(|s| visit_stmt(s, f));
        }
        StmtKind::NumFor(_, lo, hi, body) => {
            f(lo);
            f(hi);
            body.iter_mut().for_each(|s| visit_stmt(s, f));
        }
        StmtKind::GenFor(_, e, body) => {
            f(e);
            body.iter_mut().for_each(|s| visit_stmt(s, f));
        }
        StmtKind::Do(body) => body.iter_mut().for_each(|s| visit_stmt(s, f)),
        _ => visit_stmt_exprs(stmt, f),
    }
}

/// Calls `f` on the expressions of a statement that holds no blocks.
fn visit_stmt_exprs(stmt: &mut Stmt, f: &mut impl FnMut(&mut Expr)) {
    match &mut stmt.kind {
        StmtKind::Local(_, values) => values.iter_mut().for_each(f),
        StmtKind::Assign(targets, values) => {
            targets.iter_mut().for_each(&mut *f);
            values.iter_mut().for_each(f);
        }
        StmtKind::Call(e) => f(e),
        StmtKind::Return(Some(e)) => f(e),
        StmtKind::ReturnMany(values) => values.iter_mut().for_each(f),
        StmtKind::If(..)
        | StmtKind::While(..)
        | StmtKind::NumFor(..)
        | StmtKind::GenFor(..)
        | StmtKind::Do(_)
        | StmtKind::Return(None)
        | StmtKind::Goto(_)
        | StmtKind::Label(_)
        | StmtKind::Break => {}
    }
}

fn visit_expr_children(e: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    match e {
        Expr::Index(a, b) | Expr::Binary(_, a, b) => {
            f(a);
            f(b);
        }
        Expr::Field(a, _) | Expr::Unary(_, a) => f(a),
        Expr::Call(callee, args) => {
            f(callee);
            args.iter_mut().for_each(f);
        }
        Expr::Table(items) => items.iter_mut().for_each(f),
        Expr::Record(entries) => entries.iter_mut().for_each(|(_, v)| f(v)),
        Expr::Function(_, body) => body.iter_mut().for_each(|s| visit_stmt(s, f)),
        Expr::Nil | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Name(_) => {}
    }
}
