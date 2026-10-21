//! Lowering expressions, statements and patterns.
//!
//! Scoping rules that live here:
//!
//! - `let` binds after its initializer, so `let x = x + 1` sees the old `x`.
//! - Items in a block are visible in the whole block.
//! - `is` bindings in a condition flow along the `and` chain and into the
//!   body: `if x is some(v) and v > 0 { use(v) }`. Elsewhere they stay inside
//!   the expression.
//! - A hole belongs to the innermost call argument that properly contains
//!   it, so `xs.map(clamp(_, 0, 255))` is `xs.map(x => clamp(x, 0, 255))`. A
//!   call with a bare `_` argument outside any argument becomes a lambda
//!   itself: `let f = clamp(_, 0, 255)`.

use indexmap::IndexMap;
use loom_syntax::Diagnostic;
use loom_syntax::ast::{self, FieldName, Ident};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{LoopFrame, Res, Resolver, RibKind};
use crate::hir::{
    Arg, Arm, Block, CasePath, ConstIfDeferred, DefKind, Expr, ExprKind, FieldPat, HirId, LambdaParam, LocalId,
    LocalKind, ModuleId, Pat, PatKind, Stmt, StmtKind, TemplatePart, Ty, TyKind,
};

/// What the head of a dotted expression resolved to.
enum Head {
    Module(ModuleId),
    Type(Ty),
    Value(Expr),
}

/// Bindings made by one pattern. For or-patterns, later alternatives reuse
/// the locals of the first.
#[derive(Default)]
struct PatBinds {
    kind: Option<LocalKind>,
    mutable: bool,
    bound: IndexMap<String, LocalId>,
    /// Inside the second or later alternative of an or-pattern: the names the
    /// first one bound, and which of them this alternative has bound so far.
    reuse: Option<(IndexMap<String, LocalId>, Vec<String>)>,
}

