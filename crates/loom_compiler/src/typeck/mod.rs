//! Type checking: HIR → types for every expression, pattern and local.
//!
//! The results live in side tables keyed by [`HirId`] and [`LocalId`]
//! ([`Results`]) rather than in a new tree: lowering reads the HIR and the
//! tables together.
//!
//! Checking is bidirectional. Each expression is checked against an
//! optional expected type, which is how literals get their types
//! (`let x: u8 = 250`) and how operands widen before an operation
//! (`let op: u16 = hi << 8 | lo`). Without one, integer literals are `i32`
//! and float literals are `f64`.
//!
//! Only what's reachable from the root module is checked, starting from its
//! items and following references into `std`.
//!
//! Covered so far: numbers, `bool`, `char`, strings, tuples, functions,
//! control flow, operators, casts and templates; warps (records, cases,
//! shared fields), methods, associated functions, UFCS, leading dots,
//! dot-blocks, case and twill patterns. Everything else is reported with
//! U0102 ("not implemented yet") rather than guessed at.

mod call;
mod expr;
mod infer;
mod pat;
#[cfg(test)]
mod tests;
mod warp;

use std::collections::{HashMap, HashSet, VecDeque};

use loom_syntax::Diagnostic;
use loom_syntax::ast::ParamMode;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use crate::hir::{self, DefId, DefKind, Expr, ExprKind, FnRet, HirId, LocalId, Program, TyKind as HirTy};
use crate::ty::{Ty, TyCtx, TyKind};
use infer::{PendingLit, Var};
pub use warp::{CaseFieldInfo, CaseInfo, FieldInfo, Key, WarpInfo};

/// The output of type checking.
#[derive(Debug)]
pub struct Typed {
    pub tcx: TyCtx,
    pub results: Results,
    pub diagnostics: Vec<Diagnostic>,
}

impl Typed {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Types and decisions recorded for lowering.
#[derive(Debug, Default)]
pub struct Results {
    /// The type of each expression, before any coercion.
    pub expr_ty: HashMap<HirId, Ty>,
    pub pat_ty: HashMap<HirId, Ty>,
    pub local_ty: HashMap<LocalId, Ty>,
    /// Conversions applied to an expression's value where it's used.
    pub coercions: HashMap<HirId, Coercion>,
    pub sigs: HashMap<DefId, FnSig>,
    pub const_ty: HashMap<DefId, Ty>,
    /// Constants declared without a type whose value is an integer literal.
    /// Like Go's untyped constants, each use gets the type its context
    /// needs, so lowering writes the value in place.
    pub untyped_consts: HashMap<DefId, i128>,
    /// For each call: which argument (or default) feeds each parameter.
    pub calls: HashMap<HirId, Call>,
    /// The layout of every warp used.
    pub warps: HashMap<DefId, WarpInfo>,
    /// Expressions that build a warp value: constructor calls, `Shape.Dot`,
    /// `.Circle(r)`.
    pub constructs: HashMap<HirId, Construct>,
    /// Field reads and writes: `p.x`, `t.0`, and a leading `.x` that means a field.
    pub fields: HashMap<HirId, FieldAccess>,
    /// Leading dots that name a constant: `.MAX` where a type is expected.
    pub leading_consts: HashMap<HirId, DefId>,
    /// `Type.NAME` constants: a user constant, or a primitive's bound (`u8.MAX`).
    pub assoc_values: HashMap<HirId, AssocValue>,
    /// Dot-blocks, and whether each edits its subject in place.
    pub dot_blocks: HashMap<HirId, bool>,
    /// Case and record patterns: which case, and each field pattern's key.
    pub case_pats: HashMap<HirId, CasePat>,
    /// Patterns that compare against a constant (`MAX =>`).
    pub const_pats: HashMap<HirId, DefId>,
    /// Every function and constant checked, in the order they were reached.
    pub reachable: Vec<DefId>,
}

/// An implicit conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Coercion {
    /// Lossless integer widening. Free at run time: values are normalized.
    Widen,
    /// A string literal where a `String` is expected.
    StrToString,
    /// A `String` passed where a `str` parameter is expected. Only in call
    /// arguments: a view must not outlive what it points at, and a borrow
    /// lasts exactly as long as the call.
    StringToStr,
    /// `T` where `?T` is expected.
    ToOptional,
    /// `T` where `T ! E` is expected: a success.
    ToResult,
    /// A value where `any W` is expected.
    ToAny(Vec<DefId>),
    /// A value that's thrown away, where `()` is expected.
    Discard,
}

