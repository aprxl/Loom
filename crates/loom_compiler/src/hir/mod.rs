//! The HIR: the program after name resolution.
//!
//! Every name is resolved: to a local ([`LocalId`]), a definition
//! ([`DefId`]), a type, or one of a few intrinsics. Compared with the AST:
//!
//! - Modules and items live in flat tables on [`Program`].
//! - `a.b.c` through modules becomes a direct reference; `Type.member`
//!   becomes [`ExprKind::Assoc`] (type checking finds the member, since
//!   weaves can add some).
//! - `Name[...]` on a type or function becomes generic arguments; on a value
//!   it stays an index.
//! - Hole lambdas (`_ * 2`) are ordinary lambdas.
//! - `break` and `continue` point at their loop.
//! - Leading dots, method calls and field access are left to type checking,
//!   which needs types to decide them. So are chained comparisons, whose
//!   middle operands are typed in context.
//!
//! Resolution errors inside a `const if` are kept on the branch instead of
//! being reported: only the branch that's taken gets type-checked, and the
//! other one may name APIs this target doesn't have.

mod lower;
pub mod pretty;
#[cfg(test)]
mod tests;

use indexmap::IndexMap;
use loom_syntax::Diagnostic;
use loom_syntax::ast::{
    AssignOp, BinaryOp, CastKind, FieldName, FnQualifier, Ident, Lit, ParamMode, PlaceMode, UnaryOp, Vis, WarpModifier,
};
use loom_syntax::source::{FileId, Span};
use loom_syntax::token::{Keyword, PrimType};
use loom_syntax::twill::TwillPattern;

pub use lower::{Resolved, resolve};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);

        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id!(
    /// A module: a file, or a directory of modules.
    ModuleId
);
id!(
    /// A definition: an item, a member of one, or a generic parameter.
    DefId
);
id!(
    /// A local variable, parameter or pattern binding. Unique program-wide.
    LocalId
);
id!(
    /// An expression or pattern node. Unique program-wide, so later passes can
    /// key side tables by it. A loop's id is also its `break` target.
    HirId
);

/// A whole program: the root file, the modules it uses, and `std`.
#[derive(Debug, Default)]
pub struct Program {
    pub modules: Vec<Module>,
    pub defs: Vec<Def>,
    pub locals: Vec<Local>,
    /// The number of [`HirId`]s handed out.
    pub hir_ids: u32,
    pub root: Option<ModuleId>,
    /// Names every module sees without a `use` (`std/prelude.loom`).
    pub prelude: IndexMap<String, Binding>,
}

impl Program {
    pub fn module(&self, id: ModuleId) -> &Module {
        &self.modules[id.index()]
    }

    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.index()]
    }

    pub fn local(&self, id: LocalId) -> &Local {
        &self.locals[id.index()]
    }

    /// What `name` means at the top level of `module`: its items, then its
    /// imports, then the prelude. (Locals and generic parameters aren't
    /// visible from here.)
    pub fn module_name(&self, module: ModuleId, name: &str) -> Option<Binding> {
        let m = self.module(module);
        if let Some(&def) = m.items.get(name) {
            return Some(Binding::Def(def));
        }
        if let Some(import) = m.imports.get(name) {
            return Some(import.target);
        }
        self.prelude.get(name).copied()
    }

    /// The dotted path of a definition, for messages and dumps: `std.io.print`,
    /// `Shape.Circle`.
    pub fn def_path(&self, id: DefId) -> String {
        let def = self.def(id);
        let prefix = match def.parent {
            Some(parent) => self.def_path(parent),
            None => self.module(def.module).path.clone(),
        };
        if def.name.is_empty() { prefix } else { format!("{prefix}.{}", def.name) }
    }
}

#[derive(Debug)]
pub struct Module {
    /// `std.io`, or the file stem for the root.
    pub path: String,
    /// `None` for a directory module.
    pub file: Option<FileId>,
    pub parent: Option<ModuleId>,
    pub children: IndexMap<String, ModuleId>,
    /// The names this module declares, in source order.
    pub items: IndexMap<String, DefId>,
    /// Names brought in with `use`.
    pub imports: IndexMap<String, Import>,
    /// Every top-level definition, named or not (weaves, `const` blocks), in source order.
    pub decls: Vec<DefId>,
    /// Its `test`s, in source order. Their names aren't in `items`: nothing calls a test.
    pub tests: Vec<DefId>,
}

#[derive(Clone, Copy, Debug)]
pub struct Import {
    pub target: Binding,
    pub span: Span,
    /// `pub use` re-exports the name.
    pub public: bool,
}

/// What a module-level name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    Def(DefId),
    Module(ModuleId),
}

