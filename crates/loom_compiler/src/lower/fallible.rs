//! Lowering `?`, `mend`, `fail`, and optional and result values.
//!
//! A `?T` is `nil` or the value itself, boxed as `{v}` when `nil` is one of
//! T's own values (`?()`, `??T`). A `T ! E` is a table: `{true, v}` or
//! `{false, e}`.
//!
//! A `mend` is a `do` block for its left operand, whose `?`s jump out to the
//! handler with the error in a local declared before it:
//!
//! ```text
//! local _t1, _t2          -- the value, and the error
//! do
//!   local _t3 = f(x)
//!   if not _t3[1] then _t2 = _t3[2]; goto _mf1 end
//!   _t1 = _t3[2]
//!   goto _md2
//! end
//! ::_mf1::
//! do local e_4 = _t2; _t1 = handler(e_4) end
//! ::_md2::
//! ```
//!
//! Each block is its own `do … end`, so no `goto` jumps into a local's scope.

use loom_syntax::source::Span;
use loom_syntax::token::Keyword;

use super::lir::{Expr, Stmt, StmtKind, TableKey};
use super::{Dest, Lowerer};
use crate::hir::{self, ExprKind, HirId};
use crate::ty::{Ty, TyKind};
use crate::typeck::{Coercion, ErrConv, Exit, Unwrap};

/// A `mend` being lowered: where its `?`s jump, and where the error goes.
#[derive(Debug)]
pub(super) struct MendLabels {
    pub id: HirId,
    pub fail: String,
    pub err: String,
}