#[derive(Clone, Debug)]
pub struct FnSig {
    pub params: Vec<SigParam>,
    pub ret: Ty,
}

#[derive(Clone, Debug)]
pub struct SigParam {
    pub name: String,
    pub mode: ParamMode,
    pub ty: Ty,
    pub has_default: bool,
}

#[derive(Clone, Debug)]
pub struct Call {
    pub callee: DefId,
    /// One entry per parameter, in parameter order.
    pub args: Vec<ArgSource>,
    /// For a method or UFCS call, the value before the dot. It feeds the
    /// parameter marked [`ArgSource::Receiver`].
    pub receiver: Option<Receiver>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgSource {
    /// The call's argument at this index.
    Arg(usize),
    /// The parameter's default value.
    Default,
    /// The value a method was called on.
    Receiver,
}

/// The value a method is called on, or a field is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receiver {
    /// The expression before the dot (`p` in `p.len()`).
    Expr,
    /// `self`, reached through a leading dot in a method (`.len()`).
    Local(LocalId),
    /// The subject of the dot-block with this id (`.show()` inside `w.{ … }`).
    Subject(HirId),
}

/// A value of a warp being built.
#[derive(Clone, Debug)]
pub struct Construct {
    pub warp: DefId,
    /// `None` for a record or tuple warp.
    pub case: Option<DefId>,
    /// Every field, with where its value comes from.
    pub fields: Vec<(Key, FieldSource)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldSource {
    /// The constructor's argument at this index.
    Arg(usize),
    /// The default value of this field definition.
    Default(DefId),
}

#[derive(Clone, Debug)]
pub struct FieldAccess {
    pub key: Key,
    pub base: Receiver,
}

#[derive(Clone, Copy, Debug)]
pub enum AssocValue {
    Const(DefId),
    Int(i128),
}

#[derive(Clone, Debug)]
pub struct CasePat {
    pub warp: DefId,
    /// `None` for a record or tuple warp, which always matches.
    pub case: Option<DefId>,
    /// The key of each field pattern, in order.
    pub keys: Vec<Key>,
}

/// Type-checks everything reachable from the root module of `p`.
pub fn check(p: &Program) -> Typed {
    let mut c = Checker::new(p);
    if let Some(root) = p.root {
        for &def in &p.module(root).decls {
            c.queue_def(def);
        }
    }
    while let Some(def) = c.queue.pop_front() {
        c.check_def(def);
    }
    c.zonk();
    Typed { tcx: c.tcx, results: c.r, diagnostics: c.diags }
}

/// `std` definitions the checker knows by name.
#[derive(Debug, Default)]
struct Std {
    string: Option<DefId>,
    show: Option<DefId>,
    eq: Option<DefId>,
    ord: Option<DefId>,
    hash: Option<DefId>,
    copy: Option<DefId>,
    integer: Option<DefId>,
}

/// Where the enclosing loop's `break`s go.
#[derive(Debug)]
struct LoopCtx {
    id: HirId,
    /// Only `loop` can break with a value.
    takes_value: bool,
    expected: Option<Ty>,
    /// The type of the first `break` value, when nothing was expected.
    break_ty: Option<Ty>,
}

struct Checker<'p> {
    p: &'p Program,
    tcx: TyCtx,
    r: Results,
    diags: Vec<Diagnostic>,
    std: Std,
    queue: VecDeque<DefId>,
    queued: HashSet<DefId>,
    /// Constants whose type is being computed, to catch cycles.
    const_stack: Vec<DefId>,
    /// "Not implemented yet" messages already reported, per definition.
    unsupported_seen: HashSet<(Option<DefId>, String)>,
    /// Inference variables for literal types (see `infer.rs`).
    vars: Vec<Var>,
    /// Literal range checks waiting for the end of the body.
    pending_lits: Vec<PendingLit>,
    /// Negations whose operand must turn out signed.
    pending_negations: Vec<(Ty, Span)>,

