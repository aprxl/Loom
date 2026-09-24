//! Type checking: HIR → types for every expression, pattern and local.
//!
//! The results live in side tables keyed by [`HirId`] and [`LocalId`]
//! rather than in a new tree: lowering reads the HIR and the tables
//! together. Each checked [`Instance`] (a function for one `Self`, a
//! constant, or a warp's field defaults) has its own [`Body`] of tables, so
//! the same HIR can be checked more than once with different types.
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
//! dot-blocks, case and twill patterns; wefts and weaves (see `weave.rs`).
//! Everything else is reported with U0102 ("not implemented yet") rather
//! than guessed at.

mod call;
mod expr;
mod infer;
mod pat;
#[cfg(test)]
mod tests;
mod warp;
mod weave;

use std::collections::{HashMap, HashSet, VecDeque};

use loom_syntax::Diagnostic;
use loom_syntax::ast::ParamMode;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use crate::hir::{self, DefId, DefKind, Expr, ExprKind, FnRet, HirId, LocalId, Program, TyKind as HirTy};
use crate::ty::{Ty, TyCtx, TyKind};
use infer::{PendingLit, Var};
pub use warp::{CaseFieldInfo, CaseInfo, FieldInfo, Key, WarpInfo};
pub use weave::{Impl, Weaving};

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

/// A body checked for particular types: a function or constant, or the
/// field defaults of a warp.
///
/// The same HIR can be checked more than once. A weft's default method is
/// checked once for each warp that uses it, with `Self` standing for that
/// warp, so each check has its own [`Body`] of results.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Instance {
    pub def: DefId,
    /// What `Self` stands for, when it isn't simply the warp or weave the
    /// item belongs to.
    pub self_ty: Option<Ty>,
}

impl Instance {
    /// `def` checked as written.
    pub fn of(def: DefId) -> Self {
        Instance { def, self_ty: None }
    }
}

/// An instance's index in [`Results::bodies`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BodyId(pub u32);

/// Types and decisions recorded for lowering.
#[derive(Debug, Default)]
pub struct Results {
    /// Every instance, indexed by [`BodyId`].
    pub instances: Vec<Instance>,
    pub bodies: Vec<Body>,
    ids: HashMap<Instance, BodyId>,
    /// The signature of each function instance.
    pub sigs: HashMap<BodyId, FnSig>,
    pub const_ty: HashMap<DefId, Ty>,
    /// Constants declared without a type whose value is an integer literal.
    /// Like Go's untyped constants, each use gets the type its context
    /// needs, so lowering writes the value in place.
    pub untyped_consts: HashMap<DefId, i128>,
    /// The layout of every warp used.
    pub warps: HashMap<DefId, WarpInfo>,
    /// Every function and constant checked, in the order they were reached.
    pub reachable: Vec<BodyId>,
    /// Hand-written `Show`, `Eq` and `Ord` methods, which built-in
    /// operations (`{x}`, `==`, `<`) call through the type's runtime
    /// description.
    pub op_fns: HashMap<Ty, OpFns>,
}

/// The functions behind a type's built-in operations, when they aren't
/// the derived ones.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpFns {
    pub show: Option<BodyId>,
    pub eq: Option<BodyId>,
    pub lt: Option<BodyId>,
}

impl Results {
    pub fn instance(&self, id: BodyId) -> &Instance {
        &self.instances[id.0 as usize]
    }

    pub fn body(&self, id: BodyId) -> &Body {
        &self.bodies[id.0 as usize]
    }

    /// The body checked for `inst`, if it was.
    pub fn find(&self, inst: &Instance) -> Option<BodyId> {
        self.ids.get(inst).copied()
    }
}