impl Lowerer<'_> {
    /// Whether `t` (a spilled `?T` or `T ! E`) has nothing to unwrap.
    fn is_empty(unwrap: Unwrap, t: &Expr) -> Expr {
        match unwrap {
            Unwrap::Optional => Expr::bin("==", t.clone(), Expr::Nil),
            Unwrap::Result => Expr::Unary("not", Box::new(Expr::index(t.clone(), Expr::Int(1)))),
        }
    }

    /// The value inside `t`, once it's known to have one. `ty` is the type
    /// of `t`: an optional's value is boxed when `nil` is one of its values.
    fn unwrapped(&self, unwrap: Unwrap, t: &Expr, ty: Ty) -> Expr {
        match (unwrap, self.tcx().kind(ty)) {
            (Unwrap::Optional, TyKind::Optional(inner)) if self.opt_boxed(*inner) => {
                Expr::index(t.clone(), Expr::Int(1))
            }
            (Unwrap::Optional, _) => t.clone(),
            (Unwrap::Result, _) => Expr::index(t.clone(), Expr::Int(2)),
        }
    }

    /// The error in an empty `t`: `Missing()` for an optional.
    fn error_in(unwrap: Unwrap, t: &Expr) -> Expr {
        match unwrap {
            Unwrap::Optional => Expr::Table(Vec::new()),
            Unwrap::Result => Expr::index(t.clone(), Expr::Int(2)),
        }
    }

    /// `x?`.
    pub(super) fn lower_try(&mut self, e: &hir::Expr, inner: &hir::Expr) -> Expr {
        let Some(info) = self.tables().tries.get(&e.id).cloned() else {
            self.unsupported(e.span, "this `?`");
            return Expr::Nil;
        };
        let v = self.lower_expr(inner);
        let t = self.spill_always(v);
        let ty = self.ty_of(inner);
        let err = Self::error_in(info.unwrap, &t);
        let fail = self.nested(|this| this.exit(&info.exit, err, info.err_ty, e.span));
        self.push(StmtKind::If(vec![(Self::is_empty(info.unwrap, &t), fail)], None));
        let v = self.unwrapped(info.unwrap, &t, ty);
        // Out of a place, a `Copy` value is its own: `var q = opt?` mustn't
        // share `opt`'s table.
        let inner_ty = match self.tcx().kind(ty) {
            TyKind::Optional(t) | TyKind::Result(t, _) => *t,
            _ => ty,
        };
        if self.is_place(inner) && self.is_table(inner_ty) && self.is_copy(inner_ty) {
            let d = self.desc(inner_ty, e.span);
            return Expr::rt("copy", vec![d, v]);
        }
        v
    }

    /// Leaves with error `err` (of type `ty`): to a `mend`, or out of the
    /// function.
    fn exit(&mut self, exit: &Exit, err: Expr, ty: Ty, span: Span) {
        match exit {
            Exit::Mend(id, conv) => {
                let Some(labels) = self.mend_labels.iter().rev().find(|m| m.id == *id) else { return };
                let (fail, local) = (labels.fail.clone(), labels.err.clone());
                if let Some(conv) = conv {
                    let err = self.convert_err(err, ty, conv, span);
                    self.push(StmtKind::Assign(vec![Expr::name(local)], vec![err]));
                }
                self.push(StmtKind::Goto(fail));
            }
            Exit::ReturnNone => self.push(StmtKind::Return(Some(Expr::Nil))),
            Exit::ReturnErr(conv) => {
                let err = self.convert_err(err, ty, conv, span);
                self.push(StmtKind::Return(Some(Expr::Table(vec![Expr::Bool(false), err]))));
            }
        }
    }

    /// An error of type `from` as the type it's going to.
    fn convert_err(&mut self, v: Expr, from: Ty, conv: &ErrConv, span: Span) -> Expr {
        match conv {
            ErrConv::Same => v,
            ErrConv::ToUnion => {
                let vt = self.vtable(from, Vec::new(), span);
                Expr::rt("any", vec![vt, v])
            }
            ErrConv::ToAny(wefts) => {
                let vt = self.vtable(from, wefts.clone(), span);
                Expr::rt("any", vec![vt, v])
            }
            ErrConv::Absorb { warp, case, key } => {
                let tag = self.warp(*warp).and_then(|w| w.case_by_def(*case)).map_or(1, |c| c.tag);
                let key = match key {
                    crate::typeck::Key::Name(n) => TableKey::Name(n.clone()),
                    crate::typeck::Key::Pos(i) => TableKey::Index(*i as i64),
                };
                Expr::Record(vec![(TableKey::Name("tag".into()), Expr::Int(tag as i64)), (key, v)])
            }
        }
    }

    /// `a mend b`, `a mend e => b`, with its value going to `dest`.
    pub(super) fn mend_into(&mut self, e: &hir::Expr, left: &hir::Expr, handler: &hir::Expr, dest: &Dest) {
        let Some(info) = self.tables().mends.get(&e.id).cloned() else {
            self.unsupported(e.span, "this `mend`");
            return;
        };
        let fail = self.new_label("mf");
        let done = self.new_label("md");
        let err = self.new_temp();
        self.push(StmtKind::Local(vec![err.clone()], Vec::new()));
        self.mend_labels.push(MendLabels { id: e.id, fail: fail.clone(), err: err.clone() });
        let body = self.nested(|this| {
            let mut v = this.lower_expr(left);
            let mut ty = this.ty_of(left);
            if let Some((unwrap, err_ty, conv)) = &info.unwrap {
                let t = this.spill_always(v);
                let failed = this.nested(|this| {
                    if let Some(conv) = conv {
                        let value = Self::error_in(*unwrap, &t);
                        let value = this.convert_err(value, *err_ty, conv, e.span);
                        this.push(StmtKind::Assign(vec![Expr::name(err.clone())], vec![value]));
                    }
                    this.push(StmtKind::Goto(fail.clone()));
                });
                this.push(StmtKind::If(vec![(Self::is_empty(*unwrap, &t), failed)], None));
                v = this.unwrapped(*unwrap, &t, ty);
                ty = match this.tcx().kind(ty) {
                    TyKind::Optional(inner) | TyKind::Result(inner, _) => *inner,
                    _ => ty,
                };
            }
            // An owning destination gets its own copy of a place's value.
            let owning = !matches!(dest, Dest::Discard);
            if owning && this.is_table(ty) && this.is_copy(ty) && (this.is_place(left) || info.unwrap.is_some()) {
                let d = this.desc(ty, e.span);
                v = Expr::rt("copy", vec![d, v]);
            }
            v = match info.wrap {
                Some(Coercion::ToOptional) if this.opt_boxed(ty) => Expr::Table(vec![v]),
                Some(Coercion::ToResult) => Expr::Table(vec![Expr::Bool(true), v]),
                _ => v,
            };
            this.deliver(v, dest);
            this.push(StmtKind::Goto(done.clone()));
        });
        self.mend_labels.pop();
        self.push(StmtKind::Do(body));
        self.out.push(Stmt { kind: StmtKind::Label(fail), span: None });
        let handled = self.nested(|this| {
            if let Some(local) = info.binding {
                let name = this.local_name(local);
                this.push_local(name, Expr::name(err));
            }
            this.lower_into(handler, dest);
        });
        self.push(StmtKind::Do(handled));
        self.out.push(Stmt { kind: StmtKind::Label(done), span: None });
    }

    /// `fail e`.
    pub(super) fn lower_fail(&mut self, e: &hir::Expr, value: &hir::Expr) {
        let Some((ty, conv)) = self.tables().fails.get(&e.id).cloned() else {
            self.unsupported(e.span, "this `fail`");
            return;
        };
        let v = self.lower_owned(value);
        let v = self.convert_err(v, ty, &conv, e.span);
        self.push(StmtKind::Return(Some(Expr::Table(vec![Expr::Bool(false), v]))));
    }

    /// `some(x)`, `ok(x)`, `err(e)`.
    pub(super) fn builtin_case(&mut self, e: &hir::Expr, kw: Keyword, args: &[hir::Arg]) -> Expr {
        let Some(arg) = args.first() else { return Expr::Nil };
        let v = self.lower_owned(&arg.value);
        match (kw, self.tcx().kind(self.ty_of(e))) {
            (Keyword::Some, TyKind::Optional(inner)) if self.opt_boxed(*inner) => Expr::Table(vec![v]),
            (Keyword::Some, _) => v,
            (Keyword::Ok, _) => Expr::Table(vec![Expr::Bool(true), v]),
            _ => Expr::Table(vec![Expr::Bool(false), v]),
        }
    }

    /// The test for a `some`/`none`/`ok`/`err` pattern, if `pat` is one (see
    /// `pat_test`).
    pub(super) fn wrapper_pat_test(&mut self, pat: &hir::Pat, v: &Expr, ty: Ty) -> Option<Option<Expr>> {
        use hir::PatKind;
        let (unwrap, inner, present) = match &pat.kind {
            PatKind::Some(p) => (Unwrap::Optional, Some(p), true),
            PatKind::None => (Unwrap::Optional, None, false),
            PatKind::Ok(p) => (Unwrap::Result, Some(p), true),
            PatKind::Err(p) => (Unwrap::Result, Some(p), false),
            _ => return None,
        };
        let empty = Self::is_empty(unwrap, v);
        let mut tests = vec![if present { Expr::Unary("not", Box::new(empty)) } else { empty }];
        if let Some(inner) = inner {
            let value = if present { self.unwrapped(unwrap, v, ty) } else { Self::error_in(unwrap, v) };
            let inner_ty = self.pat_ty(inner);
            if let Some(t) = self.pat_test(inner, &value, inner_ty) {
                tests.push(t);
            }
        }
        Some(tests.into_iter().reduce(|a, b| Expr::bin("and", a, b)))
    }

    /// Binds the names inside a `some`/`ok`/`err` pattern (see `bind_pat`).
    pub(super) fn bind_wrapper(&mut self, pat: &hir::Pat, v: &Expr, declare: bool) -> bool {
        use hir::PatKind;
        let (inner, value) = match &pat.kind {
            PatKind::Some(p) => {
                let ty = self.pat_ty(pat);
                (p, self.unwrapped(Unwrap::Optional, v, ty))
            }
            PatKind::Ok(p) => (p, self.unwrapped(Unwrap::Result, v, self.pat_ty(pat))),
            PatKind::Err(p) => (p, Self::error_in(Unwrap::Result, v)),
            _ => return false,
        };
        self.bind_pat(inner, &value, declare);
        true
    }

    /// Whether `e` is a `mend` or a `fail`: statements, lowered here.
    pub(super) fn is_fallible_control(e: &hir::Expr) -> bool {
        matches!(e.kind, ExprKind::Mend { .. } | ExprKind::Fail(_))
    }
}
