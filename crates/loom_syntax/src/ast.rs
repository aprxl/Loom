//! The abstract syntax tree produced by the parser.
//!
//! The AST mirrors the surface syntax closely: nothing is resolved or
//! desugared yet. Names are plain [`Ident`]s. Leading dots, method calls,
//! hole lambdas and `x[...]` (index vs. generic instantiation) are all left
//! for name resolution and type checking to interpret.

use crate::source::Span;
use crate::token::{FloatLit, IntLit, Keyword, PrimType};
use crate::twill::TwillPattern;

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// One source file.
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub items: Vec<Item>,
    pub span: Span,
}

// ================================================================ items

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// `///` doc comments, one entry per line.
    pub docs: Vec<String>,
    pub attrs: Vec<Attr>,
    pub vis: Vis,
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vis {
    Private,
    Pub,
    /// `pub(get)`: readable everywhere, writable only in the module (fields only).
    PubGet,
}

/// `#name` or `#name(args)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub name: Ident,
    pub args: Vec<AttrArg>,
    pub span: Span,
}

/// An attribute argument: `C`, `"use foo"`, or `key = value`.
#[derive(Clone, Debug, PartialEq)]
pub struct AttrArg {
    pub key: Option<Ident>,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    Use(UseTree),
    Const(ConstDecl),
    Type(TypeDecl),
    Warp(WarpDecl),
    Weft(WeftDecl),
    Weave(WeaveDecl),
    Fn(FnDecl),
    Extern(ExternBlock),
    /// Compile-time code at item level, e.g. `const assert(...)`.
    ConstExpr(Expr),
}

/// `use std.fs.{File, read as read_file}`.
#[derive(Clone, Debug, PartialEq)]
pub struct UseTree {
    pub path: Vec<Ident>,
    pub kind: UseKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UseKind {
    Single { alias: Option<Ident> },
    Group(Vec<UseTree>),
}

/// `const NAME: Ty = value`.
#[derive(Clone, Debug, PartialEq)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Option<Ty>,
    pub value: Expr,
}

