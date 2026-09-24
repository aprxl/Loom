//! Lowering expressions, statements and patterns to LIR.
//!
//! Lua separates statements from expressions; Loom doesn't. An expression
//! that needs statements (`if`, `match`, `loop`, a block, `and` with a
//! statement on its right) delivers its value into a temporary, and the
//! statements go before the use. When an operand needs statements, the
//! operands to its left that were already lowered are spilled into
//! temporaries first, so Loom's left-to-right evaluation order holds.

use loom_syntax::ast::{AssignOp, BinaryOp, CastKind, Lit, ParamMode, UnaryOp};
use loom_syntax::source::Span;

use super::lir::{Expr, Stmt, StmtKind};
use super::{Dest, LoopInfo, Lowerer};
use crate::hir::{self, Block, DefKind, ExprKind, HirId, PatKind, StmtKind as HirStmt, TemplatePart};
use crate::ty::{FloatTy, IntTy, Ty, TyKind};
use crate::typeck::{ArgSource, Coercion, Receiver};

impl Lowerer<'_> {
    pub(super) fn push(&mut self, kind: StmtKind) {
        self.out.push(Stmt { kind, span: self.span });
    }

    /// Runs `f` into a fresh statement buffer and returns what it produced.
    pub(super) fn nested(&mut self, f: impl FnOnce(&mut Self)) -> Vec<Stmt> {
        let saved = std::mem::take(&mut self.out);
        f(self);
        std::mem::replace(&mut self.out, saved)
    }

    /// The type of an expression as checked (before its coercion).
    pub(super) fn ty_of(&self, e: &hir::Expr) -> Ty {
        self.tables().expr_ty.get(&e.id).copied().unwrap_or(self.typed.tcx.error)
    }

    fn int_of(&self, ty: Ty) -> Option<IntTy> {
        self.tcx().as_int(ty)
    }

    fn is_float(&self, ty: Ty) -> Option<FloatTy> {
        match self.tcx().kind(ty) {
            TyKind::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Whether a value of this type is `()` or never produced at all.
    fn is_valueless(&self, ty: Ty) -> bool {
        matches!(self.tcx().kind(ty), TyKind::Unit | TyKind::Never)
    }

    pub(super) fn push_local(&mut self, name: String, value: Expr) {
        self.push(StmtKind::Local(vec![name], vec![value]));
    }

    fn spill(&mut self, v: Expr) -> Expr {
        if v.is_constant() || is_temp(&v) {
            return v;
        }
        let t = self.new_temp();
        self.push(StmtKind::Local(vec![t.clone()], vec![v]));
        Expr::Name(t)
    }

    // ------------------------------------------------------------ values

    /// Lowers `e` to a Lua expression, with its coercion applied.
    pub(super) fn lower_expr(&mut self, e: &hir::Expr) -> Expr {
        let v = self.lower_raw(e);
        self.coerce(e, v)
    }

    pub(super) fn coerce(&mut self, e: &hir::Expr, v: Expr) -> Expr {
        let Some(coercion) = self.tables().coercions.get(&e.id) else { return v };
        match coercion {
            Coercion::Widen | Coercion::StrToString | Coercion::StringToStr | Coercion::Discard => v,
            Coercion::ToOptional => {
                let ty = self.ty_of(e);
                if self.opt_boxed(ty) { Expr::Table(vec![v]) } else { v }
            }
            Coercion::ToResult => Expr::Table(vec![Expr::Bool(true), v]),
            Coercion::ToAny(wefts) => {
                let wefts = wefts.clone();
                let vt = self.vtable(self.ty_of(e), wefts, e.span);
                Expr::rt("any", vec![vt, v])
            }
            Coercion::ToUnion => {
                let vt = self.vtable(self.ty_of(e), Vec::new(), e.span);
                Expr::rt("any", vec![vt, v])
            }
        }
    }

    /// Lowers several operands left to right, spilling earlier ones when a
    /// later one needs statements.
    fn lower_operands(&mut self, es: &[&hir::Expr]) -> Vec<Expr> {
        let flagged: Vec<(&hir::Expr, bool)> = es.iter().map(|e| (*e, false)).collect();
        self.lower_operands_flagged(&flagged)
    }

    /// `lower_operands`, copying places when the operands are owned.
    pub(super) fn lower_operands_with(&mut self, es: &[&hir::Expr], owned: bool) -> Vec<Expr> {
        let flagged: Vec<(&hir::Expr, bool)> = es.iter().map(|e| (*e, owned)).collect();
        self.lower_operands_flagged(&flagged)
    }

    /// `lower_operands`, each operand owned (copied if it's a place) or not.
    fn lower_operands_flagged(&mut self, es: &[(&hir::Expr, bool)]) -> Vec<Expr> {
        let mut vals: Vec<Expr> = Vec::with_capacity(es.len());
        for &(e, owned) in es {
            let mark = self.out.len();
            let v = if owned { self.lower_owned(e) } else { self.lower_expr(e) };
            if self.out.len() > mark {
                let mut spills = Vec::new();
                for prev in vals.iter_mut() {
                    if !prev.is_constant() && !is_temp(prev) {
                        let t = self.new_temp();
                        let value = std::mem::replace(prev, Expr::Name(t.clone()));
                        spills.push(Stmt { kind: StmtKind::Local(vec![t], vec![value]), span: self.span });
                    }
                }
                self.out.splice(mark..mark, spills);
            }
            vals.push(v);
        }
        vals
    }

    /// Lowers `e` through a temporary: for expressions that are statements in Lua.
    fn via_temp(&mut self, e: &hir::Expr) -> Expr {
        if self.is_valueless(self.ty_of(e)) {
            self.lower_control(e, &Dest::Discard);
            return Expr::Nil;
        }
        let t = self.new_temp();
        self.push(StmtKind::Local(vec![t.clone()], Vec::new()));
        self.lower_control(e, &Dest::Assign(t.clone()));
        Expr::Name(t)
    }

    pub(super) fn lower_raw(&mut self, e: &hir::Expr) -> Expr {
        let span = e.span;
        let ty = self.ty_of(e);
        if let Some(v) = self.warp_value(e) {
            return v;
        }
        match &e.kind {
            ExprKind::Lit(lit) => self.lit(lit, false, ty),
            ExprKind::Template(parts) => self.template(parts),
            ExprKind::Local(local) => Expr::Name(self.local_name(*local)),
            ExprKind::Def { def, .. } => match &self.p.def(*def).kind {
                DefKind::Const(_) => match self.typed.results.untyped_consts.get(def) {
                    Some(&value) => self.int_value(value, ty),
                    None => {
                        self.const_refs.push(*def);
                        Expr::field(Expr::name("C"), &self.def_name(*def))
                    }
                },
                _ => {
                    self.unsupported(span, "this kind of value");
                    Expr::Nil
                }
            },
            ExprKind::Tuple(items) if items.is_empty() => Expr::Nil,
            ExprKind::Tuple(items) => {
                let items: Vec<&hir::Expr> = items.iter().collect();
                Expr::Table(self.lower_owned_operands(&items))
            }
            ExprKind::Block(_)
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Loop { .. }
            | ExprKind::Mend { .. } => self.via_temp(e),
            ExprKind::Try(inner) => self.lower_try(e, inner),
            ExprKind::Fail(value) => {
                self.lower_fail(e, value);
                Expr::Nil
            }
            ExprKind::Call { callee, args } if matches!(callee.kind, ExprKind::BuiltinCase(_)) => {
                let ExprKind::BuiltinCase(kw) = callee.kind else { unreachable!() };
                self.builtin_case(e, kw, args)
            }
            ExprKind::For { .. } | ExprKind::While { .. } | ExprKind::Break { .. } | ExprKind::Continue { .. } => {
                self.lower_control(e, &Dest::Discard);
                Expr::Nil
            }
            ExprKind::Return(value) => {
                match value {
                    Some(v) => self.lower_into(v, &Dest::Return),
                    None => self.push(StmtKind::Return(None)),
                }
                Expr::Nil
            }
            ExprKind::Assign { target, op, value } => {
                self.assign(target, *op, value);
                Expr::Nil
            }
            ExprKind::Call { callee, args } => self.call(e, callee, args),
            ExprKind::Unary { op, operand } => self.unary(*op, operand, ty),
            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), lhs, rhs } => self.short_circuit(*op, lhs, rhs),
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, ty),
            ExprKind::Chain { first, rest } => self.chain(first, rest),
            ExprKind::Cast { expr, kind, .. } => self.cast(expr, *kind, ty),
            ExprKind::Range { lo, hi, inclusive } => {
                let bounds: Vec<&hir::Expr> = lo.iter().chain(hi.iter()).map(|b| b.as_ref()).collect();
                let mut vals = self.lower_operands(&bounds);
                let hi = if hi.is_some() { vals.pop().unwrap_or(Expr::Nil) } else { Expr::Nil };
                let lo = vals.pop().unwrap_or(Expr::Nil);
                Expr::Table(vec![lo, hi, Expr::Bool(*inclusive)])
            }
            ExprKind::Error => Expr::Nil,
            _ => {
                self.unsupported(span, "this expression");
                Expr::Nil
            }
        }
    }

    // ------------------------------------------------------------ literals

    fn lit(&mut self, lit: &Lit, negative: bool, ty: Ty) -> Expr {
        match lit {
            Lit::Int(i) => {
                let value = if negative { -(i.value as i128) } else { i.value as i128 };
                self.int_value(value, ty)
            }
            Lit::Float(f) => {
                let v = if negative { -f.value } else { f.value };
                self.float_value(v, ty)
            }
            Lit::Char(c) => Expr::Int(*c as i64),
            Lit::Str(s) => Expr::Str(s.clone().into_bytes()),
            Lit::Bool(b) => Expr::Bool(*b),
            Lit::None | Lit::Bytes(_) => Expr::Nil,
        }
    }

    /// An integer constant as a value of `ty`: an integer, or a float when
    /// the literal became one (`let x: f64 = 0`).
    pub(super) fn int_value(&self, value: i128, ty: Ty) -> Expr {
        let ty = match self.tcx().kind(ty) {
            TyKind::Optional(inner) => *inner,
            _ => ty,
        };
        if self.is_float(ty).is_some() {
            return self.float_value(value as f64, ty);
        }
        // A u64 above 2^63 is stored as its bit pattern.
        Expr::Int(value as u64 as i64)
    }

    fn float_value(&self, v: f64, ty: Ty) -> Expr {
        match self.is_float(ty) {
            Some(FloatTy::F32) => Expr::Float(v as f32 as f64),
            _ => Expr::Float(v),
        }
    }

    fn template(&mut self, parts: &[TemplatePart]) -> Expr {
        let interps: Vec<&hir::Expr> = parts
            .iter()
            .filter_map(|p| match p {
                TemplatePart::Interp { expr, .. } => Some(expr),
                TemplatePart::Text(_) => None,
            })
            .collect();
        let mut vals = self.lower_operands(&interps).into_iter();
        let mut pieces = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                TemplatePart::Text(text) => pieces.push(Expr::Str(text.clone().into_bytes())),
                TemplatePart::Interp { expr, spec } => {
                    let v = vals.next().unwrap_or(Expr::Nil);
                    let ty = self.ty_of(expr);
                    let plain_text = matches!(self.tcx().kind(ty), TyKind::Str)
                        || matches!(self.tcx().kind(ty), TyKind::Adt(def, _) if self.is_string(*def));
                    if plain_text && spec.is_none() {
                        pieces.push(v);
                    } else {
                        let desc = self.desc(ty, expr.span);
                        let spec = spec.as_ref().map_or(Expr::Nil, |s| Expr::Str(s.clone().into_bytes()));
                        pieces.push(Expr::rt("fmt", vec![desc, v, spec]));
                    }
                }
            }
        }
        match pieces.len() {
            0 => Expr::Str(Vec::new()),
            1 if matches!(pieces[0], Expr::Str(_)) => pieces.remove(0),
            _ => Expr::rt("cat", pieces),
        }
    }

    // ------------------------------------------------------------ operators

    fn unary(&mut self, op: UnaryOp, operand: &hir::Expr, ty: Ty) -> Expr {
        if op == UnaryOp::Neg
            && let ExprKind::Lit(lit @ (Lit::Int(_) | Lit::Float(_))) = &operand.kind
        {
            // `-128i8` is one constant.
            return self.lit(lit, true, ty);
        }
        let v = self.lower_expr(operand);
        match op {
            UnaryOp::Not => Expr::Unary("not", Box::new(v)),
            UnaryOp::Neg if self.is_float(ty).is_some() => Expr::Unary("-", Box::new(v)),
            UnaryOp::Neg => {
                let d = self.desc(ty, operand.span);
                Expr::rt("neg", vec![d, v])
            }
            UnaryOp::BitNot => {
                let d = self.desc(ty, operand.span);
                Expr::rt("bnot", vec![d, v])
            }
            UnaryOp::Alloc => {
                self.unsupported(operand.span, "`^` allocation");
                Expr::Nil
            }
        }
    }

    fn binary(&mut self, op: BinaryOp, lhs: &hir::Expr, rhs: &hir::Expr, ty: Ty) -> Expr {
        use BinaryOp as B;
        let vals = self.lower_operands(&[lhs, rhs]);
        let [a, b]: [Expr; 2] = vals.try_into().unwrap_or([Expr::Nil, Expr::Nil]);
        if op.is_comparison() {
            let operand_ty = self.common_ty(&[lhs, rhs]);
            return self.compare(op, a, b, operand_ty, lhs.span);
        }
        // The operation's type is the result's, except `+?` and friends,
        // which wrap it in an optional.
        let op_ty = match (op, self.tcx().kind(ty)) {
            (B::AddChecked | B::SubChecked | B::MulChecked, TyKind::Optional(inner)) => *inner,
            _ => ty,
        };
        if let Some(float) = self.is_float(op_ty) {
            let v = match op {
                B::Add => Expr::bin("+", a, b),
                B::Sub => Expr::bin("-", a, b),
                B::Mul => Expr::bin("*", a, b),
                B::Div => Expr::bin("/", a, b),
                B::Rem => Expr::call(Expr::field(Expr::name("math"), "fmod"), vec![a, b]),
                _ => return Expr::Nil,
            };
            return if float == FloatTy::F32 { Expr::rt("f32", vec![v]) } else { v };
        }
        let d = self.desc(op_ty, lhs.span);
        let rt_op = |name: &str| Expr::rt(name, vec![d.clone(), a.clone(), b.clone()]);
        match op {
            B::Add => rt_op("add"),
            B::Sub => rt_op("sub"),
            B::Mul => rt_op("mul"),
            B::AddWrap => rt_op("add_wrap"),
            B::SubWrap => rt_op("sub_wrap"),
            B::MulWrap => rt_op("mul_wrap"),
            B::AddSat => rt_op("add_sat"),
            B::SubSat => rt_op("sub_sat"),
            B::MulSat => rt_op("mul_sat"),
            B::AddChecked => rt_op("add_chk"),
            B::SubChecked => rt_op("sub_chk"),
            B::MulChecked => rt_op("mul_chk"),
            B::Div => rt_op("div"),
            B::Rem => rt_op("rem"),
            B::Shl => rt_op("shl"),
            B::Shr => rt_op("shr"),
            B::BitAnd => Expr::bin("&", a, b),
            B::BitOr => Expr::bin("|", a, b),
            B::BitXor => Expr::bin("~", a, b),
            _ => Expr::Nil,
        }
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

    pub(super) fn compare(&mut self, op: BinaryOp, a: Expr, b: Expr, ty: Ty, span: Span) -> Expr {
        use BinaryOp as B;
        if self.compares_structurally(ty) {
            let d = self.desc(ty, span);
            if matches!(op, B::Eq | B::Ne) {
                let eq = Expr::rt("eq", vec![d, a, b]);
                return if op == B::Ne { Expr::Unary("not", Box::new(eq)) } else { eq };
            }
            let lua_op = match op {
                B::Lt => "<",
                B::Le => "<=",
                B::Gt => ">",
                _ => ">=",
            };
            return Expr::bin(lua_op, Expr::rt("cmp", vec![d, a, b]), Expr::Int(0));
        }
        let u64_ = self.int_of(ty).is_some_and(|i| !i.signed && i.bits == 64);
        if u64_ && !matches!(op, B::Eq | B::Ne) {
            let ult = |x: Expr, y: Expr| Expr::rt("ult", vec![x, y]);
            let not = |x: Expr| Expr::Unary("not", Box::new(x));
            return match op {
                B::Lt => ult(a, b),
                B::Gt => ult(b, a),
                B::Le => not(ult(b, a)),
                _ => not(ult(a, b)),
            };
        }
        let lua_op = match op {
            B::Eq => "==",
            B::Ne => "~=",
            B::Lt => "<",
            B::Le => "<=",
            B::Gt => ">",
            _ => ">=",
        };
        Expr::bin(lua_op, a, b)
    }

    /// Whether `==` and `<` on `ty` go through the runtime (`rt.eq`,
    /// `rt.cmp`) rather than Lua's operators: tuples and optionals, and warps
    /// that are tables or that write their own `eq` or `lt`.
    fn compares_structurally(&self, ty: Ty) -> bool {
        match self.tcx().kind(ty) {
            TyKind::Tuple(_) | TyKind::Optional(_) => true,
            TyKind::Adt(def, _) if self.is_string(*def) => false,
            TyKind::Adt(def, _) => {
                let unit_only = self.warp(*def).is_some_and(|i| i.unit_only());
                let custom = self.typed.results.op_fns.get(&ty).is_some_and(|f| f.eq.is_some() || f.lt.is_some());
                !unit_only || custom
            }
            _ => false,
        }
    }

    /// `a and b`, `a or b`: Lua's own when `b` needs no statements.
    fn short_circuit(&mut self, op: BinaryOp, lhs: &hir::Expr, rhs: &hir::Expr) -> Expr {
        let a = self.lower_expr(lhs);
        let mut b = Expr::Nil;
        let stmts = self.nested(|this| b = this.lower_expr(rhs));
        let lua_op = if op == BinaryOp::And { "and" } else { "or" };
        if stmts.is_empty() {
            return Expr::bin(lua_op, a, b);
        }
        let t = self.new_temp();
        self.push(StmtKind::Local(vec![t.clone()], vec![a]));
        let cond = if op == BinaryOp::And { Expr::name(&t) } else { Expr::Unary("not", Box::new(Expr::name(&t))) };
        let mut body = stmts;
        body.push(Stmt { kind: StmtKind::Assign(vec![Expr::name(&t)], vec![b]), span: self.span });
        self.push(StmtKind::If(vec![(cond, body)], None));
        Expr::Name(t)
    }

    /// `a < b <= c`: each middle operand is evaluated once, and the chain
    /// stops at the first false comparison.
    fn chain(&mut self, first: &hir::Expr, rest: &[(BinaryOp, hir::Expr)]) -> Expr {
        let operands: Vec<&hir::Expr> = std::iter::once(first).chain(rest.iter().map(|(_, e)| e)).collect();
        let ty = self.common_ty(&operands);
        let v = self.lower_expr(first);
        let mut prev = self.spill(v);
        let result = self.new_temp();
        self.push(StmtKind::Local(vec![result.clone()], vec![Expr::Bool(true)]));
        let mut sinks: Vec<Vec<Stmt>> = Vec::new();
        for (op, e) in rest {
            let body = self.nested(|this| {
                let v = this.lower_expr(e);
                let next = this.spill(v);
                let cmp = this.compare(*op, prev.clone(), next.clone(), ty, e.span);
                this.push(StmtKind::Assign(vec![Expr::name(&result)], vec![cmp]));
                prev = next;
            });
            sinks.push(body);
        }
        // Nest: each comparison runs only if the ones before it held.
        let mut nested: Vec<Stmt> = Vec::new();
        for mut body in sinks.into_iter().rev() {
            if !nested.is_empty() {
                let guard = Expr::name(&result);
                body.push(Stmt {
                    kind: StmtKind::If(vec![(guard, std::mem::take(&mut nested))], None),
                    span: self.span,
                });
            }
            nested = body;
        }
        self.out.extend(nested);
        Expr::Name(result)
    }

    fn cast(&mut self, inner: &hir::Expr, kind: CastKind, ty: Ty) -> Expr {
        let target = match (kind, self.tcx().kind(ty)) {
            (CastKind::Opt, TyKind::Optional(t)) => *t,
            _ => ty,
        };
        let source = self.ty_of(inner);
        let v = self.lower_expr(inner);
        let span = inner.span;
        let tcx = self.tcx();
        match (tcx.kind(source).clone(), tcx.kind(target).clone()) {
            (TyKind::Int(s), TyKind::Int(t)) => {
                if s.widens_to(t) {
                    return v;
                }
                let (d, sd) = (self.desc(target, span), self.desc(source, span));
                let name = match kind {
                    CastKind::Checked => "cast",
                    CastKind::Wrap => "cast_wrap",
                    CastKind::Sat => "cast_sat",
                    CastKind::Opt => "cast_chk",
                };
                Expr::rt(name, vec![d, sd, v])
            }
            (TyKind::Int(_), TyKind::Float(f)) => {
                let sd = self.desc(source, span);
                let v = Expr::rt("itof", vec![sd, v]);
                if f == FloatTy::F32 { Expr::rt("f32", vec![v]) } else { v }
            }
            (TyKind::Float(_), TyKind::Int(_)) => {
                let d = self.desc(target, span);
                let name = match kind {
                    CastKind::Sat => "ftoi_sat",
                    CastKind::Opt => "ftoi_chk",
                    _ => "ftoi",
                };
                Expr::rt(name, vec![d, v])
            }
            (TyKind::Float(_), TyKind::Float(FloatTy::F32)) => Expr::rt("f32", vec![v]),
            (TyKind::Float(_), TyKind::Float(_)) => v,
            (TyKind::Char, TyKind::Int(t)) => {
                // A char is a code point, so a value of u21.
                if (IntTy { signed: false, bits: 21, size: false }).widens_to(t) {
                    return v;
                }
                let d = self.desc(target, span);
                let u21 = Expr::rt("int", vec![Expr::Int(21), Expr::Bool(false)]);
                Expr::rt("cast", vec![d, u21, v])
            }
            (TyKind::Bool, TyKind::Int(_)) => Expr::bin("or", Expr::bin("and", v, Expr::Int(1)), Expr::Int(0)),
            (TyKind::Int(_), TyKind::Char) => {
                let sd = self.desc(source, span);
                Expr::rt(if kind == CastKind::Opt { "to_char_chk" } else { "to_char" }, vec![sd, v])
            }
            _ => {
                self.unsupported(span, "this conversion");
                Expr::Nil
            }
        }
    }

    // ------------------------------------------------------------ calls

    fn call(&mut self, call: &hir::Expr, callee: &hir::Expr, args: &[hir::Arg]) -> Expr {
        let Some(info) = self.tables().calls.get(&call.id).cloned() else {
            self.unsupported(call.span, "this call");
            return Expr::Nil;
        };
        let p = self.p;
        // A method of an `any` value: found in its vtable, by the weft
        // method's number.
        if let Some(method) = info.dynamic {
            let values: Vec<&hir::Expr> = match &callee.kind {
                ExprKind::Field { base, .. } => {
                    std::iter::once(base.as_ref()).chain(args.iter().map(|a| &a.value)).collect()
                }
                _ => return Expr::Nil,
            };
            let mut lowered = self.lower_operands(&values);
            let recv = lowered.remove(0);
            let mut params = vec![recv, Expr::Int(method.0 as i64)];
            for source in info.args.iter().skip(1) {
                match source {
                    ArgSource::Arg(i) => params.push(lowered[*i].clone()),
                    _ => {
                        self.unsupported(call.span, "default arguments through `any`");
                        return Expr::Nil;
                    }
                }
            }
            return Expr::rt("dyn", params);
        }
        let def = self.typed.results.instance(info.callee).def;
        let DefKind::Fn(f) = &p.def(def).kind else { return Expr::Nil };
        // Externs, and the methods of warps the runtime implements (`Fmt.put`).
        let runtime = f.abi.is_some()
            || (f.body.is_none()
                && p.def(def).parent.is_some_and(|w| {
                    matches!(p.def(w).kind, DefKind::Warp(_)) && p.def(w).attrs.iter().any(|a| a.name == "builtin")
                }));
        let target = if runtime {
            match self.extern_fn(def, callee.span) {
                Some(e) => e,
                None => return Expr::Nil,
            }
        } else {
            Expr::field(Expr::name("F"), &self.fn_name(info.callee))
        };
        // `take` parameters own their argument (a `Copy` place is copied);
        // read and `mut` parameters share it.
        let modes: Vec<ParamMode> = self
            .typed
            .results
            .sigs
            .get(&info.callee)
            .map(|s| s.params.iter().map(|p| p.mode).collect())
            .unwrap_or_default();
        let owned = |slot: Option<usize>| slot.and_then(|k| modes.get(k)) == Some(&ParamMode::Take);

        // The receiver, then the arguments in source order, whatever order
        // the parameters take them in.
        let receiver_base = match (&info.receiver, &callee.kind) {
            (Some(Receiver::Expr), ExprKind::Field { base, .. }) => Some(base.as_ref()),
            _ => None,
        };
        let receiver_slot = info.args.iter().position(|s| *s == ArgSource::Receiver);
        let mut operands: Vec<(&hir::Expr, bool)> = Vec::with_capacity(args.len() + 1);
        if let Some(base) = receiver_base {
            operands.push((base, owned(receiver_slot)));
        }
        for (i, arg) in args.iter().enumerate() {
            let slot = info.args.iter().position(|s| *s == ArgSource::Arg(i));
            operands.push((&arg.value, owned(slot)));
        }
        let mut lowered = self.lower_operands_flagged(&operands);
        let receiver = match (info.receiver, receiver_base) {
            (Some(Receiver::Expr), Some(_)) => Some(lowered.remove(0)),
            (Some(r), _) => Some(self.receiver(r, None)),
            (None, _) => None,
        };
        // A method a `through` field has is called on that field.
        let receiver = receiver.map(|r| info.through.iter().fold(r, Self::access));
        let in_order = info.args.iter().filter_map(|s| match s {
            ArgSource::Arg(i) => Some(*i),
            ArgSource::Default | ArgSource::Receiver => None,
        });
        if !in_order.clone().is_sorted() {
            for v in lowered.iter_mut() {
                if !v.is_constant() && !is_temp(v) {
                    let t = self.new_temp();
                    let value = std::mem::replace(v, Expr::Name(t.clone()));
                    self.push(StmtKind::Local(vec![t], vec![value]));
                }
            }
        }
        let mut params = Vec::with_capacity(info.args.len());
        for (k, source) in info.args.iter().enumerate() {
            match source {
                ArgSource::Arg(i) => params.push(lowered[*i].clone()),
                ArgSource::Receiver => params.push(receiver.clone().unwrap_or(Expr::Nil)),
                ArgSource::Default => {
                    let Some(default) = f.params.get(k).and_then(|p| p.default.as_ref()) else {
                        params.push(Expr::Nil);
                        continue;
                    };
                    if mentions_locals(default) {
                        self.unsupported(default.span, "default values that use other parameters");
                        params.push(Expr::Nil);
                    } else {
                        // The default was checked with the function's body.
                        let value = self.in_body(info.callee, |this| this.lower_owned(default));
                        params.push(value);
                    }
                }
            }
        }
        Expr::call(target, params)
    }

    // ------------------------------------------------------------ statements and control flow

    /// Lowers `e` so its value reaches `dest`.
    pub(super) fn lower_into(&mut self, e: &hir::Expr, dest: &Dest) {
        let has_coercion = self
            .tables()
            .coercions
            .get(&e.id)
            .is_some_and(|c| !matches!(c, Coercion::Widen | Coercion::StrToString | Coercion::StringToStr));
        let control = matches!(
            e.kind,
            ExprKind::Block(_)
                | ExprKind::If { .. }
                | ExprKind::Match { .. }
                | ExprKind::Loop { .. }
                | ExprKind::For { .. }
                | ExprKind::While { .. }
                | ExprKind::Break { .. }
                | ExprKind::Continue { .. }
                | ExprKind::Return(_)
                | ExprKind::Assign { .. }
        ) || Self::is_fallible_control(e);
        if control && !has_coercion {
            let saved = self.span;
            if matches!(
                e.kind,
                ExprKind::Break { .. }
                    | ExprKind::Continue { .. }
                    | ExprKind::Return(_)
                    | ExprKind::Assign { .. }
                    | ExprKind::Fail(_)
            ) {
                self.span = Some(e.span);
            }
            self.lower_control(e, dest);
            self.span = saved;
            return;
        }
        let v = match dest {
            Dest::Discard => self.lower_expr(e),
            Dest::Assign(_) | Dest::Return => self.lower_owned(e),
        };
        self.deliver(v, dest);
    }

    pub(super) fn deliver(&mut self, v: Expr, dest: &Dest) {
        match dest {
            Dest::Discard => match v {
                Expr::Call(..) => self.push(StmtKind::Call(v)),
                v if has_call(&v) => self.push(StmtKind::Local(vec!["_".into()], vec![v])),
                _ => {}
            },
            Dest::Assign(t) => self.push(StmtKind::Assign(vec![Expr::name(t)], vec![v])),
            Dest::Return => self.push(StmtKind::Return(Some(v))),
        }
    }

    fn lower_control(&mut self, e: &hir::Expr, dest: &Dest) {
        match &e.kind {
            ExprKind::Block(block) => {
                let body = self.nested(|this| this.block_into(block, dest));
                self.push(StmtKind::Do(body));
            }
            ExprKind::If { cond, then, else_, .. } => self.if_into(cond, then, else_.as_deref(), dest),
            ExprKind::Match { scrutinee, arms } => self.match_into(scrutinee, arms, dest),
            ExprKind::Loop { body } => {
                let body = self.loop_body(e.id, dest.clone(), body);
                self.push(StmtKind::While(Expr::Bool(true), body));
                self.after_loop();
            }
            ExprKind::While { cond, body } => {
                let mut c = Expr::Nil;
                let cond_stmts = self.nested(|this| c = this.lower_expr(cond));
                let mut stmts = self.loop_body(e.id, Dest::Discard, body);
                if cond_stmts.is_empty() {
                    self.push(StmtKind::While(c, stmts));
                } else {
                    let mut full = cond_stmts;
                    let exit = Stmt { kind: StmtKind::Break, span: self.span };
                    full.push(Stmt {
                        kind: StmtKind::If(vec![(Expr::Unary("not", Box::new(c)), vec![exit])], None),
                        span: self.span,
                    });
                    full.append(&mut stmts);
                    self.push(StmtKind::While(Expr::Bool(true), full));
                }
                self.after_loop();
            }
            ExprKind::For { pat, iter, body, .. } => self.for_range(e, pat, iter, body),
            ExprKind::Break { target, value } => {
                if let Some(v) = value {
                    let loop_dest = self.loops.iter().rev().find(|l| l.id == *target).map(|l| l.dest.clone());
                    match loop_dest {
                        Some(Dest::Return) => {
                            self.lower_into(v, &Dest::Return);
                            return;
                        }
                        Some(d) => self.lower_into(v, &d),
                        None => {}
                    }
                }
                self.jump_break(*target);
            }
            ExprKind::Continue { target } => {
                if let Some(info) = self.loops.iter_mut().rev().find(|l| l.id == *target) {
                    info.continue_used = true;
                    let label = info.continue_label.clone();
                    self.push(StmtKind::Goto(label));
                }
            }
            ExprKind::Return(value) => match value {
                Some(v) => self.lower_into(v, &Dest::Return),
                None => self.push(StmtKind::Return(None)),
            },
            ExprKind::Assign { target, op, value } => self.assign(target, *op, value),
            ExprKind::Mend { expr, handler, .. } => self.mend_into(e, expr, handler, dest),
            ExprKind::Fail(value) => self.lower_fail(e, value),
            _ => {
                let v = self.lower_expr(e);
                self.deliver(v, dest);
            }
        }
    }

    /// The statements of a block, with its last expression delivered to `dest`.
    pub(super) fn block_into(&mut self, block: &Block, dest: &Dest) {
        let Some((last, rest)) = block.stmts.split_last() else { return };
        for stmt in rest {
            self.stmt(stmt);
        }
        match &last.kind {
            HirStmt::Expr(tail) if !matches!(tail.kind, ExprKind::Assign { .. }) => {
                self.span = Some(last.span);
                let discard = self.tables().coercions.get(&tail.id) == Some(&Coercion::Discard);
                self.lower_into(tail, if discard { &Dest::Discard } else { dest });
            }
            _ => self.stmt(last),
        }
    }

    fn stmt(&mut self, stmt: &hir::Stmt) {
        self.span = Some(stmt.span);
        match &stmt.kind {
            HirStmt::Let { pat, init, .. } => {
                let v = self.lower_owned(init);
                self.bind_irrefutable(pat, v);
            }
            HirStmt::Expr(e) => self.lower_into(e, &Dest::Discard),
            HirStmt::Item(_) => {}
            HirStmt::Given { .. } | HirStmt::Spool { .. } | HirStmt::Defer { .. } => {
                self.unsupported(stmt.span, "this statement");
            }
        }
    }

    /// Binds an irrefutable pattern (`let`, `for`) to a value.
    fn bind_irrefutable(&mut self, pat: &hir::Pat, v: Expr) {
        match &pat.kind {
            PatKind::Bind(local) => {
                let name = self.local_name(*local);
                self.push(StmtKind::Local(vec![name], vec![v]));
            }
            PatKind::Tuple(_) | PatKind::Case { .. } | PatKind::Twill { .. } => {
                let base = self.spill_always(v);
                self.bind_pat(pat, &base, true);
            }
            _ => self.deliver(v, &Dest::Discard),
        }
    }

    /// Like `spill`, but also for names, since the pattern reads it several times.
    pub(super) fn spill_always(&mut self, v: Expr) -> Expr {
        if matches!(v, Expr::Name(_)) || v.is_constant() {
            return v;
        }
        let t = self.new_temp();
        self.push(StmtKind::Local(vec![t.clone()], vec![v]));
        Expr::Name(t)
    }

    fn if_into(&mut self, cond: &hir::Expr, then: &Block, else_: Option<&hir::Expr>, dest: &Dest) {
        let c = self.lower_expr(cond);
        let then_body = self.nested(|this| this.block_into(then, dest));
        let mut arms = vec![(c, then_body)];
        let mut else_body = None;
        let mut next = else_;
        // `else if` chains become `elseif` when the condition needs no statements.
        while let Some(e) = next {
            match &e.kind {
                ExprKind::If { cond, then, else_, deferred: None } if !self.tables().coercions.contains_key(&e.id) => {
                    let mut c = Expr::Nil;
                    let pre = self.nested(|this| c = this.lower_expr(cond));
                    if !pre.is_empty() {
                        let body = self.nested(|this| {
                            this.out.extend(pre);
                            this.if_into_rest(c, then, else_.as_deref(), dest);
                        });
                        else_body = Some(body);
                        break;
                    }
                    let body = self.nested(|this| this.block_into(then, dest));
                    arms.push((c, body));
                    next = else_.as_deref();
                }
                _ => {
                    else_body = Some(self.nested(|this| this.lower_into(e, dest)));
                    break;
                }
            }
        }
        self.push(StmtKind::If(arms, else_body));
    }

    fn if_into_rest(&mut self, c: Expr, then: &Block, else_: Option<&hir::Expr>, dest: &Dest) {
        let then_body = self.nested(|this| this.block_into(then, dest));
        let else_body = else_.map(|e| self.nested(|this| this.lower_into(e, dest)));
        self.push(StmtKind::If(vec![(c, then_body)], else_body));
    }

    // ------------------------------------------------------------ loops

    /// A loop body: `do BODY end ::continue::`, with the loop registered for
    /// `break` and `continue`.
    fn loop_body(&mut self, id: HirId, dest: Dest, body: &Block) -> Vec<Stmt> {
        let continue_label = self.new_label("c");
        let break_label = self.new_label("b");
        self.loops.push(LoopInfo {
            id,
            dest,
            continue_label,
            continue_used: false,
            break_label,
            break_label_used: false,
        });
        let inner = self.nested(|this| this.block_into(body, &Dest::Discard));
        let info = self.loops.last().expect("pushed above");
        let mut stmts = vec![Stmt { kind: StmtKind::Do(inner), span: self.span }];
        if info.continue_used {
            stmts.push(Stmt { kind: StmtKind::Label(info.continue_label.clone()), span: None });
        }
        stmts
    }

    /// Ends the innermost loop: pops it, and places its break label if an
    /// inner loop jumps to it.
    fn after_loop(&mut self) {
        if let Some(info) = self.loops.pop()
            && info.break_label_used
        {
            self.out.push(Stmt { kind: StmtKind::Label(info.break_label), span: None });
        }
    }

    fn jump_break(&mut self, target: HirId) {
        let innermost = self.loops.last().is_some_and(|l| l.id == target);
        if innermost {
            self.push(StmtKind::Break);
            return;
        }
        if let Some(info) = self.loops.iter_mut().rev().find(|l| l.id == target) {
            info.break_label_used = true;
            let label = info.break_label.clone();
            self.push(StmtKind::Goto(label));
        }
    }

    fn for_range(&mut self, e: &hir::Expr, pat: &hir::Pat, iter: &hir::Expr, body: &Block) {
        let elem = match self.tcx().kind(self.ty_of(iter)) {
            TyKind::Range(elem) => *elem,
            _ => {
                self.unsupported(iter.span, "this kind of `for` loop");
                return;
            }
        };
        let u64_ = self.int_of(elem).is_some_and(|i| !i.signed && i.bits == 64);
        // The bounds, evaluated once, before the loop.
        let (lo, hi, inclusive) = match &iter.kind {
            ExprKind::Range { lo: Some(lo), hi, inclusive } => {
                let bounds: Vec<&hir::Expr> = std::iter::once(lo.as_ref()).chain(hi.as_deref()).collect();
                let mut vals = self.lower_operands(&bounds).into_iter();
                let lo = vals.next().unwrap_or(Expr::Nil);
                let hi = vals.next();
                (lo, hi, *inclusive)
            }
            _ => {
                // A range value: `{lo, hi, inclusive}`.
                let v = self.lower_expr(iter);
                let r = self.spill_always(v);
                let lo = Expr::index(r.clone(), Expr::Int(1));
                let hi = Expr::index(r.clone(), Expr::Int(2));
                let incl = Expr::index(r, Expr::Int(3));
                let t = self.new_temp();
                let adjusted = Expr::bin("or", Expr::bin("and", incl, hi.clone()), Expr::bin("-", hi, Expr::Int(1)));
                self.push(StmtKind::Local(vec![t.clone()], vec![adjusted]));
                (lo, Some(Expr::Name(t)), true)
            }
        };
        let control = self.new_temp();
        let mut body_stmts = self.nested(|this| this.bind_irrefutable(pat, Expr::name(&control)));
        body_stmts.extend(self.loop_body(e.id, Dest::Discard, body));
        match hi {
            Some(hi) if !u64_ => {
                let hi = if inclusive { hi } else { minus_one(hi) };
                self.push(StmtKind::NumFor(control, lo, hi, body_stmts));
            }
            hi => {
                // Open ranges and u64: a `while` loop with a checked step.
                let d = self.desc(elem, iter.span);
                let hi = hi.map(|h| self.spill_always(h));
                self.push(StmtKind::Local(vec![control.clone()], vec![lo]));
                let ctl = Expr::name(&control);
                let cond = match &hi {
                    None => Expr::Bool(true),
                    Some(h) if u64_ => {
                        let before = Expr::rt("ult", vec![ctl.clone(), h.clone()]);
                        if inclusive {
                            Expr::Unary("not", Box::new(Expr::rt("ult", vec![h.clone(), ctl.clone()])))
                        } else {
                            before
                        }
                    }
                    Some(h) => Expr::bin(if inclusive { "<=" } else { "<" }, ctl.clone(), h.clone()),
                };
                // Stepping past the last value of an inclusive range at the
                // type's maximum mustn't overflow: stop before stepping.
                let step = Stmt {
                    kind: StmtKind::Assign(
                        vec![ctl.clone()],
                        vec![Expr::rt("add_wrap", vec![d.clone(), ctl.clone(), Expr::Int(1)])],
                    ),
                    span: self.span,
                };
                let mut full = body_stmts;
                if let (Some(h), true) = (&hi, inclusive) {
                    let at_end = Stmt { kind: StmtKind::Break, span: self.span };
                    full.push(Stmt {
                        kind: StmtKind::If(vec![(Expr::bin("==", ctl.clone(), h.clone()), vec![at_end])], None),
                        span: self.span,
                    });
                }
                if hi.is_none() {
                    full.push(Stmt {
                        kind: StmtKind::Assign(vec![ctl.clone()], vec![Expr::rt("add", vec![d, ctl, Expr::Int(1)])]),
                        span: self.span,
                    });
                } else {
                    full.push(step);
                }
                self.push(StmtKind::While(cond, full));
            }
        }
        self.after_loop();
    }

    // ------------------------------------------------------------ match

    fn match_into(&mut self, scrutinee: &hir::Expr, arms: &[hir::Arm], dest: &Dest) {
        let v = self.lower_expr(scrutinee);
        let s = self.spill_always(v);
        let ty = self.ty_of(scrutinee);
        let done = self.new_label("m");
        let body = self.nested(|this| {
            for arm in arms {
                // An or-pattern with bindings is one arm per alternative, since
                // each alternative finds its bindings in different places.
                let alternatives: Vec<&hir::Pat> = match &arm.pat.kind {
                    PatKind::Or(alts) if binds_anything(&arm.pat) => alts.iter().collect(),
                    _ => vec![&arm.pat],
                };
                for pat in alternatives {
                    this.span = Some(arm.span);
                    let test = this.pat_test(pat, &s, ty);
                    let inner = this.nested(|this| {
                        this.bind_pat(pat, &s, true);
                        match &arm.guard {
                            Some(guard) => {
                                let g = this.lower_expr(guard);
                                let body = this.nested(|this| {
                                    this.lower_into(&arm.body, dest);
                                    this.push(StmtKind::Goto(done.clone()));
                                });
                                this.push(StmtKind::If(vec![(g, body)], None));
                            }
                            None => {
                                this.lower_into(&arm.body, dest);
                                this.push(StmtKind::Goto(done.clone()));
                            }
                        }
                    });
                    match test {
                        Some(test) => this.push(StmtKind::If(vec![(test, inner)], None)),
                        None => this.push(StmtKind::Do(inner)),
                    }
                }
            }
            let message = Expr::Str(b"loom internal: no match arm matched".to_vec());
            this.push(StmtKind::Call(Expr::call(Expr::name("error"), vec![message])));
        });
        self.push(StmtKind::Do(body));
        self.out.push(Stmt { kind: StmtKind::Label(done), span: None });
    }

    /// The condition under which `pat` matches `v`, or `None` if it always does.
    pub(super) fn pat_test(&mut self, pat: &hir::Pat, v: &Expr, ty: Ty) -> Option<Expr> {
        if let Some(test) = self.warp_pat_test(pat, v, ty) {
            return test;
        }
        if let Some(test) = self.wrapper_pat_test(pat, v, ty) {
            return test;
        }
        match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Error => None,
            PatKind::Lit { lit, negative } => {
                let value = self.lit(lit, *negative, ty);
                Some(self.compare(BinaryOp::Eq, v.clone(), value, ty, pat.span))
            }
            PatKind::Range { lo, hi, inclusive } => {
                let bound = |this: &mut Self, p: &Option<Box<hir::Pat>>| {
                    p.as_ref().and_then(|p| match &p.kind {
                        PatKind::Lit { lit, negative } => Some(this.lit(lit, *negative, ty)),
                        _ => None,
                    })
                };
                let (lo, hi) = (bound(self, lo), bound(self, hi));
                let mut tests = Vec::new();
                if let Some(lo) = lo {
                    tests.push(self.compare(BinaryOp::Le, lo, v.clone(), ty, pat.span));
                }
                if let Some(hi) = hi {
                    let op = if *inclusive { BinaryOp::Le } else { BinaryOp::Lt };
                    tests.push(self.compare(op, v.clone(), hi, ty, pat.span));
                }
                tests.into_iter().reduce(|a, b| Expr::bin("and", a, b))
            }
            PatKind::Tuple(pats) => {
                let items = match self.tcx().kind(ty) {
                    TyKind::Tuple(items) => items.clone(),
                    _ => return None,
                };
                let tests: Vec<Expr> = pats
                    .iter()
                    .zip(items)
                    .enumerate()
                    .filter_map(|(i, (p, t))| self.pat_test(p, &Expr::index(v.clone(), Expr::Int(i as i64 + 1)), t))
                    .collect();
                tests.into_iter().reduce(|a, b| Expr::bin("and", a, b))
            }
            PatKind::Or(alts) => {
                let mut tests = Vec::new();
                for alt in alts {
                    tests.push(self.pat_test(alt, v, ty)?);
                }
                tests.into_iter().reduce(|a, b| Expr::bin("or", a, b))
            }
            _ => {
                self.unsupported(pat.span, "this pattern");
                Some(Expr::Bool(false))
            }
        }
    }

    // ------------------------------------------------------------ assignment

    fn assign(&mut self, target: &hir::Expr, op: AssignOp, value: &hir::Expr) {
        match (&target.kind, op) {
            (ExprKind::Local(local), AssignOp::Assign) if self.overwrites(*local) => {
                // A whole new value for a `mut` parameter: written into the
                // caller's table, so the caller sees it.
                let v = self.lower_owned(value);
                let name = self.local_name(*local);
                self.push(StmtKind::Call(Expr::rt("overwrite", vec![Expr::Name(name), v])));
            }
            (ExprKind::Local(_) | ExprKind::Field { .. } | ExprKind::LeadingDot(_), AssignOp::Assign) => {
                let v = self.lower_owned(value);
                let place = self.lower_place(target);
                self.push(StmtKind::Assign(vec![place], vec![v]));
            }
            (ExprKind::Local(_) | ExprKind::Field { .. } | ExprKind::LeadingDot(_), AssignOp::Compound(bin)) => {
                let ty = self.ty_of(target);
                let v = self.lower_expr(value);
                let place = self.lower_place(target);
                let result = self.apply(bin, place.clone(), v, ty, target.span);
                self.push(StmtKind::Assign(vec![place], vec![result]));
            }
            (ExprKind::Tuple(_), AssignOp::Assign) => {
                // Every value is computed before anything is assigned, so
                // `(a, b) = (b, a)` swaps.
                let v = self.lower_expr(value);
                let t = self.spill_always(v);
                self.assign_parts(target, &t);
            }
            _ => self.unsupported(target.span, "this assignment"),
        }
    }

    fn assign_parts(&mut self, target: &hir::Expr, v: &Expr) {
        match &target.kind {
            ExprKind::Local(_) | ExprKind::Field { .. } | ExprKind::LeadingDot(_) => {
                let place = self.lower_place(target);
                self.push(StmtKind::Assign(vec![place], vec![v.clone()]));
            }
            ExprKind::Tuple(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.assign_parts(item, &Expr::index(v.clone(), Expr::Int(i as i64 + 1)));
                }
            }
            _ => self.unsupported(target.span, "this assignment"),
        }
    }

    /// `a op b` on two already-lowered values of type `ty` (for `op=`).
    fn apply(&mut self, op: BinaryOp, a: Expr, b: Expr, ty: Ty, span: Span) -> Expr {
        use BinaryOp as B;
        if let Some(float) = self.is_float(ty) {
            let v = match op {
                B::Add => Expr::bin("+", a, b),
                B::Sub => Expr::bin("-", a, b),
                B::Mul => Expr::bin("*", a, b),
                B::Div => Expr::bin("/", a, b),
                _ => Expr::call(Expr::field(Expr::name("math"), "fmod"), vec![a, b]),
            };
            return if float == FloatTy::F32 { Expr::rt("f32", vec![v]) } else { v };
        }
        let d = self.desc(ty, span);
        let name = match op {
            B::Add => "add",
            B::Sub => "sub",
            B::Mul => "mul",
            B::Div => "div",
            B::Rem => "rem",
            B::AddWrap => "add_wrap",
            B::SubWrap => "sub_wrap",
            B::MulWrap => "mul_wrap",
            B::AddSat => "add_sat",
            B::SubSat => "sub_sat",
            B::MulSat => "mul_sat",
            B::Shl => "shl",
            B::Shr => "shr",
            B::BitAnd => return Expr::bin("&", a, b),
            B::BitOr => return Expr::bin("|", a, b),
            B::BitXor => return Expr::bin("~", a, b),
            _ => {
                self.unsupported(span, "this compound assignment");
                return a;
            }
        };
        Expr::rt(name, vec![d, a, b])
    }
}