/// What checking one body found, keyed by the ids of its HIR nodes.
#[derive(Debug, Default)]
pub struct Body {
    /// The type of each expression, before any coercion.
    pub expr_ty: HashMap<HirId, Ty>,
    pub pat_ty: HashMap<HirId, Ty>,
    pub local_ty: HashMap<LocalId, Ty>,
    /// Conversions applied to an expression's value where it's used.
    pub coercions: HashMap<HirId, Coercion>,
    /// For each call: which argument (or default) feeds each parameter.
    pub calls: HashMap<HirId, Call>,
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
    /// The instance of the function called.
    pub callee: BodyId,
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
    /// Where the warp's field defaults were checked.
    pub defaults: BodyId,
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
    c.collect_weaves();
    if let Some(root) = p.root {
        for &def in &p.module(root).decls {
            c.queue_def(def);
        }
    }
    // Weaves outside std are checked wherever they are: a hand-written
    // `show` must be known even when nothing calls it by name.
    let sites: Vec<DefId> = c.weavings.iter().map(|w| w.site).collect();
    for site in sites {
        if !c.is_std_module(p.def(site).module) {
            c.queue_def(site);
        }
    }
    while let Some(id) = c.queue.pop_front() {
        c.check_instance(id);
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
    /// The tables of the body being checked.
    b: Body,
    diags: Vec<Diagnostic>,
    std: Std,
    queue: VecDeque<BodyId>,
    /// Every weave in the program (see `weave.rs`).
    weavings: Vec<Weaving>,
    /// The first weaving of each weft into each type.
    weave_index: HashMap<(DefId, Ty), usize>,
    /// Call arguments whose type was already checked, to find the method
    /// they're passed to (`Show.show(x, …)`).
    prechecked: HashMap<HirId, Ty>,
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
            b: Body::default(),
            diags: Vec::new(),
            std,
            queue: VecDeque::new(),
            weavings: Vec::new(),
            weave_index: HashMap::new(),
            prechecked: HashMap::new(),
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

    /// The body for `inst`, queuing it to be checked the first time.
    fn instance(&mut self, inst: Instance) -> BodyId {
        if let Some(id) = self.r.find(&inst) {
            return id;
        }
        let id = BodyId(self.r.instances.len() as u32);
        self.r.ids.insert(inst.clone(), id);
        self.r.instances.push(inst);
        self.r.bodies.push(Body::default());
        self.queue.push_back(id);
        id
    }

    fn queue_def(&mut self, def: DefId) -> BodyId {
        self.instance(Instance::of(def))
    }

    /// Runs `f` with `id`'s tables as the current ones, then puts back the
    /// tables of the body that was being checked. A body can be filled in
    /// more than one sitting: a constant is checked where it's first used.
    fn in_body<T>(&mut self, id: BodyId, f: impl FnOnce(&mut Self) -> T) -> T {
        let tables = std::mem::take(&mut self.r.bodies[id.0 as usize]);
        let saved = std::mem::replace(&mut self.b, tables);
        let out = f(self);
        self.r.bodies[id.0 as usize] = std::mem::replace(&mut self.b, saved);
        out
    }

    fn check_instance(&mut self, id: BodyId) {
        let p = self.p;
        let inst = self.r.instance(id).clone();
        let d = p.def(inst.def);
        self.current = Some(inst.def);
        match &d.kind {
            DefKind::Fn(f) => {
                self.r.reachable.push(id);
                self.in_body(id, |this| this.check_fn(id, &inst, f));
            }
            DefKind::Const(_) => {
                self.r.reachable.push(id);
                self.const_ty(inst.def);
            }
            DefKind::Warp(w) => {
                self.in_body(id, |this| this.check_warp(inst.def, w));
                if !self.is_std_module(d.module) && w.generics.is_empty() {
                    for t in &w.wefts {
                        self.weft_arg_problem(t);
                    }
                    self.check_weaves_at(inst.def);
                }
            }
            DefKind::Weave(w) => self.check_weave_item(inst.def, w),
            DefKind::Weft(w) => self.check_weft(inst.def, w),
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
                    DefKind::Fn(_) | DefKind::Const(_) => {
                        self.queue_def(member);
                    }
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

    /// What `Self` means for a member: its warp, or the type its weave
    /// weaves into.
    fn parent_warp_ty(&mut self, def: DefId) -> Option<Ty> {
        let parent = self.p.def(def).parent?;
        match &self.p.def(parent).kind {
            DefKind::Warp(_) | DefKind::Pending => Some(self.tcx.intern(TyKind::Adt(parent, Vec::new()))),
            DefKind::Weave(w) if w.generics.is_empty() => {
                let saved = self.current.replace(parent);
                let ty = self.lower_ty(&w.target);
                self.current = saved;
                Some(ty)
            }
            _ => None,
        }
    }

    /// A `weave W into T { … }` item.
    fn check_weave_item(&mut self, def: DefId, w: &hir::WeaveDef) {
        if !w.generics.is_empty() {
            self.unsupported(self.p.def(def).span, "generic weaves");
            return;
        }
        if self.weft_arg_problem(&w.weft) {
            return;
        }
        self.check_weaves_at(def);
    }

    /// Reports a type used where a weft must be, if it isn't one. Returns
    /// whether there was a problem.
    fn weft_arg_problem(&mut self, t: &hir::Ty) -> bool {
        match &t.kind {
            HirTy::Def { def, args } if matches!(self.p.def(*def).kind, DefKind::Weft(_)) => {
                if args.is_empty() {
                    false
                } else {
                    self.unsupported(t.span, "generic wefts");
                    true
                }
            }
            HirTy::Error => true,
            _ => {
                let ty = self.lower_ty(t);
                if !self.tcx.is_error(ty) {
                    let shown = self.show(ty);
                    self.error(codes::T0111, t.span, format!("`{shown}` isn't a weft"), "expected a weft here");
                }
                true
            }
        }
    }

    /// A weft's own declarations. Its default methods are checked for each
    /// type that uses them, with `Self` standing for that type.
    fn check_weft(&mut self, def: DefId, w: &hir::WeftDef) {
        if !w.generics.is_empty() {
            self.unsupported(self.p.def(def).span, "generic wefts");
            return;
        }
        for sup in &w.supers {
            self.weft_arg_problem(sup);
        }
        for &member in &w.members {
            match &self.p.def(member).kind {
                DefKind::Fn(f) if !f.has_self() => {
                    self.unsupported(self.p.def(member).span, "weft functions without `self`");
                }
                DefKind::Type(_) => {
                    self.unsupported(self.p.def(member).span, "associated types");
                }
                DefKind::Const(_) => {
                    self.unsupported(self.p.def(member).span, "constants in wefts");
                }
                _ => {}
            }
        }
    }

    fn check_fn(&mut self, id: BodyId, inst: &Instance, f: &hir::FnDef) {
        self.self_ty = self.instance_self(inst);
        self.self_local = f.params.first().filter(|p| p.is_self).map(|p| p.local);
        self.check_fn_body(id, f);
        self.self_ty = None;
        self.self_local = None;
    }

    /// What `Self` means in `inst`.
    fn instance_self(&mut self, inst: &Instance) -> Option<Ty> {
        inst.self_ty.or_else(|| self.parent_warp_ty(inst.def))
    }

    fn check_fn_body(&mut self, id: BodyId, f: &hir::FnDef) {
        let Some(sig) = self.fn_sig(id) else { return };
        let first_var = self.vars.len();
        for (param, sig_param) in f.params.iter().zip(&sig.params) {
            self.b.local_ty.insert(param.local, sig_param.ty);
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

    /// The signature of a function instance, computed on first use. `None`
    /// if it uses something unsupported (already reported).
    fn fn_sig(&mut self, id: BodyId) -> Option<FnSig> {
        if let Some(sig) = self.r.sigs.get(&id) {
            return Some(sig.clone());
        }
        let p = self.p;
        let inst = self.r.instance(id).clone();
        let DefKind::Fn(f) = &p.def(inst.def).kind else { return None };
        let span = p.def(inst.def).span;
        let saved = self.current.replace(inst.def);
        let this = self.instance_self(&inst);
        let saved_self = std::mem::replace(&mut self.self_ty, this);
        let sig = self.lower_sig(f, span);
        self.self_ty = saved_self;
        self.current = saved;
        let sig = sig?;
        self.r.sigs.insert(id, sig.clone());
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
        let id = self.queue_def(def);
        let (saved_current, saved_ret) = (self.current.replace(def), self.ret.take());
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_lits = std::mem::take(&mut self.pending_lits);
        let saved_negations = std::mem::take(&mut self.pending_negations);
        let saved_subjects = std::mem::take(&mut self.subjects);
        let saved_self_local = self.self_local.take();
        let parent = self.parent_warp_ty(def);
        let saved_self = std::mem::replace(&mut self.self_ty, parent);
        let first_var = self.vars.len();
        let ty = self.in_body(id, |this| {
            let ty = match &c.ty {
                Some(t) => {
                    let ty = this.lower_ty(t);
                    this.check_coerce(&c.value, ty)
                }
                None => {
                    if let Some(value) = int_literal_value(&c.value) {
                        this.r.untyped_consts.insert(def, value);
                    }
                    this.check_expr(&c.value, None)
                }
            };
            this.finish_body(first_var);
            this.deep_resolve(ty)
        });
        self.current = saved_current;
        self.ret = saved_ret;
        self.loops = saved_loops;
        self.pending_lits = saved_lits;
        self.pending_negations = saved_negations;
        self.subjects = saved_subjects;
        self.self_local = saved_self_local;
        self.self_ty = saved_self;
        self.const_stack.pop();
        self.r.const_ty.insert(def, ty);
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
            self.b.coercions.insert(e.id, Coercion::ToOptional);
            return true;
        }
        if let TyKind::Result(ok, _) = *self.tcx.kind(target)
            && (ok == actual || (self.has_vars(actual) && self.unify(actual, ok)))
        {
            self.b.coercions.insert(e.id, Coercion::ToResult);
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
                self.b.coercions.insert(e.id, coercion);
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

    /// Whether `ty` weaves `weft`: built in (the primitive types, `String`,
    /// tuples and optionals of them), or through a weave.
    fn weaves(&self, ty: Ty, weft: DefId) -> bool {
        let ty = self.resolve(ty);
        if self.weave_index.contains_key(&(weft, ty)) {
            return true;
        }
        // A warp is `Copy` when all its fields are, whether it says so or not.
        if Some(weft) == self.std.copy
            && let TyKind::Adt(def, _) = self.tcx.kind(ty)
            && let Some(info) = self.r.warps.get(def)
        {
            return info.copy;
        }
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
            scalar
                || matches!(kind, TyKind::Str)
                || is_string(ty)
                || matches!(kind, TyKind::Tuple(items) if !items.is_empty() && all(self, items))
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
            TyKind::Adt(def, _) if self.is_builtin(def) => Some(def) != self.std.string,
            TyKind::Adt(..) => self.adt_info(ty).is_some_and(|i| !i.unit_only()),
            _ => false,
        }
    }

    /// A warp std declares `#builtin`: the runtime implements it (`String`, `Fmt`).
    fn is_builtin(&self, def: DefId) -> bool {
        self.p.def(def).attrs.iter().any(|a| a.name == "builtin")
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
