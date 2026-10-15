//! Sequences: arrays (`[T; N]`), lists (`List[T]`) and slices (`[T]`).
//!
//! An array's length is part of its type, so it's computed at compile time:
//! from literals, untyped constants, typed integer constants, and arithmetic
//! on them. A slice is a view of an array's or a list's elements; `[T]` is
//! std's builtin warp `Slice[T]`, whose methods arrays and lists have too.
//!
//! An array literal is an array, or a list where one is expected: both are
//! the same at run time.

use loom_syntax::Diagnostic;
use loom_syntax::ast::{BinaryOp, Lit, UnaryOp};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::infer::VarKind;
use super::method::Found;
use super::warp::Member;
use super::{Checker, Coercion};
use crate::hir::{DefKind, Expr, ExprKind, GenericArg};
use crate::ty::{Ty, TyKind};

/// What kind of sequence a type is, and its element type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seq {
    Array(Ty, u64),
    List(Ty),
    Slice(Ty),
}

impl Seq {
    pub fn elem(self) -> Ty {
        match self {
            Seq::Array(t, _) | Seq::List(t) | Seq::Slice(t) => t,
        }
    }
}

impl Checker<'_> {
    /// The sequence `ty` is, if it's one.
    pub(super) fn seq(&self, ty: Ty) -> Option<Seq> {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty) {
            TyKind::Array(t, n) => Some(Seq::Array(*t, *n)),
            TyKind::Adt(def, args) if args.len() == 1 && Some(*def) == self.std.list => Some(Seq::List(args[0])),
            TyKind::Adt(def, args) if args.len() == 1 && Some(*def) == self.std.slice => Some(Seq::Slice(args[0])),
            _ => None,
        }
    }

    pub(super) fn slice_of(&mut self, elem: Ty) -> Ty {
        match self.std.slice {
            Some(def) => self.tcx.intern(TyKind::Adt(def, vec![elem])),
            None => self.tcx.error,
        }
    }

    pub(super) fn list_of(&mut self, elem: Ty) -> Ty {
        match self.std.list {
            Some(def) => self.tcx.intern(TyKind::Adt(def, vec![elem])),
            None => self.tcx.error,
        }
    }

    // ------------------------------------------------------------ lengths

    /// The value of an integer expression known at compile time: literals,
    /// constants, and arithmetic on them. `None` if it isn't one.
    pub(super) fn const_int(&mut self, e: &Expr) -> Option<i128> {
        if let Some(super::gauge::Val::Int(v)) = self.const_param_val(e) {
            return Some(v);
        }
        match &e.kind {
            ExprKind::Lit(Lit::Int(i)) => Some(i.value as i128),
            ExprKind::Unary { op: UnaryOp::Neg, operand } => self.const_int(operand).map(|v| -v),
            ExprKind::Def { def, .. } => {
                let p = self.p;
                let DefKind::Const(c) = &p.def(*def).kind else { return None };
                self.const_ty(*def);
                if let Some(&v) = self.r.untyped_consts.get(def) {
                    return Some(v);
                }
                self.const_int(&c.value)
            }
            ExprKind::Block(b) if b.stmts.len() == 1 => match &b.stmts[0].kind {
                crate::hir::StmtKind::Expr(inner) => self.const_int(inner),
                _ => None,
            },
            ExprKind::Binary { op, lhs, rhs } => {
                let (a, b) = (self.const_int(lhs)?, self.const_int(rhs)?);
                match op {
                    BinaryOp::Add => a.checked_add(b),
                    BinaryOp::Sub => a.checked_sub(b),
                    BinaryOp::Mul => a.checked_mul(b),
                    BinaryOp::Div if b != 0 => Some(a / b),
                    BinaryOp::Rem if b != 0 => Some(a % b),
                    BinaryOp::Shl if (0..64).contains(&b) => a.checked_shl(b as u32),
                    BinaryOp::Shr if (0..128).contains(&b) => Some(a >> b),
                    BinaryOp::BitAnd => Some(a & b),
                    BinaryOp::BitOr => Some(a | b),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// An array length: a non-negative integer known at compile time.
    pub(super) fn array_len(&mut self, e: &Expr) -> Option<u64> {
        match self.const_int(e) {
            Some(n) if (0..=u32::MAX as i128).contains(&n) => Some(n as u64),
            Some(n) => {
                self.error(codes::T0102, e.span, format!("an array can't have {n} elements"), "not a length");
                None
            }
            // A parameter that isn't `const` gets its value when the program runs.
            None if matches!(e.kind, ExprKind::Local(_)) => {
                let ExprKind::Local(local) = e.kind else { unreachable!() };
                let name = self.p.local(local).name.clone();
                self.diags.push(
                    Diagnostic::error(codes::K0105, format!("`{name}` has no value while compiling"))
                        .primary(e.span, "an array's length must be known at compile time")
                        .secondary(self.p.local(local).span, "declared here")
                        .help(format!("if it's a parameter, make it a `const` one: `const {name}: usize`")),
                );
                None
            }
            None => {
                self.unsupported(e.span, "array lengths that aren't known at compile time");
                None
            }
        }
    }

    // ------------------------------------------------------------ literals

    /// `[a, b, c]`: an array, or a list or slice where one is expected.
    pub(super) fn check_array_lit(&mut self, e: &Expr, items: &[Expr], expected: Option<Ty>) -> Ty {
        let want = expected.and_then(|t| self.seq(t));
        let elem = match want {
            Some(seq) => seq.elem(),
            None => match items.first() {
                Some(_) => self.new_var(VarKind::Type),
                None => {
                    self.error(
                        codes::T0110,
                        e.span,
                        "can't tell what this empty `[]` holds",
                        "add a type: `let xs: List[i32] = []`",
                    );
                    return self.tcx.error;
                }
            },
        };
        for item in items {
            let ty = self.check_expr(item, Some(elem));
            if !self.coerce(item, ty, elem) {
                break;
            }
        }
        let n = items.len() as u64;
        match want {
            Some(Seq::Array(_, len)) if len != n => {
                self.error(
                    codes::T0101,
                    e.span,
                    format!("expected {len} element{}, found {n}", if len == 1 { "" } else { "s" }),
                    "wrong length",
                );
                self.tcx.error
            }
            Some(Seq::List(_)) => self.list_of(elem),
            _ => self.tcx.intern(TyKind::Array(elem, n)),
        }
    }

    /// `[x; n]`: `n` copies of `x`.
    pub(super) fn check_array_repeat(&mut self, value: &Expr, count: &Expr, expected: Option<Ty>) -> Ty {
        let want = expected.and_then(|t| self.seq(t));
        let elem = self.check_expr(value, want.map(|s| s.elem()));
        let elem = match want {
            Some(seq) => {
                self.coerce(value, elem, seq.elem());
                seq.elem()
            }
            None => elem,
        };
        let Some(n) = self.array_len(count) else { return self.tcx.error };
        self.check_expr(count, Some(self.tcx.usize));
        match want {
            Some(Seq::List(_)) => self.list_of(elem),
            _ => self.tcx.intern(TyKind::Array(elem, n)),
        }
    }

    // ------------------------------------------------------------ indexing

    /// `xs[i]`, and `xs[a..b]`, which is a slice of it.
    pub(super) fn check_index(&mut self, e: &Expr, base: &Expr, args: &[GenericArg]) -> Ty {
        let base_ty = self.check_expr(base, None);
        let [GenericArg::Expr(index)] = args else {
            return self.unsupported(e.span, "generic arguments on methods");
        };
        if self.tcx.is_lenient(self.resolve(base_ty)) {
            self.check_expr(index, None);
            return self.tcx.error;
        }
        // `m[key]`: the value for `key`.
        if let Some((k, v)) = self.map_types(base_ty) {
            if matches!(index.kind, ExprKind::Range { .. }) {
                self.b.expr_ty.insert(index.id, self.tcx.error);
                self.error(codes::T0103, index.span, "a map can't be sliced", "a range of keys");
                return self.tcx.error;
            }
            let saved = std::mem::replace(&mut self.in_argument, true);
            self.check_coerce(index, k);
            self.in_argument = saved;
            self.require_hash(k, index.span);
            return v;
        }
        let Some(seq) = self.seq(base_ty) else {
            let base_ty = self.resolve(base_ty);
            if matches!(self.tcx.kind(base_ty), TyKind::Str) || Some(base_ty) == self.string_ty_opt() {
                return self.check_str_index(e, index);
            }
            let shown = self.show(base_ty);
            self.check_expr(index, None);
            // `Index` weaves (`map[key]`) come with maps.
            if let TyKind::Adt(def, _) = *self.tcx.kind(base_ty)
                && self.is_builtin(def)
            {
                return self.unsupported(e.span, &format!("indexing `{shown}`"));
            }
            self.error(codes::T0103, e.span, format!("`{shown}` can't be indexed"), "not an array, list or slice");
            return self.tcx.error;
        };
        if let ExprKind::Range { lo, hi, .. } = &index.kind {
            for bound in lo.iter().chain(hi.iter()) {
                self.check_index_value(bound);
            }
            self.b.expr_ty.insert(index.id, self.tcx.error);
            return self.slice_of(seq.elem());
        }
        self.check_index_value(index);
        seq.elem()
    }

    /// `s[a..b]`: the bytes from `a` to `b`, which must fall on character
    /// boundaries. A single position isn't a character, so `s[i]` is an error.
    fn check_str_index(&mut self, e: &Expr, index: &Expr) -> Ty {
        match &index.kind {
            ExprKind::Range { lo, hi, .. } => {
                for bound in lo.iter().chain(hi.iter()) {
                    self.check_index_value(bound);
                }
                self.b.expr_ty.insert(index.id, self.tcx.error);
                self.tcx.str
            }
            _ => {
                self.check_expr(index, None);
                self.diags.push(
                    Diagnostic::error(codes::T0103, "text can't be indexed by position")
                        .primary(e.span, "characters take different numbers of bytes")
                        .help("slice it by bytes, `s[i..j]`, or look at its characters"),
                );
                self.tcx.error
            }
        }
    }

    /// The methods primitive types get from std's builtin warps: `str` (and
    /// `String`) from `Str`, `char` from `Char`, integers from `Int[T]`,
    /// floats from `Float[T]`. The value is passed as it is.
    pub(super) fn prim_method(&mut self, ty: Ty, name: &str) -> Option<Found> {
        let ty = self.resolve(ty);
        // A gauge has its base's methods, working in the base type, except
        // `min` and `max`: the smaller or larger of two values in a range
        // is in it too, so `amount.max(25)` is still a `Cents`.
        if let TyKind::Gauge(_, base) = *self.tcx.kind(ty) {
            let keeps = matches!(name, "min" | "max")
                && matches!(self.tcx.kind(ty), TyKind::Gauge(def, _) if self.r.gauges.get(def).is_some_and(|g| g.range.is_some()));
            let mut found = self.prim_method(base, name)?;
            if keeps {
                let warp = match self.tcx.kind(found.self_ty) {
                    TyKind::Adt(w, _) => *w,
                    _ => return Some(found),
                };
                let self_ty = self.tcx.intern(TyKind::Adt(warp, vec![ty]));
                found.inst = self.member_instance(found.inst.def, self_ty);
                found.self_ty = self_ty;
            }
            return Some(found);
        }
        let (warp, args) = match self.tcx.kind(ty) {
            TyKind::Str => (self.std.str_warp?, Vec::new()),
            TyKind::Adt(def, _) if Some(*def) == self.std.string => (self.std.str_warp?, Vec::new()),
            TyKind::Char => (self.std.char_warp?, Vec::new()),
            TyKind::Int(_) | TyKind::IntVar(_) => (self.std.int_warp?, vec![ty]),
            TyKind::Float(_) | TyKind::FloatVar(_) => (self.std.float_warp?, vec![ty]),
            TyKind::Optional(inner) => (self.std.optional?, vec![*inner]),
            TyKind::Range(elem) => (self.std.range_warp?, vec![*elem]),
            _ => return None,
        };
        let Some(Member::Method(m)) = self.member(warp, name) else { return None };
        let self_ty = self.tcx.intern(TyKind::Adt(warp, args));
        let via = format!("{}.{name}", self.p.def(warp).name);
        Some(Found { inst: self.member_instance(m, self_ty), path: Vec::new(), self_ty, via, stream: None })
    }

    /// An index: any unsigned integer, used as a `usize`.
    fn check_index_value(&mut self, index: &Expr) {
        let ty = self.check_expr(index, Some(self.tcx.usize));
        let ty = self.resolve(ty);
        match self.tcx.kind(ty) {
            TyKind::Int(i) if !i.signed => {
                if *i != self.tcx.target.usize() {
                    self.b.coercions.insert(index.id, Coercion::Widen);
                }
            }
            TyKind::IntVar(_) => {
                self.unify(ty, self.tcx.usize);
            }
            _ if self.tcx.is_lenient(ty) => {}
            _ => {
                let shown = self.show(ty);
                self.error(
                    codes::T0101,
                    index.span,
                    format!("an index is an unsigned integer, and this is `{shown}`"),
                    "not an index",
                );
            }
        }
    }

    /// Whether an element of `xs[i]` can be changed: when `xs` can.
    pub(super) fn index_place(&mut self, target: &Expr) -> Option<Ty> {
        let ty = self.check_expr(target, None);
        if self.tcx.is_error(ty) {
            return None;
        }
        if self.place(target) == Some(false) {
            self.diags.push(
                Diagnostic::error(codes::T0107, "can't change this element")
                    .primary(target.span, "part of something that can't be changed")
                    .help("elements can be changed through a `var`, a `mut` parameter, or `mut self`"),
            );
        }
        Some(ty)
    }

    // ------------------------------------------------------------ methods

    /// The methods arrays and lists get from `Slice[T]`: found with `self`
    /// as the slice, which they're passed as unchanged.
    pub(super) fn slice_method(&mut self, ty: Ty, name: &str) -> Option<Found> {
        let seq = self.seq(ty)?;
        if matches!(seq, Seq::Slice(_)) {
            return None;
        }
        let slice = self.std.slice?;
        let Some(Member::Method(m)) = self.member(slice, name) else { return None };
        let self_ty = self.slice_of(seq.elem());
        Some(Found {
            inst: self.member_instance(m, self_ty),
            path: Vec::new(),
            self_ty,
            via: format!("[T].{name}"),
            stream: None,
        })
    }

    /// `K` and `V`, for a `Map[K, V]`.
    pub(super) fn map_types(&self, ty: Ty) -> Option<(Ty, Ty)> {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Adt(def, args) if Some(*def) == self.std.map && args.len() == 2 => Some((args[0], args[1])),
            _ => None,
        }
    }

    /// `T`, for a `Set[T]`.
    pub(super) fn set_elem(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(self.resolve(ty)) {
            TyKind::Adt(def, args) if Some(*def) == self.std.set && args.len() == 1 => Some(args[0]),
            _ => None,
        }
    }

    /// The types a map or a set holds: `[K, V]` or `[T]`.
    pub(super) fn coll_parts(&self, ty: Ty) -> Option<Vec<Ty>> {
        match self.map_types(ty) {
            Some((k, v)) => Some(vec![k, v]),
            None => self.set_elem(ty).map(|t| vec![t]),
        }
    }

    /// Reports a key type that can't be hashed.
    pub(super) fn require_hash(&mut self, key: Ty, span: Span) {
        let key = self.deep_resolve(key);
        if self.tcx.is_lenient(key) || self.has_vars(key) {
            return;
        }
        if let Some(hash) = self.std.hash
            && !self.weaves(key, hash)
        {
            let shown = self.show(key);
            self.diags.push(
                Diagnostic::error(codes::T0108, format!("`{shown}` can't be a key: it doesn't weave `Hash`"))
                    .primary(span, "a key")
                    .help("keys are numbers, text, `char`s, `bool`s, or warps that weave `Hash`"),
            );
        }
    }

    /// Reports lending a map's value as `mut` when it isn't a table: there's
    /// no place in the map to write its new value back to. (A table value,
    /// like a list, is changed where it is.) Returns whether it was reported.
    pub(super) fn mut_map_value(&mut self, e: &Expr) -> bool {
        let ExprKind::Index { base, .. } = &e.kind else { return false };
        let Some(base_ty) = self.b.expr_ty.get(&base.id).copied() else { return false };
        let Some((_, v)) = self.map_types(base_ty) else { return false };
        if self.is_table_backed(v) {
            return false;
        }
        self.diags.push(
            Diagnostic::error(codes::T0107, "a map's value can't be lent as `mut`")
                .primary(e.span, "a number, text or enum in a map")
                .help("take it out, change it, and put it back: `var n = m[k]; f(mut n); m[k] = n`"),
        );
        true
    }

    /// `[k: v, …]`: a map, of the expected types or the first pair's.
    pub(super) fn check_map_lit(&mut self, e: &Expr, pairs: &[(Expr, Expr)], expected: Option<Ty>) -> Ty {
        let expected = expected.and_then(|t| self.map_types(t));
        let (k, v) = match expected {
            Some(kv) => kv,
            None if pairs.is_empty() => {
                self.error(
                    codes::T0110,
                    e.span,
                    "can't tell the types of this empty map",
                    "add a type annotation, like `let m: Map[str, i32] = [:]`",
                );
                return self.tcx.error;
            }
            None => (self.new_var(VarKind::Type), self.new_var(VarKind::Type)),
        };
        for (key, value) in pairs {
            self.check_coerce(key, k);
            self.check_coerce(value, v);
        }
        if let Some((key, _)) = pairs.first() {
            self.require_hash(k, key.span);
        }
        let Some(map) = self.std.map else { return self.tcx.error };
        self.tcx.intern(TyKind::Adt(map, vec![k, v]))
    }

    /// `List()`, `Map()`, `Set()` or `String()`: an empty one.
    pub(super) fn check_new_list(&mut self, call: &Expr, ty: Ty, expected: Option<Ty>, args: &[crate::hir::Arg]) -> Ty {
        if let Some(arg) = args.first() {
            let name = match self.tcx.kind(ty) {
                TyKind::Adt(d, _) => self.p.def(*d).name.clone(),
                _ => "List".into(),
            };
            let help = match name.as_str() {
                "List" => "use `[a, b]` or `List.filled(x, n)`",
                "String" => "use a string literal, or `s.to_string()`",
                _ => "use a literal: `[k: v]`, or `Set.of(xs)`",
            };
            self.error(codes::T0104, arg.span, format!("`{name}()` takes no values"), help);
            return self.tcx.error;
        }
        if let Some(t) = expected {
            self.unify(ty, t);
        }
        self.b.new_lists.insert(call.id);
        ty
    }

    fn string_ty_opt(&mut self) -> Option<Ty> {
        self.std.string.map(|d| self.tcx.intern(TyKind::Adt(d, Vec::new())))
    }

    /// The element type a `for` loop over `ty` binds, if it's a sequence.
    pub(super) fn seq_elem(&self, ty: Ty) -> Option<Ty> {
        self.seq(ty).map(Seq::elem)
    }

    /// Reports `for mut` over something that can't be changed.
    pub(super) fn check_for_mut(&mut self, iter: &Expr, span: Span) {
        if self.place(iter) != Some(true) {
            self.diags.push(
                Diagnostic::error(codes::T0107, "`for mut` changes the elements, and these can't be changed")
                    .primary(span, "not a `var` or a `mut` parameter")
                    .help("loop with `for x in …` to read them"),
            );
        }
    }
}
