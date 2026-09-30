//! Expressions, statements and control flow.

use loom_syntax::ast::{AssignOp, BinaryOp, CastKind, Lit, ParamMode, UnaryOp};
use loom_syntax::source::Span;

use super::{Builder, LoopCtx, Vals};
use crate::heddle::{BitOp, Cmp, FOp, InstKind, IntOp, Policy, Term, VTy, Value};
use crate::hir::{self, Block, DefKind, ExprKind, LocalId, StmtKind};
use crate::ty::{ConstVal, FloatTy, IntTy, Ty, TyKind};
use crate::typeck::{ArgSource, Coercion, Key, Receiver};

/// A place a value can be written to: some of a local's parts.
#[derive(Clone, Debug)]
pub(super) struct Place {
    pub local: LocalId,
    pub start: usize,
    pub len: usize,
}

impl<'a> Builder<'a> {
    /// The value of `e`, with its coercion applied. `None` when control
    /// leaves (`return`, `break`, an unravel) or `e` can't be built yet.
    pub(super) fn expr(&mut self, e: &'a hir::Expr) -> Option<Vals> {
        let v = self.expr_raw(e)?;
        match self.tables().coercions.get(&e.id) {
            None | Some(Coercion::Widen) => Some(v),
            Some(Coercion::Discard) => Some(Vec::new()),
            Some(other) => {
                let what = match other {
                    Coercion::StrToString | Coercion::StringToStr => "`String`",
                    Coercion::ToOptional => "optionals",
                    Coercion::ToResult => "results",
                    Coercion::ToAny(_) => "`any` values",
                    Coercion::ToUnion => "unions",
                    Coercion::ToSlice => "slices",
                    Coercion::ToStream(_) => "streams",
                    _ => "this conversion",
                };
                self.unsupported(e.span, what)
            }
        }
    }