impl Resolver<'_> {
    fn expr(&mut self, kind: ExprKind, span: Span) -> Expr {
        Expr { id: self.next_hir_id(), kind, span }
    }

    fn error_expr(&mut self, span: Span) -> Expr {
        self.expr(ExprKind::Error, span)
    }

    fn boxed(&mut self, e: &ast::Expr) -> Box<Expr> {
        Box::new(self.lower_expr(e))
    }

    /// Lowers `e` outside any condition: `is` bindings stay inside it.
    fn lower_plain(&mut self, e: &ast::Expr) -> Expr {
        let saved = std::mem::replace(&mut self.in_cond, false);
        let out = self.lower_expr(e);
        self.in_cond = saved;
        out
    }

    /// Lowers the condition of an `if`, `while` or guard into the current rib,
    /// so its `is` bindings reach the body.
    fn lower_cond(&mut self, e: &ast::Expr) -> Expr {
        let saved = std::mem::replace(&mut self.in_cond, true);
        let out = self.lower_expr(e);
        self.in_cond = saved;
        out
    }

    pub(super) fn lower_expr(&mut self, e: &ast::Expr) -> Expr {
        let in_cond = self.in_cond;
        // Anything but `and` and `is` ends a condition's binding chain.
        if !matches!(e.kind, ast::ExprKind::Is { .. } | ast::ExprKind::Binary { op: ast::BinaryOp::And, .. }) {
            self.in_cond = false;
        }
        let out = self.lower_expr_inner(e, in_cond);
        self.in_cond = in_cond;
        out
    }

    fn lower_expr_inner(&mut self, e: &ast::Expr, in_cond: bool) -> Expr {
        use ast::ExprKind as E;
        let span = e.span;
        let kind = match &e.kind {
            E::Lit(lit) => ExprKind::Lit(lit.clone()),
            E::Template(parts) => ExprKind::Template(
                parts
                    .iter()
                    .map(|part| match part {
                        ast::TemplatePart::Text(text, _) => TemplatePart::Text(text.clone()),
                        ast::TemplatePart::Interp { expr, spec } => {
                            TemplatePart::Interp { expr: self.lower_expr(expr), spec: spec.clone() }
                        }
                    })
                    .collect(),
            ),
            E::Name(_) | E::Field { .. } | E::Index { .. } | E::Prim(_) | E::SelfType => {
                return self.lower_path_expr(e);
            }
            E::BuiltinCase(k) => ExprKind::BuiltinCase(*k),
            E::SelfValue => match self.fx.self_local {
                Some(local) => ExprKind::Local(local),
                None => {
                    self.error(codes::N0106, span, "`self` only exists in methods that take it", "no `self` here");
                    ExprKind::Error
                }
            },
            E::LeadingDot(field) => ExprKind::LeadingDot(field.clone()),
            E::Hole => match self.fx.hole_frames.last_mut() {
                Some(_) => {
                    let n = self.fx.hole_frames.last().map_or(0, Vec::len);
                    let local = self.new_local(&format!("_{n}"), span, false, LocalKind::Hole);
                    if let Some(frame) = self.fx.hole_frames.last_mut() {
                        frame.push(local);
                    }
                    ExprKind::Local(local)
                }
                None => {
                    self.report(
                        Diagnostic::error(codes::N0109, "a `_` placeholder only works inside a call argument")
                            .primary(span, "not inside an argument")
                            .help("`xs.map(_ * 2)` means `xs.map(x => x * 2)`; outside an argument, write the lambda"),
                    );
                    ExprKind::Error
                }
            },
            E::Tuple(items) => ExprKind::Tuple(items.iter().map(|i| self.lower_expr(i)).collect()),
            E::Array(items) => ExprKind::Array(items.iter().map(|i| self.lower_expr(i)).collect()),
            E::ArrayRepeat { value, count } => {
                ExprKind::ArrayRepeat { value: self.boxed(value), count: self.boxed(count) }
            }
            E::Map(entries) => {
                ExprKind::Map(entries.iter().map(|(k, v)| (self.lower_expr(k), self.lower_expr(v))).collect())
            }
            E::Block(block) => ExprKind::Block(self.lower_block(block)),
            E::If { cond, then, else_, is_const } => self.lower_if(cond, then, else_.as_deref(), *is_const),
            E::Match { scrutinee, arms } => {
                let scrutinee = self.boxed(scrutinee);
                let arms = arms.iter().map(|arm| self.lower_arm(arm)).collect();
                ExprKind::Match { scrutinee, arms }
            }
            E::For { label, pat, iter, body, is_const: false, is_loom: true } => {
                let _ = label;
                return self.lower_loom_for(pat, iter, body, span);
            }
            E::For { label, pat, iter, body, is_const, is_loom } => {
                let iter = self.boxed(iter);
                let id = self.next_hir_id();
                self.push_rib(RibKind::Normal);
                let pat = self.lower_pat_binding(pat, LocalKind::Pattern, false);
                let body = self.in_loop(label, id, |this| this.lower_block(body));
                self.pop_rib();
                let kind = ExprKind::For { pat: Box::new(pat), iter, body, is_const: *is_const, is_loom: *is_loom };
                return Expr { id, kind, span };
            }
            E::While { label, cond, body } => {
                let id = self.next_hir_id();
                self.push_rib(RibKind::Normal);
                let cond = Box::new(self.lower_cond(cond));
                let body = self.in_loop(label, id, |this| this.lower_block(body));
                self.pop_rib();
                return Expr { id, kind: ExprKind::While { cond, body }, span };
            }
            E::Loop { label, body } => {
                let id = self.next_hir_id();
                let body = self.in_loop(label, id, |this| this.lower_block(body));
                return Expr { id, kind: ExprKind::Loop { body }, span };
            }
            E::Loom { args, body } => {
                let args = self.lower_args(args);
                let group = self.new_local("<loom>", span, false, LocalKind::Group);
                self.fx.looms.push(group);
                let body = self.lower_block(body);
                self.fx.looms.pop();
                ExprKind::Loom { args, body, group }
            }
            E::Thread(body) => {
                let group = self.fx.looms.last().copied();
                ExprKind::Thread { group, body: self.lower_closure_like(body), item: false }
            }
            // The body of a `loom for` is a thread: `continue` ends it, and
            // a `break` or a `continue` of something outside can't leave.
            E::Continue { label: None } if self.fx.in_loom_for && self.fx.loops.is_empty() => ExprKind::Return(None),
            E::Break { label, .. } if self.fx.in_loom_for && self.outside_loom_for(label.as_ref()) => {
                self.leaves_iteration(span, "break")
            }
            E::Continue { label } if self.fx.in_loom_for && self.outside_loom_for(label.as_ref()) => {
                self.leaves_iteration(span, "continue")
            }
            E::Break { label, value } => match self.loop_target(label.as_ref(), span, "break") {
                Some(target) => ExprKind::Break { target, value: value.as_ref().map(|v| self.boxed(v)) },
                None => ExprKind::Error,
            },
            E::Continue { label } => match self.loop_target(label.as_ref(), span, "continue") {
                Some(target) => ExprKind::Continue { target },
                None => ExprKind::Error,
            },
            E::Return(_) if self.fx.in_loom_for => self.leaves_iteration(span, "return"),
            E::Return(value) => ExprKind::Return(value.as_ref().map(|v| self.boxed(v))),
            E::Fail(value) => ExprKind::Fail(self.boxed(value)),
            E::Yield { place, value } => {
                self.require_shuttle(span, "yield");
                ExprKind::Yield { place: *place, value: self.boxed(value) }
            }
            E::Pull => {
                self.require_shuttle(span, "pull");
                ExprKind::Pull
            }
            E::Unsafe { reason, body } => ExprKind::Unsafe { reason: reason.clone(), body: self.lower_block(body) },
            E::Const(inner) => ExprKind::Const(self.boxed(inner)),
            E::Lambda { params, body } => self.lower_lambda(params, body),
            E::Call { callee, args } => return self.lower_call(callee, args, span),
            E::ConstField { base, field } => ExprKind::ConstField { base: self.boxed(base), field: self.boxed(field) },
            E::Try(inner) => ExprKind::Try(self.boxed(inner)),
            E::Deref(inner) => ExprKind::Deref(self.boxed(inner)),
            E::DotBlock { subject, body } => {
                ExprKind::DotBlock { subject: self.boxed(subject), body: self.lower_block(body) }
            }
            E::Unary { op, operand } => ExprKind::Unary { op: *op, operand: self.boxed(operand) },
            E::Binary { op: ast::BinaryOp::And, lhs, rhs } => {
                // `a is some(v) and v > 0`: the right side sees `v`. Outside a
                // condition, the bindings end with the `and`.
                if !in_cond {
                    self.push_rib(RibKind::Normal);
                }
                self.in_cond = true;
                let lhs = self.boxed(lhs);
                self.in_cond = true;
                let rhs = self.boxed(rhs);
                self.in_cond = in_cond;
                if !in_cond {
                    self.pop_rib();
                }
                ExprKind::Binary { op: ast::BinaryOp::And, lhs, rhs }
            }
            E::Binary { op, lhs, rhs } => ExprKind::Binary { op: *op, lhs: self.boxed(lhs), rhs: self.boxed(rhs) },
            E::Chain { first, rest } => ExprKind::Chain {
                first: self.boxed(first),
                rest: rest.iter().map(|(op, e)| (*op, self.lower_expr(e))).collect(),
            },
            E::Is { expr, pat } => {
                self.in_cond = false;
                let expr = self.boxed(expr);
                if !in_cond {
                    self.push_rib(RibKind::Normal);
                }
                let pat = Box::new(self.lower_pat_binding(pat, LocalKind::Pattern, false));
                if !in_cond {
                    self.pop_rib();
                }
                self.in_cond = in_cond;
                ExprKind::Is { expr, pat }
            }
            E::Cast { expr, kind, ty } => {
                ExprKind::Cast { expr: self.boxed(expr), kind: *kind, ty: Box::new(self.lower_ty(ty)) }
            }
            E::Range { lo, hi, inclusive } => ExprKind::Range {
                lo: lo.as_ref().map(|e| self.boxed(e)),
                hi: hi.as_ref().map(|e| self.boxed(e)),
                inclusive: *inclusive,
            },
            E::Mend { expr, binding, handler } => {
                let expr = self.boxed(expr);
                self.push_rib(RibKind::Normal);
                let binding = binding.as_ref().map(|b| {
                    let local = self.new_local(&b.name, b.span, false, LocalKind::Pattern);
                    self.bind(&b.name, Res::Local(local));
                    local
                });
                let handler = self.boxed(handler);
                self.pop_rib();
                ExprKind::Mend { expr, binding, handler }
            }
            E::Assign { target, op, value } => {
                ExprKind::Assign { target: self.boxed(target), op: *op, value: self.boxed(value) }
            }
            E::Error => ExprKind::Error,
        };
        self.expr(kind, span)
    }

    // ------------------------------------------------------------ paths

    /// `a`, `a.b.c`, `a[T]`, `Self`, `u32`: through modules and types where
    /// the names say so, as field access and indexing otherwise.
    fn lower_path_expr(&mut self, e: &ast::Expr) -> Expr {
        match self.path_head(e) {
            Head::Value(v) => v,
            Head::Type(ty) => {
                let span = ty.span;
                self.expr(ExprKind::Type(ty), span)
            }
            Head::Module(m) => {
                let path = self.p.module(m).path.clone();
                self.error(codes::N0112, e.span, format!("`{path}` is a module, not a value"), "expected a value");
                self.error_expr(e.span)
            }
        }
    }

    fn path_head(&mut self, e: &ast::Expr) -> Head {
        use ast::ExprKind as E;
        let span = e.span;
        match &e.kind {
            E::Name(ident) => match self.lookup(&ident.name) {
                Some(Res::Module(m)) => Head::Module(m),
                Some(Res::Def(def)) if self.p.def(def).is_type => {
                    Head::Type(Ty { kind: TyKind::Def { def, args: Vec::new() }, span })
                }
                Some(Res::Def(def)) => Head::Value(self.expr(ExprKind::Def { def, args: Vec::new() }, span)),
                Some(Res::Local(local)) => Head::Value(self.expr(ExprKind::Local(local), span)),
                Some(Res::Intrinsic(i)) => Head::Value(self.expr(ExprKind::Intrinsic(i), span)),
                None => {
                    self.unknown_name(ident);
                    Head::Value(self.error_expr(span))
                }
            },
            E::SelfType => {
                if self.self_type {
                    Head::Type(Ty { kind: TyKind::SelfType, span })
                } else {
                    self.self_type_error(span);
                    Head::Value(self.error_expr(span))
                }
            }
            E::Prim(p) => Head::Type(Ty { kind: TyKind::Prim(*p), span }),
            E::Field { base, field } => {
                let head = self.path_head(base);
                match (head, field) {
                    (Head::Module(m), FieldName::Named(name)) => match self.module_member_or_report(m, name) {
                        Some(binding) => match Res::from(binding) {
                            Res::Module(m) => Head::Module(m),
                            Res::Def(def) if self.p.def(def).is_type => {
                                Head::Type(Ty { kind: TyKind::Def { def, args: Vec::new() }, span })
                            }
                            _ => {
                                let crate::hir::Binding::Def(def) = binding else { unreachable!() };
                                Head::Value(self.expr(ExprKind::Def { def, args: Vec::new() }, span))
                            }
                        },
                        None => Head::Value(self.error_expr(span)),
                    },
                    (Head::Module(m), FieldName::Index(..)) => {
                        let path = self.p.module(m).path.clone();
                        self.error(codes::N0112, span, format!("`{path}` is a module, not a tuple"), "not a value");
                        Head::Value(self.error_expr(span))
                    }
                    (Head::Type(base), FieldName::Named(name)) => {
                        Head::Value(self.expr(ExprKind::Assoc { base: Box::new(base), name: name.clone() }, span))
                    }
                    (Head::Type(base), FieldName::Index(..)) => {
                        self.error(codes::N0112, span, "a type has no tuple fields", "not a value");
                        drop(base);
                        Head::Value(self.error_expr(span))
                    }
                    (Head::Value(base), field) => {
                        Head::Value(self.expr(ExprKind::Field { base: Box::new(base), field: field.clone() }, span))
                    }
                }
            }
            E::Index { base, args } => {
                let head = self.path_head(base);
                match head {
                    // `List[i32]`, `Ring[u32, 4]`: type arguments.
                    Head::Type(Ty { kind: TyKind::Def { def, args: none }, .. }) if none.is_empty() => {
                        let args = self.lower_generic_args(args);
                        Head::Type(Ty { kind: TyKind::Def { def, args }, span })
                    }
                    // `zeros[f32]`: explicit generic arguments on a function.
                    Head::Value(Expr { kind: ExprKind::Def { def, args: none }, .. })
                        if none.is_empty() && self.fn_defs.contains(&def) =>
                    {
                        let args = self.lower_generic_args(args);
                        Head::Value(self.expr(ExprKind::Def { def, args }, span))
                    }
                    Head::Module(m) => {
                        let path = self.p.module(m).path.clone();
                        self.error(
                            codes::N0112,
                            base.span,
                            format!("`{path}` is a module, not a value"),
                            "expected a value",
                        );
                        Head::Value(self.error_expr(span))
                    }
                    Head::Type(ty) => {
                        self.error(codes::N0112, span, "this type already has its arguments", "extra `[...]`");
                        drop(ty);
                        Head::Value(self.error_expr(span))
                    }
                    Head::Value(base) => {
                        let args = self.lower_generic_args(args);
                        Head::Value(self.expr(ExprKind::Index { base: Box::new(base), args }, span))
                    }
                }
            }
            _ => Head::Value(self.lower_expr(e)),
        }
    }

    // ------------------------------------------------------------ calls and holes

    fn lower_args(&mut self, args: &[ast::Arg]) -> Vec<Arg> {
        args.iter()
            .map(|a| Arg {
                label: a.label.clone(),
                is_given: a.is_given,
                mutable: a.mutable,
                value: self.lower_arg_value(&a.value),
                span: a.span,
            })
            .collect()
    }

    /// Lowers a call argument (or a gauge predicate). If holes belong to it,
    /// it becomes a lambda over them.
    pub(super) fn lower_arg_value(&mut self, e: &ast::Expr) -> Expr {
        if matches!(e.kind, ast::ExprKind::Hole) || !has_own_holes(e) {
            return self.lower_plain(e);
        }
        self.fx.hole_frames.push(Vec::new());
        let body = self.lower_plain(e);
        let params = self.fx.hole_frames.pop().unwrap_or_default();
        self.hole_lambda(params, body)
    }

    fn hole_lambda(&mut self, params: Vec<LocalId>, body: Expr) -> Expr {
        let span = body.span;
        let params = params.into_iter().map(|local| LambdaParam { take: false, local, ty: None }).collect();
        self.expr(ExprKind::Lambda { params, body: Box::new(body) }, span)
    }

    fn lower_call(&mut self, callee: &ast::Expr, args: &[ast::Arg], span: Span) -> Expr {
        // `let f = clamp(_, 0, 255)`: a bare `_` argument with no enclosing
        // argument makes this call the lambda.
        let wraps = self.fx.hole_frames.is_empty() && args.iter().any(|a| matches!(a.value.kind, ast::ExprKind::Hole));
        if wraps {
            self.fx.hole_frames.push(Vec::new());
        }
        let saved = std::mem::replace(&mut self.in_cond, false);
        let callee = self.boxed(callee);
        let args = self.lower_args(args);
        self.in_cond = saved;
        let call = self.expr(ExprKind::Call { callee, args }, span);
        if wraps {
            let params = self.fx.hole_frames.pop().unwrap_or_default();
            return self.hole_lambda(params, call);
        }
        call
    }

    fn lower_lambda(&mut self, params: &[ast::LambdaParam], body: &ast::Expr) -> ExprKind {
        self.push_rib(RibKind::Normal);
        let params = params
            .iter()
            .map(|p| {
                let ty = p.ty.as_ref().map(|t| self.lower_ty(t));
                let local = self.new_local(&p.name.name, p.name.span, false, LocalKind::LambdaParam);
                self.bind(&p.name.name, Res::Local(local));
                LambdaParam { take: p.take, local, ty }
            })
            .collect();
        let body = self.lower_closure_like(body);
        self.pop_rib();
        ExprKind::Lambda { params, body }
    }

    /// The body of a lambda or `thread`: a new function for loops, holes and
    /// shuttles, but it still sees `self` and the enclosing locals.
    fn lower_closure_like(&mut self, body: &ast::Expr) -> Box<Expr> {
        let self_local = self.fx.self_local;
        let looms = self.fx.looms.clone();
        let saved = std::mem::replace(&mut self.fx, super::FnCtx { self_local, looms, ..Default::default() });
        let body = self.lower_plain(body);
        self.fx = saved;
        Box::new(body)
    }

    // ------------------------------------------------------------ control flow

    fn lower_if(&mut self, cond: &ast::Expr, then: &ast::Block, else_: Option<&ast::Expr>, is_const: bool) -> ExprKind {
        if is_const {
            // The branch that isn't taken is never type-checked, so its
            // resolution errors wait on the node.
            let cond = self.boxed(cond);
            self.deferred.push(Vec::new());
            let then = self.lower_block(then);
            let then_diags = self.deferred.pop().unwrap_or_default();
            self.deferred.push(Vec::new());
            let else_ = else_.map(|e| self.boxed(e));
            let else_diags = self.deferred.pop().unwrap_or_default();
            let deferred = Box::new(ConstIfDeferred { then: then_diags, else_: else_diags });
            return ExprKind::If { cond, then, else_, deferred: Some(deferred) };
        }
        self.push_rib(RibKind::Normal);
        let cond = Box::new(self.lower_cond(cond));
        let then = self.lower_block(then);
        self.pop_rib();
        let else_ = else_.map(|e| self.boxed(e));
        ExprKind::If { cond, then, else_, deferred: None }
    }

    fn lower_arm(&mut self, arm: &ast::Arm) -> Arm {
        self.push_rib(RibKind::Normal);
        let pat = self.lower_pat_binding(&arm.pat, LocalKind::Pattern, false);
        let guard = arm.guard.as_ref().map(|g| self.lower_cond(g));
        let body = self.lower_plain(&arm.body);
        self.pop_rib();
        Arm { pat, guard, body, span: arm.span }
    }

    fn in_loop<T>(&mut self, label: &Option<Ident>, id: HirId, f: impl FnOnce(&mut Self) -> T) -> T {
        self.fx.loops.push(LoopFrame { label: label.as_ref().map(|l| l.name.clone()), id });
        let out = f(self);
        self.fx.loops.pop();
        out
    }

    fn loop_target(&mut self, label: Option<&Ident>, span: Span, what: &str) -> Option<HirId> {
        let found = match label {
            Some(label) => self.fx.loops.iter().rev().find(|l| l.label.as_deref() == Some(&label.name)),
            None => self.fx.loops.last(),
        };
        if let Some(frame) = found {
            return Some(frame.id);
        }
        let message = match label {
            Some(label) => format!("`{what} {}` has no enclosing loop labeled `{}` here", label.name, label.name),
            None => format!("`{what}` outside a loop"),
        };
        self.report(
            Diagnostic::error(codes::N0108, message)
                .primary(span, "no loop to leave")
                .note("a lambda is its own function: `break` and `continue` can't leave it"),
        );
        None
    }

    /// `loom for pat in iter { body }` is `loom { for pat in iter { thread
    /// { body } }; () }`: the parent drives the iterator, and each item
    /// goes to a thread of its own (design/vm.md §11.3).
    fn lower_loom_for(&mut self, pat: &ast::Pat, iter: &ast::Expr, body: &ast::Block, span: Span) -> Expr {
        let group = self.new_local("<loom>", span, false, LocalKind::Group);
        self.fx.looms.push(group);
        let iter = self.boxed(iter);
        let for_id = self.next_hir_id();
        self.push_rib(RibKind::Normal);
        let pat = self.lower_pat_binding(pat, LocalKind::Pattern, false);
        // The body is a thread's own function: nothing in it can leave it.
        let self_local = self.fx.self_local;
        let looms = self.fx.looms.clone();
        let saved = std::mem::replace(
            &mut self.fx,
            super::FnCtx { self_local, looms, in_loom_for: true, ..Default::default() },
        );
        let block = self.lower_block(body);
        self.fx = saved;
        self.pop_rib();
        self.fx.looms.pop();
        let body_span = block.span;
        // `{ body }; ()`: what the body's last expression is isn't its value.
        let id = self.next_hir_id();
        let user = Expr { id, kind: ExprKind::Block(block), span: body_span };
        let id = self.next_hir_id();
        let unit = Expr { id, kind: ExprKind::Tuple(Vec::new()), span: body_span };
        let id = self.next_hir_id();
        let wrapped = Block {
            stmts: vec![
                Stmt { kind: StmtKind::Expr(user), span: body_span },
                Stmt { kind: StmtKind::Expr(unit), span: body_span },
            ],
            span: body_span,
        };
        let body_expr = Expr { id, kind: ExprKind::Block(wrapped), span: body_span };
        let id = self.next_hir_id();
        let thread = Expr {
            id,
            kind: ExprKind::Thread { group: Some(group), body: Box::new(body_expr), item: true },
            span: body_span,
        };
        let inner = Block { stmts: vec![Stmt { kind: StmtKind::Expr(thread), span: body_span }], span: body_span };
        let kind = ExprKind::For { pat: Box::new(pat), iter, body: inner, is_const: false, is_loom: false };
        let for_expr = Expr { id: for_id, kind, span };
        let id = self.next_hir_id();
        let unit = Expr { id, kind: ExprKind::Tuple(Vec::new()), span };
        let block = Block {
            stmts: vec![Stmt { kind: StmtKind::Expr(for_expr), span }, Stmt { kind: StmtKind::Expr(unit), span }],
            span,
        };
        let id = self.next_hir_id();
        Expr { id, kind: ExprKind::Loom { args: Vec::new(), body: block, group }, span }
    }

    /// Whether a `break` or `continue` (with `label`) aims outside the loops
    /// of the body of a `loom for`.
    fn outside_loom_for(&self, label: Option<&Ident>) -> bool {
        match label {
            Some(l) => !self.fx.loops.iter().any(|f| f.label.as_deref() == Some(&l.name)),
            None => self.fx.loops.is_empty(),
        }
    }

    /// Something that would leave the iteration of a `loom for`: each one
    /// is a thread, which ends where its body does.
    fn leaves_iteration(&mut self, span: Span, what: &str) -> ExprKind {
        self.report(
            Diagnostic::error(codes::C0406, format!("`{what}` can't leave an iteration of a `loom for`"))
                .primary(span, "each iteration is a thread of its own")
                .help("end the iteration with `continue`, or handle it in the body"),
        );
        ExprKind::Error
    }

    fn require_shuttle(&mut self, span: Span, what: &str) {
        if !self.fx.is_shuttle {
            self.report(
                Diagnostic::error(codes::N0114, format!("`{what}` only works inside a shuttle"))
                    .primary(span, "not in a shuttle")
                    .help("a shuttle is a function declared with `~> T` (yields) and/or `<~ T` (pulls)"),
            );
        }
    }

    // ------------------------------------------------------------ blocks and statements

    pub(super) fn block_expr(&mut self, block: &ast::Block) -> Expr {
        let lowered = self.lower_block(block);
        self.expr(ExprKind::Block(lowered), block.span)
    }

    pub(super) fn lower_block(&mut self, block: &ast::Block) -> Block {
        self.push_rib(RibKind::Normal);
        // Items are visible in the whole block.
        let mut item_defs = Vec::new();
        for stmt in &block.stmts {
            if let ast::StmtKind::Item(item) = &stmt.kind {
                let module = self.module;
                let defs = self.collect_item(module, item);
                if let Some(def) = defs.def {
                    let name = self.p.def(def).name.clone();
                    if !name.is_empty() {
                        self.bind(&name, Res::Def(def));
                    }
                }
                item_defs.push(defs);
            }
        }
        let mut item_defs = item_defs.into_iter();
        let stmts = block
            .stmts
            .iter()
            .map(|stmt| {
                let kind = match &stmt.kind {
                    ast::StmtKind::Item(item) => {
                        let defs = item_defs.next().unwrap_or_default();
                        self.lower_item(item, &defs);
                        match defs.def {
                            Some(def) => StmtKind::Item(def),
                            None => StmtKind::Expr(self.error_expr(stmt.span)),
                        }
                    }
                    other => self.lower_stmt(other, stmt.span),
                };
                Stmt { kind, span: stmt.span }
            })
            .collect();
        self.pop_rib();
        Block { stmts, span: block.span }
    }

    fn lower_stmt(&mut self, stmt: &ast::StmtKind, span: Span) -> StmtKind {
        match stmt {
            ast::StmtKind::Let { mutable, pat, ty, init } => {
                let ty = ty.as_ref().map(|t| self.lower_ty(t));
                let init = self.lower_plain(init);
                let pat = self.lower_pat_binding(pat, LocalKind::Let, *mutable);
                StmtKind::Let { mutable: *mutable, pat: Box::new(pat), ty, init }
            }
            ast::StmtKind::Given { name, value } => {
                let value = self.lower_plain(value);
                let local = match name {
                    Some(n) => {
                        let local = self.new_local(&n.name, n.span, false, LocalKind::Given);
                        self.bind(&n.name, Res::Local(local));
                        local
                    }
                    None => self.new_local("given", value.span, false, LocalKind::Given),
                };
                StmtKind::Given { local: Some(local), value }
            }
            ast::StmtKind::Spool { name, init } => {
                let init = self.lower_plain(init);
                let local = self.new_local(&name.name, name.span, true, LocalKind::Spool);
                self.bind(&name.name, Res::Local(local));
                StmtKind::Spool { local, init }
            }
            ast::StmtKind::Defer { binding, body } => {
                let binding = binding.as_ref().and_then(|b| match self.lookup(&b.name) {
                    Some(Res::Local(local)) => Some(local),
                    _ => {
                        self.report(
                            Diagnostic::error(codes::N0101, format!("there's no local `{}` to defer on", b.name))
                                .primary(b.span, "unknown local")
                                .help("`defer x => …` names a local declared before it"),
                        );
                        None
                    }
                });
                StmtKind::Defer { binding, body: self.lower_plain(body) }
            }
            ast::StmtKind::Expr(e) => StmtKind::Expr(self.lower_plain(e)),
            ast::StmtKind::Item(_) => StmtKind::Expr(self.error_expr(span)),
        }
    }

    // ------------------------------------------------------------ patterns

    /// Lowers a pattern and binds its names in the current rib.
    fn lower_pat_binding(&mut self, pat: &ast::Pat, kind: LocalKind, mutable: bool) -> Pat {
        let mut binds = PatBinds { kind: Some(kind), mutable, ..PatBinds::default() };
        let lowered = self.lower_pat(pat, &mut binds);
        for (name, local) in binds.bound {
            self.bind(&name, Res::Local(local));
        }
        lowered
    }

    fn pat_local(&mut self, name: &Ident, mutable: bool, binds: &mut PatBinds) -> LocalId {
        if let Some((first, seen)) = &mut binds.reuse {
            seen.push(name.name.clone());
            if let Some(&local) = first.get(&name.name) {
                return local;
            }
            self.report(
                Diagnostic::error(codes::N0115, format!("`{}` isn't bound by the first alternative", name.name))
                    .primary(name.span, "only bound here")
                    .help("every alternative of an or-pattern must bind the same names"),
            );
        }
        let kind = binds.kind.unwrap_or(LocalKind::Pattern);
        let local = self.new_local(&name.name, name.span, mutable || binds.mutable, kind);
        binds.bound.insert(name.name.clone(), local);
        local
    }

    fn lower_pat(&mut self, pat: &ast::Pat, binds: &mut PatBinds) -> Pat {
        use ast::PatKind as P;
        let span = pat.span;
        let kind = match &pat.kind {
            P::Wild => PatKind::Wild,
            P::Bind { name, mutable } => PatKind::Bind(self.pat_local(name, *mutable, binds)),
            P::Lit { lit, negative } => PatKind::Lit { lit: lit.clone(), negative: *negative },
            P::Range { lo, hi, inclusive } => PatKind::Range {
                lo: lo.as_ref().map(|p| Box::new(self.lower_pat(p, binds))),
                hi: hi.as_ref().map(|p| Box::new(self.lower_pat(p, binds))),
                inclusive: *inclusive,
            },
            P::Tuple(pats) => PatKind::Tuple(pats.iter().map(|p| self.lower_pat(p, binds)).collect()),
            P::Slice(pats) => PatKind::Slice(pats.iter().map(|p| self.lower_pat(p, binds)).collect()),
            P::Rest(name) => PatKind::Rest(name.as_ref().map(|n| self.pat_local(n, false, binds))),
            P::Case { path, fields, rest } => {
                let path = match path {
                    ast::CasePath::Dot(name) => CasePath::Dot(name.clone()),
                    ast::CasePath::Path(segments) => self.case_path(segments, span),
                };
                let fields = fields.as_ref().map(|fields| {
                    fields
                        .iter()
                        .map(|f| FieldPat { label: f.label.clone(), pat: self.lower_pat(&f.pat, binds), span: f.span })
                        .collect()
                });
                PatKind::Case { path, fields, rest: *rest }
            }
            P::ConstCase(e) => PatKind::ConstCase(Box::new(self.lower_plain(e))),
            P::Some(p) => PatKind::Some(Box::new(self.lower_pat(p, binds))),
            P::None => PatKind::None,
            P::Ok(p) => PatKind::Ok(Box::new(self.lower_pat(p, binds))),
            P::Err(p) => PatKind::Err(Box::new(self.lower_pat(p, binds))),
            P::TypeTest { name, ty } => {
                let ty = self.lower_ty(ty);
                PatKind::TypeTest { local: self.pat_local(name, false, binds), ty }
            }
            P::Or(alts) => {
                let mut out = Vec::with_capacity(alts.len());
                let before: Vec<String> = binds.bound.keys().cloned().collect();
                let outer_reuse = binds.reuse.take();
                for (i, alt) in alts.iter().enumerate() {
                    if i == 0 {
                        out.push(self.lower_pat(alt, binds));
                        continue;
                    }
                    let first: IndexMap<String, LocalId> =
                        binds.bound.iter().filter(|(k, _)| !before.contains(k)).map(|(k, v)| (k.clone(), *v)).collect();
                    binds.reuse = Some((first.clone(), Vec::new()));
                    out.push(self.lower_pat(alt, binds));
                    let seen = binds.reuse.take().map(|(_, seen)| seen).unwrap_or_default();
                    for name in first.keys() {
                        if !seen.contains(name) {
                            self.report(
                                Diagnostic::error(codes::N0115, format!("`{name}` isn't bound by every alternative"))
                                    .primary(alt.span, format!("doesn't bind `{name}`"))
                                    .help("every alternative of an or-pattern must bind the same names"),
                            );
                        }
                    }
                }
                binds.reuse = outer_reuse;
                PatKind::Or(out)
            }
            P::Twill(pattern) => {
                let captures = pattern
                    .captures
                    .iter()
                    .map(|c| {
                        let ident = Ident { name: c.name.to_string(), span };
                        self.pat_local(&ident, false, binds)
                    })
                    .collect();
                PatKind::Twill { pattern: pattern.clone(), captures }
            }
            P::Error => PatKind::Error,
        };
        Pat { id: self.next_hir_id(), kind, span }
    }

    /// `MAX`, `Point(..)`, `Color.Red`, `ast.Expr.Lit` in a pattern.
    fn case_path(&mut self, segments: &[Ident], span: Span) -> CasePath {
        use super::items::PathRes;
        match self.resolve_path(segments) {
            PathRes::Type(ty) => CasePath::Type(ty),
            PathRes::Assoc(base, name) => CasePath::Assoc { base: Box::new(base), name },
            PathRes::Value(def) if matches!(self.p.def(def).kind, DefKind::Const(_) | DefKind::Pending) => {
                CasePath::Const(def)
            }
            PathRes::Error => CasePath::Type(Ty { kind: TyKind::Error, span }),
            _ => {
                self.error(
                    codes::N0111,
                    span,
                    "a pattern path names a type, a case or a constant",
                    "can't match against this",
                );
                CasePath::Type(Ty { kind: TyKind::Error, span })
            }
        }
    }
}