    /// Checking a call argument directly: where a `String` may be viewed as a `str`.
    in_argument: bool,
    /// The warp whose members are being checked: what `Self` means.
    self_ty: Option<Ty>,
    /// The `self` of the method being checked.
    self_local: Option<LocalId>,
    /// Dot-blocks being checked, innermost last: their ids and subject types.
    subjects: Vec<(HirId, Ty)>,

    // Per-function state.
    current: Option<DefId>,
    ret: Option<Ty>,
    loops: Vec<LoopCtx>,
}

impl<'p> Checker<'p> {
    fn new(p: &'p Program) -> Self {
        let find = |module: &str, name: &str| {
            p.modules.iter().find(|m| m.path == module).and_then(|m| m.items.get(name).copied())
        };
        let std = Std {
            string: find("std.string", "String"),
            show: find("std.fmt", "Show"),
            eq: find("std.ops", "Eq"),
            ord: find("std.ops", "Ord"),
            hash: find("std.ops", "Hash"),
            copy: find("std.ops", "Copy"),
            integer: find("std.ops", "Integer"),
        };
        Checker {
            p,
            tcx: TyCtx::default(),
            r: Results::default(),
            diags: Vec::new(),
            std,
            queue: VecDeque::new(),
            queued: HashSet::new(),
            const_stack: Vec::new(),
            unsupported_seen: HashSet::new(),
            vars: Vec::new(),
            pending_lits: Vec::new(),
            pending_negations: Vec::new(),
            in_argument: false,
            self_ty: None,
            self_local: None,
            subjects: Vec::new(),
            current: None,
            ret: None,
            loops: Vec::new(),
        }
    }

    // ------------------------------------------------------------ diagnostics

    fn error(
        &mut self,
        code: loom_syntax::diag::Code,
        span: Span,
        message: impl Into<String>,
        label: impl Into<String>,
    ) {
        self.diags.push(Diagnostic::error(code, message).primary(span, label));
    }

    /// Reports a feature this compiler doesn't implement yet, once per
    /// definition, and returns the error type.
    fn unsupported(&mut self, span: Span, what: &str) -> Ty {
        if self.unsupported_seen.insert((self.current, what.to_string())) {
            self.diags.push(
                Diagnostic::error(codes::U0102, format!("not implemented yet: {what}"))
                    .primary(span, "not supported by this compiler yet")
                    .note("the type checker grows one milestone at a time; see design/lua-backend.md"),
            );
        }
        self.tcx.error
    }

    fn show(&mut self, ty: Ty) -> String {
        let ty = self.deep_resolve(ty);
        self.tcx.display(self.p, ty)
    }

    fn mismatch(&mut self, span: Span, expected: Ty, found: Ty) {
        let (e, f) = (self.show(expected), self.show(found));
        self.error(codes::T0101, span, format!("expected `{e}`, found `{f}`"), format!("this is `{f}`"));
    }

    // ------------------------------------------------------------ the work list

    fn queue_def(&mut self, def: DefId) {
        if self.queued.insert(def) {
            self.queue.push_back(def);
        }
    }

    fn check_def(&mut self, def: DefId) {
        let p = self.p;
        let d = p.def(def);
        self.current = Some(def);
        match &d.kind {
            DefKind::Fn(f) => {
                self.r.reachable.push(def);
                self.check_fn(def, f);
            }
            DefKind::Const(_) => {
                self.r.reachable.push(def);
                self.const_ty(def);
            }
            DefKind::Warp(w) => self.check_warp(def, w),
            DefKind::Weft(_) | DefKind::Weave(_) => {
                self.unsupported(d.span, "type-checking wefts and weaves");
            }
            DefKind::Type(_) => {
                self.unsupported(d.span, "type-checking `type` declarations");
            }
            DefKind::ConstBlock(e) => {
                self.unsupported(e.span, "compile-time code");
            }
            _ => {}
        }
        self.current = None;
    }

