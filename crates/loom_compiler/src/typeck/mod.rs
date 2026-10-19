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
//! dot-blocks, case and twill patterns; wefts and weaves (see `weave.rs`);
//! generics (see `generic.rs`); `any W` (see `dynamic.rs`); optionals and
//! errors (see `fallible.rs`); arrays, lists and slices (see `seq.rs`).
//! Everything else is reported with U0102 ("not implemented yet") rather
//! than guessed at.

mod call;
mod consts;
mod dynamic;
mod expr;
mod fallible;
mod gauge;
mod generic;
mod infer;
mod iter;
mod lambda;
mod method;
mod moves;
mod pat;
mod reflect;
mod seq;
mod shuttle;
#[cfg(test)]
mod tests;
mod thread;
mod unroll;
mod usefulness;
mod warp;
mod weave;

use std::collections::{HashMap, HashSet, VecDeque};

use loom_syntax::Diagnostic;
use loom_syntax::ast::{FnQualifier, ParamMode};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use crate::hir::{self, DefId, DefKind, Expr, ExprKind, FnRet, HirId, LocalId, Program, TyKind as HirTy};
use crate::ty::{Ty, TyCtx, TyKind};
pub(crate) use consts::assert_args;
pub use fallible::{ErrConv, Exit, MendInfo, TryInfo, Unwrap};
pub use gauge::{GaugeInfo, GaugeRange};
use infer::{PendingLit, Var};
pub(crate) use reflect::QUERIES as REFLECTION_QUERIES;
pub use seq::Seq;
pub use warp::{CaseFieldInfo, CaseInfo, FieldInfo, Key, WarpInfo};
pub use weave::{Impl, Weaving};

/// How many instances in a row one may lead to (Rust stops at 128).
const MAX_INSTANCE_DEPTH: u32 = 100;

/// The most parts a type an instance is made for may have.
const MAX_TYPE_SIZE: usize = 10_000;

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
    /// A weft method `Self` gets from a `through` field: its body calls the
    /// field's method (see [`Results::forwards`]).
    pub forward: bool,
    /// What each generic parameter stands for: the parameters of the warp
    /// it's in first, then its own, then one per weft-typed parameter.
    pub args: Vec<Ty>,
}

impl Instance {
    /// `def` checked as written.
    pub fn of(def: DefId) -> Self {
        Instance { def, self_ty: None, forward: false, args: Vec::new() }
    }
}

/// What a forwarding instance calls.
#[derive(Clone, Debug)]
pub struct Forward {
    /// The `through` fields from `self` to the value the method is called on.
    pub path: Vec<Key>,
    pub target: BodyId,
}

/// An instance's index in [`Results::bodies`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// Untyped constants whose value is a float, like `const TAU = 6.28`.
    pub untyped_floats: HashMap<DefId, f64>,
    /// The layout of every warp used. A generic warp's field types mention
    /// its parameters.
    pub warps: HashMap<DefId, WarpInfo>,
    /// The layout of every warp type used, with its field types for its
    /// generic arguments: `Pair[i32, str]`.
    pub adts: HashMap<Ty, WarpInfo>,
    /// Every function and constant checked, in the order they were reached.
    pub reachable: Vec<BodyId>,
    /// Hand-written `Show`, `Eq` and `Ord` methods, which built-in
    /// operations (`{x}`, `==`, `<`) call through the type's runtime
    /// description.
    pub op_fns: HashMap<Ty, OpFns>,
    /// The method each forwarding instance calls.
    pub forwards: HashMap<BodyId, Forward>,
    /// The vtable of each type made into an `any`, by the type and the wefts
    /// (with their super-wefts): the function for each weft method called
    /// through `any`.
    pub vtables: HashMap<VtableKey, Vec<(DefId, BodyId)>>,
    /// Each gauge used: its base type and what it allows.
    pub gauges: HashMap<DefId, gauge::GaugeInfo>,
    /// `const` items (`const assert(…)` at the top of a module or in a
    /// warp) left for the sandbox to run, in the order they were checked.
    pub const_blocks: Vec<BodyId>,
    /// Each `const` expression checked, by the body it was checked in and
    /// its id, in the order they were checked (one may be there more than
    /// once: a `const for` checks its body for each item). A `const` in a
    /// branch that isn't taken isn't here, and one in a field's or a
    /// parameter's default is, in the body that used the default.
    pub const_exprs: Vec<(BodyId, crate::hir::HirId)>,
    /// What each instance checked for particular types (or values) is, for
    /// messages: `` `zeros` with `n = 4` ``, `` `Ring[u32, 4]` ``.
    pub instance_names: HashMap<BodyId, String>,
}

/// A vtable: the type made into an `any`, and the wefts (with their
/// super-wefts, sorted).
pub type VtableKey = (Ty, Vec<DefId>);

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
    /// The type each converted expression's value is converted to.
    pub coerced: HashMap<HirId, Ty>,
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
    /// Type-test patterns (`is b: Button`), and the type each tests for.
    pub type_tests: HashMap<HirId, Ty>,
    /// Every `?`: what it unwraps, and where it goes when there's nothing.
    pub tries: HashMap<HirId, TryInfo>,
    pub mends: HashMap<HirId, MendInfo>,
    /// Every `fail`: the type of its error, and how it becomes the
    /// function's error type.
    pub fails: HashMap<HirId, (Ty, ErrConv)>,
    /// `List()` calls: empty lists.
    pub new_lists: HashSet<HirId>,
    /// `Atomic[T](value)` calls (and the other types of `std.sync`).
    pub sync_new: HashSet<HirId>,
    /// Calls of function values: lambdas, and functions passed around.
    pub closure_calls: HashSet<HirId>,
    /// Functions used as values (`xs.filter(is_prime)`): which instance.
    pub fn_values: HashMap<HirId, BodyId>,
    /// Values turned into a `Stream` to loop over or to call its methods
    /// on: the `for` loop, or the method call, and where the items come from.
    pub streams: HashMap<HirId, IterSource>,
    /// Expressions whose value moves out of a local (or part of it): which
    /// local. A fallback `defer` on it is off from there.
    pub moves: HashMap<HirId, LocalId>,
    /// For each `thread`: the locals it gets copies of, because they don't
    /// outlive its loom (a loop variable, a lambda's parameter).
    pub thread_copies: HashMap<HirId, Vec<LocalId>>,
    /// For each `thread`: the locals that don't outlive its loom and own
    /// something, which move into it (an item of a `loom for`, say).
    pub thread_moves: HashMap<HirId, Vec<LocalId>>,
    /// Patterns that test for a gauge (`x is Cents`, `c: Cents =>`): which.
    pub gauge_tests: HashMap<HirId, Ty>,
    /// Uses of `const` generic parameters (`N`): their values here.
    pub const_values: HashMap<HirId, crate::ty::ConstVal>,
    /// Each `const if`: whether it takes its first branch.
    pub const_ifs: HashMap<HirId, bool>,
    /// Each `v.(c)`: `v` as its case `c`, whose payload `.(f)` reads
    /// (see `reflect.rs`).
    pub case_views: HashMap<HirId, DefId>,
    /// Each `const for`: the body checked for each item, in order.
    pub unrolled: HashMap<HirId, Vec<BodyId>>,
    /// Each `f.Frame.size` and `f.Frame.align`: which shuttle's frame.
    pub frames: HashMap<HirId, FrameQuery>,
    /// Calls of projections (`fn at(mut self, i: usize) ~> mut T`): each is
    /// a place, for the statement it's in, and whether it can be changed.
    pub lends: HashMap<HirId, PlaceMode>,
}