/// Whether `e` has holes that belong to it rather than to a nested argument.
/// A nested call's bare `_` argument belongs to the enclosing level; its
/// other arguments are their own level. Lambdas are their own level too.
fn has_own_holes(e: &ast::Expr) -> bool {
    use ast::ExprKind as E;
    match &e.kind {
        E::Hole => true,
        E::Lambda { .. } | E::Thread(_) => false,
        E::Call { callee, args } => has_own_holes(callee) || args.iter().any(|a| matches!(a.value.kind, E::Hole)),
        _ => {
            let mut found = false;
            for_each_child(e, &mut |child| found |= has_own_holes(child));
            found
        }
    }
}

/// Calls `f` on each direct subexpression of `e` (including those in blocks).
fn for_each_child(e: &ast::Expr, f: &mut dyn FnMut(&ast::Expr)) {
    use ast::ExprKind as E;
    let block = |b: &ast::Block, f: &mut dyn FnMut(&ast::Expr)| {
        for stmt in &b.stmts {
            match &stmt.kind {
                ast::StmtKind::Let { init, .. } => f(init),
                ast::StmtKind::Given { value, .. } => f(value),
                ast::StmtKind::Spool { init, .. } => f(init),
                ast::StmtKind::Defer { body, .. } => f(body),
                ast::StmtKind::Expr(e) => f(e),
                ast::StmtKind::Item(_) => {}
            }
        }
    };
    match &e.kind {
        E::Template(parts) => {
            for part in parts {
                if let ast::TemplatePart::Interp { expr, .. } = part {
                    f(expr);
                }
            }
        }
        E::Tuple(items) | E::Array(items) => {
            for item in items {
                f(item);
            }
        }
        E::ArrayRepeat { value, count } => {
            f(value);
            f(count);
        }
        E::Map(entries) => {
            for (k, v) in entries {
                f(k);
                f(v);
            }
        }
        E::Block(b) | E::Loop { body: b, .. } | E::Unsafe { body: b, .. } => block(b, f),
        E::If { cond, then, else_, .. } => {
            f(cond);
            block(then, f);
            if let Some(e) = else_ {
                f(e);
            }
        }
        E::Match { scrutinee, arms } => {
            f(scrutinee);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    f(g);
                }
                f(&arm.body);
            }
        }
        E::For { iter, body, .. } => {
            f(iter);
            block(body, f);
        }
        E::While { cond, body, .. } => {
            f(cond);
            block(body, f);
        }
        E::Loom { args, body } => {
            args.iter().for_each(|a| f(&a.value));
            block(body, f);
        }
        E::Break { value: Some(v), .. } | E::Return(Some(v)) => f(v),
        E::Fail(v) | E::Yield { value: v, .. } | E::Const(v) | E::Try(v) | E::Deref(v) => f(v),
        E::Call { callee, args } => {
            f(callee);
            args.iter().for_each(|a| f(&a.value));
        }
        E::Index { base, args } => {
            f(base);
            for arg in args {
                if let ast::GenericArg::Expr(e) = arg {
                    f(e);
                }
            }
        }
        E::Field { base, .. } => f(base),
        E::ConstField { base, field } => {
            f(base);
            f(field);
        }
        E::DotBlock { subject, body } => {
            f(subject);
            block(body, f);
        }
        E::Unary { operand, .. } => f(operand),
        E::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        E::Chain { first, rest } => {
            f(first);
            rest.iter().for_each(|(_, e)| f(e));
        }
        E::Is { expr, .. } | E::Cast { expr, .. } => f(expr),
        E::Range { lo, hi, .. } => {
            if let Some(lo) = lo {
                f(lo);
            }
            if let Some(hi) = hi {
                f(hi);
            }
        }
        E::Mend { expr, handler, .. } => {
            f(expr);
            f(handler);
        }
        E::Assign { target, value, .. } => {
            f(target);
            f(value);
        }
        _ => {}
    }
}
