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
    /// `[T; N]`: exactly `N` elements, stored inline and copied like a value.
    Array(Ty, u64),
    /// `A | B`: a value of one of these types. Always two or more members,
    /// sorted, none of them unions themselves (see [`TyCtx::union`]).
    Union(Vec<Ty>),
    /// An integer literal's type while it's still being inferred (`{integer}`).
    /// It becomes whatever integer (or float) type it meets first, and `i32`
    /// if it meets none by the end of the function.
    IntVar(u32),
    /// The same for a float literal (`{float}`), defaulting to `f64`.
    FloatVar(u32),
    /// A type being inferred: a generic argument not given at a call
    /// (`largest(a, b)`) or a constructor (`Pair(1, 2)`).
    Var(u32),
    /// A generic parameter, in a template: the field types of a generic
    /// warp. Instances replace it with the argument (see [`TyCtx::subst`]).
    Param(DefId),
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

    /// The type `kind` describes, if it's been interned.
    pub fn find(&self, kind: &TyKind) -> Option<Ty> {
        self.ids.get(kind).copied()
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

    /// The union of `members`: nested unions are flattened and duplicates
    /// dropped, and a union of one type is that type.
    pub fn union(&mut self, members: impl IntoIterator<Item = Ty>) -> Ty {
        let mut flat: Vec<Ty> = Vec::new();
        for ty in members {
            match self.kind(ty) {
                TyKind::Union(items) => flat.extend(items.iter().copied()),
                _ => flat.push(ty),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => self.never,
            1 => flat[0],
            _ => self.intern(TyKind::Union(flat)),
        }
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

    /// `ty` with each generic parameter in `map` replaced by its type.
    pub fn subst(&mut self, ty: Ty, map: &[(DefId, Ty)]) -> Ty {
        if map.is_empty() {
            return ty;
        }
        let kind = match self.kind(ty).clone() {
            TyKind::Param(def) => return map.iter().find(|(p, _)| *p == def).map_or(ty, |(_, t)| *t),
            TyKind::Tuple(items) => TyKind::Tuple(items.into_iter().map(|t| self.subst(t, map)).collect()),
            TyKind::Optional(t) => TyKind::Optional(self.subst(t, map)),
            TyKind::Array(t, n) => TyKind::Array(self.subst(t, map), n),
            TyKind::Result(t, e) => TyKind::Result(self.subst(t, map), self.subst(e, map)),
            TyKind::Range(t) => TyKind::Range(self.subst(t, map)),
            TyKind::Fn(params, ret) => {
                TyKind::Fn(params.into_iter().map(|t| self.subst(t, map)).collect(), self.subst(ret, map))
            }
            TyKind::Adt(def, args) => TyKind::Adt(def, args.into_iter().map(|t| self.subst(t, map)).collect()),
            TyKind::Union(items) => {
                let items: Vec<Ty> = items.into_iter().map(|t| self.subst(t, map)).collect();
                return self.union(items);
            }
            _ => return ty,
        };
        self.intern(kind)
    }

    /// Whether `ty` mentions a generic parameter.
    pub fn has_params(&self, ty: Ty) -> bool {
        match self.kind(ty) {
            TyKind::Param(_) => true,
            TyKind::Tuple(items) => items.iter().any(|&t| self.has_params(t)),
            TyKind::Optional(t) | TyKind::Range(t) | TyKind::Array(t, _) => self.has_params(*t),
            TyKind::Result(t, e) => self.has_params(*t) || self.has_params(*e),
            TyKind::Fn(params, ret) => params.iter().any(|&t| self.has_params(t)) || self.has_params(*ret),
            TyKind::Adt(_, args) | TyKind::Union(args) => args.iter().any(|&t| self.has_params(t)),
            _ => false,
        }
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
            TyKind::Array(t, n) => format!("[{}; {n}]", self.display(p, *t)),
            TyKind::Result(t, e) => format!("{} ! {}", self.display(p, *t), self.display(p, *e)),
            TyKind::Range(t) => format!("Range[{}]", self.display(p, *t)),
            TyKind::Fn(params, ret) => {
                let params: Vec<String> = params.iter().map(|t| self.display(p, *t)).collect();
                format!("({}) => {}", params.join(", "), self.display(p, *ret))
            }
            // `Slice[T]`, the builtin warp behind slices, is written `[T]`.
            TyKind::Adt(def, args)
                if args.len() == 1
                    && p.def(*def).name == "Slice"
                    && p.module(p.def(*def).module).path == "std.slice" =>
            {
                format!("[{}]", self.display(p, args[0]))
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
            TyKind::Union(items) => {
                let items: Vec<String> = items.iter().map(|t| self.display(p, *t)).collect();
                items.join(" | ")
            }
            TyKind::IntVar(_) => "{integer}".into(),
            TyKind::FloatVar(_) => "{float}".into(),
            TyKind::Var(_) => "_".into(),
            TyKind::Param(def) => p.def(*def).name.clone(),
            TyKind::Error => "{error}".into(),
        }
    }
}