/// `f.Frame.size` (or `.align`): about the frame of shuttle function
/// instance `shuttle`, which is worked out on Treadle's Heddle, so it's
/// known once the program is built (see `heddle::frame`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameQuery {
    pub shuttle: BodyId,
    pub align: bool,
}

pub use loom_syntax::ast::PlaceMode;

/// How a value that isn't a `Stream` gives its items one at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IterSource {
    /// An array, list or slice of this element type: copies of its
    /// elements.
    Seq(Ty),
    /// A range value, of this element type.
    Range(Ty),
    /// A type that weaves `Iter`: its `next`, until it gives `none`. `item`
    /// is the type of what it gives.
    Woven { next: BodyId, item: Ty },
    /// A map's pairs (copies), by its key and value types.
    MapPairs(Ty, Ty),
    /// A set's elements (copies).
    SetItems(Ty),
    /// A generator (`Shuttle[T, ()]`): what it yields, until it ends.
    Shuttle(Ty),
    /// A `Burst[T]`: what its shuttle yields up to its next `pull`.
    Burst(Ty),
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
    /// A value where `any W` is expected. The wefts are `W` and its
    /// super-wefts, sorted: with the value's type, they pick its vtable.
    ToAny(Vec<DefId>),
    /// A value where a union that has its type is expected. It's boxed
    /// like an `any`, with a vtable that only describes its type.
    ToUnion,
    /// An array or a list where a slice (`[T]`) is expected: a view of
    /// all of it.
    ToSlice,
    /// A value that's thrown away, where `()` is expected.
    Discard,
    /// A union where a union of more types is expected: the same value,
    /// with its tag renumbered (on Treadle; on Lua, nothing changes).
    WidenUnion,
    /// An array, list, slice, range or `Iter` where a `Stream` of its
    /// items is expected.
    ToStream(IterSource),
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
    /// For a `const` parameter: its value, a [`TyKind::Value`] in an
    /// instance, or a variable at a call until the argument gives it.
    pub value: Option<Ty>,
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
    /// For a method a `through` field has: the fields to follow from the
    /// receiver to the value it's called on (`button.size()` calls
    /// `button.frame.size()`).
    pub through: Vec<Key>,
    /// For a method called on an `any` value: the weft method, found in the
    /// value's vtable at run time. `callee` means nothing then.
    pub dynamic: Option<DefId>,
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

/// Type-checks everything reachable from the root module of `p`, for the
/// Lua target.
pub fn check(p: &Program) -> Typed {
    check_for(p, crate::target::Target::LUA)
}

/// Type-checks everything reachable from the root module of `p`, for
/// `target`: `usize` and `target` differ between targets.
pub fn check_for(p: &Program, target: crate::target::Target) -> Typed {
    let mut c = Checker::new(p, target);
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
    // Describing the warp types the results mention can need more
    // instances (a generic warp's `show`), which need checking in turn.
    loop {
        while let Some(id) = c.queue.pop_front() {
            c.check_instance(id);
        }
        c.zonk();
        if !c.register_types() {
            break;
        }
    }
    for i in 0..c.r.instances.len() {
        let inst = c.r.instances[i].clone();
        if !inst.args.is_empty() || inst.self_ty.is_some() {
            let name = c.describe_instance(&inst);
            c.r.instance_names.insert(BodyId(i as u32), name);
        }
    }
    Typed { tcx: c.tcx, results: c.r, diagnostics: c.diags }
}