#[derive(Debug)]
pub struct Def {
    /// Empty for weaves and `const` blocks.
    pub name: String,
    pub span: Span,
    pub vis: Vis,
    pub module: ModuleId,
    /// The warp, weft or weave a member belongs to, or the item a generic
    /// parameter belongs to.
    pub parent: Option<DefId>,
    pub docs: Vec<String>,
    pub attrs: Vec<Attr>,
    /// Whether this names a type (a warp, weft, alias, associated type or type
    /// parameter). Known from the moment it's collected, before `kind` is.
    pub is_type: bool,
    pub kind: DefKind,
}

/// An attribute. Its arguments are attribute-specific words (`#layout(C)`,
/// `#layout(bits, big)`), so they're kept as written rather than resolved.
#[derive(Debug)]
pub struct Attr {
    pub name: String,
    pub args: Vec<loom_syntax::ast::AttrArg>,
    pub span: Span,
}

#[derive(Debug)]
pub enum DefKind {
    Fn(FnDef),
    Warp(WarpDef),
    Case(CaseDef),
    /// A field, or a `through` field.
    Field(FieldDef),
    Weft(WeftDef),
    Weave(WeaveDef),
    Const(ConstDef),
    /// `type Name = T`, a gauge, or an associated type (in a weft, with no value).
    Type(TypeDef),
    GenericParam(GenericParamDef),
    /// Item-level compile-time code, like `const assert(...)`.
    ConstBlock(Expr),
    /// Filled in once the item has been lowered.
    Pending,
}

impl DefKind {
    pub fn describe(&self) -> &'static str {
        match self {
            DefKind::Fn(_) => "function",
            DefKind::Warp(_) => "warp",
            DefKind::Case(_) => "case",
            DefKind::Field(_) => "field",
            DefKind::Weft(_) => "weft",
            DefKind::Weave(_) => "weave",
            DefKind::Const(_) => "constant",
            DefKind::Type(_) => "type",
            DefKind::GenericParam(_) => "generic parameter",
            DefKind::ConstBlock(_) => "const block",
            DefKind::Pending => "item",
        }
    }
}

#[derive(Debug)]
pub struct FnDef {
    pub qualifier: FnQualifier,
    pub generics: Vec<DefId>,
    pub params: Vec<Param>,
    pub ret: FnRet,
    pub givens: Vec<GivenParam>,
    pub where_clause: Vec<WherePred>,
    /// `None` for signatures (wefts, extern blocks, builtins).
    pub body: Option<Expr>,
    /// `Some("lua")` inside `extern "lua" { … }`.
    pub abi: Option<String>,
}

impl FnDef {
    pub fn has_self(&self) -> bool {
        self.params.first().is_some_and(|p| p.is_self)
    }
}

#[derive(Debug)]
pub struct Param {
    pub mode: ParamMode,
    pub local: LocalId,
    pub is_self: bool,
    /// `None` only for `self`.
    pub ty: Option<Ty>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug)]
pub enum FnRet {
    Unit,
    Value(Ty),
    Shuttle { yields: Option<(Option<PlaceMode>, Ty)>, pulls: Option<Ty> },
}

#[derive(Debug)]
pub struct GivenParam {
    pub local: LocalId,
    pub ty: Ty,
}

#[derive(Debug)]
pub struct WherePred {
    pub ty: Ty,
    pub bounds: Vec<Ty>,
    pub span: Span,
}

#[derive(Debug)]
pub struct WarpDef {
    pub modifier: WarpModifier,
    pub generics: Vec<DefId>,
    pub wefts: Vec<Ty>,
    pub where_clause: Vec<WherePred>,
    /// `warp Meters(f64)`: one unnamed field per entry of `fields`.
    pub tuple: bool,
    /// Fields and `through` fields, in order.
    pub fields: Vec<DefId>,
    pub cases: Vec<DefId>,
    /// Functions, constants, types and `const` blocks.
    pub members: Vec<DefId>,
}

#[derive(Debug)]
pub struct CaseDef {
    /// `None` for a unit case.
    pub fields: Option<Vec<CaseField>>,
}

#[derive(Debug)]
pub struct CaseField {
    pub name: Option<Ident>,
    /// `from T`: absorbs `T` errors on `?`.
    pub absorbs: bool,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Debug)]
pub struct FieldDef {
    pub ty: Ty,
    pub default: Option<Expr>,
    pub through: bool,
}

#[derive(Debug)]
pub struct WeftDef {
    pub generics: Vec<DefId>,
    pub supers: Vec<Ty>,
    pub members: Vec<DefId>,
}

#[derive(Debug)]
pub struct WeaveDef {
    pub generics: Vec<DefId>,
    pub weft: Ty,
    pub target: Ty,
    pub where_clause: Vec<WherePred>,
    pub members: Vec<DefId>,
}

#[derive(Debug)]
pub struct ConstDef {
    pub ty: Option<Ty>,
    pub value: Expr,
}

