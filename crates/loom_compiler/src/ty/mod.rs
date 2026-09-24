//! Types, interned. A [`Ty`] is a small id into a [`TyCtx`], so types
//! compare and hash cheaply.

pub mod int;

use std::collections::HashMap;

use loom_syntax::token::PrimType;

use crate::hir::{DefId, Program};

/// An integer type. `usize`/`isize` are `size` types with 63 bits: that's
/// `target.pointer_bits` on the Lua target (see design/lua-backend.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IntTy {
    pub signed: bool,
    pub bits: u8,
    pub size: bool,
}

impl IntTy {
    pub const I32: IntTy = IntTy { signed: true, bits: 32, size: false };
    pub const USIZE: IntTy = IntTy { signed: false, bits: 63, size: true };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloatTy {
    F32,
    F64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TyKind {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    Char,
    Str,
    Unit,
    Never,
    Tuple(Vec<Ty>),
    Optional(Ty),
    Result(Ty, Ty),
    /// `a..b` and friends; the parameter is the element type.
    Range(Ty),
    Fn(Vec<Ty>, Ty),
    /// A warp, with its generic arguments.
    Adt(DefId, Vec<Ty>),
    /// `any W`: some type that weaves every weft listed.
    Any(Vec<DefId>),
    /// An integer literal's type while it's still being inferred (`{integer}`).
    /// It becomes whatever integer (or float) type it meets first, and `i32`
    /// if it meets none by the end of the function.
    IntVar(u32),
    /// The same for a float literal (`{float}`), defaulting to `f64`.
    FloatVar(u32),
    /// Something already reported as an error: it matches anything, so one
    /// mistake doesn't cascade.
    Error,
}

/// An interned type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ty(u32);

/// Owns every type.
#[derive(Debug)]
pub struct TyCtx {
    kinds: Vec<TyKind>,
    ids: HashMap<TyKind, Ty>,
    pub unit: Ty,
    pub bool: Ty,
    pub never: Ty,
    pub error: Ty,
    pub str: Ty,
    pub char: Ty,
    pub i32: Ty,
    pub f64: Ty,
    pub usize: Ty,
}

impl Default for TyCtx {
    fn default() -> Self {
        let mut tcx = TyCtx {
            kinds: Vec::new(),
            ids: HashMap::new(),
            unit: Ty(0),
            bool: Ty(0),
            never: Ty(0),
            error: Ty(0),
            str: Ty(0),
            char: Ty(0),
            i32: Ty(0),
            f64: Ty(0),
            usize: Ty(0),
        };
        tcx.unit = tcx.intern(TyKind::Unit);
        tcx.bool = tcx.intern(TyKind::Bool);
        tcx.never = tcx.intern(TyKind::Never);
        tcx.error = tcx.intern(TyKind::Error);
        tcx.str = tcx.intern(TyKind::Str);
        tcx.char = tcx.intern(TyKind::Char);
        tcx.i32 = tcx.intern(TyKind::Int(IntTy::I32));
        tcx.f64 = tcx.intern(TyKind::Float(FloatTy::F64));
        tcx.usize = tcx.intern(TyKind::Int(IntTy::USIZE));
        tcx
    }
}

impl TyCtx {
    pub fn intern(&mut self, kind: TyKind) -> Ty {
        if let Some(&ty) = self.ids.get(&kind) {
            return ty;
        }
        let ty = Ty(self.kinds.len() as u32);
        self.kinds.push(kind.clone());
        self.ids.insert(kind, ty);
        ty
    }

    pub fn kind(&self, ty: Ty) -> &TyKind {
        &self.kinds[ty.0 as usize]
    }

    pub fn int(&mut self, int: IntTy) -> Ty {
        self.intern(TyKind::Int(int))
    }

    pub fn optional(&mut self, ty: Ty) -> Ty {
        self.intern(TyKind::Optional(ty))
    }

    pub fn as_int(&self, ty: Ty) -> Option<IntTy> {
        match self.kind(ty) {
            TyKind::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn is_error(&self, ty: Ty) -> bool {
        matches!(self.kind(ty), TyKind::Error)
    }

    /// Whether a type check involving `ty` should be skipped: it's already an
    /// error, or it never produces a value.
    pub fn is_lenient(&self, ty: Ty) -> bool {
        matches!(self.kind(ty), TyKind::Error | TyKind::Never)
    }

    /// The type a primitive type name denotes, or `None` for one v1 doesn't
    /// support (`f16`, integers wider than 64 bits).
    pub fn prim(&mut self, p: PrimType) -> Option<Ty> {
        Some(match p {
            PrimType::Int { signed, bits } if bits <= 64 => self.int(IntTy { signed, bits, size: false }),
            PrimType::Int { .. } | PrimType::F16 => return None,
            PrimType::Isize => self.int(IntTy { signed: true, bits: 63, size: true }),
            PrimType::Usize => self.usize,
            PrimType::F32 => self.intern(TyKind::Float(FloatTy::F32)),
            PrimType::F64 => self.f64,
            PrimType::Bool => self.bool,
            PrimType::Char => self.char,
            PrimType::Str => self.str,
            PrimType::Never => self.never,
        })
    }

    /// The type as Loom source would write it.
    pub fn display(&self, p: &Program, ty: Ty) -> String {
        match self.kind(ty) {
            TyKind::Int(i) if i.size => (if i.signed { "isize" } else { "usize" }).into(),
            TyKind::Int(i) => format!("{}{}", if i.signed { 'i' } else { 'u' }, i.bits),
            TyKind::Float(FloatTy::F32) => "f32".into(),
            TyKind::Float(FloatTy::F64) => "f64".into(),
            TyKind::Bool => "bool".into(),
            TyKind::Char => "char".into(),
            TyKind::Str => "str".into(),
            TyKind::Unit => "()".into(),
            TyKind::Never => "never".into(),
            TyKind::Tuple(items) => {
                let items: Vec<String> = items.iter().map(|t| self.display(p, *t)).collect();
                if items.len() == 1 { format!("({},)", items[0]) } else { format!("({})", items.join(", ")) }
            }
            TyKind::Optional(t) => format!("?{}", self.display(p, *t)),
            TyKind::Result(t, e) => format!("{} ! {}", self.display(p, *t), self.display(p, *e)),
            TyKind::Range(t) => format!("Range[{}]", self.display(p, *t)),
            TyKind::Fn(params, ret) => {
                let params: Vec<String> = params.iter().map(|t| self.display(p, *t)).collect();
                format!("({}) => {}", params.join(", "), self.display(p, *ret))
            }
            TyKind::Adt(def, args) => {
                let name = p.def(*def).name.clone();
                if args.is_empty() {
                    name
                } else {
                    let args: Vec<String> = args.iter().map(|t| self.display(p, *t)).collect();
                    format!("{name}[{}]", args.join(", "))
                }
            }
            TyKind::Any(wefts) => {
                let names: Vec<String> = wefts.iter().map(|w| p.def(*w).name.clone()).collect();
                format!("any {}", names.join(" + "))
            }
            TyKind::IntVar(_) => "{integer}".into(),
            TyKind::FloatVar(_) => "{float}".into(),
            TyKind::Error => "{error}".into(),
        }
    }
}
