//! Method lookup: what `x.f(…)` calls, before UFCS.
//!
//! In order, the first rule that finds anything wins (see 02-types, "UFCS
//! and method lookup"):
//!
//! 1. a method written in the warp's body;
//! 2. a method a weave gives the type, from a weft in scope;
//! 3. a method a `through` field has (by these same rules), called with
//!    `self` rebound to that field.
//!
//! More than one find at the same level is ambiguous.

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::warp::{Key, Member};
use super::{Checker, Instance};
use crate::ty::Ty;

/// How deep `through` fields are followed; deeper chains are surely cycles.
const MAX_THROUGH: u32 = 8;

/// A method found for a type.
#[derive(Clone, Debug)]
pub(super) struct Found {
    pub inst: Instance,
    /// The `through` fields from the value to the method's `self`.
    pub path: Vec<Key>,
    /// The type of the method's `self`.
    pub self_ty: Ty,
    /// Where it comes from, for messages: `Widget.size`, `frame.size`.
    pub via: String,
}

/// The outcome of looking up a method.
pub(super) enum Lookup {
    None,
    One(Found),
    /// More than one method at the same level (already reported).
    Ambiguous,
}

impl Checker<'_> {
    /// The method `name` of `ty` by rules 1–3, reporting ambiguity.
    pub(super) fn find_method(&mut self, ty: Ty, name: &str, span: Span) -> Lookup {
        let mut found = self.methods_of(ty, name, 0);
        match found.len() {
            0 => Lookup::None,
            1 => Lookup::One(found.remove(0)),
            _ => {
                let shown = self.show(ty);
                let vias: Vec<String> = found.iter().map(|f| format!("`{}`", f.via)).collect();
                let help = if found[0].path.is_empty() {
                    let weft = found[0].via.split('.').next().unwrap_or_default().to_string();
                    format!("say which one: `{weft}.{name}(x)`")
                } else {
                    format!("write `{name}` in `{shown}` itself to choose")
                };
                self.diags.push(
                    Diagnostic::error(codes::T0116, format!("`{shown}` has more than one method `{name}`"))
                        .primary(span, format!("could be {}", vias.join(" or ")))
                        .help(help),
                );
                Lookup::Ambiguous
            }
        }
    }

    /// Rules 1–3 without reporting anything: every method at the first
    /// level that has one.
    pub(super) fn methods_of(&mut self, ty: Ty, name: &str, depth: u32) -> Vec<Found> {
        let ty = self.resolve(ty);
        if let Some(def) = self.warp_def(ty)
            && let Some(Member::Method(m)) = self.member(def, name)
        {
            let via = format!("{}.{name}", self.p.def(def).name);
            return vec![Found { inst: self.member_instance(m, ty), path: Vec::new(), self_ty: ty, via }];
        }
        let woven = self.weave_methods(ty, name, depth);
        if !woven.is_empty() {
            return woven
                .into_iter()
                .map(|(weft, inst)| Found {
                    inst,
                    path: Vec::new(),
                    self_ty: ty,
                    via: format!("{}.{name}", self.p.def(weft).name),
                })
                .collect();
        }
        self.forwarded(ty, name, depth)
    }

    /// Rule 3: the methods `name` of `ty`'s `through` fields.
    pub(super) fn forwarded(&mut self, ty: Ty, name: &str, depth: u32) -> Vec<Found> {
        if depth >= MAX_THROUGH {
            return Vec::new();
        }
        let Some(info) = self.adt_info(ty) else { return Vec::new() };
        let mut out = Vec::new();
        for field in info.fields.iter().filter(|f| f.through) {
            for mut found in self.methods_of(field.ty, name, depth + 1) {
                found.path.insert(0, field.key.clone());
                found.via = format!("{}.{name}", field.name);
                out.push(found);
            }
        }
        out
    }
}
