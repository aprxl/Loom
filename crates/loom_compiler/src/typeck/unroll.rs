//! `const if` and `const for`: control flow decided while compiling.
//!
//! A `const if` condition must fold (see `gauge.rs`). Only the branch it
//! takes is type-checked, move-checked and lowered, so the other can name
//! things that don't exist on this target; its resolution errors are
//! reported only if it's taken.
//!
//! A `const for` goes over a sequence the constant folder can list: a range,
//! an array literal, a constant holding one, and `.indexed()` of any of
//! them. Its body is checked once per item, each time in a [`Body`] of its
//! own with the loop variable standing for that item, the way an instance
//! is checked for its arguments: the item can decide a `const if`, size an
//! array, or pick a type. Lowering writes the body out once per item.

use loom_syntax::Diagnostic;
use loom_syntax::ast::FieldName;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::gauge::Val;
use super::{Body, BodyId, Checker};
use crate::hir::{Block, ConstIfDeferred, DefKind, Expr, ExprKind, Pat, PatKind};
use crate::ty::{ConstVal, Ty, TyKind};

/// How many items a `const for` may go over.
const MAX_UNROLL: usize = 10_000;

/// An item of a `const for`'s sequence.
#[derive(Clone, Debug)]
enum Item {
    Val(ConstVal),
    Tuple(Vec<Item>),
}

/// What `target` says about the machine the program is built for: on the
/// Lua backend, the one doing the build. Each is the name of a case of
/// `std.target.Os`, `Arch` and `Endian`.
pub(crate) struct TargetInfo {
    pub os: &'static str,
    pub arch: &'static str,
    pub endian: &'static str,
    pub pointer_bits: u8,
}

pub(crate) const TARGET: TargetInfo = TargetInfo {
    os: if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "MacOS"
    } else {
        "Other"
    },
    arch: if cfg!(target_arch = "x86_64") {
        "Amd64"
    } else if cfg!(target_arch = "aarch64") {
        "Arm64"
    } else {
        "Other"
    },
    endian: if cfg!(target_endian = "little") { "Little" } else { "Big" },
    // `usize` is 63 bits on the Lua backend (see design/lua-backend.md).
    pointer_bits: 63,
};

