//! Optionals and results (design/vm.md §10.5).
//!
//! `?T` is `T` with `none` in one of `T`'s spare bit patterns (a niche) when
//! it has one, and `T` then a flag otherwise; in registers, the same parts.
//! `T ! E` is a tag (0 for `ok`) and the value or the error, in memory.

use loom_syntax::source::Span;

use super::Builder;
use super::repr::{Repr, loaded};
use super::value::Val;
use crate::heddle::{Cmp, InstKind, Mem, VTy, Value};
use crate::ty::layout::{Niche, Shape};
use crate::ty::{Ty, TyKind};

/// Where an optional keeps whether it's `some`.
#[derive(Clone, Copy, Debug)]
enum Presence {
    /// A byte at this offset, after the value: 1 for `some`.
    Flag(u64),
    /// `none` is this pattern of the value's niche.
    Niche(Niche),
}

impl<'a> Builder<'a> {
    /// How `opt` (an optional type) says whether it holds a value.
    fn presence(&mut self, opt: Ty, span: Span) -> Option<Presence> {
        let TyKind::Optional(inner) = *self.tcx().kind(opt) else {
            return self.internal(span, "the presence of something that isn't optional");
        };
        match self.layout(opt)?.shape {
            Shape::Optional { flag: Some(off) } => Some(Presence::Flag(off)),
            Shape::Optional { flag: None } => {
                let niche = self.layout(inner)?.niche?;
                Some(Presence::Niche(niche))
            }
            _ => self.internal(span, "an optional laid out as something else"),
        }
    }

    /// Of a value in registers: the part at `off` with `bytes` bytes.
    fn part_at(&mut self, ty: Ty, off: u64, span: Span) -> Option<(usize, Mem)> {
        let repr = self.repr_of(ty, span)?;
        let i = repr.parts().iter().position(|p| p.off == off)?;
        Some((i, repr.parts()[i].mem))
    }

    /// `some(v)` as a value of `opt`, where `v` is a value of `inner`.
    pub(super) fn some(&mut self, opt: Ty, inner: Ty, v: Val, span: Span) -> Option<Val> {
        let presence = self.presence(opt, span)?;
        match self.repr_of(opt, span)? {
            Repr::Regs(_) => {
                let mut vals = self.regs(v, inner, span)?;
                if let Presence::Flag(_) = presence {
                    vals.push(self.iconst(1));
                }
                Some(Val::Regs(vals))
            }
            Repr::Mem { .. } => {
                let ptr = self.temp(opt, span)?;
                self.move_to(ptr, 0, v, inner, span)?;
                if let Presence::Flag(off) = presence {
                    let one = self.iconst(1);
                    self.store(Mem::U8, ptr, off as i64, one);
                }
                self.own_temp(ptr, opt);
                Some(Val::Mem(ptr))
            }
        }
    }

    /// `none` as a value of `opt`.
    pub(super) fn none(&mut self, opt: Ty, span: Span) -> Option<Val> {
        let presence = self.presence(opt, span)?;
        match self.repr_of(opt, span)? {
            Repr::Regs(parts) => {
                let mut vals: Vec<Value> = parts
                    .iter()
                    .map(|p| match p.vty {
                        VTy::F32 | VTy::F64 => self.emit1(InstKind::Fconst(0), p.vty),
                        vty => self.emit1(InstKind::Iconst(0), vty),
                    })
                    .collect();
                if let Presence::Niche(n) = presence {
                    let (i, mem) = self.part_at(opt, n.offset, span)?;
                    vals[i] = self.emit1(InstKind::Iconst(loaded(mem, n.start)), parts[i].vty);
                }
                Some(Val::Regs(vals))
            }
            Repr::Mem { size, .. } => {
                let ptr = self.temp(opt, span)?;
                self.emit(InstKind::Zero { dst: ptr, size }, &[]);
                if let Presence::Niche(n) = presence {
                    let mem = niche_mem(n.bytes);
                    let k = self.iconst(n.start as u64);
                    self.store(mem, ptr, n.offset as i64, k);
                }
                self.own_temp(ptr, opt);
                Some(Val::Mem(ptr))
            }
        }
    }

    /// Whether `v`, a value of `opt`, holds a value: a `bool`.
    pub(super) fn is_some(&mut self, v: &Val, opt: Ty, span: Span) -> Option<Value> {
        let presence = self.presence(opt, span)?;
        match (v, presence) {
            (Val::Regs(vals), Presence::Flag(_)) => Some(*vals.last()?),
            (Val::Regs(vals), Presence::Niche(n)) => {
                let (i, mem) = self.part_at(opt, n.offset, span)?;
                let none = self.iconst(loaded(mem, n.start));
                Some(self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: vals[i], b: none }, VTy::Int))
            }
            (&Val::Mem(ptr), Presence::Flag(off)) => Some(self.load(Mem::U8, ptr, off as i64, VTy::Int)),
            (&Val::Mem(ptr), Presence::Niche(n)) => {
                let got = self.load(niche_mem(n.bytes), ptr, n.offset as i64, VTy::Int);
                let none = self.iconst(n.start as u64);
                Some(self.emit1(InstKind::ICmp { cmp: Cmp::Ne, unsigned: true, a: got, b: none }, VTy::Int))
            }
        }
    }

    /// The value inside `v`, a value of `opt` that holds one.
    #[allow(dead_code)] // `?`, `some(x)` patterns and `mend` read it next.
    pub(super) fn unwrap_some(&mut self, v: &Val, opt: Ty, span: Span) -> Option<Val> {
        let TyKind::Optional(inner) = *self.tcx().kind(opt) else {
            return self.internal(span, "unwrapping something that isn't optional");
        };
        let presence = self.presence(opt, span)?;
        match v {
            Val::Regs(vals) => {
                let n = match presence {
                    Presence::Flag(_) => vals.len() - 1,
                    Presence::Niche(_) => vals.len(),
                };
                Some(Val::Regs(vals[..n].to_vec()))
            }
            &Val::Mem(ptr) => self.load_val(ptr, 0, inner, span),
        }
    }
}

/// How a niche of `bytes` bytes is loaded: zero-extended, as its patterns
/// are counted.
fn niche_mem(bytes: u8) -> Mem {
    match bytes {
        1 => Mem::U8,
        2 => Mem::U16,
        4 => Mem::U32,
        _ => Mem::W64,
    }
}