    pub(super) fn expr_raw(&mut self, e: &'a hir::Expr) -> Option<Vals> {
        let span = e.span;
        let ty = self.ty_of(e);
        if let Some(&v) = self.tables().const_values.get(&e.id) {
            return self.const_val(v, ty, span);
        }
        match &e.kind {
            ExprKind::Lit(lit) => self.lit(lit, false, ty, span),
            ExprKind::Local(l) => {
                let vars = self.local_vars(*l, span)?;
                Some(self.read_all(&vars))
            }
            ExprKind::Def { def, .. } => match &self.p.def(*def).kind {
                DefKind::Const(_) => self.const_def(*def, ty, span),
                _ => self.unsupported(span, "this kind of value"),
            },
            ExprKind::Tuple(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.extend(self.expr(item)?);
                }
                Some(out)
            }
            ExprKind::Block(block) => self.block(block),
            ExprKind::If { cond, then, else_, deferred } => {
                self.if_expr(e, cond, then, else_.as_deref(), deferred.is_some(), ty)
            }
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, ty, span),
            ExprKind::Loop { body } => self.loop_expr(e, body, ty),
            ExprKind::While { cond, body } => {
                self.while_expr(e, cond, body);
                self.live().then(Vec::new)
            }
            ExprKind::For { is_const: true, body, .. } => {
                for id in self.tables().unrolled.get(&e.id).cloned().unwrap_or_default() {
                    let saved = std::mem::replace(&mut self.f.body, id);
                    let done = self.block(body);
                    self.f.body = saved;
                    done?;
                }
                Some(Vec::new())
            }
            ExprKind::For { pat, iter, body, is_loom: false, .. } => {
                self.for_expr(e, pat, iter, body)?;
                self.live().then(Vec::new)
            }
            ExprKind::Break { target, value } => {
                self.f.span = Some(span);
                let loop_ = self.f.loops.iter().rposition(|l| l.id == *target)?;
                if let Some(value) = value {
                    let vals = self.expr(value)?;
                    let vars = self.f.loops[loop_].result.clone();
                    self.write_all(&vars, &vals);
                }
                let to = self.f.loops[loop_].brk;
                self.jump(to);
                None
            }
            ExprKind::Continue { target } => {
                self.f.span = Some(span);
                let to = self.f.loops.iter().rev().find(|l| l.id == *target)?.cont;
                self.jump(to);
                None
            }
            ExprKind::Return(value) => {
                self.f.span = Some(span);
                let mut vals = match value {
                    Some(v) => self.expr(v)?,
                    None => Vec::new(),
                };
                vals.extend(self.read_outs());
                self.terminate(Term::Ret(vals));
                None
            }
            ExprKind::Assign { target, op, value } => {
                self.f.span = Some(span);
                self.assign(target, *op, value)?;
                Some(Vec::new())
            }
            ExprKind::Call { callee, args } => self.call(e, callee, args, ty),
            ExprKind::Unary { op, operand } => self.unary(*op, operand, ty, span),
            ExprKind::Binary { op: BinaryOp::And | BinaryOp::Or, .. } => self.bool_value(e),
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, ty, span),
            ExprKind::Chain { .. } | ExprKind::Is { .. } => self.bool_value(e),
            ExprKind::Cast { expr, kind, .. } => self.cast(e, expr, *kind, ty),
            ExprKind::Field { base, .. } => self.field(e, base),
            ExprKind::Assoc { .. } => match self.tables().assoc_values.get(&e.id).copied() {
                Some(crate::typeck::AssocValue::Int(v)) => self.int_value(v, ty, span),
                Some(crate::typeck::AssocValue::Const(def)) => self.const_def(def, ty, span),
                None => self.unsupported(span, "this kind of value"),
            },
            ExprKind::LeadingDot(_) => match self.tables().leading_consts.get(&e.id).copied() {
                Some(def) => self.const_def(def, ty, span),
                None => self.unsupported(span, "methods and fields of warps"),
            },
            // Checked while compiling: nothing is left to run.
            ExprKind::Const(inner) if crate::typeck::assert_args(self.p, inner).is_some() => Some(Vec::new()),
            ExprKind::Const(_) => self.unsupported(span, "`const` expressions computed while compiling"),
            ExprKind::Template(_) => self.unsupported(span, "`String`"),
            _ => self.unsupported(span, "this expression"),
        }
    }

    // ------------------------------------------------------------ constants

    /// An integer as a value of `ty`: an integer, or a float when the
    /// literal became one (`let x: f64 = 0`).
    pub(super) fn int_value(&mut self, v: i128, ty: Ty, span: Span) -> Option<Vals> {
        if let Some(f) = self.float_of(ty) {
            return Some(vec![self.fconst(v as f64, f)]);
        }
        match self.tcx().kind(ty) {
            TyKind::Int(_) | TyKind::Bool | TyKind::Char => Some(vec![self.iconst(v as u64)]),
            _ => {
                let shown = self.tcx().display(self.p, ty);
                self.unsupported(span, &format!("values of type `{shown}`"))
            }
        }
    }

    pub(super) fn fconst(&mut self, v: f64, ty: FloatTy) -> Value {
        match ty {
            FloatTy::F32 => self.emit1(InstKind::Fconst(u64::from((v as f32).to_bits())), VTy::F32),
            FloatTy::F64 => self.emit1(InstKind::Fconst(v.to_bits()), VTy::F64),
        }
    }

    pub(super) fn lit(&mut self, lit: &Lit, negative: bool, ty: Ty, span: Span) -> Option<Vals> {
        match lit {
            Lit::Int(i) => {
                let v = if negative { -(i.value as i128) } else { i.value as i128 };
                self.int_value(v, ty, span)
            }
            Lit::Float(f) => {
                let v = if negative { -f.value } else { f.value };
                let Some(ft) = self.float_of(ty) else { return self.unsupported(span, "this literal") };
                Some(vec![self.fconst(v, ft)])
            }
            Lit::Char(c) => Some(vec![self.iconst(*c as u64)]),
            Lit::Bool(b) => Some(vec![self.iconst(u64::from(*b))]),
            Lit::Str(s) if matches!(self.tcx().kind(ty), TyKind::Str) => Some(self.str_lit(s.as_bytes())),
            Lit::Str(_) => self.unsupported(span, "`String`"),
            Lit::Bytes(_) => self.unsupported(span, "byte strings"),
            Lit::None => self.unsupported(span, "optionals"),
        }
    }

    fn const_val(&mut self, v: ConstVal, ty: Ty, span: Span) -> Option<Vals> {
        match v {
            ConstVal::Int(v) => self.int_value(v, ty, span),
            ConstVal::Bool(b) => Some(vec![self.iconst(u64::from(b))]),
            ConstVal::Str(s) => {
                let text = self.tcx().sym_str(s).as_bytes().to_vec();
                Some(self.str_lit(&text))
            }
            ConstVal::Unit(_) => self.unsupported(span, "cases"),
            ConstVal::Field { .. } | ConstVal::Case { .. } => {
                let code = loom_syntax::diag::codes::K0111;
                if !self.diags.iter().any(|d| d.code == code && d.labels.first().is_some_and(|l| l.span == span)) {
                    self.diags.push(
                        loom_syntax::Diagnostic::error(code, "reflection only exists while compiling")
                            .primary(span, "needed when the program runs")
                            .help("read what you need from it at compile time: `f.name`, `self.(f)`, `const for f in T.fields`"),
                    );
                }
                None
            }
        }
    }

    /// A constant's value: untyped ones are literals; typed ones are
    /// computed while compiling, which Treadle doesn't read yet.
    fn const_def(&mut self, def: crate::hir::DefId, ty: Ty, span: Span) -> Option<Vals> {
        let results = &self.typed.results;
        if let Some(&v) = results.untyped_consts.get(&def) {
            return self.int_value(v, ty, span);
        }
        if let Some(&v) = results.untyped_floats.get(&def) {
            let Some(ft) = self.float_of(ty) else { return self.unsupported(span, "this constant") };
            return Some(vec![self.fconst(v, ft)]);
        }
        self.unsupported(span, "constants with a type")
    }

    // ------------------------------------------------------------ statements and blocks

    pub(super) fn block(&mut self, block: &'a Block) -> Option<Vals> {
        let Some((last, rest)) = block.stmts.split_last() else { return Some(Vec::new()) };
        for stmt in rest {
            self.stmt(stmt)?;
            if !self.live() {
                return None;
            }
        }
        match &last.kind {
            StmtKind::Expr(tail) if !matches!(tail.kind, ExprKind::Assign { .. }) => {
                self.f.span = Some(last.span);
                self.expr(tail)
            }
            _ => {
                self.stmt(last)?;
                self.live().then(Vec::new)
            }
        }
    }

    fn stmt(&mut self, stmt: &'a hir::Stmt) -> Option<()> {
        self.f.span = Some(stmt.span);
        match &stmt.kind {
            StmtKind::Let { pat, init, .. } => {
                let vals = self.expr(init)?;
                let ty = self.tables().pat_ty.get(&pat.id).copied().unwrap_or_else(|| self.ty_of(init));
                self.bind_irrefutable(pat, &vals, self.tcx().degauge(ty))
            }
            StmtKind::Expr(e) => self.expr(e).map(drop),
            StmtKind::Item(_) => Some(()),
            StmtKind::Defer { .. } => self.unsupported(stmt.span, "`defer`"),
            StmtKind::Given { .. } | StmtKind::Spool { .. } => self.unsupported(stmt.span, "`given` and spools"),
        }
    }

    // ------------------------------------------------------------ if and match

    fn if_expr(
        &mut self,
        e: &'a hir::Expr,
        cond: &'a hir::Expr,
        then: &'a Block,
        else_: Option<&'a hir::Expr>,
        is_const: bool,
        ty: Ty,
    ) -> Option<Vals> {
        if is_const {
            // `const if`: only the branch taken.
            return match (self.tables().const_ifs.get(&e.id).copied(), else_) {
                (Some(true), _) => self.block(then),
                (Some(false), Some(else_)) => self.expr(else_),
                _ => Some(Vec::new()),
            };
        }
        let tys = self.flat(ty, e.span)?;
        let vars = self.new_vars(&tys);
        let (t, f, join) = (self.new_block(), self.new_block(), self.new_block());
        self.branch(cond, t, f)?;
        self.enter(t);
        if self.live()
            && let Some(vals) = self.block(then)
        {
            self.write_all(&vars, &vals);
            self.jump(join);
        }
        self.enter(f);
        if self.live() {
            let vals = match else_ {
                Some(else_) => self.expr(else_),
                None => Some(Vec::new()),
            };
            if let Some(vals) = vals.filter(|_| self.live()) {
                self.write_all(&vars, &vals);
                self.jump(join);
            }
        }
        self.enter(join);
        if !self.live() {
            return None;
        }
        Some(self.read_all(&vars))
    }

    fn match_expr(&mut self, scrutinee: &'a hir::Expr, arms: &'a [hir::Arm], ty: Ty, span: Span) -> Option<Vals> {
        let s = self.expr(scrutinee)?;
        let sty = self.ty_of(scrutinee);
        let tys = self.flat(ty, span)?;
        let vars = self.new_vars(&tys);
        let join = self.new_block();
        for arm in arms {
            // An or-pattern with bindings is one arm per alternative, since
            // each alternative finds its bindings in different places.
            let alts: Vec<&'a hir::Pat> = match &arm.pat.kind {
                hir::PatKind::Or(alts) if super::pat::binds_anything(&arm.pat) => alts.iter().collect(),
                _ => vec![&arm.pat],
            };
            for pat in alts {
                if !self.live() {
                    break;
                }
                self.f.span = Some(arm.span);
                let (matched, next) = (self.new_block(), self.new_block());
                self.test_pat(pat, &s, sty, matched, next)?;
                self.enter(matched);
                if self.live() {
                    self.bind_pat(pat, &s, sty)?;
                    if let Some(guard) = &arm.guard {
                        let body = self.new_block();
                        self.branch(guard, body, next)?;
                        self.enter(body);
                    }
                    if self.live()
                        && let Some(vals) = self.expr(&arm.body)
                        && self.live()
                    {
                        self.write_all(&vars, &vals);
                        self.jump(join);
                    }
                }
                self.enter(next);
            }
        }
        if self.live() {
            // Exhaustiveness is checked: this can't happen.
            self.terminate(Term::Crash(0));
        }
        self.enter(join);
        if !self.live() {
            return None;
        }
        Some(self.read_all(&vars))
    }

    // ------------------------------------------------------------ conditions

    /// Jumps to `t` if `cond` holds and to `f` if not. `and`, `or`, `not`,
    /// comparisons and `is` become branches rather than `bool` values.
    pub(super) fn branch(&mut self, cond: &'a hir::Expr, t: super::BlockId, f: super::BlockId) -> Option<()> {
        match &cond.kind {
            ExprKind::Binary { op: BinaryOp::And, lhs, rhs } => {
                let mid = self.new_block();
                self.branch(lhs, mid, f)?;
                self.enter(mid);
                if self.live() {
                    self.branch(rhs, t, f)?;
                }
                Some(())
            }
            ExprKind::Binary { op: BinaryOp::Or, lhs, rhs } => {
                let mid = self.new_block();
                self.branch(lhs, t, mid)?;
                self.enter(mid);
                if self.live() {
                    self.branch(rhs, t, f)?;
                }
                Some(())
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => self.branch(operand, f, t),
            ExprKind::Is { expr, pat } => {
                let vals = self.expr(expr)?;
                let ty = self.ty_of(expr);
                let matched = self.new_block();
                self.test_pat(pat, &vals, ty, matched, f)?;
                self.enter(matched);
                if self.live() {
                    self.bind_pat(pat, &vals, ty)?;
                    self.jump(t);
                }
                Some(())
            }
            ExprKind::Chain { first, rest } => {
                let operands: Vec<&hir::Expr> =
                    std::iter::once(first.as_ref()).chain(rest.iter().map(|(_, e)| e)).collect();
                let ty = self.common_ty(&operands);
                let mut prev = self.expr(first)?;
                for (op, e) in rest {
                    let next = self.expr(e)?;
                    let c = self.compare(*op, &prev, &next, ty, e.span)?;
                    let cont = self.new_block();
                    self.br(c, cont, f);
                    self.enter(cont);
                    prev = next;
                }
                self.jump(t);
                Some(())
            }
            _ if !self.tables().coercions.contains_key(&cond.id)
                && matches!(cond.kind, ExprKind::Lit(Lit::Bool(_))) =>
            {
                let ExprKind::Lit(Lit::Bool(b)) = cond.kind else { unreachable!() };
                self.jump(if b { t } else { f });
                Some(())
            }
            _ => {
                let v = self.expr(cond)?;
                self.br(v[0], t, f);
                Some(())
            }
        }
    }

    /// A condition (`and`, `or`, `is`, a chain) as a `bool` value.
    fn bool_value(&mut self, e: &'a hir::Expr) -> Option<Vals> {
        let var = self.new_var(VTy::Int);
        let (t, f, join) = (self.new_block(), self.new_block(), self.new_block());
        self.branch(e, t, f)?;
        for (b, v) in [(t, 1), (f, 0)] {
            self.enter(b);
            if self.live() {
                let c = self.iconst(v);
                self.write(var, c);
                self.jump(join);
            }
        }
        self.enter(join);
        if !self.live() {
            return None;
        }
        Some(vec![self.read(var)])
    }

    // ------------------------------------------------------------ loops

    fn loop_expr(&mut self, e: &'a hir::Expr, body: &'a Block, ty: Ty) -> Option<Vals> {
        let tys = self.flat(ty, e.span)?;
        let result = self.new_vars(&tys);
        let (head, exit) = (self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        self.f.loops.push(LoopCtx { id: e.id, brk: exit, cont: head, result: result.clone() });
        let done = self.block(body);
        if self.live() && done.is_some() {
            self.jump(head);
        }
        self.f.loops.pop();
        self.seal(head);
        self.enter(exit);
        if !self.live() {
            return None;
        }
        Some(self.read_all(&result))
    }

    fn while_expr(&mut self, e: &'a hir::Expr, cond: &'a hir::Expr, body: &'a Block) {
        let (head, body_b, exit) = (self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        if self.branch(cond, body_b, exit).is_some() {
            self.enter(body_b);
            self.f.loops.push(LoopCtx { id: e.id, brk: exit, cont: head, result: Vec::new() });
            if self.live() && self.block(body).is_some() && self.live() {
                self.jump(head);
            }
            self.f.loops.pop();
        }
        self.seal(head);
        self.enter(exit);
    }

    fn for_expr(&mut self, e: &'a hir::Expr, pat: &'a hir::Pat, iter: &'a hir::Expr, body: &'a Block) -> Option<()> {
        let ExprKind::Range { lo: Some(lo), hi, inclusive } = &iter.kind else {
            return self.unsupported(iter.span, "`for` loops over anything but a range");
        };
        let TyKind::Range(elem) = *self.tcx().kind(self.ty_of(iter)) else {
            return self.unsupported(iter.span, "this kind of `for` loop");
        };
        let Some(int) = self.int_of(elem) else {
            return self.unsupported(iter.span, "`for` loops over ranges that aren't integers");
        };
        // The bounds, evaluated once, before the loop.
        let lo = self.expr(lo)?[0];
        let hi = match hi {
            Some(hi) => Some(self.expr(hi)?[0]),
            None => None,
        };
        let i = self.new_var(VTy::Int);
        self.write(i, lo);
        let (head, body_b, step, exit) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
        self.jump(head);
        self.switch_to(head);
        let cur = self.read(i);
        match hi {
            Some(hi) => {
                let cmp = if *inclusive { Cmp::Le } else { Cmp::Lt };
                let c = self.emit1(InstKind::ICmp { cmp, unsigned: !int.signed, a: cur, b: hi }, VTy::Int);
                self.br(c, body_b, exit);
            }
            None => self.jump(body_b),
        }
        self.enter(body_b);
        self.f.loops.push(LoopCtx { id: e.id, brk: exit, cont: step, result: Vec::new() });
        let item = self.read(i);
        let done = self.bind_irrefutable(pat, &[item], elem).and_then(|()| self.block(body));
        if self.live() && done.is_some() {
            self.jump(step);
        }
        self.f.loops.pop();
        self.enter(step);
        if self.live() {
            let cur = self.read(i);
            let one = self.iconst(1);
            match hi {
                // Stepping past the last value of an inclusive range at the
                // type's maximum mustn't overflow: stop before stepping.
                Some(hi) if *inclusive => {
                    let at_end = self.emit1(InstKind::ICmp { cmp: Cmp::Eq, unsigned: true, a: cur, b: hi }, VTy::Int);
                    let inc = self.new_block();
                    self.br(at_end, exit, inc);
                    self.enter(inc);
                    let next = self.emit1(
                        InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: int, a: cur, b: one },
                        VTy::Int,
                    );
                    self.write(i, next);
                }
                Some(_) => {
                    let next = self.emit1(
                        InstKind::Arith { op: IntOp::Add, policy: Policy::Wrap, ty: int, a: cur, b: one },
                        VTy::Int,
                    );
                    self.write(i, next);
                }
                // An open range traps when it runs past the type's maximum.
                None => {
                    let next = self.emit1(
                        InstKind::Arith { op: IntOp::Add, policy: Policy::Trap, ty: int, a: cur, b: one },
                        VTy::Int,
                    );
                    self.write(i, next);
                }
            }
            self.jump(head);
        }
        self.seal(head);
        self.enter(exit);
        Some(())
    }

    // ------------------------------------------------------------ assignment

    /// The place `e` names: a local, or part of a tuple held in one.
    pub(super) fn place(&mut self, e: &'a hir::Expr) -> Option<Place> {
        match &e.kind {
            ExprKind::Local(l) => {
                let len = self.local_vars(*l, e.span)?.len();
                Some(Place { local: *l, start: 0, len })
            }
            ExprKind::Field { base, .. } => {
                let key = self.tables().fields.get(&e.id).map(|f| f.key.clone());
                let (Some(Key::Pos(i)), TyKind::Tuple(_)) = (key, self.tcx().kind(self.ty_of(base))) else {
                    return self.unsupported(e.span, "assigning to fields of warps");
                };
                let base_place = self.place(base)?;
                let parts = self.tuple_parts(self.ty_of(base), e.span)?;
                let start: usize = parts[..i as usize - 1].iter().map(|(_, n)| n).sum();
                Some(Place { local: base_place.local, start: base_place.start + start, len: parts[i as usize - 1].1 })
            }
            _ => self.unsupported(e.span, "assigning to this"),
        }
    }

    pub(super) fn write_place(&mut self, place: &Place, vals: &[Value]) -> Option<()> {
        let vars = self.f.locals.get(&place.local)?.clone();
        self.write_all(&vars[place.start..place.start + place.len], vals);
        Some(())
    }

    fn read_place(&mut self, place: &Place) -> Option<Vals> {
        let vars = self.f.locals.get(&place.local)?.clone();
        Some(self.read_all(&vars[place.start..place.start + place.len]))
    }

    fn assign(&mut self, target: &'a hir::Expr, op: AssignOp, value: &'a hir::Expr) -> Option<()> {
        match op {
            AssignOp::Assign => {
                // Every value is worked out before anything is assigned, so
                // `(a, b) = (b, a)` swaps.
                let vals = self.expr(value)?;
                self.assign_parts(target, &vals)
            }
            AssignOp::Compound(bin) => {
                let ty = self.ty_of(target);
                let rhs = self.expr(value)?;
                let place = self.place(target)?;
                let cur = self.read_place(&place)?;
                let result = self.apply(bin, &cur, &rhs, ty, target.span)?;
                self.write_place(&place, &result)
            }
        }
    }

    fn assign_parts(&mut self, target: &'a hir::Expr, vals: &[Value]) -> Option<()> {
        match &target.kind {
            ExprKind::Tuple(items) => {
                let mut at = 0;
                for item in items {
                    let n = self.flat(self.ty_of(item), item.span)?.len();
                    self.assign_parts(item, &vals[at..at + n])?;
                    at += n;
                }
                Some(())
            }
            _ => {
                let place = self.place(target)?;
                self.write_place(&place, vals)
            }
        }
    }

    // ------------------------------------------------------------ operators

    fn unary(&mut self, op: UnaryOp, operand: &'a hir::Expr, ty: Ty, span: Span) -> Option<Vals> {
        if op == UnaryOp::Neg
            && let ExprKind::Lit(lit @ (Lit::Int(_) | Lit::Float(_))) = &operand.kind
            && !self.tables().coercions.contains_key(&operand.id)
        {
            // `-128i8` is one constant.
            return self.lit(lit, true, ty, span);
        }
        let a = self.expr(operand)?[0];
        let kind = match op {
            UnaryOp::Not => InstKind::Not(a),
            UnaryOp::Neg => match (self.float_of(ty), self.int_of(ty)) {
                (Some(f), _) => InstKind::FNeg { ty: f, a },
                (_, Some(i)) => InstKind::Neg { ty: i, a },
                _ => return self.unsupported(span, "this negation"),
            },
            UnaryOp::BitNot => match self.int_of(ty) {
                Some(i) => InstKind::BitNot { ty: i, a },
                None => return self.unsupported(span, "this `~`"),
            },
            UnaryOp::Alloc => return self.unsupported(span, "`^` allocation"),
        };
        let vty = if self.float_of(ty) == Some(FloatTy::F32) {
            VTy::F32
        } else if self.float_of(ty).is_some() {
            VTy::F64
        } else {
            VTy::Int
        };
        Some(vec![self.emit1(kind, vty)])
    }

    fn binary(&mut self, op: BinaryOp, lhs: &'a hir::Expr, rhs: &'a hir::Expr, ty: Ty, span: Span) -> Option<Vals> {
        let a = self.expr(lhs)?;
        let b = self.expr(rhs)?;
        if op.is_comparison() {
            let operand_ty = self.common_ty(&[lhs, rhs]);
            return Some(vec![self.compare(op, &a, &b, operand_ty, span)?]);
        }
        self.apply(op, &a, &b, ty, span)
    }

    /// `a op b`, both already worked out, of type `ty` (for `op=` too).
    pub(super) fn apply(&mut self, op: BinaryOp, a: &[Value], b: &[Value], ty: Ty, span: Span) -> Option<Vals> {
        use BinaryOp as B;
        let (a, b) = (a[0], b[0]);
        if let Some(f) = self.float_of(ty) {
            let fop = match op {
                B::Add => FOp::Add,
                B::Sub => FOp::Sub,
                B::Mul => FOp::Mul,
                B::Div => FOp::Div,
                B::Rem => FOp::Rem,
                _ => return self.unsupported(span, "this operator on floats"),
            };
            let vty = if f == FloatTy::F32 { VTy::F32 } else { VTy::F64 };
            return Some(vec![self.emit1(InstKind::FArith { op: fop, ty: f, a, b }, vty)]);
        }
        let Some(t) = self.int_of(ty) else { return self.unsupported(span, "this operator") };
        let arith = |op, policy| InstKind::Arith { op, policy, ty: t, a, b };
        let kind = match op {
            B::Add => arith(IntOp::Add, Policy::Trap),
            B::Sub => arith(IntOp::Sub, Policy::Trap),
            B::Mul => arith(IntOp::Mul, Policy::Trap),
            B::AddWrap => arith(IntOp::Add, Policy::Wrap),
            B::SubWrap => arith(IntOp::Sub, Policy::Wrap),
            B::MulWrap => arith(IntOp::Mul, Policy::Wrap),
            B::AddSat => arith(IntOp::Add, Policy::Sat),
            B::SubSat => arith(IntOp::Sub, Policy::Sat),
            B::MulSat => arith(IntOp::Mul, Policy::Sat),
            B::Div => InstKind::Div { ty: t, a, b },
            B::Rem => InstKind::Rem { ty: t, a, b },
            B::Shl => InstKind::Shl { ty: t, a, b },
            B::Shr => InstKind::Shr { ty: t, a, b },
            B::BitAnd => InstKind::Bit { op: BitOp::And, a, b },
            B::BitOr => InstKind::Bit { op: BitOp::Or, a, b },
            B::BitXor => InstKind::Bit { op: BitOp::Xor, a, b },
            B::AddChecked | B::SubChecked | B::MulChecked => return self.unsupported(span, "optionals"),
            _ => return self.unsupported(span, "this operator"),
        };
        Some(vec![self.emit1(kind, VTy::Int)])
    }

    /// The type several operands are compared at: the widest of their
    /// types, which the narrower ones were widened to.
    fn common_ty(&self, es: &[&hir::Expr]) -> Ty {
        let mut best = self.ty_of(es[0]);
        for e in &es[1..] {
            let ty = self.ty_of(e);
            if let (Some(a), Some(b)) = (self.int_of(best), self.int_of(ty))
                && a.widens_to(b)
            {
                best = ty;
            }
        }
        best
    }

    /// `a op b` for a comparison, as a `bool`.
    pub(super) fn compare(&mut self, op: BinaryOp, a: &[Value], b: &[Value], ty: Ty, span: Span) -> Option<Value> {
        let cmp = match op {
            BinaryOp::Eq => Cmp::Eq,
            BinaryOp::Ne => Cmp::Ne,
            BinaryOp::Lt => Cmp::Lt,
            BinaryOp::Le => Cmp::Le,
            BinaryOp::Gt => Cmp::Gt,
            _ => Cmp::Ge,
        };
        let tcx = self.tcx();
        match tcx.kind(tcx.degauge(ty)).clone() {
            TyKind::Int(i) => Some(self.emit1(InstKind::ICmp { cmp, unsigned: !i.signed, a: a[0], b: b[0] }, VTy::Int)),
            TyKind::Bool | TyKind::Char => {
                Some(self.emit1(InstKind::ICmp { cmp, unsigned: true, a: a[0], b: b[0] }, VTy::Int))
            }
            TyKind::Float(f) => Some(self.emit1(InstKind::FCmp { cmp, ty: f, a: a[0], b: b[0] }, VTy::Int)),
            TyKind::Tuple(items) if matches!(cmp, Cmp::Eq | Cmp::Ne) => {
                // Equal when every item is.
                let mut at = 0;
                let mut all: Option<Value> = None;
                for item in items {
                    let n = self.flat(item, span)?.len();
                    let eq = self.compare(BinaryOp::Eq, &a[at..at + n], &b[at..at + n], item, span)?;
                    at += n;
                    all = Some(match all {
                        None => eq,
                        Some(prev) => self.emit1(InstKind::Bit { op: BitOp::And, a: prev, b: eq }, VTy::Int),
                    });
                }
                let all = match all {
                    Some(all) => all,
                    None => self.iconst(1),
                };
                Some(if cmp == Cmp::Ne { self.emit1(InstKind::Not(all), VTy::Int) } else { all })
            }
            _ => {
                let shown = tcx.display(self.p, ty);
                self.unsupported(span, &format!("comparing values of type `{shown}`"))
            }
        }
    }

    fn cast(&mut self, e: &'a hir::Expr, inner: &'a hir::Expr, kind: CastKind, ty: Ty) -> Option<Vals> {
        let span = e.span;
        let raw = self.tables().expr_ty.get(&e.id).copied().unwrap_or(ty);
        if kind == CastKind::Opt {
            return self.unsupported(span, "optionals");
        }
        if matches!(self.tcx().kind(raw), TyKind::Gauge(..)) {
            return self.unsupported(span, "casts into gauges");
        }
        let source = self.ty_of(inner);
        let v = self.expr(inner)?[0];
        let policy = match kind {
            CastKind::Wrap => Policy::Wrap,
            CastKind::Sat => Policy::Sat,
            _ => Policy::Trap,
        };
        let tcx = self.tcx();
        let (k, vty) = match (tcx.kind(source).clone(), tcx.kind(ty).clone()) {
            (TyKind::Int(s), TyKind::Int(t)) => {
                if s.widens_to(t) {
                    return Some(vec![v]);
                }
                (InstKind::IntCast { policy, to: t, from: s, a: v }, VTy::Int)
            }
            (TyKind::Int(s), TyKind::Float(f)) => (InstKind::IntToFloat { from: s, to: f, a: v }, float_vty(f)),
            (TyKind::Float(s), TyKind::Int(t)) => {
                (InstKind::FloatToInt { sat: kind == CastKind::Sat, from: s, to: t, a: v }, VTy::Int)
            }
            (TyKind::Float(s), TyKind::Float(t)) if s == t => return Some(vec![v]),
            (TyKind::Float(_), TyKind::Float(t)) => (InstKind::FloatCast { to: t, a: v }, float_vty(t)),
            (TyKind::Char, TyKind::Int(t)) => {
                // A char is a code point: a value of u21.
                let u21 = IntTy { signed: false, bits: 21, size: false };
                if u21.widens_to(t) {
                    return Some(vec![v]);
                }
                (InstKind::IntCast { policy: Policy::Trap, to: t, from: u21, a: v }, VTy::Int)
            }
            (TyKind::Bool, TyKind::Int(_)) => return Some(vec![v]),
            (TyKind::Int(s), TyKind::Char) => (InstKind::IntToChar { from: s, a: v }, VTy::Int),
            (TyKind::Char, TyKind::Char) | (TyKind::Bool, TyKind::Bool) => return Some(vec![v]),
            _ => return self.unsupported(span, "this conversion"),
        };
        Some(vec![self.emit1(k, vty)])
    }

    /// `t.0`: an item of a tuple.
    fn field(&mut self, e: &'a hir::Expr, base: &'a hir::Expr) -> Option<Vals> {
        let access = self.tables().fields.get(&e.id).cloned();
        let base_ty = self.ty_of(base);
        match (access, self.tcx().kind(base_ty)) {
            (Some(access), TyKind::Tuple(_)) if access.base == Receiver::Expr => {
                let Key::Pos(i) = access.key else { return self.unsupported(e.span, "this field") };
                let vals = self.expr(base)?;
                let parts = self.tuple_parts(base_ty, e.span)?;
                let start: usize = parts[..i as usize - 1].iter().map(|(_, n)| n).sum();
                Some(vals[start..start + parts[i as usize - 1].1].to_vec())
            }
            _ => self.unsupported(e.span, "methods and fields of warps"),
        }
    }

    // ------------------------------------------------------------ calls

    fn call(&mut self, e: &'a hir::Expr, callee: &'a hir::Expr, args: &'a [hir::Arg], ty: Ty) -> Option<Vals> {
        let span = e.span;
        let tables = self.tables();
        if tables.closure_calls.contains(&e.id) {
            return self.unsupported(span, "calling function values");
        }
        if tables.new_lists.contains(&e.id) || tables.constructs.contains_key(&e.id) {
            return self.unsupported(span, "values of warps");
        }
        if matches!(callee.kind, ExprKind::BuiltinCase(_)) {
            return self.unsupported(span, "optionals and results");
        }
        let Some(info) = tables.calls.get(&e.id).cloned() else {
            return self.unsupported(span, "this call");
        };
        if info.dynamic.is_some() {
            return self.unsupported(span, "`any` values");
        }
        if tables.streams.contains_key(&e.id) {
            return self.unsupported(span, "streams");
        }
        if !info.through.is_empty() {
            return self.unsupported(span, "`through`");
        }
        let p = self.p;
        let def = self.typed.results.instance(info.callee).def;
        let DefKind::Fn(f) = &p.def(def).kind else { return self.unsupported(span, "this call") };
        if f.abi.is_some() {
            return self.extern_call(def, &info, args, span);
        }
        if f.body.is_none() {
            return self.unsupported(span, &format!("`{}` on Treadle", p.def_path(def)));
        }
        let sig = self.typed.results.sigs.get(&info.callee)?.clone();
        // The receiver, then the arguments in source order.
        let method = match &callee.kind {
            ExprKind::Index { base, .. } => base.as_ref(),
            _ => callee,
        };
        let receiver_base = match (&info.receiver, &method.kind) {
            (Some(Receiver::Expr), ExprKind::Field { base, .. }) => Some(base.as_ref()),
            (Some(_), _) => return self.unsupported(span, "methods of warps"),
            (None, _) => None,
        };
        let receiver = match receiver_base {
            Some(base) => Some(self.expr(base)?),
            None => None,
        };
        let mut vals: Vec<Vals> = Vec::with_capacity(args.len());
        for arg in args {
            vals.push(self.expr(&arg.value)?);
        }
        let mut flat_args = Vec::new();
        let mut places = Vec::new();
        for (k, source) in info.args.iter().enumerate() {
            let param = sig.params.get(k)?;
            let arg_expr = match source {
                ArgSource::Arg(i) => {
                    flat_args.extend(vals[*i].iter().copied());
                    Some(&args[*i].value)
                }
                ArgSource::Receiver => {
                    flat_args.extend(receiver.iter().flatten().copied());
                    receiver_base
                }
                ArgSource::Default => {
                    let default = f.params.get(k).and_then(|p| p.default.as_ref())?;
                    if mentions_locals(default) {
                        return self.unsupported(default.span, "default values that use other parameters");
                    }
                    // The default was checked with the function's body.
                    let saved = std::mem::replace(&mut self.f.body, info.callee);
                    let v = self.expr(default);
                    self.f.body = saved;
                    flat_args.extend(v?);
                    None
                }
            };
            if param.mode == ParamMode::Mut {
                let Some(place) = arg_expr.and_then(|a| self.place(a)) else {
                    return self.unsupported(span, "this `mut` argument");
                };
                places.push(place);
            }
        }
        let ret = if matches!(f.ret, crate::hir::FnRet::Unit) { Vec::new() } else { self.flat(sig.ret, span)? };
        let mut tys = ret.clone();
        for param in sig.params.iter().filter(|p| p.mode == ParamMode::Mut) {
            tys.extend(self.flat(param.ty, span)?);
        }
        let func = self.func_id(info.callee);
        let results = self.emit(InstKind::Call { func, args: flat_args }, &tys);
        // `mut` arguments come back after the result.
        let mut at = ret.len();
        for place in &places {
            self.write_place(place, &results[at..at + place.len])?;
            at += place.len;
        }
        if matches!(self.tcx().kind(ty), TyKind::Never) {
            self.terminate(Term::Crash(1));
            return None;
        }
        Some(results[..ret.len()].to_vec())
    }
}

fn float_vty(f: FloatTy) -> VTy {
    match f {
        FloatTy::F32 => VTy::F32,
        FloatTy::F64 => VTy::F64,
    }
}

/// Whether an expression reads any local (so it can't be moved to a call site).
fn mentions_locals(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Local(_) => true,
        ExprKind::Lit(_) | ExprKind::Def { .. } | ExprKind::Const(_) => false,
        ExprKind::Unary { operand, .. } => mentions_locals(operand),
        ExprKind::Binary { lhs, rhs, .. } => mentions_locals(lhs) || mentions_locals(rhs),
        ExprKind::Tuple(items) => items.iter().any(mentions_locals),
        ExprKind::Cast { expr, .. } => mentions_locals(expr),
        ExprKind::Template(parts) => parts.iter().any(|p| match p {
            hir::TemplatePart::Interp { expr, .. } => mentions_locals(expr),
            hir::TemplatePart::Text(_) => false,
        }),
        _ => true,
    }
}
