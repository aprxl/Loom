//! Checking expressions and statements.

use loom_syntax::Diagnostic;
use loom_syntax::ast::{AssignOp, BinaryOp, CastKind, Lit, UnaryOp};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;
use loom_syntax::token::IntLit;

use super::infer::{PendingLit, VarKind};
use super::{Checker, Coercion, LoopCtx};
use crate::hir::{Block, DefKind, Expr, ExprKind, StmtKind, TemplatePart};
use crate::ty::{Ty, TyKind};

/// Whether `s` is a format spec: `[[fill]align][+][#][0][width][.precision][type]`,
/// as `rt.fmt` reads it.
fn valid_spec(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let align = |c: char| matches!(c, '<' | '>' | '^');
    let mut i = match chars.as_slice() {
        [_, a, ..] if align(*a) => 2,
        [a, ..] if align(*a) => 1,
        _ => 0,
    };
    for flag in ['+', '#', '0'] {
        if chars.get(i) == Some(&flag) {
            i += 1;
        }
    }
    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
    }
    if chars.get(i) == Some(&'.') {
        i += 1;
        while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    }
    if chars.get(i).is_some_and(|c| matches!(c, '?' | 'x' | 'X' | 'o' | 'b')) {
        i += 1;
    }
    i == chars.len()
}

/// What an operator needs from its operand type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operand {
    /// `+ - * / %`: integers and floats.
    Numeric,
    /// Overflow policies and bitwise operators: integers only.
    Integer,
}