    /// A warp's field types and defaults. The members of a warp in the root
    /// module are all checked, used or not; std's only when they're used.
    fn check_warp(&mut self, def: DefId, w: &hir::WarpDef) {
        let Some(info) = self.warp_info(def) else { return };
        self.self_ty = Some(self.tcx.intern(TyKind::Adt(def, Vec::new())));
        let first_var = self.vars.len();
        for field in &info.fields {
            if let DefKind::Field(f) = &self.p.def(field.def).kind
                && let Some(default) = &f.default
            {
                self.check_coerce(default, field.ty);
            }
        }
        self.finish_body(first_var);
        self.self_ty = None;
        if Some(self.p.def(def).module) == self.p.root {
            for &member in &w.members {
                match &self.p.def(member).kind {
                    DefKind::Fn(_) | DefKind::Const(_) => self.queue_def(member),
                    DefKind::Type(_) => {
                        self.unsupported(self.p.def(member).span, "types inside warps");
                    }
                    DefKind::ConstBlock(e) => {
                        self.unsupported(e.span, "compile-time code");
                    }
                    _ => {}
                }
            }
        }
    }

    /// The warp type a member belongs to, for `Self` and `self`.
    fn parent_warp_ty(&mut self, def: DefId) -> Option<Ty> {
        let parent = self.p.def(def).parent?;
        matches!(self.p.def(parent).kind, DefKind::Warp(_) | DefKind::Pending)
            .then(|| self.tcx.intern(TyKind::Adt(parent, Vec::new())))
    }

    fn check_fn(&mut self, def: DefId, f: &hir::FnDef) {
        self.self_ty = self.parent_warp_ty(def);
        self.self_local = f.params.first().filter(|p| p.is_self).map(|p| p.local);
        self.check_fn_body(def, f);
        self.self_ty = None;
        self.self_local = None;
    }

    fn check_fn_body(&mut self, def: DefId, f: &hir::FnDef) {
        let Some(sig) = self.fn_sig(def) else { return };
        let first_var = self.vars.len();
        for (param, sig_param) in f.params.iter().zip(&sig.params) {
            self.r.local_ty.insert(param.local, sig_param.ty);
            if let Some(default) = &param.default {
                self.check_coerce(default, sig_param.ty);
            }
        }
        if let Some(body) = &f.body {
            self.ret = Some(sig.ret);
            self.loops.clear();
            self.check_coerce(body, sig.ret);
            self.ret = None;
        }
        self.finish_body(first_var);
    }

    // ------------------------------------------------------------ signatures and types

    /// The signature of a function, computed on first use. `None` if it uses
    /// something unsupported (already reported).
    fn fn_sig(&mut self, def: DefId) -> Option<FnSig> {
        if let Some(sig) = self.r.sigs.get(&def) {
            return Some(sig.clone());
        }
        let p = self.p;
        let DefKind::Fn(f) = &p.def(def).kind else { return None };
        let span = p.def(def).span;
        let saved = self.current.replace(def);
        let parent = self.parent_warp_ty(def);
        let saved_self = std::mem::replace(&mut self.self_ty, parent);
        let sig = self.lower_sig(f, span);
        self.self_ty = saved_self;
        self.current = saved;
        let sig = sig?;
        self.r.sigs.insert(def, sig.clone());
        Some(sig)
    }

    fn lower_sig(&mut self, f: &hir::FnDef, span: Span) -> Option<FnSig> {
        if !f.generics.is_empty() {
            self.unsupported(span, "generic functions");
            return None;
        }
        if !f.givens.is_empty() {
            self.unsupported(span, "`given` parameters");
            return None;
        }
        let mut params = Vec::with_capacity(f.params.len());
        for param in &f.params {
            if param.mode == ParamMode::Const {
                self.unsupported(param.span, "`const` parameters");
                return None;
            }
            let ty = if param.is_self {
                match self.self_ty {
                    Some(ty) => ty,
                    None => {
                        self.unsupported(param.span, "methods outside warps");
                        return None;
                    }
                }
            } else {
                param.ty.as_ref().map_or(self.tcx.error, |t| self.lower_ty(t))
            };
            if self.tcx.is_error(ty) {
                return None;
            }
            // A `mut` parameter shares the caller's table; scalars would need
            // copy-in/copy-out, which isn't implemented yet.
            if param.mode == ParamMode::Mut && !self.is_table_backed(ty) {
                self.unsupported(param.span, "`mut` parameters of numbers, strings and enums");
                return None;
            }
            let name = self.p.local(param.local).name.clone();
            params.push(SigParam { name, mode: param.mode, ty, has_default: param.default.is_some() });
        }
        let ret = match &f.ret {
            FnRet::Unit => self.tcx.unit,
            FnRet::Value(t) => self.lower_ty(t),
            FnRet::Shuttle { .. } => {
                self.unsupported(span, "shuttles");
                return None;
            }
        };
        if self.tcx.is_error(ret) {
            return None;
        }
        Some(FnSig { params, ret })
    }