impl Checker<'_> {
    /// `target.name` at compile time.
    pub(super) fn target_val(&mut self, name: &str) -> Option<Val> {
        let (warp, case) = match name {
            "os" => ("Os", TARGET.os),
            "arch" => ("Arch", TARGET.arch),
            "endian" => ("Endian", TARGET.endian),
            "pointer_bits" => return Some(Val::Int(i128::from(TARGET.pointer_bits))),
            _ => return None,
        };
        let def = self.std_item("std.target", warp)?;
        let info = self.warp_info(def)?;
        info.cases.iter().find(|c| c.name == case).map(|c| Val::Case(c.def))
    }

    /// The type of `target`.
    pub(super) fn target_ty(&mut self, span: Span) -> Ty {
        match self.std_item("std.target", "Target") {
            Some(def) => self.tcx.intern(TyKind::Adt(def, Vec::new())),
            None => self.unsupported(span, "`target`"),
        }
    }

    fn std_item(&self, module: &str, name: &str) -> Option<crate::hir::DefId> {
        self.p.modules.iter().find(|m| m.path == module)?.items.get(name).copied()
    }

    /// A condition decided while compiling: `None` if it can't be (reported).
    fn const_cond(&mut self, cond: &Expr) -> Option<bool> {
        let (mark, hits) = (self.diags.len(), self.unsupported_hits);
        self.check_const_code(cond.span, cond, Some(self.tcx.bool));
        match self.fold(cond, None) {
            Some(Val::Bool(b)) => Some(b),
            // Already reported.
            _ if self.unsupported_hits > hits || self.diags[mark..].iter().any(Diagnostic::is_error) => None,
            _ => {
                self.not_known(cond.span, "a `const if` condition");
                None
            }
        }
    }

    fn not_known(&mut self, span: Span, what: &str) {
        self.diags.push(
            Diagnostic::error(codes::K0110, format!("{what} must be known at compile time"))
                .primary(span, "the constant folder can't work this out")
                .note("it can use literals, constants, `const` parameters, `target`, arithmetic and comparisons"),
        );
    }

    /// `const if cond { then } else …`, the expression `e`.
    pub(super) fn check_const_if(
        &mut self,
        e: &Expr,
        (cond, then, else_): (&Expr, &Block, Option<&Expr>),
        deferred: &ConstIfDeferred,
        expected: Option<Ty>,
    ) -> Ty {
        let Some(taken) = self.const_cond(cond) else { return self.tcx.error };
        self.b.const_ifs.insert(e.id, taken);
        // What name resolution found wrong in the branch taken.
        let found = if taken { &deferred.then } else { &deferred.else_ };
        self.diags.extend(found.iter().cloned());
        match (taken, else_) {
            (true, _) => self.check_block(then, expected),
            (false, Some(else_)) => self.check_expr(else_, expected),
            (false, None) => self.tcx.unit,
        }
    }

    /// `const for pat in iter { body }`, the expression `e`.
    pub(super) fn check_const_for(&mut self, e: &Expr, pat: &Pat, iter: &Expr, body: &Block) -> Ty {
        let (mark, hits) = (self.diags.len(), self.unsupported_hits);
        let iter_ty = self.check_const_code(iter.span, iter, None);
        let iter_ty = self.resolve(iter_ty);
        let elem = match self.tcx.kind(iter_ty) {
            TyKind::Range(elem) => *elem,
            TyKind::Error => return self.tcx.unit,
            _ => match self.seq_elem(iter_ty).or_else(|| self.for_items(e, iter_ty)) {
                Some(elem) => elem,
                None => {
                    let shown = self.show(iter_ty);
                    self.error(codes::T0108, iter.span, format!("can't loop over `{shown}`"), "no items");
                    return self.tcx.unit;
                }
            },
        };
        let Some(items) = self.const_items(iter, 0) else {
            // Unless it's already reported.
            if self.unsupported_hits == hits && !self.diags[mark..].iter().any(Diagnostic::is_error) {
                self.not_known(iter.span, "a `const for` sequence");
            }
            return self.tcx.unit;
        };
        if items.len() > MAX_UNROLL {
            self.diags.push(
                Diagnostic::error(codes::K0102, format!("a `const for` over more than {MAX_UNROLL} items"))
                    .primary(iter.span, format!("at most {MAX_UNROLL} items are written out"))
                    .help("use a plain `for`, which runs when the program does"),
            );
            return self.tcx.unit;
        }
        self.const_loops.insert(e.id);
        let outer = self.b.local_ty.clone();
        let parent = self.cur_body.expect("checking a body");
        let mut bodies = Vec::with_capacity(items.len());
        let mark = self.diags.len();
        for item in items {
            // Its own tables, which start knowing the locals around it.
            let id = BodyId(self.r.instances.len() as u32);
            let inst = self.r.instance(parent).clone();
            self.r.instances.push(inst);
            self.r.bodies.push(Body { local_ty: outer.clone(), ..Body::default() });
            self.in_body(id, |this| {
                this.check_pat_irrefutable(pat, elem, "a `const for` loop");
                let mark = this.const_locals.len();
                this.bind_item(pat, &item);
                this.check_block(body, Some(this.tcx.unit));
                this.const_locals.truncate(mark);
            });
            bodies.push(id);
        }
        // A mistake in the body is usually the same for every item: once is enough.
        let mut seen = std::collections::HashSet::new();
        let fresh: Vec<Diagnostic> = self.diags.drain(mark..).collect();
        for diag in fresh {
            let span = diag.labels.iter().find(|l| l.primary).map(|l| l.span);
            if seen.insert((diag.code.id, span, diag.message.clone())) {
                self.diags.push(diag);
            }
        }
        self.b.unrolled.insert(e.id, bodies);
        self.tcx.unit
    }

    /// The items of a `const for` sequence, if the folder can list them.
    fn const_items(&mut self, e: &Expr, depth: u32) -> Option<Vec<Item>> {
        if depth > 16 {
            return None;
        }
        match &e.kind {
            ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive } => {
                let (Val::Int(lo), Val::Int(hi)) = (self.fold(lo, None)?, self.fold(hi, None)?) else { return None };
                let hi = if *inclusive { hi.checked_add(1)? } else { hi };
                // Too many to write out: listing them would take forever too.
                let hi = hi.min(lo.saturating_add(MAX_UNROLL as i128 + 1));
                Some((lo..hi).map(|i| Item::Val(ConstVal::Int(i))).collect())
            }
            ExprKind::Array(items) => items
                .iter()
                .map(|item| match self.fold(item, None)? {
                    Val::Int(i) => Some(Item::Val(ConstVal::Int(i))),
                    Val::Bool(b) => Some(Item::Val(ConstVal::Bool(b))),
                    _ => None,
                })
                .collect(),
            ExprKind::Def { def, args } if args.is_empty() => match &self.p.def(*def).kind {
                DefKind::Const(c) => self.const_items(&c.value, depth + 1),
                _ => None,
            },
            ExprKind::Block(block) if block.stmts.len() == 1 => match &block.stmts[0].kind {
                crate::hir::StmtKind::Expr(inner) => self.const_items(inner, depth + 1),
                _ => None,
            },
            ExprKind::Call { callee, args } if args.is_empty() => match &callee.kind {
                ExprKind::Field { base, field: FieldName::Named(name) } if name.name == "indexed" => {
                    let items = self.const_items(base, depth + 1)?;
                    let indexed = items.into_iter().enumerate();
                    Some(
                        indexed.map(|(i, item)| Item::Tuple(vec![Item::Val(ConstVal::Int(i as i128)), item])).collect(),
                    )
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// The locals `pat` binds stand for the parts of `item`.
    fn bind_item(&mut self, pat: &Pat, item: &Item) {
        match (&pat.kind, item) {
            (PatKind::Wild, _) => {}
            // Each item is a compile-time value, written into the code.
            (PatKind::Bind(local), _) if self.p.local(*local).mutable => {
                self.unsupported(pat.span, "a `mut` loop variable in a `const for`");
            }
            (PatKind::Bind(local), Item::Val(v)) => {
                let v = self.tcx.intern(TyKind::Value(*v));
                self.const_locals.push((*local, v));
            }
            (PatKind::Tuple(pats), Item::Tuple(items)) if pats.len() == items.len() => {
                for (pat, item) in pats.iter().zip(items) {
                    self.bind_item(pat, item);
                }
            }
            _ => {
                self.unsupported(pat.span, "this pattern in a `const for`");
            }
        }
    }
}