#[derive(Debug)]
pub struct TypeDef {
    pub generics: Vec<DefId>,
    /// Bounds of an associated type: `type Item: Show`.
    pub bounds: Vec<Ty>,
    pub value: Option<Ty>,
    /// A gauge's range or predicate.
    pub gauge: Option<Expr>,
}

#[derive(Debug)]
pub struct GenericParamDef {
    pub kind: GenericParamKind,
}

#[derive(Debug)]
pub enum GenericParamKind {
    Type { bounds: Vec<Ty> },
    Const { ty: Ty },
    Region,
}

#[derive(Debug)]
pub struct Local {
    pub name: String,
    pub span: Span,
    pub mutable: bool,
    pub kind: LocalKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    /// `let` or `var`, or a binding inside their pattern.
    Let,
    Param,
    SelfParam,
    /// `given name: T` in a signature, or a `given name = …` statement.
    Given,
    /// `spool name = …`: also usable as a region, `@name`.
    Spool,
    /// A binding in a `match` arm, `is`, `for` or `mend e =>`.
    Pattern,
    LambdaParam,
    /// A `loom` block's group of threads.
    Group,
    /// A parameter made for a hole lambda: `_` in `xs.map(_ * 2)`.
    Hole,
}

// ================================================================ types

#[derive(Debug)]
pub struct Ty {
    pub kind: TyKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum TyKind {
    Prim(PrimType),
    /// A warp, weft, alias, or generic parameter, with arguments.
    Def {
        def: DefId,
        args: Vec<GenericArg>,
    },
    /// A `const T: type` value parameter used as a type.
    Local(LocalId),
    SelfType,
    /// `Self.Item`, `T.Item`: an associated type, found by type checking.
    Assoc {
        base: Box<Ty>,
        name: Ident,
    },
    /// `type`, the type of types.
    TypeType,
    /// `uint(12)`: a `const fn` that returns a type.
    ConstCall {
        def: DefId,
        args: Vec<Expr>,
    },
    Unit,
    Tuple(Vec<Ty>),
    Array {
        elem: Box<Ty>,
        len: Box<Expr>,
    },
    Slice(Box<Ty>),
    Optional(Box<Ty>),
    Pointer(Box<Ty>),
    RawPointer(Box<Ty>),
    Region {
        ty: Box<Ty>,
        region: Region,
    },
    Result {
        ok: Box<Ty>,
        err: Box<Ty>,
    },
    Union(Vec<Ty>),
    Fn {
        params: Vec<Ty>,
        ret: Box<Ty>,
    },
    Any(Vec<Ty>),
    Infer,
    Error,
}

/// What `@name` refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// A `spool` local or a `given` spool.
    Local(LocalId),
    /// A region parameter, `warp Link[@a]`.
    Param(DefId),
    Heap,
    Static,
    Error,
}

#[derive(Debug)]
pub enum GenericArg {
    Type(Ty),
    Expr(Expr),
}

// ================================================================ statements

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    Let {
        mutable: bool,
        pat: Box<Pat>,
        ty: Option<Ty>,
        init: Expr,
    },
    /// `given name = value` (the name is optional).
    Given {
        local: Option<LocalId>,
        value: Expr,
    },
    Spool {
        local: LocalId,
        init: Expr,
    },
    /// `defer body`, or `defer x => body`, which names the local `x`.
    Defer {
        binding: Option<LocalId>,
        body: Expr,
    },
    /// A nested item; its definition is in the program's tables.
    Item(DefId),
    Expr(Expr),
}

// ================================================================ expressions

#[derive(Debug)]
pub struct Expr {
    pub id: HirId,
    pub kind: ExprKind,
    pub span: Span,
}

/// Names that aren't definitions anywhere: they're built into the compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intrinsic {
    /// `target.os`, `target.pointer_bits`: facts about the build target.
    Target,
    /// The global spool.
    Heap,
}