    /// The type a HIR type denotes.
    fn lower_ty(&mut self, t: &hir::Ty) -> Ty {
        let span = t.span;
        match &t.kind {
            HirTy::Prim(prim) => match self.tcx.prim(*prim) {
                Some(ty) => ty,
                None => self.unsupported(span, "`f16` and integers wider than 64 bits"),
            },
            HirTy::Def { def, args } => {
                let d = self.p.def(*def);
                match &d.kind {
                    DefKind::Warp(_) => {
                        let mut lowered = Vec::with_capacity(args.len());
                        for arg in args {
                            match arg {
                                hir::GenericArg::Type(t) => lowered.push(self.lower_ty(t)),
                                hir::GenericArg::Expr(e) => return self.unsupported(e.span, "value generic arguments"),
                            }
                        }
                        self.tcx.intern(TyKind::Adt(*def, lowered))
                    }
                    DefKind::Weft(_) => self.unsupported(span, "wefts used as types (implicit generics)"),
                    DefKind::Type(alias) if alias.gauge.is_none() && alias.generics.is_empty() => match &alias.value {
                        Some(value) => self.lower_ty(value),
                        None => self.unsupported(span, "associated types"),
                    },
                    DefKind::Type(_) => self.unsupported(span, "gauges and generic type aliases"),
                    DefKind::GenericParam(_) => self.unsupported(span, "generic parameters"),
                    _ => self.tcx.error,
                }
            }
            HirTy::Unit => self.tcx.unit,
            HirTy::Tuple(items) => {
                let items = items.iter().map(|t| self.lower_ty(t)).collect();
                self.tcx.intern(TyKind::Tuple(items))
            }
            HirTy::Optional(inner) => {
                let inner = self.lower_ty(inner);
                self.tcx.optional(inner)
            }
            HirTy::Result { ok, err } => {
                let (ok, err) = (self.lower_ty(ok), self.lower_ty(err));
                self.tcx.intern(TyKind::Result(ok, err))
            }
            HirTy::Fn { params, ret } => {
                let params = params.iter().map(|t| self.lower_ty(t)).collect();
                let ret = self.lower_ty(ret);
                self.tcx.intern(TyKind::Fn(params, ret))
            }
            HirTy::Any(wefts) => {
                let mut defs = Vec::with_capacity(wefts.len());
                for weft in wefts {
                    match &weft.kind {
                        HirTy::Def { def, args }
                            if args.is_empty() && matches!(self.p.def(*def).kind, DefKind::Weft(_)) =>
                        {
                            defs.push(*def)
                        }
                        _ => return self.unsupported(weft.span, "this kind of `any` type"),
                    }
                }
                self.tcx.intern(TyKind::Any(defs))
            }
            HirTy::Error => self.tcx.error,
            HirTy::Array { .. } => self.unsupported(span, "fixed-size arrays"),
            HirTy::Slice(_) => self.unsupported(span, "slices"),
            HirTy::Pointer(_) | HirTy::Region { .. } => self.unsupported(span, "spool pointers"),
            HirTy::RawPointer(_) => self.unsupported(span, "raw pointers"),
            HirTy::Union(_) => self.unsupported(span, "union types"),
            HirTy::SelfType => match self.self_ty {
                Some(ty) => ty,
                None => self.unsupported(span, "`Self` outside warps"),
            },
            HirTy::Assoc { .. } => self.unsupported(span, "associated types"),
            HirTy::Local(_) | HirTy::ConstCall { .. } | HirTy::TypeType => {
                self.unsupported(span, "types computed at compile time")
            }
            HirTy::Infer => self.unsupported(span, "inferred error sets (`! _`)"),
        }
    }

