//! Lowering expressions, statements and patterns to LIR.
//!
//! Lua separates statements from expressions; Loom doesn't. An expression
//! that needs statements (`if`, `match`, `loop`, a block, `and` with a
//! statement on its right) delivers its value into a temporary, and the
//! statements go before the use. When an operand needs statements, the
//! operands to its left that were already lowered are spilled into
//! temporaries first, so Loom's left-to-right evaluation order holds.

use std::collections::HashMap;

use loom_syntax::ast::{AssignOp, BinaryOp, CastKind, Lit, ParamMode, UnaryOp};
use loom_syntax::source::Span;

use super::lir::{Expr, Stmt, StmtKind, TableKey};
use super::seq::Coll;
use super::{Dest, LoopInfo, Lowerer};
use crate::hir::{self, Block, DefKind, ExprKind, HirId, PatKind, StmtKind as HirStmt, TemplatePart};
use crate::ty::{FloatTy, IntTy, Ty, TyKind};
use crate::typeck::{ArgSource, Coercion, IterSource, Receiver};

/// How deep a value's Lua expression may nest before it goes into a
/// temporary (see `lower_expr`).
const MAX_NESTING: u32 = 24;

/// How many arguments one call may pass: each takes a register.
const MAX_ARGS: usize = 64;

/// `rt.cat(pieces…)`, in groups of at most [`MAX_ARGS`].
fn cat(mut pieces: Vec<Expr>) -> Expr {
    // A few pieces are one concatenation in the generated code (`a .. b ..
    // c`: one allocation, no call, no table); more go through the runtime.
    if pieces.len() <= 32 {
        return pieces
            .into_iter()
            .rev()
            .reduce(|rest, piece| Expr::bin("..", piece, rest))
            .unwrap_or_else(|| Expr::Str(Vec::new()));
    }
    while pieces.len() > MAX_ARGS {
        pieces = pieces.chunks(MAX_ARGS).map(|group| Expr::rt("cat", group.to_vec())).collect();
    }
    Expr::rt("cat", pieces)
}

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
    /// The type of an expression as checked (before its coercion). A gauge
    /// is lowered as its base type.
    pub(super) fn ty_of(&self, e: &hir::Expr) -> Ty {
        let ty = self.tables().expr_ty.get(&e.id).copied().unwrap_or(self.typed.tcx.error);
        self.tcx().degauge(ty)
    }

    pub(super) fn int_of(&self, ty: Ty) -> Option<IntTy> {
        self.tcx().as_int(ty)
    }

    fn is_float(&self, ty: Ty) -> Option<FloatTy> {
        match self.tcx().kind(self.tcx().degauge(ty)) {
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

    pub(super) fn spill(&mut self, v: Expr) -> Expr {
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
        let v = self.coerce(e, v);
        // Lua keeps a register for each pending call and operand, 255 at
        // most, so a long `a + b + c + …` goes through temporaries.
        if v.deeper_than(MAX_NESTING) { self.spill_always(v) } else { v }
    }

    pub(super) fn coerce(&mut self, e: &hir::Expr, v: Expr) -> Expr {
        let Some(coercion) = self.tables().coercions.get(&e.id) else { return v };
        match coercion {
            Coercion::Widen
            | Coercion::StrToString
            | Coercion::StringToStr
            | Coercion::Discard
            | Coercion::WidenUnion => v,
            Coercion::ToOptional => {
                let ty = self.ty_of(e);
                if self.opt_boxed(ty) { Expr::Table(vec![v]) } else { v }
            }
            Coercion::ToResult => Expr::Table(vec![Expr::Bool(true), v]),
            // The box owns its value: a `Copy` place goes in as a copy, or
            // changing it through the box would change the place.
            Coercion::ToAny(wefts) => {
                let wefts = wefts.clone();
                let v = self.copied_place(e, v);
                let vt = self.vtable(self.ty_of(e), wefts, e.span);
                Expr::rt("any", vec![vt, v])
            }
            Coercion::ToUnion => {
                let v = self.copied_place(e, v);
                let vt = self.vtable(self.ty_of(e), Vec::new(), e.span);
                Expr::rt("any", vec![vt, v])
            }
            Coercion::ToSlice => Expr::rt("view", vec![v]),
            Coercion::ToStream(source) => {
                let source = source.clone();
                // An `Iter` changes as its stream goes: a place gives a copy.
                let v = if matches!(source, IterSource::Woven { .. }) { self.copied_place(e, v) } else { v };
                self.stream_from(&source, v, e.span)
            }
        }
    }

    /// `v`, the value of `e`, copied if `e` is a place of a table-backed
    /// `Copy` type or part of a constant.
    fn copied_place(&mut self, e: &hir::Expr, v: Expr) -> Expr {
        let ty = self.ty_of(e);
        if self.copies_into_owner(e, ty) {
            let d = self.desc(ty, e.span);
            return self.copy_call(d, v);
        }
        v
    }

    /// Lowers several operands left to right, spilling earlier ones when a
    /// later one needs statements.
    pub(super) fn lower_operands(&mut self, es: &[&hir::Expr]) -> Vec<Expr> {
        let flagged: Vec<(&hir::Expr, bool)> = es.iter().map(|e| (*e, false)).collect();
        self.lower_operands_flagged(&flagged)
    }

    /// `lower_operands`, copying places when the operands are owned.
    pub(super) fn lower_operands_with(&mut self, es: &[&hir::Expr], owned: bool) -> Vec<Expr> {
        let flagged: Vec<(&hir::Expr, bool)> = es.iter().map(|e| (*e, owned)).collect();
        self.lower_operands_flagged(&flagged)
    }

    /// `lower_operands_flagged`, where the operands at `places` are places a
    /// call writes back to: each is lowered once, with its index and base in
    /// temporaries, and returned by operand number too.
    fn lower_operands_places(
        &mut self,
        es: &[(&hir::Expr, bool)],
        places: &[usize],
    ) -> (Vec<Expr>, HashMap<usize, Expr>) {
        if places.is_empty() {
            return (self.lower_operands_flagged(es), HashMap::new());
        }
        let mut vals: Vec<Expr> = Vec::with_capacity(es.len());
        let mut found = HashMap::new();
        for (k, &(e, owned)) in es.iter().enumerate() {
            let mark = self.out.len();
            let v = if places.contains(&k) && self.is_place(e) {
                let place = self.lower_place(e);
                found.insert(k, place.clone());
                place
            } else if owned {
                self.lower_owned(e)
            } else {
                self.lower_expr(e)
            };
            if self.out.len() > mark {
                let mut spills = Vec::new();
                for (j, prev) in vals.iter_mut().enumerate() {
                    // A place is read when the call starts, like a `mut`
                    // access in Swift; only its parts had to go first.
                    if found.contains_key(&j) || prev.is_constant() || is_temp(prev) {
                        continue;
                    }
                    let t = self.new_temp();
                    let value = std::mem::replace(prev, Expr::Name(t.clone()));
                    spills.push(Stmt { kind: StmtKind::Local(vec![t], vec![value]), span: self.span });
                }
                self.out.splice(mark..mark, spills);
            }
            vals.push(v);
        }
        (vals, found)
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

    /// A value known while compiling, of type `ty`, as a Lua value. A field
    /// or a case has none: reflection only exists while compiling.
    fn const_val(&mut self, v: crate::ty::ConstVal, ty: Ty, span: Span) -> Expr {
        use crate::ty::ConstVal as C;
        match v {
            C::Int(v) => self.int_value(v, ty),
            C::Bool(b) => Expr::Bool(b),
            C::Str(s) => Expr::Str(self.tcx().sym_str(s).as_bytes().to_vec()),
            // A unit case: its tag, or a table holding it.
            C::Unit(case) => {
                let info = self.p.def(case).parent.and_then(|w| self.warp(w)).cloned();
                match info.as_ref().and_then(|info| info.case_by_def(case).map(|c| (info.unit_only(), c.tag))) {
                    Some((true, tag)) => Expr::Int(tag as i64),
                    Some((false, tag)) => {
                        Expr::Record(vec![(super::lir::TableKey::Name("tag".into()), Expr::Int(tag as i64))])
                    }
                    None => Expr::Nil,
                }
            }
            C::Field { .. } | C::Case { .. } => {
                self.runtime_reflection(span);
                Expr::Nil
            }
        }
    }

    /// Reports reflection the program would need when it runs.
    pub(super) fn runtime_reflection(&mut self, span: Span) {
        // Once, though a `const for` writes the code out many times.
        let code = loom_syntax::diag::codes::K0111;
        if self.diags.iter().any(|d| d.code == code && d.labels.first().is_some_and(|l| l.span == span)) {
            return;
        }
        self.diags.push(
            loom_syntax::Diagnostic::error(loom_syntax::diag::codes::K0111, "reflection only exists while compiling")
                .primary(span, "needed when the program runs")
                .help("read what you need from it at compile time: `f.name`, `self.(f)`, `const for f in T.fields`"),
        );
    }

    pub(super) fn lower_raw(&mut self, e: &hir::Expr) -> Expr {
        let span = e.span;
        let ty = self.ty_of(e);
        if self.tables().lends.contains_key(&e.id) {
            return self.lend_place(e);
        }
        if let Some(v) = self.warp_value(e) {
            return v;
        }
        if let Some(&id) = self.tables().fn_values.get(&e.id) {
            return self.fn_value(id, span);
        }
        self.moved_away(e.id);
        // Known while compiling: a `const` parameter, a `const for` item,
        // what reflection says (`Self.name`).
        if let Some(&v) = self.tables().const_values.get(&e.id) {
            return self.const_val(v, ty, span);
        }
        if let Some(&q) = self.tables().frames.get(&e.id) {
            let n = self.frame_query(q, span);
            return self.const_val(crate::ty::ConstVal::Int(i128::from(n)), ty, span);
        }
        match &e.kind {
            ExprKind::Lambda { params, body } => self.lambda(e, params, body),
            ExprKind::Thread { body, .. } => self.thread(e, body),
            ExprKind::Lit(lit) => self.lit(lit, false, ty),
            ExprKind::Template(parts) => self.template(parts),
            ExprKind::Local(local) => match self.aliases.get(local) {
                Some(place) => place.clone(),
                None => Expr::Name(self.local_name(*local)),
            },
            ExprKind::Array(items) => self.array_lit(items),
            ExprKind::ArrayRepeat { value, count } => self.array_repeat(e, value, count),
            ExprKind::Index { base, args } => self.index(base, args),
            // `Atomic[T](value)`: its value in a table.
            ExprKind::Call { args, .. } if self.tables().sync_new.contains(&e.id) => {
                let value = self.lower_owned(&args[0].value);
                Expr::Table(vec![value])
            }
            ExprKind::Call { .. } if self.tables().new_lists.contains(&e.id) => match self.coll(ty) {
                Some(Coll::Map(..)) => Expr::rt("map_new", Vec::new()),
                Some(Coll::Set(_)) => Expr::rt("set_new", Vec::new()),
                None if self.seq(ty).is_none() => Expr::Str(Vec::new()),
                None => Self::new_list(),
            },
            ExprKind::Map(pairs) => self.map_lit(e, pairs),
            ExprKind::Def { def, .. } => match &self.p.def(*def).kind {
                DefKind::Const(_) => self.const_value(*def, ty),
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
            | ExprKind::Loom { .. }
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
            ExprKind::Cast { expr, kind, .. } => {
                // Into a gauge: converted to the base, then checked.
                let raw = self.tables().expr_ty.get(&e.id).copied().unwrap_or(ty);
                let target = match self.tcx().kind(raw) {
                    TyKind::Optional(t) => *t,
                    _ => raw,
                };
                match *self.tcx().kind(target) {
                    TyKind::Gauge(def, base) => self.gauge_cast(expr, *kind, def, base, span),
                    _ => self.cast(expr, *kind, ty),
                }
            }
            // A projection's: the place, as its table and its key.
            ExprKind::Yield { place: Some(_), value } => {
                let (t, k) = match self.lower_place(value) {
                    Expr::Index(t, k) => (*t, *k),
                    Expr::Field(t, name) => (*t, Expr::Str(name.into_bytes())),
                    _ => {
                        self.unsupported(value.span, "lending a local's own place (on the Lua backend)");
                        return Expr::Nil;
                    }
                };
                self.push(StmtKind::Call(Expr::rt("out", vec![Expr::Table(vec![t, k])])));
                Expr::Nil
            }
            // In a shuttle's body, which runs as a coroutine.
            ExprKind::Yield { value, .. } => {
                let v = self.lower_owned(value);
                self.push(StmtKind::Call(Expr::rt("out", vec![v])));
                Expr::Nil
            }
            ExprKind::Pull => {
                let boxed = match self.tcx().kind(ty) {
                    TyKind::Optional(u) => self.opt_boxed(*u),
                    _ => false,
                };
                Expr::rt("pull", vec![Expr::Bool(boxed)])
            }
            ExprKind::Const(inner) => self.lower_const(e, inner),
            ExprKind::Intrinsic(hir::Intrinsic::Target) => self.target_value(ty),
            ExprKind::Range { lo, hi, inclusive } => {
                let bounds: Vec<&hir::Expr> = lo.iter().chain(hi.iter()).map(|b| b.as_ref()).collect();
                let mut vals = self.lower_operands(&bounds);
                let hi = if hi.is_some() { vals.pop().unwrap_or(Expr::Nil) } else { Expr::Nil };
                let lo = vals.pop().unwrap_or(Expr::Nil);
                Expr::Table(vec![lo, hi, Expr::Bool(*inclusive)])
            }
            ExprKind::Error => Expr::Nil,
            // `T.fields` and the like, still needed when the program runs.
            ExprKind::Assoc { name, .. } if crate::typeck::REFLECTION_QUERIES.contains(&name.name.as_str()) => {
                self.runtime_reflection(span);
                Expr::Nil
            }
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
            Lit::Bytes(bytes) => {
                let mut entries = vec![(TableKey::Name("n".into()), Expr::Int(bytes.len() as i64))];
                for (i, &b) in bytes.iter().enumerate() {
                    entries.push((TableKey::Index(i as i64 + 1), Expr::Int(b as i64)));
                }
                Expr::Record(entries)
            }
            Lit::None => Expr::Nil,
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

    pub(super) fn float_value(&self, v: f64, ty: Ty) -> Expr {
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
            _ => cat(pieces),
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
        let name = match op {
            B::Add => "add",
            B::Sub => "sub",
            B::Mul => "mul",
            B::AddWrap => "add_wrap",
            B::SubWrap => "sub_wrap",
            B::MulWrap => "mul_wrap",
            B::AddSat => "add_sat",
            B::SubSat => "sub_sat",
            B::MulSat => "mul_sat",
            B::AddChecked => "add_chk",
            B::SubChecked => "sub_chk",
            B::MulChecked => "mul_chk",
            B::Div => "div",
            B::Rem => "rem",
            B::Shl => "shl",
            B::Shr => "shr",
            B::BitAnd => return Expr::bin("&", a, b),
            B::BitOr => return Expr::bin("|", a, b),
            B::BitXor => return Expr::bin("~", a, b),
            _ => return Expr::Nil,
        };
        self.op_call(name, d, a, b)
    }

    /// A copy of `v`, a value of the type whose descriptor is `d`: a call
    /// of the copier made for the type once, at the start of the bundle
    /// (`rt.spec("copy", d)`, see `rt.copier`), which is a constructor of
    /// the right shape rather than a walk of the descriptor.
    pub(super) fn copy_call(&mut self, d: Expr, v: Expr) -> Expr {
        if let Expr::Index(_, k) = &d
            && let Expr::Int(k) = k.as_ref()
        {
            return self.spec_call("copy", (*k - 1) as usize, usize::MAX, vec![v]);
        }
        Expr::rt("copy", vec![d, v])
    }

    /// `O[n](args…)`, `O[n]` being `rt.spec(name, T[desc])` (and the source
    /// type's `T[source]` for a conversion): made once at the start of the
    /// bundle for each (type, operation) used.
    pub(super) fn spec_call(&mut self, name: &'static str, desc: usize, source: usize, args: Vec<Expr>) -> Expr {
        let key = (desc, source, name);
        self.ops.entry(key).or_insert(());
        let n = self.ops.get_index_of(&key).expect("just made");
        Expr::call(Expr::index(Expr::name("O"), Expr::Int(n as i64 + 1)), args)
    }

    /// The index of descriptor `d` (`T[k]`), if it's one.
    pub(super) fn desc_index(d: &Expr) -> Option<usize> {
        match d {
            Expr::Index(_, k) => match k.as_ref() {
                Expr::Int(k) => Some((*k - 1) as usize),
                _ => None,
            },
            _ => None,
        }
    }

    /// The operation `name` of the runtime on integers of the type whose
    /// descriptor is `d`, applied to `a` and `b`. The common ones call a
    /// closure made for the type once (`O[n]`, see `rt.spec`); the rest
    /// call `rt.<name>(d, a, b)`.
    pub(super) fn op_call(&mut self, name: &'static str, d: Expr, a: Expr, b: Expr) -> Expr {
        const SPECIALIZED: [&str; 10] =
            ["add", "sub", "mul", "add_wrap", "sub_wrap", "mul_wrap", "div", "rem", "shl", "shr"];
        if SPECIALIZED.contains(&name)
            && let Expr::Index(_, k) = &d
            && let Expr::Int(k) = k.as_ref()
        {
            return self.spec_call(name, (*k - 1) as usize, usize::MAX, vec![a, b]);
        }
        Expr::rt(name, vec![d, a, b])
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
    /// `rt.cmp`) rather than Lua's operators: tuples, optionals, arrays and
    /// unions, and warps that are tables or that write their own `eq` or `lt`.
    fn compares_structurally(&self, ty: Ty) -> bool {
        match self.tcx().kind(ty) {
            TyKind::Tuple(_) | TyKind::Optional(_) | TyKind::Array(..) | TyKind::Union(_) => true,
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
        self.cast_to(inner, kind, target)
    }

    /// `inner` converted to `target` (with `as?`, `nil` when it doesn't fit).
    pub(super) fn cast_to(&mut self, inner: &hir::Expr, kind: CastKind, target: Ty) -> Expr {
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
                if matches!(name, "cast" | "cast_wrap")
                    && let (Some(to), Some(from)) = (Self::desc_index(&d), Self::desc_index(&sd))
                {
                    return self.spec_call(name, to, from, vec![v]);
                }
                Expr::rt(name, vec![d, sd, v])
            }
            (TyKind::Int(s), TyKind::Float(f)) => {
                // A Lua integer is a float by adding 0.0 (rounded once, as
                // the runtime does); only a u64's bit pattern needs more. An
                // f32 is that, rounded to 24 bits, while the integer is
                // exact in an f64.
                let exact = !(s.bits == 64 && !s.signed);
                match f {
                    FloatTy::F64 if exact => Expr::bin("+", v, Expr::Float(0.0)),
                    FloatTy::F32 if exact && s.bits <= 52 => Expr::rt("f32", vec![Expr::bin("+", v, Expr::Float(0.0))]),
                    _ => {
                        let sd = self.desc(source, span);
                        Expr::rt(if f == FloatTy::F32 { "itof32" } else { "itof" }, vec![sd, v])
                    }
                }
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
        if self.tables().closure_calls.contains(&call.id) {
            return self.call_value(callee, args);
        }
        // `.consume()` in a method takes `self`.
        self.moved_away(callee.id);
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
        // The runtime's generic methods (`List[T].push`) get the descriptor
        // of each type argument first.
        let mut descs = Vec::new();
        if runtime {
            let targs = self.typed.results.instance(info.callee).args.clone();
            for t in targs {
                descs.push(self.desc(t, callee.span));
            }
        }
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
        // `s.parse[u16]()`: the method is inside the type arguments.
        let method = match &callee.kind {
            ExprKind::Index { base, .. } => base.as_ref(),
            _ => callee,
        };
        let receiver_base = match (&info.receiver, &method.kind) {
            (Some(Receiver::Expr), ExprKind::Field { base, .. }) => Some(base.as_ref()),
            _ => None,
        };
        let receiver_slot = info.args.iter().position(|s| *s == ArgSource::Receiver);
        // A `Stream` method called on something else with items: on a
        // stream of them. A list's or a range's stream only reads it; an
        // `Iter`'s changes it, so it gets its own copy.
        let stream = self.tables().streams.get(&call.id).cloned();
        let reads_only = matches!(
            stream,
            Some(IterSource::Seq(_) | IterSource::Range(_) | IterSource::MapPairs(..) | IterSource::SetItems(_))
        );
        let mut operands: Vec<(&hir::Expr, bool)> = Vec::with_capacity(args.len() + 1);
        if let Some(base) = receiver_base {
            operands.push((base, owned(receiver_slot) && !reads_only));
        }
        for (i, arg) in args.iter().enumerate() {
            let slot = info.args.iter().position(|s| *s == ArgSource::Arg(i));
            operands.push((&arg.value, owned(slot)));
        }
        // Scalar `mut` arguments come back after the result and are written
        // to where they came from: each such place is worked out once.
        let modes = self.typed.results.sigs.get(&info.callee).map(|s| s.params.clone()).unwrap_or_default();
        let outs: Vec<usize> = modes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.mode == ParamMode::Mut && !self.mut_in_place(p.ty))
            .map(|(k, _)| k)
            .collect();
        let shift = usize::from(receiver_base.is_some());
        let out_operands: Vec<usize> = outs
            .iter()
            .filter_map(|&k| match info.args.get(k) {
                Some(ArgSource::Arg(i)) => Some(i + shift),
                Some(ArgSource::Receiver) if receiver_base.is_some() => Some(0),
                _ => None,
            })
            .collect();
        let (mut lowered, places) = self.lower_operands_places(&operands, &out_operands);
        let receiver = match (info.receiver, receiver_base) {
            (Some(Receiver::Expr), Some(_)) => Some(lowered.remove(0)),
            (Some(r), _) => Some(self.receiver(r, None)),
            (None, _) => None,
        };
        // A method a `through` field has is called on that field.
        let receiver = receiver.map(|r| info.through.iter().fold(r, Self::access));
        let receiver = match (receiver, &stream) {
            (Some(r), Some(source)) => Some(self.stream_from(source, r, call.span)),
            (r, _) => r,
        };
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
        descs.extend(params);
        // A length is a field of the table (an array, a list, a slice view
        // all keep it in `n`): no call, and no descriptor to pass.
        if runtime && outs.is_empty() && descs.len() == 4 && self.p.def_path(def) == "std.map.Map.has" {
            let k = descs.pop().unwrap_or(Expr::Nil);
            let m = descs.pop().unwrap_or(Expr::Nil);
            let dk = descs.swap_remove(0);
            return self.map_op("map_has", dk, vec![m, k]);
        }
        if runtime && outs.is_empty() && descs.len() == 2 {
            match self.p.def_path(def).as_str() {
                "std.slice.Slice.len" => return Expr::field(descs.pop().unwrap_or(Expr::Nil), "n"),
                "std.slice.Slice.is_empty" => {
                    let n = Expr::field(descs.pop().unwrap_or(Expr::Nil), "n");
                    return Expr::bin("==", n, Expr::Int(0));
                }
                _ => {}
            }
        }
        let call_expr = Expr::call(target, descs);

        if outs.is_empty() {
            return call_expr;
        }
        let result = self.new_temp();
        let temps: Vec<String> = outs.iter().map(|_| self.new_temp()).collect();
        let mut names = vec![result.clone()];
        names.extend(temps.iter().cloned());
        self.push(StmtKind::Local(names, vec![call_expr]));
        for (&k, temp) in outs.iter().zip(&temps) {
            let place = match info.args.get(k) {
                Some(ArgSource::Arg(i)) => match places.get(&(i + shift)) {
                    Some(place) => place.clone(),
                    None => self.lower_place(&args[*i].value),
                },
                Some(ArgSource::Receiver) => match (info.receiver, receiver_base) {
                    (Some(Receiver::Expr), Some(base)) => {
                        let place = match places.get(&0) {
                            Some(place) => place.clone(),
                            None => self.lower_place(base),
                        };
                        info.through.iter().fold(place, Self::access)
                    }
                    (Some(r), _) => self.receiver(r, None),
                    _ => continue,
                },
                _ => continue,
            };
            self.push(StmtKind::Assign(vec![place], vec![Expr::name(temp.clone())]));
        }
        Expr::Name(result)
    }

    // ------------------------------------------------------------ statements and control flow

    /// Lowers `e` so its value reaches `dest`.
    pub(super) fn lower_into(&mut self, e: &hir::Expr, dest: &Dest) {
        let has_coercion = self.tables().coercions.get(&e.id).is_some_and(|c| {
            !matches!(c, Coercion::Widen | Coercion::StrToString | Coercion::StringToStr | Coercion::WidenUnion)
        });
        let control = matches!(
            e.kind,
            ExprKind::Block(_)
                | ExprKind::Loom { .. }
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
            // What projections lent is done with once the value is read.
            Dest::Return if !self.lend_ends.is_empty() => {
                let t = self.spill_always(v);
                self.finish_lends(0);
                self.push(StmtKind::Return(Some(t)));
            }
            Dest::Return => self.push(StmtKind::Return(Some(v))),
        }
    }

    fn lower_control(&mut self, e: &hir::Expr, dest: &Dest) {
        match &e.kind {
            ExprKind::Block(block) => {
                let body = self.nested(|this| this.block_into(block, dest));
                self.push(StmtKind::Do(body));
            }
            ExprKind::Loom { body, .. } => self.loom_block(body, dest),
            // `const if`: only the branch taken.
            ExprKind::If { then, else_, deferred: Some(_), .. } => {
                match (self.tables().const_ifs.get(&e.id).copied(), else_) {
                    (Some(true), _) => {
                        let body = self.nested(|this| this.block_into(then, dest));
                        self.push(StmtKind::Do(body));
                    }
                    (Some(false), Some(else_)) => self.lower_into(else_, dest),
                    _ => self.deliver(Expr::Nil, dest),
                }
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
            // `const for`: the body once per item, each with its own tables.
            ExprKind::For { body, is_const: true, .. } => {
                for id in self.tables().unrolled.get(&e.id).cloned().unwrap_or_default() {
                    let stmts = self.in_body(id, |this| this.nested(|this| this.block_into(body, &Dest::Discard)));
                    self.push(StmtKind::Do(stmts));
                }
            }
            ExprKind::For { pat, iter, body, .. } => match self.seq(self.ty_of(iter)) {
                Some(seq) => self.for_seq(e, pat, iter, body, seq),
                None if matches!(self.tcx().kind(self.ty_of(iter)), TyKind::Range(_)) => {
                    self.for_range(e, pat, iter, body)
                }
                None => self.for_stream(e, pat, iter, body),
            },
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
        // A fallback defer's flag is a local of this block.
        let flags = self.live_flags.clone();
        // What comes after the block is reported where it was before it.
        let span = self.span;
        self.block_stmts_into(last, rest, dest);
        self.span = span;
        self.live_flags = flags;
    }

    fn block_stmts_into(&mut self, last: &hir::Stmt, rest: &[hir::Stmt], dest: &Dest) {
        for stmt in rest {
            self.stmt(stmt);
        }
        match &last.kind {
            HirStmt::Expr(tail) if !matches!(tail.kind, ExprKind::Assign { .. }) => {
                self.span = Some(last.span);
                let discard = self.tables().coercions.get(&tail.id) == Some(&Coercion::Discard);
                let lent = self.lend_ends.len();
                self.lower_into(tail, if discard { &Dest::Discard } else { dest });
                self.finish_lends(lent);
            }
            _ => self.stmt(last),
        }
    }

    fn stmt(&mut self, stmt: &hir::Stmt) {
        let lent = self.lend_ends.len();
        self.stmt_inner(stmt);
        self.finish_lends(lent);
    }

    /// Resumes the projections that lent places since `from` to their
    /// ends: their statements are done.
    pub(super) fn finish_lends(&mut self, from: usize) {
        for r in self.lend_ends.split_off(from).into_iter().rev() {
            self.push(StmtKind::Call(Expr::rt("lend_end", vec![r])));
        }
    }

    /// A call of a projection (`g.at(1, 2)`): the place it lends, `t[k]`
    /// with the table and the key it yielded, until the statement ends.
    pub(super) fn lend_place(&mut self, e: &hir::Expr) -> Expr {
        let ExprKind::Call { callee, args } = &e.kind else { return Expr::Nil };
        let sh = self.call(e, callee, args);
        let r = self.spill_always(Expr::rt("lend", vec![sh]));
        self.lend_ends.push(r.clone());
        Expr::index(Expr::index(r.clone(), Expr::Int(1)), Expr::index(r, Expr::Int(2)))
    }

    fn stmt_inner(&mut self, stmt: &hir::Stmt) {
        self.span = Some(stmt.span);
        match &stmt.kind {
            HirStmt::Let { pat, init, .. } => {
                // `let (a, b) = (x, y)` of plain values is `local a, b = x, y`:
                // no table made to take apart.
                if let (PatKind::Tuple(pats), ExprKind::Tuple(items)) = (&pat.kind, &init.kind)
                    && pats.len() == items.len()
                    && !items.is_empty()
                    && pats.iter().all(|p| matches!(p.kind, PatKind::Bind(_)))
                    && items.iter().all(|item| !self.is_table(self.ty_of(item)))
                {
                    let refs: Vec<&hir::Expr> = items.iter().collect();
                    let vals = self.lower_operands(&refs);
                    let names = pats
                        .iter()
                        .filter_map(|p| match p.kind {
                            PatKind::Bind(local) => Some(self.local_name(local)),
                            _ => None,
                        })
                        .collect();
                    self.push(StmtKind::Local(names, vals));
                    return;
                }
                let v = self.lower_owned(init);
                self.bind_irrefutable(pat, v);
            }
            HirStmt::Expr(e) => self.lower_into(e, &Dest::Discard),
            HirStmt::Item(_) => {}
            HirStmt::Defer { binding, body } => self.defer(*binding, body, stmt.span),
            HirStmt::Given { .. } | HirStmt::Spool { .. } => {
                self.unsupported(stmt.span, "this statement");
            }
        }
    }

    /// Binds an irrefutable pattern (`let`, `for`) to a value.
    pub(super) fn bind_irrefutable(&mut self, pat: &hir::Pat, v: Expr) {
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
    pub(super) fn loop_body(&mut self, id: HirId, dest: Dest, body: &Block) -> Vec<Stmt> {
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
    pub(super) fn after_loop(&mut self) {
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
                // A range value, `{lo, hi, inclusive}`, which may be open or
                // empty at any bound: its stream knows how to step it.
                let v = self.lower_expr(iter);
                let s = self.stream_from(&IterSource::Range(elem), v, iter.span);
                self.for_stream_of(e, pat, s, body);
                return;
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
                        vec![self.op_call("add_wrap", d.clone(), ctl.clone(), Expr::Int(1))],
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
                        kind: StmtKind::Assign(vec![ctl.clone()], vec![self.op_call("add", d, ctl, Expr::Int(1))]),
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
        // What comes after the arms is reported where it was before them.
        let span = self.span;
        self.match_arms_into(scrutinee, arms, dest);
        self.span = span;
    }

    fn match_arms_into(&mut self, scrutinee: &hir::Expr, arms: &[hir::Arm], dest: &Dest) {
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
        self.assign_inner(target, op, value);
        if op == AssignOp::Assign {
            self.rearm(target);
        }
    }

    /// Each local `target` gives a new value is a new value for its
    /// fallback `defer` to clean up.
    fn rearm(&mut self, target: &hir::Expr) {
        match &target.kind {
            ExprKind::Local(l) => {
                for flag in self.live_flags.get(l).cloned().unwrap_or_default() {
                    self.push(StmtKind::Assign(vec![Expr::name(flag)], vec![Expr::Bool(true)]));
                }
            }
            ExprKind::Tuple(items) => items.iter().for_each(|item| self.rearm(item)),
            _ => {}
        }
    }

    fn assign_inner(&mut self, target: &hir::Expr, op: AssignOp, value: &hir::Expr) {
        if let ExprKind::Index { base, args } = &target.kind
            && let Some(Coll::Map(k, v)) = self.coll(self.ty_of(base))
            && let [hir::GenericArg::Expr(key)] = args.as_slice()
        {
            return self.map_assign(base, key, k, v, op, value, target.span);
        }
        if self.tables().lends.contains_key(&target.id) {
            let v = if op == AssignOp::Assign { self.lower_owned(value) } else { self.lower_expr(value) };
            let place = self.lower_place(target);
            let result = match op {
                AssignOp::Assign => v,
                AssignOp::Compound(bin) => self.apply(bin, place.clone(), v, self.ty_of(target), target.span),
            };
            self.push(StmtKind::Assign(vec![place], vec![result]));
            return;
        }
        match (&target.kind, op) {
            (ExprKind::Local(local), AssignOp::Assign) if self.overwrites(*local) => {
                // A whole new value for a `mut` parameter: written into the
                // caller's table, so the caller sees it.
                let v = self.lower_owned(value);
                let name = self.local_name(*local);
                self.push(StmtKind::Call(Expr::rt("overwrite", vec![Expr::Name(name), v])));
            }
            (
                ExprKind::Local(_)
                | ExprKind::Field { .. }
                | ExprKind::ConstField { .. }
                | ExprKind::LeadingDot(_)
                | ExprKind::Index { .. },
                AssignOp::Assign,
            ) => {
                let v = self.lower_owned(value);
                let place = self.lower_place(target);
                self.push(StmtKind::Assign(vec![place], vec![v]));
            }
            (
                ExprKind::Local(_)
                | ExprKind::Field { .. }
                | ExprKind::ConstField { .. }
                | ExprKind::LeadingDot(_)
                | ExprKind::Index { .. },
                AssignOp::Compound(bin),
            ) => {
                let ty = self.ty_of(target);
                let v = self.lower_expr(value);
                let place = self.lower_place(target);
                let result = self.apply(bin, place.clone(), v, ty, target.span);
                self.push(StmtKind::Assign(vec![place], vec![result]));
            }
            (ExprKind::Tuple(targets), AssignOp::Assign)
                if let ExprKind::Tuple(items) = &value.kind
                    && targets.len() == items.len()
                    && targets.iter().all(|t| {
                        matches!(&t.kind, ExprKind::Local(l) if !self.overwrites(*l)) && !self.is_table(self.ty_of(t))
                    })
                    && items.iter().all(|item| !self.is_table(self.ty_of(item))) =>
            {
                // Of plain values to plain locals, one multiple assignment:
                // Lua computes every value before it assigns any, so
                // `(a, b) = (b, a)` swaps, with no table made.
                let refs: Vec<&hir::Expr> = items.iter().collect();
                let vals = self.lower_operands(&refs);
                let places: Vec<Expr> = targets.iter().map(|t| self.lower_place(t)).collect();
                self.push(StmtKind::Assign(places, vals));
            }
            (ExprKind::Tuple(_), AssignOp::Assign) => {
                // Every value is computed before anything is assigned, so
                // `(a, b) = (b, a)` swaps.
                let v = self.lower_expr(value);
                let t = self.spill_always(v);
                let mut overwrites = Vec::new();
                let copies = self.writes_into_caller(target);
                self.assign_parts(target, &t, copies, &mut overwrites);
                for (name, part) in overwrites {
                    self.push(StmtKind::Call(Expr::rt("overwrite", vec![Expr::Name(name), part])));
                }
            }
            _ => self.unsupported(target.span, "this assignment"),
        }
    }

    /// Whether `target` has a `mut` parameter among its parts, which is
    /// written into rather than given a new value.
    fn writes_into_caller(&self, target: &hir::Expr) -> bool {
        match &target.kind {
            ExprKind::Local(local) => self.overwrites(*local),
            ExprKind::Tuple(items) => items.iter().any(|item| self.writes_into_caller(item)),
            _ => false,
        }
    }

    /// Assigns the parts of `v` to the parts of `target`. A `mut` parameter
    /// is written into after every other part. When one is, each part that's
    /// a table is copied first, since it may be a written-into table's old
    /// value (`(a, b) = (b, a)`).
    fn assign_parts(&mut self, target: &hir::Expr, v: &Expr, copies: bool, overwrites: &mut Vec<(String, Expr)>) {
        let ty = self.ty_of(target);
        let part = |this: &mut Self| {
            if copies && this.is_table(ty) {
                let d = this.desc(ty, target.span);
                let copied = this.copy_call(d, v.clone());
                this.spill_always(copied)
            } else {
                v.clone()
            }
        };
        match &target.kind {
            ExprKind::Local(local) if self.overwrites(*local) => {
                let part = part(self);
                overwrites.push((self.local_name(*local), part));
            }
            ExprKind::Local(_) | ExprKind::Field { .. } | ExprKind::LeadingDot(_) => {
                let part = part(self);
                let place = self.lower_place(target);
                self.push(StmtKind::Assign(vec![place], vec![part]));
            }
            ExprKind::Tuple(items) => {
                for (i, item) in items.iter().enumerate() {
                    let v = Expr::index(v.clone(), Expr::Int(i as i64 + 1));
                    self.assign_parts(item, &v, copies, overwrites);
                }
            }
            _ => self.unsupported(target.span, "this assignment"),
        }
    }

    /// `a op b` on two already-lowered values of type `ty` (for `op=`).
    pub(super) fn apply(&mut self, op: BinaryOp, a: Expr, b: Expr, ty: Ty, span: Span) -> Expr {
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
        self.op_call(name, d, a, b)
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
        // Compile-time code can't read the locals around it.
        ExprKind::Lit(_) | ExprKind::Def { .. } | ExprKind::Const(_) => false,
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