#[derive(Debug)]
pub enum ExprKind {
    Lit(Lit),
    Template(Vec<TemplatePart>),
    Local(LocalId),
    /// A function, constant, case or other value definition, with explicit
    /// generic arguments if any.
    Def {
        def: DefId,
        args: Vec<GenericArg>,
    },
    /// A type used as a value: the callee of a constructor (`Point(1, 2)`,
    /// `List[i32]()`), or a type passed to a `const T: type` parameter.
    Type(Ty),
    /// `Type.name`: a case, associated function, constant or reflection
    /// query, found by type checking.
    Assoc {
        base: Box<Ty>,
        name: Ident,
    },
    /// `some`, `ok` or `err` used as a constructor.
    BuiltinCase(Keyword),
    Intrinsic(Intrinsic),
    /// `.name` or `.0`.
    LeadingDot(FieldName),
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    ArrayRepeat {
        value: Box<Expr>,
        count: Box<Expr>,
    },
    Map(Vec<(Expr, Expr)>),
    Block(Block),
    If {
        cond: Box<Expr>,
        then: Block,
        else_: Option<Box<Expr>>,
        /// For `const if`: the resolution errors of each branch, reported
        /// only if that branch is taken.
        deferred: Option<Box<ConstIfDeferred>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    For {
        pat: Box<Pat>,
        iter: Box<Expr>,
        body: Block,
        is_const: bool,
        is_loom: bool,
    },
    While {
        cond: Box<Expr>,
        body: Block,
    },
    Loop {
        body: Block,
    },
    Loom {
        args: Vec<Arg>,
        body: Block,
        /// The block's group of threads: a local of its own, so lambdas and
        /// threads inside carry it like any captured variable.
        group: LocalId,
    },
    Thread {
        /// The innermost enclosing loom's group, if there is one.
        group: Option<LocalId>,
        body: Box<Expr>,
    },
    /// `target` is the [`HirId`] of the loop.
    Break {
        target: HirId,
        value: Option<Box<Expr>>,
    },
    Continue {
        target: HirId,
    },
    Return(Option<Box<Expr>>),
    Fail(Box<Expr>),
    Yield {
        place: Option<PlaceMode>,
        value: Box<Expr>,
    },
    Pull,
    Unsafe {
        reason: String,
        body: Block,
    },
    Const(Box<Expr>),
    Lambda {
        params: Vec<LambdaParam>,
        body: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    /// `a[i]`, or generic arguments on a method: `s.parse[u16]`.
    Index {
        base: Box<Expr>,
        args: Vec<GenericArg>,
    },
    Field {
        base: Box<Expr>,
        field: FieldName,
    },
    ConstField {
        base: Box<Expr>,
        field: Box<Expr>,
    },
    Try(Box<Expr>),
    Deref(Box<Expr>),
    DotBlock {
        subject: Box<Expr>,
        body: Block,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Chain {
        first: Box<Expr>,
        rest: Vec<(BinaryOp, Expr)>,
    },
    Is {
        expr: Box<Expr>,
        pat: Box<Pat>,
    },
    Cast {
        expr: Box<Expr>,
        kind: CastKind,
        ty: Box<Ty>,
    },
    Range {
        lo: Option<Box<Expr>>,
        hi: Option<Box<Expr>>,
        inclusive: bool,
    },
    Mend {
        expr: Box<Expr>,
        binding: Option<LocalId>,
        handler: Box<Expr>,
    },
    Assign {
        target: Box<Expr>,
        op: AssignOp,
        value: Box<Expr>,
    },
    Error,
}

#[derive(Debug, Default)]
pub struct ConstIfDeferred {
    pub then: Vec<Diagnostic>,
    pub else_: Vec<Diagnostic>,
}

#[derive(Debug)]
pub enum TemplatePart {
    Text(String),
    Interp { expr: Expr, spec: Option<String> },
}

#[derive(Debug)]
pub struct Arm {
    pub pat: Pat,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct Arg {
    pub label: Option<Ident>,
    pub is_given: bool,
    pub mutable: bool,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct LambdaParam {
    pub take: bool,
    pub local: LocalId,
    pub ty: Option<Ty>,
}

// ================================================================ patterns

#[derive(Debug)]
pub struct Pat {
    pub id: HirId,
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatKind {
    Wild,
    Bind(LocalId),
    Lit {
        lit: Lit,
        negative: bool,
    },
    Range {
        lo: Option<Box<Pat>>,
        hi: Option<Box<Pat>>,
        inclusive: bool,
    },
    Tuple(Vec<Pat>),
    Slice(Vec<Pat>),
    Rest(Option<LocalId>),
    Case {
        path: CasePath,
        fields: Option<Vec<FieldPat>>,
        rest: bool,
    },
    ConstCase(Box<Expr>),
    Some(Box<Pat>),
    None,
    Ok(Box<Pat>),
    Err(Box<Pat>),
    TypeTest {
        local: LocalId,
        ty: Ty,
    },
    Or(Vec<Pat>),
    /// The captures bind locals, in the order the pattern lists them.
    Twill {
        pattern: Box<TwillPattern>,
        captures: Vec<LocalId>,
    },
    Error,
}

#[derive(Debug)]
pub enum CasePath {
    /// `.Name`: a case of the expected type.
    Dot(Ident),
    /// `Point(x, y)`: a record destructured by its type.
    Type(Ty),
    /// `MAX`: a constant.
    Const(DefId),
    /// `Color.Red`, `ast.Expr.Lit`: a member of a type.
    Assoc { base: Box<Ty>, name: Ident },
}

#[derive(Debug)]
pub struct FieldPat {
    pub label: Option<Ident>,
    pub pat: Pat,
    pub span: Span,
}