    /// The type of a constant, checking its value on first use.
    fn const_ty(&mut self, def: DefId) -> Ty {
        if let Some(&ty) = self.r.const_ty.get(&def) {
            return ty;
        }
        let p = self.p;
        let DefKind::Const(c) = &p.def(def).kind else { return self.tcx.error };
        if self.const_stack.contains(&def) {
            let name = p.def(def).name.clone();
            self.error(codes::T0110, p.def(def).span, format!("`{name}` is defined in terms of itself"), "cycle");
            return self.tcx.error;
        }
        self.const_stack.push(def);
        let (saved_current, saved_ret) = (self.current.replace(def), self.ret.take());
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_lits = std::mem::take(&mut self.pending_lits);
        let saved_negations = std::mem::take(&mut self.pending_negations);
        let first_var = self.vars.len();
        let ty = match &c.ty {
            Some(t) => {
                let ty = self.lower_ty(t);
                self.check_coerce(&c.value, ty)
            }
            None => {
                if let Some(value) = int_literal_value(&c.value) {
                    self.r.untyped_consts.insert(def, value);
                }
                self.check_expr(&c.value, None)
            }
        };
        self.finish_body(first_var);
        let ty = self.deep_resolve(ty);
        self.current = saved_current;
        self.ret = saved_ret;
        self.loops = saved_loops;
        self.pending_lits = saved_lits;
        self.pending_negations = saved_negations;
        self.const_stack.pop();
        self.r.const_ty.insert(def, ty);
        self.queue_def(def);
        ty
    }

    // ------------------------------------------------------------ coercion

    /// Checks `e` against `target`, converting implicitly where Loom allows.
    /// Returns `target`.
    fn check_coerce(&mut self, e: &Expr, target: Ty) -> Ty {
        let actual = self.check_expr(e, Some(target));
        self.coerce(e, actual, target);
        target
    }

    /// Records the conversion from `actual` to `target` on `e`, or reports a
    /// mismatch. Returns whether it worked.
    fn coerce(&mut self, e: &Expr, actual: Ty, target: Ty) -> bool {
        let (actual, target) = (self.resolve(actual), self.resolve(target));
        if actual == target || self.tcx.is_lenient(actual) || self.tcx.is_error(target) {
            return true;
        }
        // A literal whose type is still open takes the target's.
        if (self.has_vars(actual) || self.has_vars(target)) && self.unify(actual, target) {
            return true;
        }
        if let TyKind::Optional(inner) = *self.tcx.kind(target)
            && self.has_vars(actual)
            && self.unify(actual, inner)
        {
            self.r.coercions.insert(e.id, Coercion::ToOptional);
            return true;
        }
        if let TyKind::Result(ok, _) = *self.tcx.kind(target)
            && (ok == actual || (self.has_vars(actual) && self.unify(actual, ok)))
        {
            self.r.coercions.insert(e.id, Coercion::ToResult);
            return true;
        }
        let coercion = match (self.tcx.kind(actual).clone(), self.tcx.kind(target).clone()) {
            (TyKind::Int(a), TyKind::Int(b)) if a.widens_to(b) => Some(Coercion::Widen),
            (TyKind::Str, TyKind::Adt(def, _)) if Some(def) == self.std.string && is_str_literal(e) => {
                Some(Coercion::StrToString)
            }
            (TyKind::Adt(def, _), TyKind::Str) if Some(def) == self.std.string && self.in_argument => {
                Some(Coercion::StringToStr)
            }
            (_, TyKind::Optional(inner)) if inner == actual => Some(Coercion::ToOptional),
            (_, TyKind::Any(wefts)) => {
                // Whether a warp weaves a weft is only known once weaves are
                // type-checked; until then, say so rather than guess.
                if self.adt_info(actual).is_some() {
                    self.unsupported(e.span, "weaving wefts into warps");
                    return false;
                }
                let missing: Vec<DefId> = wefts.iter().copied().filter(|&w| !self.weaves(actual, w)).collect();
                if let Some(&weft) = missing.first() {
                    let (ty, weft) = (self.show(actual), self.p.def(weft).name.clone());
                    self.error(
                        codes::T0108,
                        e.span,
                        format!("`{ty}` doesn't weave `{weft}`"),
                        format!("this is `{ty}`, which isn't `{weft}`"),
                    );
                    return false;
                }
                Some(Coercion::ToAny(wefts))
            }
            _ => None,
        };
        match coercion {
            Some(coercion) => {
                self.r.coercions.insert(e.id, coercion);
                true
            }
            None => {
                self.mismatch(e.span, target, actual);
                let is_string = matches!(self.tcx.kind(actual), TyKind::Adt(d, _) if Some(*d) == self.std.string);
                if is_string
                    && matches!(self.tcx.kind(target), TyKind::Str)
                    && let Some(last) = self.diags.last_mut()
                {
                    *last = std::mem::replace(last, Diagnostic::error(codes::T0101, ""))
                        .note("a `str` is a view of text that lives elsewhere; this `String` would be gone")
                        .help("store or return a `String` instead; a `String` argument can be passed to a `str` parameter");
                }
                false
            }
        }
    }