fn is_temp(e: &Expr) -> bool {
    matches!(e, Expr::Name(n) if n.starts_with("_t"))
}

fn has_call(e: &Expr) -> bool {
    match e {
        Expr::Call(..) => true,
        Expr::Binary(_, a, b) | Expr::Index(a, b) => has_call(a) || has_call(b),
        Expr::Unary(_, a) | Expr::Field(a, _) => has_call(a),
        Expr::Table(items) => items.iter().any(has_call),
        _ => false,
    }
}

/// `hi - 1` for an exclusive range's last value, folded for constants.
fn minus_one(hi: Expr) -> Expr {
    match hi {
        Expr::Int(i) if i > i64::MIN => Expr::Int(i - 1),
        hi => Expr::bin("-", hi, Expr::Int(1)),
    }
}

fn binds_anything(pat: &hir::Pat) -> bool {
    let mut locals = Vec::new();
    super::warp::collect_bindings(pat, &mut locals);
    !locals.is_empty()
}

/// Whether an expression reads any local (so it can't be moved to a call site).
fn mentions_locals(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Local(_) => true,
        ExprKind::Lit(_) | ExprKind::Def { .. } => false,
        ExprKind::Unary { operand, .. } => mentions_locals(operand),
        ExprKind::Binary { lhs, rhs, .. } => mentions_locals(lhs) || mentions_locals(rhs),
        ExprKind::Tuple(items) => items.iter().any(mentions_locals),
        ExprKind::Cast { expr, .. } => mentions_locals(expr),
        ExprKind::Template(parts) => parts.iter().any(|p| match p {
            TemplatePart::Interp { expr, .. } => mentions_locals(expr),
            TemplatePart::Text(_) => false,
        }),
        _ => true,
    }
}
