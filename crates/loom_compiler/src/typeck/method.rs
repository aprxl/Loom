//! Method lookup: what `x.f(…)` calls, before UFCS.
//!
//! In order, the first rule that finds anything wins (see 02-types, "UFCS
//! and method lookup"):
//!
//! 1. a method written in the warp's body;
//! 2. a method a weave gives the type, from a weft in scope;
//! 3. for something with items that isn't a `Stream` (an array, list or
//!    slice, a range, a type that weaves `Iter`): a method of `Stream`,
//!    called on a stream of its items;
//! 4. a method a `through` field has (by these same rules), called with
//!    `self` rebound to that field.
//!
//! More than one find at the same level is ambiguous.

use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::infer::VarKind;
use super::warp::{Key, Member};
use super::{Checker, Instance};
use crate::hir::DefKind;
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
    /// For a `Stream` method called on something else that has items (a
    /// list, a range, an `Iter`): how it becomes the stream.
    pub stream: Option<super::IterSource>,
}

/// The outcome of looking up a method.
pub(super) enum Lookup {
    None,
    One(Found),
    /// More than one method at the same level (already reported).
    Ambiguous,
}

impl Checker<'_> {
    /// The method `name` of `ty` by rules 1–4, reporting ambiguity.
    pub(super) fn find_method(&mut self, ty: Ty, name: &str, span: Span) -> Lookup {
        let mut found = self.methods_of(ty, name, 0);
        match found.len() {
            0 => Lookup::None,
            1 => Lookup::One(found.remove(0)),
            _ => {
                let shown = self.show(ty);
                let vias: Vec<String> = found.iter().map(|f| format!("`{}`", f.via)).collect();
                let help = if found[0].path.is_empty() {
                    let via = &found[0].via;
                    format!("say which one: `{via}(x)`")
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

    /// Rules 1–4 without reporting anything: every method at the first
    /// level that has one.
    pub(super) fn methods_of(&mut self, ty: Ty, name: &str, depth: u32) -> Vec<Found> {
        let ty = self.resolve(ty);
        let mut rejected = None;
        if let Some(def) = self.warp_def(ty)
            && let Some(Member::Method(m)) = self.member(def, name)
        {
            let via = format!("{}.{name}", self.p.def(def).name);
            let found = Found { inst: self.member_instance(m, ty), path: Vec::new(), self_ty: ty, via, stream: None };
            if self.shape_fits(&found, ty) {
                return vec![found];
            }
            rejected = Some(found);
        }
        // Arrays and lists have the methods of slices, and primitive types
        // the methods std gives them.
        match self.slice_method(ty, name) {
            Some(f) if self.shape_fits(&f, ty) => return vec![f],
            Some(f) => rejected = Some(f),
            None => {}
        }
        if let Some(found) = self.prim_method(ty, name) {
            return vec![found];
        }
        let woven = self.weave_methods(ty, name, depth);
        if !woven.is_empty() {
            return woven
                .into_iter()
                .map(|(weft, args, inst)| Found {
                    inst,
                    path: Vec::new(),
                    self_ty: ty,
                    via: format!("{}.{name}", self.show_weft(weft, &args)),
                    stream: None,
                })
                .collect();
        }
        // Anything with items has the methods of `Stream`.
        if let Some(found) = self.stream_method(ty, name) {
            return vec![found];
        }
        let forwarded = self.forwarded(ty, name, depth);
        match rejected {
            Some(found) if forwarded.is_empty() => vec![found],
            _ => forwarded,
        }
    }

    /// A slice method with a shape bound (`where T: [U]`) is only a candidate when the elements can be slices; otherwise lookup goes on (to `Stream.join`).
    fn shape_fits(&mut self, f: &Found, ty: Ty) -> bool {
        let DefKind::Fn(fd) = &self.p.def(f.inst.def).kind else { return true };
        if self.shape_bounds(fd).is_empty() {
            return true;
        }
        let Some(seq) = self.seq(ty) else { return true };
        let elem = self.resolve(seq.elem());
        matches!(self.var_of(elem), Some((_, VarKind::Type))) || self.seq(elem).is_some()
    }

    /// Rule 4: the methods `name` of `ty`'s `through` fields.
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