    /// Whether `ty` weaves `weft`. For now only the built-in weaves of the
    /// primitive types and `String` are known.
    fn weaves(&self, ty: Ty, weft: DefId) -> bool {
        let ty = self.resolve(ty);
        let is_string = |ty: Ty| matches!(self.tcx.kind(ty), TyKind::Adt(d, _) if Some(*d) == self.std.string);
        let kind = self.tcx.kind(ty);
        let numeric = matches!(kind, TyKind::Int(_) | TyKind::Float(_) | TyKind::IntVar(_) | TyKind::FloatVar(_));
        let scalar = numeric || matches!(kind, TyKind::Bool | TyKind::Char);
        let all = |this: &Self, items: &[Ty]| items.iter().all(|&t| this.weaves(t, weft));
        let w = Some(weft);
        // `?T` shows as the value or `none`, and compares, copies and hashes
        // like `T` does.
        if let TyKind::Optional(inner) = kind
            && [self.std.show, self.std.eq, self.std.copy, self.std.hash].contains(&w)
        {
            return self.weaves(*inner, weft);
        }
        if w == self.std.show {
            scalar || matches!(kind, TyKind::Str) || is_string(ty)
        } else if w == self.std.eq {
            scalar
                || matches!(kind, TyKind::Str | TyKind::Unit)
                || is_string(ty)
                || matches!(kind, TyKind::Tuple(items) if all(self, items))
        } else if w == self.std.ord {
            numeric || matches!(kind, TyKind::Char | TyKind::Str) || is_string(ty)
        } else if w == self.std.hash {
            matches!(kind, TyKind::Int(_) | TyKind::IntVar(_) | TyKind::Bool | TyKind::Char | TyKind::Str)
                || is_string(ty)
        } else if w == self.std.copy {
            scalar
                || matches!(kind, TyKind::Str | TyKind::Unit)
                || matches!(kind, TyKind::Tuple(items) if all(self, items))
        } else if w == self.std.integer {
            matches!(kind, TyKind::Int(_) | TyKind::IntVar(_))
        } else {
            false
        }
    }

    /// Whether values of `ty` are Lua tables, which a `mut` parameter can
    /// share with its caller.
    fn is_table_backed(&mut self, ty: Ty) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty).clone() {
            TyKind::Tuple(items) => !items.is_empty(),
            TyKind::Adt(..) => self.adt_info(ty).is_some_and(|i| !i.unit_only()),
            _ => false,
        }
    }

    fn string_ty(&mut self) -> Ty {
        match self.std.string {
            Some(def) => self.tcx.intern(TyKind::Adt(def, Vec::new())),
            None => self.tcx.error,
        }
    }
}

/// Whether `e` is a plain string literal (possibly in a block), which may
/// become a `String`.
fn is_str_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Lit(loom_syntax::ast::Lit::Str(_)) => true,
        ExprKind::Block(b) => match b.stmts.last().map(|s| &s.kind) {
            Some(hir::StmtKind::Expr(tail)) => is_str_literal(tail),
            _ => false,
        },
        _ => false,
    }
}

/// The value of an integer literal, possibly negated: what an untyped
/// constant can hold.
fn int_literal_value(e: &Expr) -> Option<i128> {
    match &e.kind {
        ExprKind::Lit(loom_syntax::ast::Lit::Int(i)) if i.suffix.is_none() => Some(i.value as i128),
        ExprKind::Unary { op: loom_syntax::ast::UnaryOp::Neg, operand } => int_literal_value(operand).map(|v| -v),
        _ => None,
    }
}
