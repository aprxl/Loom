//! Lowering warps: construction, fields, receivers, copies, dot-blocks, and
//! case and twill patterns.
//!
//! Representation (see design/lua-backend.md):
//! - a record is `{x = …, y = …}`, a tuple warp `{[1] = …}`;
//! - a case is `{tag = k, …payload, …shared fields}`, a fresh table every time;
//! - a warp whose cases have no payloads and no shared fields is its tag, an
//!   integer from 1.
//!
//! Value semantics: Lua shares tables, Loom copies `Copy` values. Every
//! expression that isn't a place makes a fresh table, so a copy is only
//! needed where a *place* (a variable or a field of one) of a table-backed
//! `Copy` type flows into a slot that owns it: a `let`/`var`, an assignment,
//! a constructor argument, a tuple element, a `take` argument, a returned or
//! `break` value, or the subject of a copying dot-block.

use loom_syntax::ast::FieldName;

use super::lir::{Expr, StmtKind, TableKey};
use super::{Dest, Lowerer};
use crate::hir::{self, DefId, DefKind, ExprKind, LocalId, PatKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::{Construct, FieldSource, Key, Receiver, WarpInfo};

impl Lowerer<'_> {
    pub(super) fn warp(&self, def: DefId) -> Option<&WarpInfo> {
        self.typed.results.warps.get(&def)
    }

    /// `base.key`.
    pub(super) fn access(base: Expr, key: &Key) -> Expr {
        match key {
            Key::Name(n) => Expr::field(base, n),
            Key::Pos(i) => Expr::index(base, Expr::Int(*i as i64)),
        }
    }

    fn table_key(key: &Key) -> TableKey {
        match key {
            Key::Name(n) => TableKey::Name(n.clone()),
            Key::Pos(i) => TableKey::Index(*i as i64),
        }
    }

    // ------------------------------------------------------------ copies

    /// Whether values of `ty` are copied on assignment: `Copy` types.
    pub(super) fn is_copy(&self, ty: Ty) -> bool {
        match self.tcx().kind(ty) {
            TyKind::Tuple(items) => items.iter().all(|&t| self.is_copy(t)),
            TyKind::Optional(t) => self.is_copy(*t),
            TyKind::Result(t, e) => self.is_copy(*t) && self.is_copy(*e),
            TyKind::Adt(def, _) if self.is_string(*def) => false,
            TyKind::Array(t, _) => self.is_copy(*t),
            // A slice is a view, and a list is moved.
            TyKind::Adt(..) if self.seq(ty).is_some() => false,
            // Copy-ness depends on a generic warp's arguments.
            TyKind::Adt(..) => self.typed.results.adts.get(&ty).is_some_and(|w| w.copy),
            TyKind::Fn(..) | TyKind::Any(_) | TyKind::Union(_) => false,
            _ => true,
        }
    }

    /// Whether values of `ty` are Lua tables.
    pub(super) fn is_table(&self, ty: Ty) -> bool {
        match self.tcx().kind(ty) {
            TyKind::Tuple(items) => !items.is_empty(),
            TyKind::Optional(t) => self.is_table(*t),
            TyKind::Adt(..) if self.seq(ty).is_some() => true,
            // The runtime's warps are tables, except `String`, a Lua string,
            // and `Optional[T]`, the methods of `?T`.
            TyKind::Adt(def, _) if self.p.def(*def).attrs.iter().any(|a| a.name == "builtin") => {
                !self.is_string(*def) && self.p.def_path(*def) != "std.option.Optional"
            }
            TyKind::Adt(def, _) => self.warp(*def).is_some_and(|w| !w.unit_only()),
            TyKind::Any(_) | TyKind::Union(_) | TyKind::Result(..) | TyKind::Array(..) => true,
            _ => false,
        }
    }

    /// Whether a `mut` parameter of type `ty` is the caller's table, written
    /// into in place; otherwise it's copied in and out. A `?T` isn't, even
    /// when `T` is a table: `none` is `nil`, which nothing can be written
    /// into.
    pub(super) fn mut_in_place(&self, ty: Ty) -> bool {
        self.is_table(ty) && !matches!(self.tcx().kind(ty), TyKind::Optional(_))
    }

    /// Whether `e` names a place: a variable, a constant (computed once,
    /// and read by every use), or a field of one.
    pub(super) fn is_place(&self, e: &hir::Expr) -> bool {
        match &e.kind {
            ExprKind::Local(_) => true,
            _ if self.is_const(e) => true,
            ExprKind::Field { base, .. } => self.is_place(base),
            ExprKind::Index { base, args } => {
                !matches!(args.as_slice(), [crate::hir::GenericArg::Expr(i)] if matches!(i.kind, ExprKind::Range { .. }))
                    && (self.seq(self.ty_of(base)).is_some()
                        || matches!(self.coll(self.ty_of(base)), Some(super::seq::Coll::Map(..))))
            }
            ExprKind::LeadingDot(_) => self.tables().fields.contains_key(&e.id),
            _ => false,
        }
    }

    /// `target`, as a value: a `std.target.Target` record.
    pub(super) fn target_value(&self, ty: Ty) -> Expr {
        let TyKind::Adt(def, _) = self.tcx().kind(ty) else { return Expr::Nil };
        let t = crate::typeck::TARGET;
        let module = self.p.def(*def).module;
        // A unit case is its tag: its place among the warp's cases, from 1.
        let tag = |warp: &str, case: &str| {
            let tag = self.p.module(module).items.get(warp).and_then(|&w| match &self.p.def(w).kind {
                DefKind::Warp(w) => w.cases.iter().position(|&c| self.p.def(c).name == case),
                _ => None,
            });
            Expr::Int(tag.map_or(0, |i| i as i64 + 1))
        };
        Expr::Record(vec![
            (TableKey::Name("os".into()), tag("Os", t.os)),
            (TableKey::Name("arch".into()), tag("Arch", t.arch)),
            (TableKey::Name("endian".into()), tag("Endian", t.endian)),
            (TableKey::Name("pointer_bits".into()), Expr::Int(i64::from(t.pointer_bits))),
        ])
    }

    /// Whether `e` is a constant that lives in `C`: a typed one, or a
    /// `const` expression.
    fn is_const(&self, e: &hir::Expr) -> bool {
        let typed_const = |def: &DefId| {
            matches!(self.p.def(*def).kind, DefKind::Const(_))
                && !self.typed.results.untyped_consts.contains_key(def)
                && !self.typed.results.untyped_floats.contains_key(def)
        };
        match &e.kind {
            ExprKind::Def { def, .. } => typed_const(def),
            ExprKind::Const(inner) => crate::typeck::assert_args(self.p, inner).is_none(),
            ExprKind::Assoc { .. } => matches!(
                self.tables().assoc_values.get(&e.id),
                Some(crate::typeck::AssocValue::Const(def)) if typed_const(def)
            ),
            ExprKind::LeadingDot(_) => self.tables().leading_consts.get(&e.id).is_some_and(typed_const),
            _ => false,
        }
    }

    /// Whether `e` is a constant or part of one (`TABLE[i]`, `CONFIG.name`).
    fn in_const(&self, e: &hir::Expr) -> bool {
        match &e.kind {
            ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => self.is_place(e) && self.in_const(base),
            _ => self.is_const(e),
        }
    }

    /// Whether the value of `e`, of type `ty`, must be copied for a slot
    /// that owns it, so the two don't share a table: a place of a `Copy`
    /// type, or any part of a constant (every use of a constant gets a
    /// value of its own).
    pub(super) fn copies_into_owner(&self, e: &hir::Expr, ty: Ty) -> bool {
        self.is_table(ty) && ((self.is_place(e) && self.is_copy(ty)) || self.in_const(e))
    }

    /// Lowers `e` for a slot that owns its value, copied if
    /// [`copies_into_owner`](Self::copies_into_owner) says so.
    pub(super) fn lower_owned(&mut self, e: &hir::Expr) -> Expr {
        let ty = self.ty_of(e);
        if self.copies_into_owner(e, ty) {
            let v = self.lower_raw(e);
            let d = self.desc(ty, e.span);
            let copied = Expr::rt("copy", vec![d, v]);
            return self.coerce(e, copied);
        }
        self.lower_expr(e)
    }

    /// `lower_operands`, for owning slots.
    pub(super) fn lower_owned_operands(&mut self, es: &[&hir::Expr]) -> Vec<Expr> {
        self.lower_operands_with(es, true)
    }

    // ------------------------------------------------------------ construction

    /// A new value of a warp: `Point(3, 4)`, `Shape.Circle(r: 2.0)`, `.Dot`.
    pub(super) fn construct(&mut self, c: &Construct, args: &[hir::Arg]) -> Expr {
        let Some(info) = self.warp(c.warp).cloned() else { return Expr::Nil };
        let tag = c.case.and_then(|case| info.case_by_def(case)).map(|c| c.tag);
        if info.unit_only() {
            return Expr::Int(tag.unwrap_or(1) as i64);
        }
        // Arguments run in source order, whatever order the fields take them in.
        let values: Vec<&hir::Expr> = args.iter().map(|a| &a.value).collect();
        let mut lowered = self.lower_owned_operands(&values);
        let order = c.fields.iter().filter_map(|(_, s)| match s {
            FieldSource::Arg(i) => Some(*i),
            FieldSource::Default(_) => None,
        });
        if !order.is_sorted() {
            for v in lowered.iter_mut() {
                if !v.is_constant() {
                    let t = self.new_temp();
                    let value = std::mem::replace(v, Expr::Name(t.clone()));
                    self.push_local(t, value);
                }
            }
        }
        let mut entries = Vec::with_capacity(c.fields.len() + 1);
        if let Some(tag) = tag {
            entries.push((TableKey::Name("tag".into()), Expr::Int(tag as i64)));
        }
        for (key, source) in &c.fields {
            let value = match source {
                FieldSource::Arg(i) => lowered[*i].clone(),
                FieldSource::Default(field) => match &self.p.def(*field).kind {
                    DefKind::Field(f) => match &f.default {
                        Some(default) => self.in_body(c.defaults, |this| this.lower_owned(default)),
                        None => Expr::Nil,
                    },
                    _ => Expr::Nil,
                },
            };
            entries.push((Self::table_key(key), value));
        }
        Expr::Record(entries)
    }

    // ------------------------------------------------------------ fields and receivers

    /// The value a method or field is reached through.
    pub(super) fn receiver(&mut self, receiver: Receiver, base: Option<&hir::Expr>) -> Expr {
        match receiver {
            Receiver::Expr => base.map_or(Expr::Nil, |b| self.lower_expr(b)),
            Receiver::Local(l) => Expr::Name(self.local_name(l)),
            Receiver::Subject(id) => Expr::Name(self.subjects.get(&id).cloned().unwrap_or_default()),
        }
    }

    /// A field read: `p.x`, `t.0`, or a leading `.x`.
    pub(super) fn field_value(&mut self, e: &hir::Expr) -> Option<Expr> {
        let access = self.tables().fields.get(&e.id)?.clone();
        let base = match &e.kind {
            ExprKind::Field { base, .. } => Some(base.as_ref()),
            _ => None,
        };
        let v = self.receiver(access.base, base);
        Some(Self::access(v, &access.key))
    }

    /// Something that can be assigned to, as a Lua expression.
    pub(super) fn lower_place(&mut self, e: &hir::Expr) -> Expr {
        match &e.kind {
            ExprKind::Local(l) => match self.aliases.get(l) {
                Some(place) => place.clone(),
                None => Expr::Name(self.local_name(*l)),
            },
            ExprKind::Index { base, args } => self.index(base, args),
            ExprKind::Field { base, .. } => {
                let key = self.tables().fields.get(&e.id).map(|f| f.key.clone());
                let base = if self.is_place(base) { self.lower_place(base) } else { self.lower_expr(base) };
                match key {
                    Some(key) => Self::access(base, &key),
                    None => base,
                }
            }
            // A constant: `C.NAME`, which every read shares.
            ExprKind::Def { .. } | ExprKind::Const(_) | ExprKind::Assoc { .. } => self.lower_raw(e),
            _ => match self.field_value(e) {
                Some(v) => v,
                None => self.lower_raw(e),
            },
        }
    }

    /// Whether assigning a whole new value to local `l` must write into the
    /// table the caller passed (`mut` parameters and `mut self`).
    pub(super) fn overwrites(&self, l: LocalId) -> bool {
        let local = self.p.local(l);
        let ty = self.tables().local_ty.get(&l).copied().unwrap_or(self.typed.tcx.error);
        local.mutable
            && matches!(local.kind, crate::hir::LocalKind::Param | crate::hir::LocalKind::SelfParam)
            && self.mut_in_place(ty)
    }

    // ------------------------------------------------------------ dot-blocks

    /// `subject.{ … }`: edits the subject in place (and is `()`), or edits a
    /// copy and is that copy.
    pub(super) fn dot_block(&mut self, e: &hir::Expr, subject: &hir::Expr, body: &hir::Block) -> Expr {
        let in_place = self.tables().dot_blocks.get(&e.id).copied().unwrap_or(false);
        let value = if in_place { self.lower_place(subject) } else { self.lower_owned(subject) };
        let name = self.new_temp();
        self.push_local(name.clone(), value);
        self.subjects.insert(e.id, name.clone());
        let stmts = self.nested(|this| this.block_into(body, &Dest::Discard));
        self.push(StmtKind::Do(stmts));
        if in_place { Expr::Nil } else { Expr::Name(name) }
    }

    // ------------------------------------------------------------ `is`

    /// `x is pat`: a boolean, and when it's true, the pattern's bindings.
    pub(super) fn is_expr(&mut self, expr: &hir::Expr, pat: &hir::Pat) -> Expr {
        let ty = self.ty_of(expr);
        let v = self.lower_expr(expr);
        let s = self.spill_always(v);
        let mut locals = Vec::new();
        collect_bindings(pat, &mut locals);
        let test = self.pat_test(pat, &s, ty);
        if locals.is_empty() {
            return test.unwrap_or(Expr::Bool(true));
        }
        // The bindings exist after the test, set only if it matched.
        let names: Vec<String> = locals.iter().map(|&l| self.local_name(l)).collect();
        self.push(StmtKind::Local(names, Vec::new()));
        let result = self.new_temp();
        self.push_local(result.clone(), Expr::Bool(false));
        let mut body = self.nested(|this| this.bind_pat(pat, &s, false));
        body.push(super::lir::Stmt {
            kind: StmtKind::Assign(vec![Expr::name(&result)], vec![Expr::Bool(true)]),
            span: self.span,
        });
        match test {
            Some(test) => self.push(StmtKind::If(vec![(test, body)], None)),
            None => self.out.extend(body),
        }
        Expr::Name(result)
    }

    // ------------------------------------------------------------ patterns

    /// The type a pattern was checked against.
    pub(super) fn pat_ty(&self, pat: &hir::Pat) -> Ty {
        let ty = self.tables().pat_ty.get(&pat.id).copied().unwrap_or(self.typed.tcx.error);
        self.tcx().degauge(ty)
    }

    /// The test for a case, record or twill pattern (see `pat_test`).
    pub(super) fn warp_pat_test(&mut self, pat: &hir::Pat, v: &Expr, ty: Ty) -> Option<Option<Expr>> {
        // `x is Cents`, `c: Cents =>`: the gauge's check.
        if let Some(&gauge) = self.tables().gauge_tests.get(&pat.id)
            && let TyKind::Gauge(def, _) = *self.tcx().kind(gauge)
        {
            let check = self.gauge_check(def);
            return Some(Some(Expr::call(check, vec![v.clone()])));
        }
        match &pat.kind {
            PatKind::Case { fields, .. } => {
                if let Some(&def) = self.tables().const_pats.get(&pat.id) {
                    let value = self.const_value(def, ty);
                    return Some(Some(self.compare(loom_syntax::ast::BinaryOp::Eq, v.clone(), value, ty, pat.span)));
                }
                let case_pat = self.tables().case_pats.get(&pat.id)?.clone();
                let info = self.warp(case_pat.warp)?.clone();
                let mut tests = Vec::new();
                if let Some(case) = case_pat.case.and_then(|c| info.case_by_def(c)) {
                    let tag = Expr::Int(case.tag as i64);
                    let actual = if info.unit_only() { v.clone() } else { Expr::field(v.clone(), "tag") };
                    tests.push(Expr::bin("==", actual, tag));
                }
                for (f, key) in fields.iter().flatten().zip(&case_pat.keys) {
                    let sub_ty = self.pat_ty(&f.pat);
                    if let Some(t) = self.pat_test(&f.pat, &Self::access(v.clone(), key), sub_ty) {
                        tests.push(t);
                    }
                }
                Some(tests.into_iter().reduce(|a, b| Expr::bin("and", a, b)))
            }
            // `is b: Button`: the value's vtable describes a `Button`.
            PatKind::TypeTest { .. } => {
                let tested = self.tables().type_tests.get(&pat.id).copied()?;
                let desc = self.desc(tested, pat.span);
                let actual = Expr::field(Expr::index(v.clone(), Expr::Int(2)), "d");
                Some(Some(Expr::bin("==", actual, desc)))
            }
            PatKind::Twill { pattern, .. } => {
                if pattern.mask == 0 {
                    return Some(None);
                }
                let masked = Expr::bin("&", v.clone(), Expr::Int(pattern.mask as u64 as i64));
                Some(Some(Expr::bin("==", masked, Expr::Int(pattern.value as u64 as i64))))
            }
            _ => None,
        }
    }

    /// Binds the names of a pattern that matched `v`: as new locals, or (for
    /// `is`, whose locals are declared up front) by assignment.
    pub(super) fn bind_pat(&mut self, pat: &hir::Pat, v: &Expr, declare: bool) {
        if self.bind_wrapper(pat, v, declare) {
            return;
        }
        let bind = |this: &mut Self, l: LocalId, value: Expr| {
            let name = this.local_name(l);
            // A binding of a `Copy` value is a value of its own: what it was
            // matched against may change while it's in scope.
            let ty = this.tables().local_ty.get(&l).map(|&t| this.tcx().degauge(t));
            let value = match ty {
                Some(ty) if this.is_table(ty) && this.is_copy(ty) => {
                    let d = this.desc(ty, pat.span);
                    Expr::rt("copy", vec![d, value])
                }
                _ => value,
            };
            if declare {
                this.push_local(name, value);
            } else {
                this.push(StmtKind::Assign(vec![Expr::Name(name)], vec![value]));
            }
        };
        match &pat.kind {
            PatKind::Bind(l) => bind(self, *l, v.clone()),
            PatKind::Tuple(pats) => {
                for (i, p) in pats.iter().enumerate() {
                    self.bind_pat(p, &Expr::index(v.clone(), Expr::Int(i as i64 + 1)), declare);
                }
            }
            PatKind::Case { fields, .. } => {
                let Some(case_pat) = self.tables().case_pats.get(&pat.id).cloned() else { return };
                for (f, key) in fields.iter().flatten().zip(&case_pat.keys) {
                    self.bind_pat(&f.pat, &Self::access(v.clone(), key), declare);
                }
            }
            PatKind::Twill { pattern, captures } => {
                for (capture, &l) in pattern.captures.iter().zip(captures) {
                    let value = match capture.contiguous() {
                        Some((shift, width)) => {
                            let shifted = if shift == 0 {
                                v.clone()
                            } else {
                                Expr::bin(">>", v.clone(), Expr::Int(shift as i64))
                            };
                            Expr::bin("&", shifted, Expr::Int(((1u128 << width) - 1) as i64))
                        }
                        None => {
                            // Scattered bits, most significant first.
                            let n = capture.bits.len();
                            capture
                                .bits
                                .iter()
                                .enumerate()
                                .map(|(i, &bit)| {
                                    let one =
                                        Expr::bin("&", Expr::bin(">>", v.clone(), Expr::Int(bit as i64)), Expr::Int(1));
                                    let place = (n - 1 - i) as i64;
                                    if place == 0 { one } else { Expr::bin("<<", one, Expr::Int(place)) }
                                })
                                .reduce(|a, b| Expr::bin("|", a, b))
                                .unwrap_or(Expr::Int(0))
                        }
                    };
                    bind(self, l, value);
                }
            }
            PatKind::Or(alts) => {
                if let Some(first) = alts.first() {
                    self.bind_pat(first, v, declare);
                }
            }
            // A gauge test binds the number itself; an `any` test, what's in it.
            PatKind::TypeTest { local, .. } if self.tables().gauge_tests.contains_key(&pat.id) => {
                bind(self, *local, v.clone())
            }
            PatKind::TypeTest { local, .. } => bind(self, *local, Expr::index(v.clone(), Expr::Int(1))),
            _ => {}
        }
    }

    /// A constant's value where it's used: untyped constants are written in
    /// place, typed ones read from `C`.
    pub(super) fn const_value(&mut self, def: DefId, ty: Ty) -> Expr {
        if let Some(&value) = self.typed.results.untyped_floats.get(&def) {
            return self.float_value(value, ty);
        }
        match self.typed.results.untyped_consts.get(&def) {
            Some(&value) => self.int_value(value, ty),
            None => Expr::field(Expr::name("C"), &self.def_name(def)),
        }
    }

    /// The value of a construct, field, constant or dot-block expression, if
    /// `e` is one.
    pub(super) fn warp_value(&mut self, e: &hir::Expr) -> Option<Expr> {
        let ty = self.ty_of(e);
        if let Some(c) = self.tables().constructs.get(&e.id).cloned() {
            let args: &[hir::Arg] = match &e.kind {
                ExprKind::Call { args, .. } => args,
                _ => &[],
            };
            return Some(self.construct(&c, args));
        }
        match &e.kind {
            ExprKind::Field { .. } | ExprKind::LeadingDot(FieldName::Named(_) | FieldName::Index(..)) => {
                if let Some(v) = self.field_value(e) {
                    return Some(v);
                }
                let def = self.tables().leading_consts.get(&e.id).copied()?;
                Some(self.const_value(def, ty))
            }
            ExprKind::Assoc { .. } => match self.tables().assoc_values.get(&e.id).copied()? {
                crate::typeck::AssocValue::Const(def) => Some(self.const_value(def, ty)),
                crate::typeck::AssocValue::Int(v) => Some(self.int_value(v, ty)),
            },
            ExprKind::DotBlock { subject, body } => Some(self.dot_block(e, subject, body)),
            ExprKind::Is { expr, pat } => Some(self.is_expr(expr, pat)),
            _ => None,
        }
    }
}

/// Every local a pattern binds.
pub(super) fn collect_bindings(pat: &hir::Pat, out: &mut Vec<LocalId>) {
    match &pat.kind {
        PatKind::Bind(l) => out.push(*l),
        PatKind::Tuple(pats) => pats.iter().for_each(|p| collect_bindings(p, out)),
        PatKind::Or(alts) => {
            if let Some(first) = alts.first() {
                collect_bindings(first, out);
            }
        }
        PatKind::Case { fields, .. } => fields.iter().flatten().for_each(|f| collect_bindings(&f.pat, out)),
        PatKind::Twill { captures, .. } => out.extend(captures),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => collect_bindings(p, out),
        PatKind::TypeTest { local, .. } => out.push(*local),
        _ => {}
    }
}