/// Code that control flow can't leave.
#[derive(Clone, Copy, Debug)]
enum Fence {
    /// A `defer`'s body, from where it's declared.
    Defer(Span),
    /// Compile-time code: a `const` expression.
    Const(Span),
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
    missing: Option<DefId>,
    list: Option<DefId>,
    slice: Option<DefId>,
    /// The builtin warps whose methods primitive types have.
    str_warp: Option<DefId>,
    char_warp: Option<DefId>,
    int_warp: Option<DefId>,
    optional: Option<DefId>,
    result_warp: Option<DefId>,
    float_warp: Option<DefId>,
    parse: Option<DefId>,
    num: Option<DefId>,
    map: Option<DefId>,
    set: Option<DefId>,
    iter: Option<DefId>,
    stream: Option<DefId>,
    range_warp: Option<DefId>,
    shuttle: Option<DefId>,
    burst: Option<DefId>,
    spun: Option<DefId>,
    atomic: Option<DefId>,
    mutex: Option<DefId>,
    pipe: Option<DefId>,
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
    /// What each generic parameter stands for, where types are being lowered.
    subst: Vec<(DefId, Ty)>,
    /// How a weft as a parameter's type is read (see `generic.rs`).
    implicit: generic::Implicit,
    /// Generic instances the body needs, until its variables are known.
    pending: Vec<generic::Pending>,
    /// Lowering the type in `Pair(1, 2)` or `Pair.new(…)`: a generic warp
    /// without arguments gets variables for them.
    infer_args: bool,
    /// Types `register_types` has seen.
    registered: HashSet<Ty>,
    /// Diagnostics already reported from generic instances (see `note_instance`).
    instance_diags: HashSet<(&'static str, Option<Span>)>,
    /// Weft methods called through `any` (see `dynamic.rs`).
    dyn_methods: HashSet<DefId>,
    /// `mend`s whose left operand is being checked, innermost last: where a
    /// `?` goes (see `fallible.rs`).
    mends: Vec<fallible::MendCtx>,
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
    /// `I.Item` where `I` is a variable: the variable standing for it, `I`,
    /// the name, and where (see `iter.rs`).
    projections: Vec<(Ty, Ty, String, Span)>,
    /// Values interpolated into templates, which must be `Show`.
    pending_shows: Vec<(Ty, Span)>,

    /// Checking a call argument directly: where a `String` may be viewed as a `str`.
    in_argument: bool,
    /// How many `loom` blocks enclose what is being checked, in this body.
    looms: u32,
    /// Checking the body of an iteration of a `loom for` (not a lambda in it).
    in_item: bool,
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
    /// The `defer` or compile-time code being checked: nothing in it may
    /// leave it.
    fence: Option<Fence>,
    /// Inside compile-time code: the locals declared outside it, which have
    /// no values yet while compiling.
    runtime_locals: Option<HashSet<LocalId>>,
    /// The `const` parameters in scope, with their values (or variables).
    const_locals: Vec<(LocalId, Ty)>,
    /// How many times something unsupported was met, reported or not:
    /// `unsupported` reports each thing once per definition.
    unsupported_hits: usize,
    /// The body whose tables are `self.b`.
    cur_body: Option<BodyId>,
    /// `const for` loops: `break` and `continue` can't target them.
    const_loops: HashSet<HirId>,
    /// How many instances led to each (none for what the program names
    /// itself): `inf(n + 1)` would lead to new ones forever.
    depths: HashMap<BodyId, u32>,
    /// The definitions whose instances nested too deeply (reported).
    too_deep: HashSet<DefId>,
    /// How many bodies `const for` loops have written out, and whether
    /// going past the limit was reported.
    unrolled_total: usize,
    unroll_reported: bool,
    /// Gauges being worked out, or found wrong (reported once).
    gauge_errors: HashSet<DefId>,
    /// The shuttle whose body is being checked.
    shuttle: Option<shuttle::ShuttleCtx>,
}

impl<'p> Checker<'p> {
    fn new(p: &'p Program, target: crate::target::Target) -> Self {
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
            missing: find("std.error", "Missing"),
            list: find("std.list", "List"),
            slice: find("std.slice", "Slice"),
            str_warp: find("std.string", "Str"),
            char_warp: find("std.string", "Char"),
            int_warp: find("std.num", "Int"),
            optional: find("std.option", "Optional"),
            result_warp: find("std.error", "Fallible"),
            float_warp: find("std.num", "Float"),
            parse: find("std.num", "Parse"),
            num: find("std.num", "Num"),
            map: find("std.map", "Map"),
            set: find("std.set", "Set"),
            iter: find("std.iter", "Iter"),
            stream: find("std.iter", "Stream"),
            range_warp: find("std.iter", "Range"),
            shuttle: find("std.iter", "Shuttle"),
            burst: find("std.iter", "Burst"),
            spun: find("std.sync", "Spun"),
            atomic: find("std.sync", "Atomic"),
            mutex: find("std.sync", "Mutex"),
            pipe: find("std.sync", "Pipe"),
        };
        Checker {
            p,
            tcx: TyCtx::new(target),
            r: Results::default(),
            b: Body::default(),
            diags: Vec::new(),
            std,
            queue: VecDeque::new(),
            weavings: Vec::new(),
            weave_index: HashMap::new(),
            prechecked: HashMap::new(),
            subst: Vec::new(),
            implicit: generic::Implicit::Off,
            pending: Vec::new(),
            infer_args: false,
            registered: HashSet::new(),
            instance_diags: HashSet::new(),
            dyn_methods: HashSet::new(),
            mends: Vec::new(),
            const_stack: Vec::new(),
            unsupported_seen: HashSet::new(),
            vars: Vec::new(),
            pending_lits: Vec::new(),
            pending_negations: Vec::new(),
            projections: Vec::new(),
            pending_shows: Vec::new(),
            in_argument: false,
            looms: 0,
            in_item: false,
            self_ty: None,
            self_local: None,
            subjects: Vec::new(),
            current: None,
            ret: None,
            loops: Vec::new(),
            fence: None,
            runtime_locals: None,
            const_locals: Vec::new(),
            cur_body: None,
            unsupported_hits: 0,
            const_loops: HashSet::new(),
            depths: HashMap::new(),
            too_deep: HashSet::new(),
            unrolled_total: 0,
            unroll_reported: false,
            gauge_errors: HashSet::new(),
            shuttle: None,
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
        self.unsupported_hits += 1;
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

    /// The body for `inst`, queuing it to be checked the first time. Its
    /// types are resolved first: an instance is keyed by what they are.
    fn instance(&mut self, mut inst: Instance) -> BodyId {
        // `f[(T, T)]` calling itself doubles its type each time: that
        // nests without end too, and gets too big to walk long before.
        let mut sizes = HashMap::new();
        let types = inst.args.iter().chain(inst.self_ty.iter());
        let too_big = types.copied().any(|t| self.ty_size(t, &mut sizes) > MAX_TYPE_SIZE);
        if !too_big {
            inst.args = inst.args.iter().map(|&t| self.deep_resolve(t)).collect();
            inst.self_ty = inst.self_ty.map(|t| self.deep_resolve(t));
        }
        if let Some(id) = self.r.find(&inst) {
            return id;
        }
        let id = BodyId(self.r.instances.len() as u32);
        let depth = self.cur_body.and_then(|b| self.depths.get(&b)).map_or(0, |d| d + 1);
        self.depths.insert(id, depth);
        self.r.ids.insert(inst.clone(), id);
        self.r.instances.push(inst.clone());
        self.r.bodies.push(Body::default());
        if depth <= MAX_INSTANCE_DEPTH && !too_big {
            self.queue.push_back(id);
        } else if self.too_deep.insert(inst.def) {
            // Never checked: the program doesn't compile.
            let def = self.p.def(inst.def);
            let mut diag = Diagnostic::error(codes::T0122, format!("instances of `{}` nest without end", def.name))
                .primary(def.span, "each instance needs another")
                .help("a generic function that calls itself needs the same arguments, or ones that stop changing");
            diag = if too_big {
                diag.note(format!("the types it's instantiated with grow past {MAX_TYPE_SIZE} parts"))
            } else {
                diag.note(format!("it gets to {} after {MAX_INSTANCE_DEPTH} in a row", self.describe_instance(&inst)))
            };
            self.diags.push(diag);
        }
        id
    }

    /// How many parts `ty` has, written out (up to `usize::MAX`): the size
    /// of each type is worked out once, so a type made of shared halves
    /// costs as much as its halves.
    fn ty_size(&mut self, ty: Ty, sizes: &mut HashMap<Ty, usize>) -> usize {
        let ty = self.resolve(ty);
        if let Some(&n) = sizes.get(&ty) {
            return n;
        }
        let parts: Vec<Ty> = match self.tcx.kind(ty) {
            TyKind::Tuple(ts) | TyKind::Adt(_, ts) | TyKind::Union(ts) => ts.clone(),
            TyKind::Optional(t) | TyKind::Range(t) | TyKind::Array(t, _) | TyKind::Gauge(_, t) => vec![*t],
            TyKind::Result(a, b) | TyKind::ArrayOf(a, b) => vec![*a, *b],
            TyKind::Fn(params, ret) => params.iter().chain([ret]).copied().collect(),
            _ => Vec::new(),
        };
        let n = parts.into_iter().fold(1usize, |n, t| n.saturating_add(self.ty_size(t, sizes)));
        sizes.insert(ty, n);
        n
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
        let saved_id = self.cur_body.replace(id);
        let saved_looms = std::mem::take(&mut self.looms);
        let out = f(self);
        self.looms = saved_looms;
        self.cur_body = saved_id;
        self.r.bodies[id.0 as usize] = std::mem::replace(&mut self.b, saved);
        out
    }

    fn check_instance(&mut self, id: BodyId) {
        let p = self.p;
        let inst = self.r.instance(id).clone();
        let d = p.def(inst.def);
        self.current = Some(inst.def);
        match &d.kind {
            DefKind::Fn(_) if inst.forward => {
                self.r.reachable.push(id);
                self.check_forward_instance(id);
            }
            // A generic function is only checked for particular types, and
            // nothing gives a test any.
            DefKind::Fn(f) if inst.args.is_empty() && self.is_generic(inst.def) => {
                if f.qualifier == FnQualifier::Test {
                    self.diags.push(
                        Diagnostic::error(codes::T0119, "a test can't have generic parameters or `given`s")
                            .primary(d.span, "in this test"),
                    );
                }
            }
            DefKind::Fn(f) => {
                self.r.reachable.push(id);
                let mark = self.diags.len();
                self.in_body(id, |this| this.check_fn(id, &inst, f));
                self.note_instance(mark, &inst);
            }
            DefKind::Const(_) => {
                self.r.reachable.push(id);
                self.const_ty(inst.def);
            }
            DefKind::Warp(w) => {
                self.in_body(id, |this| this.check_warp(&inst, w));
                // A generic warp's weaves are checked once, for its template.
                if !self.is_std_module(d.module) && inst.args.is_empty() {
                    for t in &w.wefts {
                        self.weft_arg_problem(t);
                    }
                    self.check_weaves_at(inst.def);
                }
            }
            DefKind::Weave(w) => self.check_weave_item(inst.def, w),
            DefKind::Weft(w) => self.check_weft(inst.def, w),
            DefKind::Type(alias) if alias.gauge.is_some() => self.check_gauge_def(id, inst.def),
            DefKind::Type(_) => self.check_alias(inst.def),
            DefKind::ConstBlock(e) => {
                // In a generic warp: checked for each list of arguments.
                let mark = self.diags.len();
                self.subst = self.instance_subst(&inst);
                let this = self.instance_self(&inst);
                let saved_self = std::mem::replace(&mut self.self_ty, this);
                self.check_const_block(id, e);
                self.self_ty = saved_self;
                self.subst = Vec::new();
                self.note_instance(mark, &inst);
            }
            _ => {}
        }
        self.current = None;
    }

    /// A warp's field types and defaults. The members of a warp in the root
    /// module are all checked, used or not; std's only when they're used.
    ///
    /// A generic warp's defaults are checked for each list of arguments it's
    /// built with, and its members when they're used.
    fn check_warp(&mut self, inst: &Instance, w: &hir::WarpDef) {
        let def = inst.def;
        if !w.generics.is_empty() && inst.args.is_empty() {
            self.warp_info(def);
            return;
        }
        let ty = self.tcx.intern(TyKind::Adt(def, inst.args.clone()));
        let Some(info) = self.adt_info(ty) else { return };
        self.self_ty = Some(ty);
        self.subst = self.instance_subst(inst);
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
        self.subst = Vec::new();
        if Some(self.p.def(def).module) == self.p.root && w.generics.is_empty() {
            for &member in &w.members {
                match &self.p.def(member).kind {
                    DefKind::Fn(_) | DefKind::Const(_) => {
                        self.queue_def(member);
                    }
                    // `type Item = u8` for a weft the header lists.
                    DefKind::Type(_) if self.binds_assoc(w, member) => {}
                    // `type Idx = u8`: checked where it's used.
                    DefKind::ConstBlock(_) => {
                        self.queue_def(member);
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
            // A generic warp's parameters stand for what `self.subst` says.
            DefKind::Warp(w) => {
                let args = w
                    .generics
                    .iter()
                    .map(|g| {
                        self.subst
                            .iter()
                            .find(|(p, _)| p == g)
                            .map(|(_, t)| *t)
                            .unwrap_or_else(|| self.tcx.intern(TyKind::Param(*g)))
                    })
                    .collect();
                Some(self.tcx.intern(TyKind::Adt(parent, args)))
            }
            DefKind::Pending => Some(self.tcx.intern(TyKind::Adt(parent, Vec::new()))),
            // A generic weave's parameters stand for what `self.subst` says.
            DefKind::Weave(w) => {
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
        if let Some(&g) = w.generics.iter().find(|&&g| !self.is_type_param(g)) {
            self.unsupported(self.p.def(g).span, "`const` and region parameters");
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
                DefKind::Type(t) if t.value.is_some() || !t.bounds.is_empty() || !t.generics.is_empty() => {
                    self.unsupported(self.p.def(member).span, "associated types with bounds or defaults");
                }
                DefKind::Const(_) => {
                    self.unsupported(self.p.def(member).span, "constants in wefts");
                }
                _ => {}
            }
        }
    }

    fn check_fn(&mut self, id: BodyId, inst: &Instance, f: &hir::FnDef) {
        self.subst = self.instance_subst(inst);
        self.self_ty = self.instance_self(inst);
        self.self_local = f.params.first().filter(|p| p.is_self).map(|p| p.local);
        self.check_fn_body(id, f);
        self.self_ty = None;
        self.self_local = None;
        self.subst = Vec::new();
    }

    /// What `Self` means in `inst`. Reads `self.subst`, which must be
    /// `inst`'s.
    fn instance_self(&mut self, inst: &Instance) -> Option<Ty> {
        inst.self_ty.or_else(|| self.parent_warp_ty(inst.def))
    }

    fn check_fn_body(&mut self, id: BodyId, f: &hir::FnDef) {
        let Some(sig) = self.fn_sig(id) else { return };
        if f.qualifier == FnQualifier::Test {
            self.check_test_sig(f, &sig);
        }
        let first_var = self.vars.len();
        let const_locals = self.const_locals.len();
        for (param, sig_param) in f.params.iter().zip(&sig.params) {
            if let Some(value) = sig_param.value {
                self.const_locals.push((param.local, value));
            }
            self.b.local_ty.insert(param.local, sig_param.ty);
            if let Some(default) = &param.default {
                self.check_coerce(default, sig_param.ty);
            }
        }
        // A shuttle's body runs when it's driven, and gives back nothing.
        self.shuttle = self.shuttle_ctx(f, &sig);
        let body_ty = if self.shuttle.is_some() { self.tcx.unit } else { sig.ret };
        if let Some(body) = &f.body {
            self.ret = Some(body_ty);
            self.loops.clear();
            self.check_coerce(body, body_ty);
            self.ret = None;
        }
        self.shuttle = None;
        self.finish_body(first_var);
        self.check_moves(f, body_ty == self.tcx.unit);
        self.const_locals.truncate(const_locals);
    }

    /// A test takes nothing, and returns nothing or fails with an error:
    /// `loom test` calls it with no arguments and looks at how it ended.
    fn check_test_sig(&mut self, f: &hir::FnDef, sig: &FnSig) {
        let Some(def) = self.current else { return };
        let span = self.p.def(def).span;
        let returns_error = matches!(self.tcx.kind(sig.ret), TyKind::Result(ok, _) if *ok == self.tcx.unit);
        let problem = if !f.params.is_empty() {
            Some("a test can't have parameters")
        } else if !f.generics.is_empty() || !f.givens.is_empty() {
            Some("a test can't have generic parameters or `given`s")
        } else if sig.ret != self.tcx.unit && !returns_error && sig.ret != self.tcx.error {
            Some("a test returns nothing, or `! E`")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.diags.push(
                Diagnostic::error(codes::T0119, problem)
                    .primary(span, "in this test")
                    .help("write it as `test name() { … }`, or `test name() ! E { … }` to use `?` in it"),
            );
        }
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
        let subst = self.instance_subst(&inst);
        let saved_subst = std::mem::replace(&mut self.subst, subst);
        let implicit = self.implicit_args(&inst);
        let saved_implicit = std::mem::replace(&mut self.implicit, implicit);
        let this = self.instance_self(&inst);
        let saved_self = std::mem::replace(&mut self.self_ty, this);
        let sig = self.lower_sig(f, span);
        self.self_ty = saved_self;
        self.implicit = saved_implicit;
        self.subst = saved_subst;
        self.current = saved;
        let sig = sig?;
        self.r.sigs.insert(id, sig.clone());
        Some(sig)
    }

    /// A function's signature. A weft as a parameter's type is read the way
    /// `self.implicit` says; anywhere else, it's not a type.
    fn lower_sig(&mut self, f: &hir::FnDef, span: Span) -> Option<FnSig> {
        let mode = std::mem::take(&mut self.implicit);
        self.lower_sig_with(f, span, mode)
    }

    fn lower_sig_with(&mut self, f: &hir::FnDef, span: Span, mut mode: generic::Implicit) -> Option<FnSig> {
        if !f.givens.is_empty() {
            self.unsupported(span, "`given` parameters");
            return None;
        }
        let mut params = Vec::with_capacity(f.params.len());
        let const_locals = self.const_locals.len();
        let sig = self.lower_sig_params(f, span, &mut mode, &mut params);
        self.const_locals.truncate(const_locals);
        // Callers read the variables a `Fresh` mode collected.
        self.implicit = mode;
        sig
    }

    /// The parameters and return type of `f`. `mode` says how wefts as
    /// parameter types, and `const` parameters, get their arguments. Each
    /// `const` parameter's value goes into `self.const_locals`, for the
    /// types after it (`fn zeros(const n: usize) -> [f32; n]`).
    fn lower_sig_params(
        &mut self,
        f: &hir::FnDef,
        span: Span,
        mode: &mut generic::Implicit,
        params: &mut Vec<SigParam>,
    ) -> Option<FnSig> {
        for param in &f.params {
            let ty = if param.is_self {
                match self.self_ty {
                    Some(ty) => self.builtin_self(ty),
                    None => {
                        self.unsupported(param.span, "methods outside warps");
                        return None;
                    }
                }
            } else {
                self.implicit = std::mem::take(mode);
                let ty = param.ty.as_ref().map_or(self.tcx.error, |t| self.lower_ty(t));
                *mode = std::mem::take(&mut self.implicit);
                ty
            };
            if self.tcx.is_error(ty) {
                return None;
            }
            let value = (param.mode == ParamMode::Const).then(|| match mode {
                generic::Implicit::Fresh(vars) => {
                    let var = self.new_var(infer::VarKind::Type);
                    vars.push((var, None));
                    var
                }
                generic::Implicit::Given { args, next } => {
                    let value = args.get(*next).copied();
                    *next += 1;
                    value.unwrap_or(self.tcx.error)
                }
                generic::Implicit::Off => self.tcx.error,
            });
            if let Some(value) = value {
                if !self.const_param_ty(ty) {
                    let shown = self.show(ty);
                    self.error(
                        codes::T0101,
                        param.span,
                        format!("a `const` parameter can't be a `{shown}`"),
                        "`const` parameters are integers or `bool`",
                    );
                    return None;
                }
                self.const_locals.push((param.local, value));
            }
            // A `mut` parameter shares the caller's table; a scalar one (a
            // number, a string, an enum) is copied back after the call.
            let name = self.p.local(param.local).name.clone();
            params.push(SigParam { name, mode: param.mode, ty, has_default: param.default.is_some(), value });
        }
        let ret = match &f.ret {
            FnRet::Unit => self.tcx.unit,
            FnRet::Value(t) => self.lower_ty(t),
            FnRet::Shuttle { yields: Some((Some(_), _)), pulls: Some(_) } => {
                self.unsupported(span, "a projection that pulls");
                return None;
            }
            FnRet::Shuttle { yields, pulls } => {
                let t = yields.as_ref().map(|(_, t)| self.lower_ty(t));
                let u = pulls.as_ref().map(|t| self.lower_ty(t));
                if t.is_some_and(|t| self.tcx.is_error(t)) || u.is_some_and(|u| self.tcx.is_error(u)) {
                    return None;
                }
                self.shuttle_ty(t, u)
            }
        };
        if self.tcx.is_error(ret) {
            return None;
        }
        Some(FnSig { params: std::mem::take(params), ret })
    }

    /// A type computed at compile time: `uint(8)` is `u8`, `int(12)` is
    /// `i12`. In a generic warp's template, where `uint(N.log2())` can't
    /// be worked out, it stands for itself: each instance works it out.
    fn const_call_ty(&mut self, def: DefId, args: &[Expr], span: Span) -> Ty {
        let signed = match self.p.def_path(def).as_str() {
            "std.prelude.uint" => false,
            "std.prelude.int" => true,
            _ => return self.unsupported(span, "types computed by `const fn`s"),
        };
        let [bits] = args else {
            return self.error_ty(codes::T0104, span, "`uint` and `int` take the number of bits");
        };
        let template = self
            .subst
            .iter()
            .any(|&(g, t)| self.is_const_param(g) && !matches!(self.tcx.kind(self.resolve(t)), TyKind::Value(_)));
        if template && self.fold(bits, None).is_none() {
            return self.tcx.intern(TyKind::Param(def));
        }
        match self.fold(bits, None) {
            Some(gauge::Val::Int(n)) if (1..=64).contains(&n) => {
                self.tcx.int(crate::ty::IntTy { signed, bits: n as u8, size: false })
            }
            Some(gauge::Val::Int(n)) if n > 64 => self.unsupported(span, "integers wider than 64 bits"),
            Some(gauge::Val::Int(n)) => {
                self.error_ty(codes::T0102, bits.span, &format!("an integer type can't have {n} bits"))
            }
            _ => {
                self.diags.push(
                    Diagnostic::error(codes::K0109, "this `const` argument isn't known at compile time")
                        .primary(bits.span, "the constant folder can't work this out"),
                );
                self.tcx.error
            }
        }
    }

    fn error_ty(&mut self, code: loom_syntax::diag::Code, span: Span, message: &str) -> Ty {
        self.error(code, span, message, "here");
        self.tcx.error
    }

    /// Whether a `const` parameter can have type `ty`: its values must be
    /// ones [`TyKind::Value`] holds.
    fn const_param_ty(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(self.tcx.degauge(ty)), TyKind::Int(_) | TyKind::Bool)
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
                let infer = std::mem::take(&mut self.infer_args);
                match &d.kind {
                    DefKind::Warp(w) => {
                        let mut lowered = if args.len() == w.generics.len() {
                            match self.generic_args(&w.generics, args) {
                                Some(args) => args,
                                None => return self.tcx.error,
                            }
                        } else {
                            // The wrong number, reported below.
                            args.iter().map(|_| self.tcx.error).collect()
                        };
                        if lowered.is_empty() && !w.generics.is_empty() && infer {
                            lowered = w.generics.iter().map(|_| self.new_var(infer::VarKind::Type)).collect();
                        }
                        if lowered.len() != w.generics.len() {
                            let n = w.generics.len();
                            let name = d.name.clone();
                            let params: Vec<&str> = w.generics.iter().map(|&g| self.p.def(g).name.as_str()).collect();
                            let diag = if n == 0 {
                                Diagnostic::error(codes::T0104, format!("`{name}` takes no type arguments"))
                                    .primary(span, "remove the brackets")
                            } else {
                                Diagnostic::error(
                                    codes::T0104,
                                    format!("`{name}` takes {n} type argument{}", if n == 1 { "" } else { "s" }),
                                )
                                .primary(span, format!("write `{name}[{}]`", params.join(", ")))
                            };
                            self.diags.push(diag);
                            return self.tcx.error;
                        }
                        // std's `Range[T]` is the type of `a..b`.
                        if Some(*def) == self.std.range_warp && lowered.len() == 1 {
                            return self.tcx.intern(TyKind::Range(lowered[0]));
                        }
                        self.tcx.intern(TyKind::Adt(*def, lowered))
                    }
                    // A weft as a parameter's type is an implicit generic.
                    DefKind::Weft(_) => {
                        if matches!(self.implicit, generic::Implicit::Fresh(_)) {
                            let var = self.new_var(infer::VarKind::Type);
                            if let generic::Implicit::Fresh(vars) = &mut self.implicit {
                                vars.push((var, Some(*def)));
                            }
                            var
                        } else if let generic::Implicit::Given { args, next } = &mut self.implicit {
                            let ty = args.get(*next).copied();
                            *next += 1;
                            ty.unwrap_or(self.tcx.error)
                        } else {
                            self.unsupported(span, "wefts used as types outside parameters (write `any W`)")
                        }
                    }
                    DefKind::Type(alias) if alias.gauge.is_none() && alias.generics.is_empty() => match &alias.value {
                        Some(value) => self.lower_ty(value),
                        None => self.weft_assoc(*def, span),
                    },
                    DefKind::Type(alias) if alias.gauge.is_some() && alias.generics.is_empty() => {
                        self.gauge_ty(*def, span)
                    }
                    DefKind::Type(_) => self.unsupported(span, "generic type aliases"),
                    DefKind::GenericParam(_) => match self.subst.iter().find(|(g, _)| g == def) {
                        Some(&(_, ty)) => ty,
                        None => self.unsupported(span, "generic parameters here"),
                    },
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
            HirTy::Array { elem, len } => {
                let elem = self.lower_ty(elem);
                // `[T; N]` (or `[T; R * C]`) in a template, or at a call before
                // `N` is known.
                if let Some(n) = self.unknown_const_in(len) {
                    return if self.tcx.is_error(elem) { elem } else { self.tcx.array_of(elem, n) };
                }
                match self.array_len(len) {
                    Some(n) if !self.tcx.is_error(elem) => self.tcx.intern(TyKind::Array(elem, n)),
                    _ => self.tcx.error,
                }
            }
            HirTy::Slice(elem) => {
                let elem = self.lower_ty(elem);
                if self.tcx.is_error(elem) { elem } else { self.slice_of(elem) }
            }
            HirTy::Pointer(_) | HirTy::Region { .. } => self.unsupported(span, "spool pointers"),
            HirTy::RawPointer(_) => self.unsupported(span, "raw pointers"),
            HirTy::Union(items) => {
                let items: Vec<Ty> = items.iter().map(|t| self.lower_ty(t)).collect();
                if items.iter().any(|&t| self.tcx.is_error(t)) {
                    return self.tcx.error;
                }
                self.tcx.union(items)
            }
            HirTy::SelfType => match self.self_ty {
                Some(ty) => self.builtin_self(ty),
                None => self.unsupported(span, "`Self` outside warps"),
            },
            HirTy::Assoc { base, name } => {
                let ty = self.lower_ty(base);
                self.assoc_named(ty, &name.name, span)
            }
            HirTy::ConstCall { def, args } => self.const_call_ty(*def, args, span),
            HirTy::Local(_) | HirTy::TypeType => self.unsupported(span, "types computed at compile time"),
            HirTy::Infer => self.unsupported(span, "inferred error sets (`! _`)"),
        }
    }

    /// What `Self` means in the methods of `Optional[T]`, `Fallible[T, E]` and
    /// `Range[T]`: `?T`, `T ! E` and the range itself.
    pub(super) fn builtin_self(&mut self, ty: Ty) -> Ty {
        match self.tcx.kind(ty).clone() {
            TyKind::Adt(def, args) if Some(def) == self.std.optional && args.len() == 1 => self.tcx.optional(args[0]),
            TyKind::Adt(def, args) if Some(def) == self.std.str_warp && args.is_empty() => self.tcx.str,
            TyKind::Adt(def, args) if Some(def) == self.std.char_warp && args.is_empty() => self.tcx.char,
            TyKind::Adt(def, args) if Some(def) == self.std.result_warp && args.len() == 2 => {
                self.tcx.intern(TyKind::Result(args[0], args[1]))
            }
            TyKind::Adt(def, args) if Some(def) == self.std.range_warp && args.len() == 1 => {
                self.tcx.intern(TyKind::Range(args[0]))
            }
            _ => ty,
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
        let saved_projections = std::mem::take(&mut self.projections);
        let saved_shows = std::mem::take(&mut self.pending_shows);
        let saved_subjects = std::mem::take(&mut self.subjects);
        let saved_self_local = self.self_local.take();
        let saved_subst = std::mem::take(&mut self.subst);
        let saved_pending = std::mem::take(&mut self.pending);
        let saved_mends = std::mem::take(&mut self.mends);
        let parent = self.parent_warp_ty(def);
        let saved_self = std::mem::replace(&mut self.self_ty, parent);
        let first_var = self.vars.len();
        let ty = self.in_body(id, |this| {
            let ty = match &c.ty {
                Some(t) => {
                    let ty = this.lower_ty(t);
                    this.check_coerce(&c.value, ty)
                }
                // An untyped constant of numbers is a number, which each
                // use types the way it would a literal.
                None => match this.untyped_value(&c.value) {
                    Some(gauge::Val::Int(v)) => {
                        this.r.untyped_consts.insert(def, v);
                        let int = |signed, bits| crate::ty::IntTy { signed, bits, size: false };
                        let fits = [int(true, 32), int(true, 64), int(false, 64)].into_iter().find(|i| i.contains(v));
                        this.tcx.intern(TyKind::Int(fits.unwrap_or(int(true, 64))))
                    }
                    Some(gauge::Val::Float(f)) => {
                        this.r.untyped_floats.insert(def, f);
                        this.tcx.f64
                    }
                    _ => this.check_expr(&c.value, None),
                },
            };
            this.finish_body(first_var);
            // An untyped number's value wasn't checked as an expression.
            if !this.r.untyped_consts.contains_key(&def) && !this.r.untyped_floats.contains_key(&def) {
                this.check_value_moves(&c.value);
                this.check_const_holds(ty, p.def(def).span);
            }
            this.deep_resolve(ty)
        });
        self.current = saved_current;
        self.ret = saved_ret;
        self.loops = saved_loops;
        self.pending_lits = saved_lits;
        self.pending_negations = saved_negations;
        self.projections = saved_projections;
        self.pending_shows = saved_shows;
        self.subjects = saved_subjects;
        self.self_local = saved_self_local;
        self.subst = saved_subst;
        self.pending = saved_pending;
        self.mends = saved_mends;
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
        let (actual, target) = (self.deep_resolve(actual), self.deep_resolve(target));
        if actual == target || self.tcx.is_lenient(actual) || self.tcx.has_error(target) {
            return true;
        }
        // A gauge's value is a value of its base: it widens to it (and to
        // whatever that widens to) for free.
        // (Unless the target holds the gauge itself: `?Cents`, `Cents ! E`.)
        if let TyKind::Gauge(_, base) = *self.tcx.kind(actual)
            && !self.wraps(target, actual)
        {
            return self.coerce(e, base, target);
        }
        // A literal whose type is still open takes the target's.
        if (self.has_vars(actual) || self.has_vars(target)) && self.unify(actual, target) {
            return true;
        }
        if let TyKind::Optional(inner) = *self.tcx.kind(target)
            && (self.has_vars(actual) || self.has_vars(inner))
            && self.unify(actual, inner)
        {
            self.b.coerced.insert(e.id, target);
            self.b.coercions.insert(e.id, Coercion::ToOptional);
            return true;
        }
        // A view of a whole array or list.
        if let (Some(Seq::Array(t, _) | Seq::List(t)), Some(Seq::Slice(u))) = (self.seq(actual), self.seq(target))
            && (t == u || self.unify(t, u))
        {
            self.b.coerced.insert(e.id, target);
            self.b.coercions.insert(e.id, Coercion::ToSlice);
            return true;
        }
        // Anything with items, where a stream of them is expected.
        if let Some(want) = self.stream_item(target)
            && self.stream_item(actual).is_none()
            && let Some((source, item)) = self.iter_source(actual)
            && (item == want || self.unify(item, want))
        {
            self.b.coerced.insert(e.id, target);
            self.b.coercions.insert(e.id, Coercion::ToStream(source));
            return true;
        }
        if let TyKind::Result(ok, _) = *self.tcx.kind(target)
            && (ok == actual || (self.has_vars(actual) && self.unify(actual, ok)))
        {
            self.b.coerced.insert(e.id, target);
            self.b.coercions.insert(e.id, Coercion::ToResult);
            return true;
        }
        // A number literal where a union is expected: the union's integer
        // type (or float type), if it has just one.
        if let TyKind::Union(members) = self.tcx.kind(target).clone()
            && matches!(self.tcx.kind(actual), TyKind::IntVar(_) | TyKind::FloatVar(_))
        {
            let int_lit = matches!(self.tcx.kind(actual), TyKind::IntVar(_));
            let ints: Vec<Ty> =
                members.iter().copied().filter(|&m| matches!(self.tcx.kind(m), TyKind::Int(_))).collect();
            let floats: Vec<Ty> =
                members.iter().copied().filter(|&m| matches!(self.tcx.kind(m), TyKind::Float(_))).collect();
            let pick = match (int_lit, ints.as_slice(), floats.as_slice()) {
                (true, [one], _) | (true, [], [one]) | (false, _, [one]) => Some(*one),
                _ => None,
            };
            if let Some(member) = pick
                && self.unify(actual, member)
            {
                self.b.coerced.insert(e.id, target);
                self.b.coercions.insert(e.id, Coercion::ToUnion);
                return true;
            }
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
            // A union of fewer types: the same on Lua, where each is boxed
            // with its own type; on Treadle, its tag is renumbered.
            (TyKind::Union(have), TyKind::Union(want)) if have.iter().all(|t| want.contains(t)) => {
                Some(Coercion::WidenUnion)
            }
            (_, TyKind::Union(members)) if members.contains(&actual) => Some(Coercion::ToUnion),
            // An `any` is already an `any` of fewer wefts: it carries the
            // same vtable.
            (TyKind::Any(have), TyKind::Any(want)) => {
                let closure = self.weft_closure(&have);
                let Some(&missing) = want.iter().find(|w| !closure.contains(w)) else { return true };
                let (from, to, weft) = (self.show(actual), self.show(target), self.p.def(missing).name.clone());
                self.diags.push(
                    Diagnostic::error(codes::T0108, format!("an `{from}` isn't always `{weft}`"))
                        .primary(e.span, format!("can't be made an `{to}`"))
                        .help(format!("ask for it: `any ({} + {weft})`", from.trim_start_matches("any "))),
                );
                return false;
            }
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
                Some(Coercion::ToAny(self.weft_closure(&wefts)))
            }
            _ => None,
        };
        match coercion {
            Some(coercion) => {
                self.b.coerced.insert(e.id, target);
                self.b.coercions.insert(e.id, coercion);
                true
            }
            None => {
                self.mismatch(e.span, target, actual);
                if let TyKind::Gauge(def, _) = *self.tcx.kind(target)
                    && let Some(last) = self.diags.last_mut()
                {
                    let name = self.p.def(def).name.clone();
                    *last = std::mem::replace(last, Diagnostic::error(codes::T0101, "")).help(format!(
                        "getting into a gauge is checked: `x as {name}` (unravels), `x as? {name}` or `x as| {name}`"
                    ));
                }
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

    /// Whether `outer` is `inner`, or an optional, result or union with
    /// `inner` in it.
    fn wraps(&self, outer: Ty, inner: Ty) -> bool {
        outer == inner
            || match self.tcx.kind(outer) {
                TyKind::Optional(t) => self.wraps(*t, inner),
                TyKind::Result(t, _) => self.wraps(*t, inner),
                TyKind::Union(members) => members.contains(&inner),
                _ => false,
            }
    }

    /// Whether `ty` weaves `weft`: built in (the primitive types, `String`,
    /// tuples and optionals of them), or through a weave.
    fn weaves(&self, ty: Ty, weft: DefId) -> bool {
        let ty = self.resolve(ty);
        if self.weaving(weft, ty).is_some() {
            return true;
        }
        // A gauge weaves what its base does.
        if let TyKind::Gauge(_, base) = *self.tcx.kind(ty) {
            return self.weaves(base, weft);
        }
        // Two `any` or union values may hold different types, which have no
        // order between them (they're just unequal).
        if matches!(self.tcx.kind(ty), TyKind::Any(_) | TyKind::Union(_)) && Some(weft) == self.std.ord {
            return false;
        }
        if let TyKind::Any(wefts) = self.tcx.kind(ty) {
            return self.weft_closure(wefts).contains(&weft);
        }
        // A sequence shows, compares and hashes like its elements; an array
        // copies like them, and a slice is a view, always copied.
        if let Some(seq) = self.seq(ty)
            && [self.std.show, self.std.eq, self.std.hash, self.std.copy].contains(&Some(weft))
        {
            return match seq {
                Seq::Slice(_) if Some(weft) == self.std.copy => true,
                Seq::List(_) if Some(weft) == self.std.copy => false,
                _ => self.weaves(seq.elem(), weft),
            };
        }
        // A map or a set shows and compares like its contents; it's moved,
        // never `Copy`.
        if let Some(parts) = self.coll_parts(ty)
            && [self.std.show, self.std.eq, self.std.copy, self.std.hash].contains(&Some(weft))
        {
            return [self.std.show, self.std.eq].contains(&Some(weft)) && parts.iter().all(|&t| self.weaves(t, weft));
        }
        // A union weaves what every one of its types weaves.
        if let TyKind::Union(members) = self.tcx.kind(ty) {
            return members.iter().all(|&m| self.weaves(m, weft));
        }
        // A warp is `Copy` when all its fields are, whether it says so or not.
        if Some(weft) == self.std.copy
            && let TyKind::Adt(def, _) = self.tcx.kind(ty)
            && let Some(info) = self.r.adts.get(&ty).or_else(|| self.r.warps.get(def))
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
            // Tuples compare item by item.
            numeric
                || matches!(kind, TyKind::Char | TyKind::Str)
                || is_string(ty)
                || matches!(kind, TyKind::Tuple(items) if !items.is_empty() && all(self, items))
        } else if w == self.std.hash {
            matches!(kind, TyKind::Int(_) | TyKind::IntVar(_) | TyKind::Bool | TyKind::Char | TyKind::Str)
                || is_string(ty)
                || matches!(kind, TyKind::Tuple(items) if !items.is_empty() && all(self, items))
        } else if w == self.std.copy {
            scalar
                || matches!(kind, TyKind::Str | TyKind::Unit)
                || matches!(kind, TyKind::Tuple(items) if all(self, items))
        } else if w == self.std.integer {
            matches!(kind, TyKind::Int(_) | TyKind::IntVar(_))
        } else if w == self.std.parse || w == self.std.num {
            numeric
        } else if w == self.std.iter {
            matches!(kind, TyKind::Adt(def, _) if Some(*def) == self.std.stream)
        } else {
            false
        }
    }

    /// Whether values of `ty` are Lua tables, which a `mut` parameter can
    /// share with its caller.
    pub(super) fn is_table_backed(&mut self, ty: Ty) -> bool {
        let ty = self.resolve(ty);
        match self.tcx.kind(ty).clone() {
            TyKind::Tuple(items) => !items.is_empty(),
            // `String` is a Lua string, and `?T` (whose methods are
            // `Optional[T]`'s) a value or `nil`.
            TyKind::Adt(def, _) if self.is_builtin(def) => {
                Some(def) != self.std.string && Some(def) != self.std.optional
            }
            TyKind::Array(..) => true,
            TyKind::Adt(..) => self.adt_info(ty).is_some_and(|i| !i.unit_only()),
            // A `mut self` method called through `any` changes the value in
            // it, which is table-backed: only those have `mut` methods yet.
            TyKind::Any(_) => true,
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