/// `type Name[G] = Ty where gauge`, or an associated type `type Item: Bounds`.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    /// Bounds of an associated type (`type Item: Show`).
    pub bounds: Vec<Ty>,
    pub value: Option<Ty>,
    /// The range or predicate of a gauge: `where 1..` or `where _ % 2 == 0`.
    pub gauge: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarpModifier {
    None,
    /// `once warp`: values must be used up exactly once.
    Once,
    /// `ref warp`: may hold borrows, and is second-class itself.
    Ref,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WarpDecl {
    pub modifier: WarpModifier,
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    /// Wefts listed in the header: `warp Circle: Shape, Show`.
    pub wefts: Vec<Ty>,
    pub where_clause: Vec<WherePred>,
    pub body: WarpBody,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WarpBody {
    Members(Vec<Member>),
    /// `warp Meters(f64)`.
    Tuple(Vec<CaseField>),
}

/// Something declared inside a warp, weft, weave or extern block.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub docs: Vec<String>,
    pub attrs: Vec<Attr>,
    pub vis: Vis,
    pub kind: MemberKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MemberKind {
    Field(FieldDecl),
    /// `through frame: Frame`.
    Through(FieldDecl),
    /// `case A, B(x: T)`: one or more cases on one line.
    Cases(Vec<CaseDecl>),
    Const(ConstDecl),
    Type(TypeDecl),
    Fn(FnDecl),
    ConstExpr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDecl {
    pub name: Ident,
    pub ty: Ty,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseDecl {
    pub name: Ident,
    /// `None` for a unit case, `Some` (possibly empty) for `Name(...)`.
    pub fields: Option<Vec<CaseField>>,
    pub span: Span,
}

/// A payload field of a case (or of a tuple warp): `name: Ty`, `Ty`, or `from Ty`.
#[derive(Clone, Debug, PartialEq)]
pub struct CaseField {
    pub name: Option<Ident>,
    /// `from Ty`: this case absorbs `Ty` errors on `?`.
    pub absorbs: bool,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeftDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub supers: Vec<Ty>,
    /// Associated types and function signatures (with optional default bodies).
    pub members: Vec<Member>,
}

/// `weave[G] Weft into Target where … { fns }`.
#[derive(Clone, Debug, PartialEq)]
pub struct WeaveDecl {
    pub generics: Vec<GenericParam>,
    pub weft: Ty,
    pub target: Ty,
    pub where_clause: Vec<WherePred>,
    pub members: Vec<Member>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnQualifier {
    None,
    /// `const fun`: compile-time only.
    Const,
    /// `unsafe fun`: callers must be inside `unsafe`.
    Unsafe,
    /// `test name() { … }`: run by `loom test`, left out of every other build.
    Test,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnDecl {
    pub qualifier: FnQualifier,
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub ret: FnRet,
    pub givens: Vec<GivenParam>,
    pub where_clause: Vec<WherePred>,
    /// `None` for signatures in wefts and extern blocks.
    pub body: Option<FnBody>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamMode {
    /// No keyword: a read-only borrow.
    Read,
    Mut,
    Take,
    /// `const n: usize`: known at compile time.
    Const,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamName {
    SelfValue,
    Named(Ident),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub mode: ParamMode,
    pub name: ParamName,
    /// `None` only for `self`.
    pub ty: Option<Ty>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FnRet {
    /// Nothing written: returns `()`.
    Unit,
    /// `-> T`, `-> T ! E`, or `! E` (stored as a result type with a `()` success type).
    Value(Ty),
    /// A shuttle: `~> T`, `~> T <~ U`, `<~ U`, `~> mut T`.
    Shuttle { yields: Option<YieldSpec>, pulls: Option<Ty> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct YieldSpec {
    /// `~> mut T` / `~> ref T` make the shuttle a projection that lends places.
    pub place: Option<PlaceMode>,
    pub ty: Ty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceMode {
    Ref,
    Mut,
}

/// `given name: Ty` in a signature.
#[derive(Clone, Debug, PartialEq)]
pub struct GivenParam {
    pub name: Ident,
    pub ty: Ty,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FnBody {
    Block(Block),
    /// `= expr`.
    Expr(Expr),
}

/// `extern "C" { fun … }`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExternBlock {
    pub abi: String,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenericParam {
    pub kind: GenericParamKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GenericParamKind {
    /// `T` or `T: Show + Eq`.
    Type { name: Ident, bounds: Vec<Ty> },
    /// `const N: usize`.
    Const { name: Ident, ty: Ty },
    /// `@r`.
    Region { name: Ident },
}

/// `T: Show + Hash` in a `where` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct WherePred {
    pub ty: Ty,
    pub bounds: Vec<Ty>,
    pub span: Span,
}

// ================================================================ types

#[derive(Clone, Debug, PartialEq)]
pub struct Ty {
    pub kind: TyKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TyKind {
    Prim(PrimType),
    /// `Point`, `List[T]`, `fs.IoError`, `Self.Item`.
    Path {
        segments: Vec<Ident>,
        args: Vec<GenericArg>,
    },
    SelfType,
    /// `type`, the type of types (for `const T: type`).
    TypeType,
    /// A compile-time function producing a type: `uint(12)`, `Fixed(16, 16)`.
    ConstCall {
        segments: Vec<Ident>,
        args: Vec<Expr>,
    },
    /// `()`.
    Unit,
    Tuple(Vec<Ty>),
    /// `[T; N]`.
    Array {
        elem: Box<Ty>,
        len: Box<Expr>,
    },
    /// `[T]`.
    Slice(Box<Ty>),
    /// `?T`.
    Optional(Box<Ty>),
    /// `^T`.
    Pointer(Box<Ty>),
    /// `*T`.
    RawPointer(Box<Ty>),
    /// `T@region`.
    Region {
        ty: Box<Ty>,
        region: Ident,
    },
    /// `T ! E`.
    Result {
        ok: Box<Ty>,
        err: Box<Ty>,
    },
    /// `A | B`.
    Union(Vec<Ty>),
    /// `A => B`, `(A, B) => C`.
    Fn {
        params: Vec<Ty>,
        ret: Box<Ty>,
    },
    /// `any W`, `any (A + B)`.
    Any(Vec<Ty>),
    /// `_`: inferred (e.g. `! _`).
    Infer,
    Error,
}

/// An entry in `[...]` after a name: a type, or a compile-time value.
#[derive(Clone, Debug, PartialEq)]
pub enum GenericArg {
    Type(Ty),
    Expr(Expr),
}

// ================================================================ statements

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// A block evaluates to its last statement when that is an expression.
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// `let pat: Ty = init` / `var pat: Ty = init`.
    Let {
        mutable: bool,
        pat: Pat,
        ty: Option<Ty>,
        init: Expr,
    },
    /// `given name = value` or `given value`.
    Given {
        name: Option<Ident>,
        value: Expr,
    },
    /// `spool name = init`.
    Spool {
        name: Ident,
        init: Expr,
    },
    /// `defer expr` or the fallback form `defer name => expr`.
    Defer {
        binding: Option<Ident>,
        body: Expr,
    },
    Item(Box<Item>),
    Expr(Expr),
}

// ================================================================ expressions

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Lit(Lit),
    Template(Vec<TemplatePart>),
    /// Any identifier: `x`, `Point`, `MAX`.
    Name(Ident),
    /// A primitive type used as a value, e.g. `u32.MAX`.
    Prim(PrimType),
    /// `some`, `ok` or `err` used as a constructor.
    BuiltinCase(Keyword),
    SelfValue,
    SelfType,
    /// `.name` or `.0`: resolved against the subject, `self`, or the expected type.
    LeadingDot(FieldName),
    /// `_` in an argument: part of a hole lambda.
    Hole,
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
        is_const: bool,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    For {
        label: Option<Ident>,
        pat: Box<Pat>,
        iter: Box<Expr>,
        body: Block,
        is_const: bool,
        is_loom: bool,
    },
    While {
        label: Option<Ident>,
        cond: Box<Expr>,
        body: Block,
    },
    Loop {
        label: Option<Ident>,
        body: Block,
    },
    /// `loom { }` or `loom(given: pool) { }`.
    Loom {
        args: Vec<Arg>,
        body: Block,
    },
    Thread(Box<Expr>),
    Break {
        label: Option<Ident>,
        value: Option<Box<Expr>>,
    },
    Continue {
        label: Option<Ident>,
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
    /// `const expr` or `const { … }`: evaluated at compile time.
    Const(Box<Expr>),
    Lambda {
        params: Vec<LambdaParam>,
        body: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    /// `a[...]`: an index or explicit generic arguments, decided by resolution.
    Index {
        base: Box<Expr>,
        args: Vec<GenericArg>,
    },
    Field {
        base: Box<Expr>,
        field: FieldName,
    },
    /// `v.(f)`: field access through a compile-time field value.
    ConstField {
        base: Box<Expr>,
        field: Box<Expr>,
    },
    /// Postfix `?`.
    Try(Box<Expr>),
    /// Postfix `^`.
    Deref(Box<Expr>),
    /// `subject.{ … }`.
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
    /// A chained comparison, `a < b <= c` (two or more operators).
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
    /// `expr mend handler` or `expr mend e => handler`.
    Mend {
        expr: Box<Expr>,
        binding: Option<Ident>,
        handler: Box<Expr>,
    },
    /// Only in statement position, match-arm bodies and lambda bodies.
    Assign {
        target: Box<Expr>,
        op: AssignOp,
        value: Box<Expr>,
    },
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Int(IntLit),
    Float(FloatLit),
    Char(char),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TemplatePart {
    Text(String, Span),
    Interp { expr: Expr, spec: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pat: Pat,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

/// A call argument: `x`, `name: x`, `mut x`, or `given: x`.
#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub label: Option<Ident>,
    pub is_given: bool,
    pub mutable: bool,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LambdaParam {
    pub take: bool,
    pub name: Ident,
    pub ty: Option<Ty>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldName {
    Named(Ident),
    /// A tuple index: `t.0`.
    Index(u32, Span),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    BitNot,
    Not,
    /// Prefix `^`: allocate in the ambient spool.
    Alloc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    AddWrap,
    SubWrap,
    MulWrap,
    AddSat,
    SubSat,
    MulSat,
    AddChecked,
    SubChecked,
    MulChecked,
    Shl,
    Shr,
    BitAnd,
    BitXor,
    BitOr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinaryOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::AddWrap => "+%",
            BinaryOp::SubWrap => "-%",
            BinaryOp::MulWrap => "*%",
            BinaryOp::AddSat => "+|",
            BinaryOp::SubSat => "-|",
            BinaryOp::MulSat => "*|",
            BinaryOp::AddChecked => "+?",
            BinaryOp::SubChecked => "-?",
            BinaryOp::MulChecked => "*?",
            BinaryOp::Shl => "<<",
            BinaryOp::Shr => ">>",
            BinaryOp::BitAnd => "&",
            BinaryOp::BitXor => "~",
            BinaryOp::BitOr => "|",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "and",
            BinaryOp::Or => "or",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(self, BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastKind {
    /// `as`: checked, unravels on loss.
    Checked,
    /// `as%`: wrapping / truncating.
    Wrap,
    /// `as|`: saturating / clamping.
    Sat,
    /// `as?`: gives an optional.
    Opt,
}

impl CastKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CastKind::Checked => "as",
            CastKind::Wrap => "as%",
            CastKind::Sat => "as|",
            CastKind::Opt => "as?",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    /// `+=`, `+%=`, `-|=`, `<<=`, … : the compound operator.
    Compound(BinaryOp),
}

// ================================================================ patterns

#[derive(Clone, Debug, PartialEq)]
pub struct Pat {
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatKind {
    /// `_`.
    Wild,
    /// `x` or `mut x`.
    Bind {
        name: Ident,
        mutable: bool,
    },
    /// `42`, `-1`, `'a'`, `"GET"`, `true`.
    Lit {
        lit: Lit,
        negative: bool,
    },
    /// `0..10`, `'a'..='z'`, `..=9` (endpoints are literal patterns).
    Range {
        lo: Option<Box<Pat>>,
        hi: Option<Box<Pat>>,
        inclusive: bool,
    },
    Tuple(Vec<Pat>),
    /// `[first, ..rest]`.
    Slice(Vec<Pat>),
    /// `..` or `..rest` inside a slice pattern.
    Rest(Option<Ident>),
    /// `.Circle(r)`, `Point(x, y)`, `Color.Red`, `MAX`, `.Rect(w: width, ..)`.
    Case {
        path: CasePath,
        fields: Option<Vec<FieldPat>>,
        rest: bool,
    },
    /// `.(c)`: a case chosen by a compile-time value.
    ConstCase(Box<Expr>),
    Some(Box<Pat>),
    None,
    Ok(Box<Pat>),
    Err(Box<Pat>),
    /// `e: IoError`.
    TypeTest {
        name: Ident,
        ty: Ty,
    },
    /// `a | b`.
    Or(Vec<Pat>),
    Twill(Box<TwillPattern>),
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CasePath {
    /// `.Name`, resolved against the expected type.
    Dot(Ident),
    /// `Name` or `Type.Name`.
    Path(Vec<Ident>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldPat {
    pub label: Option<Ident>,
    pub pat: Pat,
    pub span: Span,
}
