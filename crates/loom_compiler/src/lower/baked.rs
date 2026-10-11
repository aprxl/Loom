//! Baked constants in Lua bundles (design/vm.md §17, V4): what compile-time
//! code computed, written as a literal.
//!
//! A constant it computed isn't computed again when the program starts:
//! `C.name = <literal>` takes the place of the function that computed it,
//! and a number is written where it's read. The literal is the value as
//! the program's own code would have made it (see `lower/warp.rs` and
//! `lower/seq.rs`, which build these shapes for the same types): a Lua
//! number or string, a table with `n` and positions for an array or a
//! list, one with a field's name for a record, `nil` for `none`.

use super::Lowerer;
use super::lir::{Expr, TableKey};
use crate::baked::BakedVal;
use crate::heddle::ConstKey;
use crate::ty::{Ty, TyKind};

impl Lowerer<'_> {
    /// The literal for constant `key` when compile-time code computed it
    /// and it can be written, `Expr::Nil` for `none`.
    pub(super) fn baked_literal(&self, key: ConstKey) -> Option<Expr> {
        let c = self.baked?.get(&key)?;
        self.literal(c.ty, &c.val)
    }

    /// The literal for constant `key`, if it's a number, `bool` or `char`:
    /// small enough to write where it's read.
    pub(super) fn baked_scalar(&self, key: ConstKey) -> Option<Expr> {
        let c = self.baked?.get(&key)?;
        let ty = self.tcx().degauge(c.ty);
        match self.tcx().kind(ty) {
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char => self.literal(ty, &c.val),
            _ => None,
        }
    }

    /// `v`, a value of type `ty`, as Lua makes it. `None` when its type
    /// isn't one it knows (which the reader doesn't give).
    fn literal(&self, ty: Ty, v: &BakedVal) -> Option<Expr> {
        let ty = self.tcx().degauge(ty);
        let tcx = self.tcx();
        Some(match (tcx.kind(ty).clone(), v) {
            (TyKind::Int(_), BakedVal::Int(n)) => self.int_value(*n, ty),
            (TyKind::Float(_), BakedVal::Float(bits)) => self.float_value(f64::from_bits(*bits), ty),
            (TyKind::Bool, BakedVal::Bool(b)) => Expr::Bool(*b),
            (TyKind::Char, BakedVal::Char(c)) => Expr::Int(i64::from(*c)),
            (TyKind::Unit, BakedVal::Unit) => Expr::Nil,
            (TyKind::Str, BakedVal::Str(bytes)) => Expr::Str(bytes.clone()),
            (TyKind::Tuple(items), BakedVal::Tuple(vals)) if items.len() == vals.len() => {
                let mut out = Vec::with_capacity(vals.len());
                for (item, val) in items.iter().zip(vals) {
                    out.push(self.literal(*item, val)?);
                }
                Expr::Table(out)
            }
            (TyKind::Array(elem, len), BakedVal::Seq(vals)) if len as usize == vals.len() => {
                self.sequence(elem, vals)?
            }
            (TyKind::Optional(inner), BakedVal::None) => {
                let _ = inner;
                Expr::Nil
            }
            (TyKind::Optional(inner), BakedVal::Some(val)) => {
                let v = self.literal(inner, val)?;
                // A `some` of something that's nil on its own, or of
                // another optional, is a table around it.
                let inner = tcx.degauge(inner);
                if matches!(tcx.kind(inner), TyKind::Unit | TyKind::Optional(_)) { Expr::Table(vec![v]) } else { v }
            }
            (TyKind::Adt(def, args), val) => match (self.p.def_path(def).as_str(), val) {
                ("std.string.String", BakedVal::Str(bytes)) => Expr::Str(bytes.clone()),
                ("std.list.List", BakedVal::Seq(vals)) => self.sequence(*args.first()?, vals)?,
                (_, BakedVal::Tuple(vals)) => {
                    let info = self.typed.results.adts.get(&ty).or_else(|| self.typed.results.warps.get(&def))?;
                    if !info.cases.is_empty() || info.fields.len() != vals.len() {
                        return None;
                    }
                    let mut entries = Vec::with_capacity(vals.len());
                    for (field, val) in info.fields.iter().zip(vals) {
                        entries.push((Self::table_key(&field.key), self.literal(field.ty, val)?));
                    }
                    Expr::Record(entries)
                }
                _ => return None,
            },
            _ => return None,
        })
    }

    /// An array or a list: `{n = 3, [1] = a, [2] = b, [3] = c}`.
    fn sequence(&self, elem: Ty, vals: &[BakedVal]) -> Option<Expr> {
        let mut entries = vec![(TableKey::Name("n".into()), Expr::Int(vals.len() as i64))];
        for (i, val) in vals.iter().enumerate() {
            entries.push((TableKey::Index(i as i64 + 1), self.literal(elem, val)?));
        }
        Some(Expr::Record(entries))
    }
}