impl Checker<'_> {
    pub(super) fn check_expr(&mut self, e: &Expr, expected: Option<Ty>) -> Ty {
        let ty = self.check_expr_inner(e, expected);
        self.b.expr_ty.insert(e.id, ty);
        ty
    }

    fn check_expr_inner(&mut self, e: &Expr, expected: Option<Ty>) -> Ty {
        let span = e.span;
        match &e.kind {
            ExprKind::Lit(lit) => self.check_lit(lit, false, span, expected),
            ExprKind::Template(parts) => {
                for part in parts {
                    if let TemplatePart::Interp { expr, spec } = part {
                        let ty = self.check_expr(expr, None);
                        if let Some(spec) = spec.as_deref()
                            && !valid_spec(spec)
                        {
                            self.diags.push(
                                Diagnostic::error(codes::T0117, format!("`{spec}` isn't a format spec"))
                                    .primary(expr.span, "what's after the `:` is read as a format spec")
                                    .help(
                                        "a `:` inside `{…}` starts one: wrap the expression in parentheses to use it",
                                    ),
                            );
                        }
                        // Checked once the body's types are known.
                        let debug = spec.as_deref().is_some_and(|s| s.contains('?'));
                        if !debug {
                            self.pending_shows.push((ty, expr.span));
                        }
                    }
                }
                self.string_ty()
            }
            ExprKind::Local(local) => self.b.local_ty.get(local).copied().unwrap_or(self.tcx.error),
            ExprKind::Def { def, args } => {
                if !args.is_empty() {
                    return self.unsupported(span, "explicit generic arguments");
                }
                match &self.p.def(*def).kind {
                    DefKind::Const(_) => {
                        let ty = self.const_ty(*def);
                        match self.r.untyped_consts.get(def) {
                            Some(&value) => self.int_value_ty(value, span, expected),
                            None => ty,
                        }
                    }
                    DefKind::Fn(_) => self.fn_value(e, *def),
                    DefKind::Case(_) => self.unsupported(span, "cases"),
                    _ => self.unsupported(span, "this kind of value"),
                }
            }
            ExprKind::Type(_) => self.unsupported(span, "types used as values"),
            ExprKind::Assoc { base, name } => {
                let ty = self.lower_ty(base);
                self.check_assoc_value(e, ty, &name.name, name.span)
            }
            ExprKind::BuiltinCase(_) => self.unsupported(span, "`some`, `ok` and `err`"),
            ExprKind::Intrinsic(_) => self.unsupported(span, "`target` and `heap`"),
            ExprKind::LeadingDot(field) => self.check_leading_value(e, field, expected),
            ExprKind::Tuple(items) => self.check_tuple(items, expected),
            ExprKind::Array(items) => self.check_array_lit(e, items, expected),
            ExprKind::ArrayRepeat { value, count } => self.check_array_repeat(value, count, expected),
            ExprKind::Map(pairs) => self.check_map_lit(e, pairs, expected),
            ExprKind::Block(block) => self.check_block(block, expected),
            ExprKind::If { cond, then, else_, deferred } => {
                if deferred.is_some() {
                    return self.unsupported(span, "`const if`");
                }
                self.check_coerce(cond, self.tcx.bool);
                let narrowed = self.narrowings(cond);
                self.check_if(then, else_.as_deref(), expected, &narrowed)
            }
            ExprKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms, span, expected),
            ExprKind::For { pat, iter, body, is_const, is_loom } => {
                if *is_const {
                    return self.unsupported(span, "`const for`");
                }
                if *is_loom {
                    return self.unsupported(span, "`loom for`");
                }
                let iter_ty = self.check_expr(iter, None);
                let iter_ty = self.resolve(iter_ty);
                let elem = match self.tcx.kind(iter_ty) {
                    TyKind::Range(elem) => *elem,
                    TyKind::Error => self.tcx.error,
                    _ if self.seq_elem(iter_ty).is_some() => {
                        // `for mut x in xs` changes the elements themselves.
                        if let crate::hir::PatKind::Bind(local) = &pat.kind
                            && self.p.local(*local).mutable
                        {
                            self.check_for_mut(iter, pat.span);
                        }
                        self.seq_elem(iter_ty).unwrap_or(self.tcx.error)
                    }
                    _ => match self.for_items(e, iter_ty) {
                        Some(item) => {
                            if let crate::hir::PatKind::Bind(local) = &pat.kind
                                && self.p.local(*local).mutable
                            {
                                self.diags.push(
                                    Diagnostic::error(codes::T0107, "`for mut` needs elements to change")
                                        .primary(pat.span, "a stream's items aren't stored anywhere")
                                        .help("`for mut` works on arrays, lists and slices"),
                                );
                            }
                            item
                        }
                        None => {
                            let shown = self.show(iter_ty);
                            self.diags.push(
                                Diagnostic::error(codes::T0108, format!("can't loop over `{shown}`"))
                                    .primary(iter.span, format!("`{shown}` has no items"))
                                    .help("`for` loops over arrays, lists, slices, ranges, streams and types that weave `Iter`"),
                            );
                            self.tcx.error
                        }
                    },
                };
                self.check_pat_irrefutable(pat, elem, "a `for` loop");
                self.in_loop(e.id, false, None, |this| this.check_block(body, Some(this.tcx.unit)));
                self.tcx.unit
            }
            ExprKind::While { cond, body } => {
                self.check_coerce(cond, self.tcx.bool);
                self.in_loop(e.id, false, None, |this| this.check_block(body, Some(this.tcx.unit)));
                self.tcx.unit
            }
            ExprKind::Loop { body } => {
                let ctx = self.in_loop(e.id, true, expected, |this| this.check_block(body, Some(this.tcx.unit)));
                match (ctx.expected, ctx.break_ty) {
                    (Some(t), Some(_)) => t,
                    (_, Some(t)) => t,
                    (_, None) => self.tcx.never,
                }
            }
            ExprKind::Break { target, value } => {
                self.check_break(*target, value.as_deref(), span);
                self.tcx.never
            }
            ExprKind::Continue { target } => {
                if self.in_defer.is_some() && !self.loops.iter().any(|l| l.id == *target) {
                    self.leaves_defer(span, "continue");
                }
                self.tcx.never
            }
            ExprKind::Return(_) | ExprKind::Fail(_) if self.in_defer.is_some() => {
                let (inner, what) = match &e.kind {
                    ExprKind::Return(v) => (v.as_deref(), "return"),
                    ExprKind::Fail(v) => (Some(v.as_ref()), "fail"),
                    _ => unreachable!(),
                };
                // Already an error: nothing says what the value should be.
                if let Some(v) = inner {
                    self.check_expr(v, Some(self.tcx.error));
                }
                self.leaves_defer(span, what);
                self.tcx.never
            }
            ExprKind::Return(value) => {
                let ret = self.ret.unwrap_or(self.tcx.unit);
                match value {
                    Some(v) => {
                        self.check_coerce(v, ret);
                    }
                    None if ret != self.tcx.unit && !self.tcx.is_error(ret) => {
                        let shown = self.show(ret);
                        self.error(
                            codes::T0101,
                            span,
                            format!("this function returns `{shown}`, so `return` needs a value"),
                            "no value",
                        );
                    }
                    None => {}
                }
                self.tcx.never
            }
            ExprKind::Fail(value) => self.check_fail(e, value),
            ExprKind::Yield { place: Some(_), .. } => self.unsupported(span, "projections (`yield mut`, `yield ref`)"),
            ExprKind::Yield { place: None, value } => self.check_yield(value, span),
            ExprKind::Pull => self.check_pull(span),
            ExprKind::Unsafe { .. } => self.unsupported(span, "`unsafe` blocks"),
            ExprKind::Const(_) => self.unsupported(span, "compile-time code"),
            ExprKind::Lambda { params, body } => self.check_lambda(e, params, body, expected),
            ExprKind::Loom { .. } | ExprKind::Thread(_) => self.unsupported(span, "`loom` and `thread`"),
            ExprKind::Call { callee, args } => self.check_call(e, callee, args, expected),
            ExprKind::Index { base, args } => self.check_index(e, base, args),
            ExprKind::Field { base, field } => self.check_field(e, base, field),
            ExprKind::ConstField { .. } => self.unsupported(span, "compile-time field access"),
            ExprKind::Try(inner) => self.check_try(e, inner),
            ExprKind::Mend { expr, binding, handler } => self.check_mend(e, expr, *binding, handler),
            ExprKind::Deref(_) => self.unsupported(span, "spool pointers"),
            ExprKind::DotBlock { subject, body } => {
                // A changeable place is edited in place, and the block's
                // value is `()`. Anything else is edited as a copy, which is
                // the block's value.
                let ty = self.check_expr(subject, None);
                let in_place = self.place(subject) == Some(true);
                self.b.dot_blocks.insert(e.id, in_place);
                self.subjects.push((e.id, ty));
                self.check_block(body, Some(self.tcx.unit));
                self.subjects.pop();
                if in_place { self.tcx.unit } else { ty }
            }
            ExprKind::Is { expr, pat } => {
                let ty = self.check_expr(expr, None);
                self.check_pat(pat, ty);
                self.tcx.bool
            }
            ExprKind::Unary { op, operand } => self.check_unary(*op, operand, span, expected),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, span, expected),
            ExprKind::Chain { first, rest } => {
                let mut operands = vec![first.as_ref()];
                operands.extend(rest.iter().map(|(_, e)| e));
                let ty = self.unify_operands(&operands);
                for (op, _) in rest {
                    self.require_comparable(*op, ty, span);
                }
                self.tcx.bool
            }
            ExprKind::Cast { expr, kind, ty } => self.check_cast(expr, *kind, ty, span),
            ExprKind::Range { lo, hi, .. } => {
                let operands: Vec<&Expr> = lo.iter().chain(hi.iter()).map(|e| e.as_ref()).collect();
                if lo.is_none() {
                    return self.unsupported(span, "ranges without a start");
                }
                let elem = self.unify_operands(&operands);
                if !matches!(self.tcx.kind(elem), TyKind::Int(_) | TyKind::IntVar(_) | TyKind::Char | TyKind::Error) {
                    let shown = self.show(elem);
                    self.error(codes::T0103, span, format!("a range needs integers or chars, not `{shown}`"), "here");
                    return self.tcx.error;
                }
                self.tcx.intern(TyKind::Range(elem))
            }
            ExprKind::Assign { target, op, value } => {
                self.check_assign(target, *op, value);
                self.tcx.unit
            }
            ExprKind::Error => self.tcx.error,
        }
    }

    // ------------------------------------------------------------ literals

    /// A literal, negated when `negative` (for `-128i8`, which is in range
    /// only as a whole).
    fn check_lit(&mut self, lit: &Lit, negative: bool, span: Span, expected: Option<Ty>) -> Ty {
        // `let x: ?u8 = 5`: the literal takes the inner type.
        let expected = expected.map(|t| self.resolve(t)).map(|t| match self.tcx.kind(t) {
            TyKind::Optional(inner) => self.resolve(*inner),
            _ => t,
        });
        match lit {
            Lit::Int(i) => self.check_int_lit(i, negative, span, expected),
            Lit::Float(f) => match f.suffix {
                Some(p) => self.tcx.prim(p).unwrap_or_else(|| self.unsupported(span, "`f16`")),
                // A literal for a float gauge must be in it.
                None if expected
                    .is_some_and(|t| matches!(self.tcx.kind(t), TyKind::Gauge(_, b) if matches!(self.tcx.kind(*b), TyKind::Float(_)))) =>
                {
                    let ty = expected.expect("checked");
                    let value = if negative { -f.value } else { f.value };
                    self.check_into_gauge(ty, super::gauge::Val::Float(value), span);
                    ty
                }
                None => match expected.map(|t| self.tcx.kind(t).clone()) {
                    Some(TyKind::Float(_) | TyKind::FloatVar(_)) => expected.unwrap_or(self.tcx.f64),
                    _ => self.new_var(VarKind::Float),
                },
            },
            Lit::Char(_) => self.tcx.char,
            Lit::Str(_) => self.tcx.str,
            Lit::Bool(_) => self.tcx.bool,
            Lit::Bytes(_) => self.unsupported(span, "byte strings"),
            Lit::None => match expected.map(|t| self.tcx.kind(t).clone()) {
                // `expected` was unwrapped above; `none` needs the optional back.
                Some(_) => match expected {
                    Some(inner) => self.tcx.optional(inner),
                    None => self.tcx.error,
                },
                None => {
                    self.error(
                        codes::T0110,
                        span,
                        "can't tell what type this `none` has",
                        "add a type annotation, like `let x: ?i32 = none`",
                    );
                    self.tcx.error
                }
            },
        }
    }

    fn check_int_lit(&mut self, lit: &IntLit, negative: bool, span: Span, expected: Option<Ty>) -> Ty {
        let value = if negative { -(lit.value as i128) } else { lit.value as i128 };
        match lit.suffix {
            Some(p) => match self.tcx.prim(p) {
                Some(ty) => {
                    self.check_int_range(value, ty, span);
                    ty
                }
                None => self.unsupported(span, "`f16` and integers wider than 64 bits"),
            },
            None => self.int_value_ty(value, span, expected),
        }
    }

    /// The type of an integer value with no suffix: the expected numeric
    /// type, or a fresh `{integer}`. The range is checked once it's known.
    fn int_value_ty(&mut self, value: i128, span: Span, expected: Option<Ty>) -> Ty {
        let expected = expected.map(|t| self.resolve(t)).map(|t| match self.tcx.kind(t) {
            TyKind::Optional(inner) => self.resolve(*inner),
            _ => t,
        });
        let ty = match expected.map(|t| self.tcx.kind(t).clone()) {
            // An integer literal can be a float: `let x: f64 = 0`.
            Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::IntVar(_) | TyKind::FloatVar(_)) => {
                expected.unwrap_or(self.tcx.i32)
            }
            // One for a gauge must fit its base, and be in it.
            Some(TyKind::Gauge(_, base)) => {
                self.check_int_range(value, base, span);
                let ty = expected.expect("matched");
                self.check_into_gauge(ty, super::gauge::Val::Int(value), span);
                return ty;
            }
            _ => self.new_var(VarKind::Int),
        };
        self.check_int_range(value, ty, span);
        ty
    }

    fn check_int_range(&mut self, value: i128, ty: Ty, span: Span) {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Int(int) if !int.contains(value) => {
                let int = *int;
                let shown = self.show(ty);
                self.diags.push(
                    Diagnostic::error(codes::T0102, format!("`{value}` doesn't fit in `{shown}`"))
                        .primary(span, format!("`{shown}` holds {} through {}", int.min(), int.max()))
                        .help("use a wider type, or say what should happen: `as%` wraps, `as|` clamps"),
                );
            }
            TyKind::IntVar(_) => self.pending_lits.push(PendingLit { ty, value, span }),
            _ => {}
        }
    }

    fn check_tuple(&mut self, items: &[Expr], expected: Option<Ty>) -> Ty {
        if items.is_empty() {
            return self.tcx.unit;
        }
        let expected_items = match expected.map(|t| self.resolve(t)).map(|t| self.tcx.kind(t).clone()) {
            Some(TyKind::Tuple(ts)) if ts.len() == items.len() => Some(ts),
            _ => None,
        };
        let tys = items
            .iter()
            .enumerate()
            .map(|(i, item)| match &expected_items {
                Some(ts) => self.check_coerce(item, ts[i]),
                None => self.check_expr(item, None),
            })
            .collect();
        self.tcx.intern(TyKind::Tuple(tys))
    }

    // ------------------------------------------------------------ blocks and statements

    pub(super) fn check_block(&mut self, block: &Block, expected: Option<Ty>) -> Ty {
        let saved = std::mem::replace(&mut self.in_argument, false);
        let ty = self.check_block_inner(block, expected);
        self.in_argument = saved;
        ty
    }

    fn check_block_inner(&mut self, block: &Block, expected: Option<Ty>) -> Ty {
        let Some((last, rest)) = block.stmts.split_last() else {
            return self.expect_unit_tail(block.span, expected);
        };
        for stmt in rest {
            self.check_stmt(&stmt.kind);
        }
        match &last.kind {
            StmtKind::Expr(tail) if !matches!(tail.kind, ExprKind::Assign { .. }) => match expected {
                // The value is thrown away where `()` is expected.
                Some(t) if t == self.tcx.unit => {
                    let ty = self.check_expr(tail, None);
                    if ty == self.tcx.never {
                        return ty;
                    }
                    if ty != self.tcx.unit && !self.tcx.is_error(ty) {
                        self.b.coercions.insert(tail.id, Coercion::Discard);
                    }
                    self.tcx.unit
                }
                // `() ! E` takes a result, and throws away any other value.
                Some(t) if matches!(self.tcx.kind(self.resolve(t)), TyKind::Result(ok, _) if *ok == self.tcx.unit) => {
                    let ty = self.check_expr(tail, Some(t));
                    let r = self.resolve(ty);
                    if matches!(self.tcx.kind(r), TyKind::Result(..)) || r == self.tcx.unit || self.tcx.is_lenient(r) {
                        self.coerce(tail, ty, t);
                        t
                    } else {
                        self.b.coercions.insert(tail.id, Coercion::Discard);
                        self.tcx.unit
                    }
                }
                Some(t) => self.check_coerce(tail, t),
                None => self.check_expr(tail, None),
            },
            other => {
                self.check_stmt(other);
                let diverges = matches!(other, StmtKind::Expr(e) if self.b.expr_ty.get(&e.id) == Some(&self.tcx.never));
                if diverges { self.tcx.never } else { self.expect_unit_tail(block.span, expected) }
            }
        }
    }

    /// A block with no value: fine where `()` fits, including `() ! E`.
    /// Otherwise it's reported here, and the error type keeps the caller
    /// from reporting it again.
    fn expect_unit_tail(&mut self, span: Span, expected: Option<Ty>) -> Ty {
        let Some(t) = expected else { return self.tcx.unit };
        let t = self.resolve(t);
        let unit_fits = match self.tcx.kind(t) {
            TyKind::Unit | TyKind::Error | TyKind::Never => true,
            TyKind::Result(ok, _) | TyKind::Optional(ok) => *ok == self.tcx.unit,
            _ => false,
        };
        if unit_fits {
            return self.tcx.unit;
        }
        let shown = self.show(t);
        let end = Span::new(span.file, span.hi.saturating_sub(1), span.hi);
        self.diags.push(
            Diagnostic::error(codes::T0101, format!("expected `{shown}`, but this block has no value"))
                .primary(end, format!("a `{shown}` should be the last thing here")),
        );
        self.tcx.error
    }

    fn check_stmt(&mut self, stmt: &StmtKind) {
        match stmt {
            StmtKind::Let { pat, ty, init, .. } => {
                let ty = match (ty, &pat.kind) {
                    (Some(t), _) => {
                        let ty = self.lower_ty(t);
                        self.check_coerce(init, ty)
                    }
                    // A twill pattern is as wide as its digits, and that's the
                    // type the value is computed in.
                    (None, crate::hir::PatKind::Twill { pattern, .. }) if pattern.width <= 64 => {
                        let ty =
                            self.tcx.int(crate::ty::IntTy { signed: false, bits: pattern.width as u8, size: false });
                        self.check_coerce(init, ty)
                    }
                    (None, _) => self.check_expr(init, None),
                };
                self.check_pat_irrefutable(pat, ty, "a `let`");
            }
            StmtKind::Expr(e) => {
                self.check_expr(e, None);
            }
            StmtKind::Item(def) => {
                self.queue_def(*def);
            }
            StmtKind::Given { value, .. } => {
                self.unsupported(value.span, "`given`");
            }
            StmtKind::Spool { init, .. } => {
                self.unsupported(init.span, "spools");
            }
            // The body runs when the scope ends, however it ends. It can't
            // leave: loops and `mend`s outside it are out of reach.
            StmtKind::Defer { body, .. } => {
                let saved_loops = std::mem::take(&mut self.loops);
                let saved_mends = std::mem::take(&mut self.mends);
                let saved_defer = self.in_defer.replace(body.span);
                self.check_expr(body, None);
                self.in_defer = saved_defer;
                self.mends = saved_mends;
                self.loops = saved_loops;
            }
        }
    }

    fn check_if(
        &mut self,
        then: &Block,
        else_: Option<&Expr>,
        expected: Option<Ty>,
        narrowed: &[(crate::hir::LocalId, Ty)],
    ) -> Ty {
        let Some(else_) = else_ else {
            // Without `else`, the `then` value is thrown away.
            self.with_narrowed(narrowed, |this| this.check_block(then, Some(this.tcx.unit)));
            return self.tcx.unit;
        };
        if let Some(t) = expected {
            self.with_narrowed(narrowed, |this| this.check_block(then, Some(t)));
            self.check_coerce(else_, t);
            return t;
        }
        let then_ty = self.with_narrowed(narrowed, |this| this.check_block(then, None));
        if then_ty == self.tcx.never {
            return self.check_expr(else_, None);
        }
        let else_ty = self.check_expr(else_, Some(then_ty));
        if else_ty == self.tcx.never {
            return then_ty;
        }
        self.coerce(else_, else_ty, then_ty);
        then_ty
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[crate::hir::Arm], span: Span, expected: Option<Ty>) -> Ty {
        let scrutinee_ty = self.check_expr(scrutinee, None);
        let mut result = expected;
        let mut all_diverge = true;
        for arm in arms {
            self.check_pat(&arm.pat, scrutinee_ty);
            if let Some(guard) = &arm.guard {
                self.check_coerce(guard, self.tcx.bool);
            }
            let ty = match result {
                Some(t) => self.check_coerce(&arm.body, t),
                None => {
                    let ty = self.check_expr(&arm.body, None);
                    if ty != self.tcx.never {
                        result = Some(ty);
                    }
                    ty
                }
            };
            if ty != self.tcx.never {
                all_diverge = false;
            }
        }
        self.check_exhaustive(arms, scrutinee_ty, span);
        match result {
            Some(t) if !all_diverge || expected.is_some() => t,
            _ => self.tcx.never,
        }
    }

    // ------------------------------------------------------------ loops

    /// Reports `what` (`return`, `?`…) trying to leave a `defer`'s body.
    pub(super) fn leaves_defer(&mut self, span: Span, what: &str) {
        let at = self.in_defer.expect("inside a defer");
        self.diags.push(
            Diagnostic::error(codes::T0118, format!("`{what}` can't leave a `defer`"))
                .primary(span, "this would leave the deferred code")
                .secondary(at, "it runs when the scope ends, from here")
                .note("a `defer` runs while its scope is already ending, so it can't change where that goes")
                .help("handle it inside: `mend`, or an `if`"),
        );
    }

    fn in_loop(
        &mut self,
        id: crate::hir::HirId,
        takes_value: bool,
        expected: Option<Ty>,
        f: impl FnOnce(&mut Self) -> Ty,
    ) -> LoopCtx {
        self.loops.push(LoopCtx { id, takes_value, expected, break_ty: None });
        f(self);
        self.loops.pop().expect("pushed above")
    }

    fn check_break(&mut self, target: crate::hir::HirId, value: Option<&Expr>, span: Span) {
        let Some(index) = self.loops.iter().rposition(|l| l.id == target) else {
            if let Some(v) = value {
                self.check_expr(v, None);
            }
            if self.in_defer.is_some() {
                self.leaves_defer(span, "break");
            }
            return;
        };
        let (takes_value, expected, break_ty) = {
            let ctx = &self.loops[index];
            (ctx.takes_value, ctx.expected, ctx.break_ty)
        };
        match value {
            Some(v) if !takes_value => {
                self.error(
                    codes::T0111,
                    v.span,
                    "only `loop` can `break` with a value",
                    "`while` and `for` loops have no value",
                );
                self.check_expr(v, None);
            }
            Some(v) => {
                let ty = match expected.or(break_ty) {
                    Some(t) => self.check_coerce(v, t),
                    None => self.check_expr(v, None),
                };
                if self.loops[index].break_ty.is_none() && ty != self.tcx.never {
                    self.loops[index].break_ty = Some(ty);
                }
            }
            None if takes_value => {
                match expected.or(break_ty) {
                    Some(t) if t != self.tcx.unit && !self.tcx.is_lenient(t) => {
                        let shown = self.show(t);
                        self.error(codes::T0101, span, format!("this `loop` needs a `{shown}` value here"), "no value");
                    }
                    _ => {}
                }
                if self.loops[index].break_ty.is_none() {
                    self.loops[index].break_ty = Some(self.tcx.unit);
                }
            }
            None => {}
        }
    }

    // ------------------------------------------------------------ calls

    // ------------------------------------------------------------ operators

    fn check_unary(&mut self, op: UnaryOp, operand: &Expr, span: Span, expected: Option<Ty>) -> Ty {
        match op {
            UnaryOp::Neg => {
                // `-128i8` is checked as one literal.
                let ty = match &operand.kind {
                    ExprKind::Lit(lit @ (Lit::Int(_) | Lit::Float(_))) => {
                        let ty = self.check_lit(lit, true, operand.span, expected);
                        self.b.expr_ty.insert(operand.id, ty);
                        ty
                    }
                    _ => {
                        let expected = expected.map(|t| self.tcx.degauge(self.resolve(t)));
                        self.check_expr(operand, expected)
                    }
                };
                let literal = matches!(operand.kind, ExprKind::Lit(_));
                // A negative literal can be a gauge's value; negating a
                // gauge's value gives its base type.
                let ty = self.resolve(ty);
                if literal && matches!(self.tcx.kind(ty), TyKind::Gauge(..)) {
                    return ty;
                }
                let ty = self.tcx.degauge(ty);
                match self.tcx.kind(self.resolve(ty)) {
                    TyKind::Int(i) if i.signed => ty,
                    // `-1` as a `u8` was already reported as out of range.
                    TyKind::Int(_) if literal => ty,
                    TyKind::Float(_) | TyKind::FloatVar(_) | TyKind::Error | TyKind::Never => ty,
                    TyKind::IntVar(_) => {
                        // A literal's sign was checked with its range; anything
                        // else must turn out signed.
                        if !literal {
                            self.pending_negations.push((ty, span));
                        }
                        ty
                    }
                    _ => {
                        let shown = self.show(ty);
                        self.error(
                            codes::T0103,
                            span,
                            format!("can't negate a `{shown}`"),
                            "only signed integers and floats have `-`",
                        );
                        self.tcx.error
                    }
                }
            }
            UnaryOp::BitNot => {
                let expected = expected.map(|t| self.tcx.degauge(self.resolve(t)));
                let ty = self.check_expr(operand, expected);
                let ty = self.tcx.degauge(self.resolve(ty));
                self.require_operand(ty, Operand::Integer, "~", span)
            }
            UnaryOp::Not => self.check_coerce(operand, self.tcx.bool),
            UnaryOp::Alloc => self.unsupported(span, "`^` allocation"),
        }
    }

    fn check_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span, expected: Option<Ty>) -> Ty {
        use BinaryOp as B;
        match op {
            B::And | B::Or => {
                self.check_coerce(lhs, self.tcx.bool);
                // `n is Cents and n > 5`: the right side sees `n` as a `Cents`.
                let narrowed = if op == B::And { self.narrowings(lhs) } else { Vec::new() };
                self.with_narrowed(&narrowed, |this| this.check_coerce(rhs, this.tcx.bool));
                self.tcx.bool
            }
            B::Eq | B::Ne | B::Lt | B::Le | B::Gt | B::Ge => {
                let ty = self.unify_operands(&[lhs, rhs]);
                self.require_comparable(op, ty, span);
                self.tcx.bool
            }
            B::Shl | B::Shr => {
                // The amount can be any integer type; the result has the left side's type.
                let expected = expected.map(|t| self.tcx.degauge(self.resolve(t)));
                let ty = self.check_arith_operand(lhs, expected);
                let ty = self.tcx.degauge(self.resolve(ty));
                let amount = self.check_expr(rhs, None);
                self.require_operand(amount, Operand::Integer, op.as_str(), rhs.span);
                self.require_operand(ty, Operand::Integer, op.as_str(), span)
            }
            _ => {
                let need = match op {
                    B::Add | B::Sub | B::Mul | B::Div | B::Rem => Operand::Numeric,
                    _ => Operand::Integer,
                };
                // Gauges are worked with in their base type; the result is
                // a gauge again when its range still holds every result.
                let expected = expected.map(|t| self.tcx.degauge(self.resolve(t)));
                // With an expected numeric type, both operands widen to it
                // first: `let op: u16 = hi << 8 | lo`.
                // An operand of a wider integer type widens the whole
                // instead; whoever expected the narrower type then says so.
                let ty = match expected.filter(|&t| self.is_numeric(t)) {
                    Some(t) if !matches!(op, B::AddChecked | B::SubChecked | B::MulChecked) => {
                        let a = self.check_expr(lhs, Some(t));
                        let b = self.check_expr(rhs, Some(t));
                        let mut target = self.resolve(t);
                        for ty in [a, b] {
                            let ty = self.resolve(ty);
                            if let (Some(x), Some(y)) = (self.tcx.as_int(target), self.tcx.as_int(ty))
                                && x != y
                                && x.widens_to(y)
                            {
                                target = ty;
                            }
                        }
                        self.coerce(lhs, a, target);
                        self.coerce(rhs, b, target);
                        target
                    }
                    _ => self.unify_operands(&[lhs, rhs]),
                };
                let ty = self.require_operand(ty, need, op.as_str(), span);
                if matches!(op, B::AddChecked | B::SubChecked | B::MulChecked) && !self.tcx.is_error(ty) {
                    return self.tcx.optional(ty);
                }
                self.gauge_arith(op, lhs, rhs, self.resolve(ty)).unwrap_or(ty)
            }
        }
    }

    /// A shift's left side: widened to the expected type, if there is one.
    fn check_arith_operand(&mut self, e: &Expr, expected: Option<Ty>) -> Ty {
        match expected.filter(|&t| self.is_numeric(t)) {
            Some(t) => self.check_coerce(e, t),
            None => self.check_expr(e, None),
        }
    }

    fn is_numeric(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(self.resolve(ty)),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::IntVar(_) | TyKind::FloatVar(_)
        )
    }

    /// Checks operands that must end up with one type. Open literal types
    /// take the type of the typed operands, and a narrower integer widens to
    /// a wider one: `u8_value < 1000u16` compares as `u16`.
    fn unify_operands(&mut self, operands: &[&Expr]) -> Ty {
        // A leading dot needs a type to look in: `light == .Red` finds
        // `.Red` in the other operand's type, so it's checked last. So does
        // `none`, which needs to know which optional it is.
        // Arithmetic is checked last too, in the other operands' type:
        // `xs.len() > 4096 - START` subtracts in `usize`.
        let leading = |e: &Expr| match &e.kind {
            ExprKind::LeadingDot(_) | ExprKind::Lit(Lit::None) => true,
            // `some(none)`, `ok(.Red)`, `[]`: their type comes from the others.
            ExprKind::Call { callee, .. } => matches!(callee.kind, ExprKind::LeadingDot(_) | ExprKind::BuiltinCase(_)),
            ExprKind::Array(items) => items.is_empty(),
            ExprKind::Binary { op, .. } => matches!(
                op,
                BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Rem
                    | BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr
            ),
            _ => false,
        };
        // A gauge is compared (and worked with) as its base type.
        let mut tys: Vec<Option<Ty>> = operands
            .iter()
            .map(|e| {
                (!leading(e)).then(|| {
                    let ty = self.check_expr(e, None);
                    self.tcx.degauge(self.resolve(ty))
                })
            })
            .collect();
        let hint = tys.iter().flatten().next().copied();
        for (e, ty) in operands.iter().zip(tys.iter_mut()) {
            if ty.is_none() {
                let t = self.check_expr(e, hint);
                *ty = Some(self.tcx.degauge(self.resolve(t)));
            }
        }
        let tys: Vec<Ty> = tys.into_iter().flatten().collect();
        let Some(&first) = tys.first() else { return self.tcx.error };
        let mut target = first;
        for &ty in &tys[1..] {
            if self.unify(target, ty) {
                continue;
            }
            let (a, b) = (self.resolve(target), self.resolve(ty));
            if let (Some(x), Some(y)) = (self.tcx.as_int(a), self.tcx.as_int(b))
                && x.widens_to(y)
            {
                target = b;
            }
        }
        for (e, &ty) in operands.iter().zip(&tys) {
            self.coerce(e, ty, target);
        }
        self.resolve(target)
    }

    fn require_operand(&mut self, ty: Ty, need: Operand, op: &str, span: Span) -> Ty {
        let ok = match self.tcx.kind(self.resolve(ty)) {
            TyKind::Int(_) | TyKind::IntVar(_) | TyKind::Error | TyKind::Never => true,
            TyKind::Float(_) | TyKind::FloatVar(_) => need == Operand::Numeric,
            _ => false,
        };
        if ok {
            return ty;
        }
        let shown = self.show(ty);
        let what = if need == Operand::Integer { "integers" } else { "numbers" };
        self.error(codes::T0103, span, format!("`{op}` doesn't work on `{shown}`"), format!("`{op}` works on {what}"));
        self.tcx.error
    }

    fn require_comparable(&mut self, op: BinaryOp, ty: Ty, span: Span) {
        if self.tcx.is_lenient(ty) {
            return;
        }
        let ordered = !matches!(op, BinaryOp::Eq | BinaryOp::Ne);
        let weft = if ordered { self.std.ord } else { self.std.eq };
        if weft.is_some_and(|w| self.weaves(ty, w)) {
            return;
        }
        let shown = self.show(ty);
        let what = if ordered { "ordered" } else { "compared" };
        self.error(codes::T0103, span, format!("`{shown}` can't be {what} with `{}`", op.as_str()), "here");
    }

    fn check_cast(&mut self, e: &Expr, kind: CastKind, ty: &crate::hir::Ty, span: Span) -> Ty {
        let target = self.lower_ty(ty);
        let source = self.check_expr(e, None);
        let source = self.tcx.degauge(self.resolve(source));
        if self.tcx.is_lenient(source) || self.tcx.is_error(target) {
            return self.tcx.error;
        }
        // Into a gauge: converted to its base, then checked.
        if let TyKind::Gauge(def, _) = *self.tcx.kind(target) {
            return self.check_gauge_cast(kind, source, target, def, span);
        }
        use TyKind as T;
        let allowed = match (self.tcx.kind(source), self.tcx.kind(target)) {
            (T::Int(_) | T::IntVar(_), T::Int(_)) => true,
            (T::Int(_) | T::IntVar(_), T::Float(_)) => kind == CastKind::Checked,
            (T::Float(_) | T::FloatVar(_), T::Int(_)) => {
                matches!(kind, CastKind::Checked | CastKind::Sat | CastKind::Opt)
            }
            (T::Float(_) | T::FloatVar(_), T::Float(_)) => kind == CastKind::Checked,
            (T::Char | T::Bool, T::Int(_)) => kind == CastKind::Checked,
            (T::Int(_) | T::IntVar(_), T::Char) => matches!(kind, CastKind::Checked | CastKind::Opt),
            _ => false,
        };
        if !allowed {
            let (from, to) = (self.show(source), self.show(target));
            self.error(
                codes::T0109,
                span,
                format!("can't convert `{from}` to `{to}` with `{}`", kind.as_str()),
                "not allowed",
            );
            return self.tcx.error;
        }
        if kind == CastKind::Opt { self.tcx.optional(target) } else { target }
    }

    fn check_assign(&mut self, target: &Expr, op: AssignOp, value: &Expr) {
        if let (AssignOp::Compound(_), ExprKind::Tuple(_)) = (op, &target.kind) {
            self.error(codes::T0107, target.span, "`op=` needs a single variable", "a tuple here");
            return;
        }
        let Some(ty) = self.check_place(target) else {
            self.check_expr(value, None);
            return;
        };
        match op {
            AssignOp::Assign => {
                self.check_coerce(value, ty);
            }
            AssignOp::Compound(bin) => {
                let need = match bin {
                    BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => Operand::Numeric,
                    _ => Operand::Integer,
                };
                // `x += 1` on a gauge: worked out in the base type, and the
                // result must stay in the gauge.
                let gauge = matches!(self.tcx.kind(self.resolve(ty)), TyKind::Gauge(..));
                let base = self.tcx.degauge(self.resolve(ty));
                self.require_operand(base, need, bin.as_str(), target.span);
                if matches!(bin, BinaryOp::Shl | BinaryOp::Shr) {
                    let amount = self.check_expr(value, None);
                    self.require_operand(amount, Operand::Integer, bin.as_str(), value.span);
                } else {
                    self.check_coerce(value, base);
                }
                if gauge {
                    let span = target.span.to(value.span);
                    self.check_gauge_assign(ty, bin, target, value, span);
                }
            }
        }
    }

    /// The type of something assigned to: a `var`, or a tuple of them
    /// (`(x, y) = (y, x)`). `None` if it can't be assigned (already reported).
    fn check_place(&mut self, target: &Expr) -> Option<Ty> {
        match &target.kind {
            ExprKind::Local(local) => {
                let ty = self.check_expr(target, None);
                let l = self.p.local(*local);
                if !l.mutable {
                    self.diags.push(
                        Diagnostic::error(codes::T0107, format!("`{}` can't be changed", l.name))
                            .primary(target.span, "assigned here")
                            .secondary(l.span, "declared with `let`")
                            .help(format!("declare it with `var {}` to make it changeable", l.name)),
                    );
                }
                Some(ty)
            }
            ExprKind::Tuple(items) if !items.is_empty() => {
                let mut tys = Vec::with_capacity(items.len());
                for item in items {
                    tys.push(self.check_place(item)?);
                }
                let ty = self.tcx.intern(TyKind::Tuple(tys));
                self.b.expr_ty.insert(target.id, ty);
                Some(ty)
            }
            ExprKind::Field { .. } | ExprKind::LeadingDot(_) => {
                let ty = self.check_expr(target, None);
                if self.tcx.is_error(ty) {
                    return None;
                }
                if self.place(target) != Some(true) {
                    self.diags.push(
                        Diagnostic::error(codes::T0107, "can't change this field")
                            .primary(target.span, "part of something that can't be changed")
                            .help("fields can be changed through a `var`, a `mut` parameter, or `mut self`"),
                    );
                }
                Some(ty)
            }
            ExprKind::Index { .. } => self.index_place(target),
            ExprKind::Call { .. } => {
                self.unsupported(target.span, "assigning through projections");
                None
            }
            ExprKind::Deref(_) => {
                self.unsupported(target.span, "spool pointers");
                None
            }
            _ => {
                self.error(codes::T0107, target.span, "can't assign to this", "not a variable");
                None
            }
        }
    }
}
