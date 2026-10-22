//! The hidden region parameter of a warp that holds pointers
//! (design/v6-spools.md): `Expr` with a `^Expr` field is `Expr@r` for
//! whichever spool its nodes were allocated in. It is made after names are
//! resolved, because whether a warp holds a pointer can depend on another
//! warp's fields.

use std::collections::HashSet;

use crate::hir::{
    DefId, DefKind, GenericArg, GenericParamDef, GenericParamKind, Program, Ty, TyKind, WarpDef, WarpModifier,
};

/// The name of the hidden parameter: not one a program can write.
pub const HIDDEN: &str = "@";

/// Gives each warp that holds a pointer, directly or through another warp
/// that does, a region parameter (the last of its generics).
pub(super) fn add_params(p: &mut Program) {
    let warps: Vec<DefId> = (0..p.defs.len() as u32)
        .map(DefId)
        .filter(|&d| matches!(p.def(d).kind, DefKind::Warp(_)) && !is_builtin(p, d))
        .collect();
    let mut regional: HashSet<DefId> = HashSet::new();
    loop {
        let before = regional.len();
        for &w in &warps {
            if !regional.contains(&w) && holds_pointer(p, w, &regional) {
                regional.insert(w);
            }
        }
        if regional.len() == before {
            break;
        }
    }
    // A builtin marked `#region` (text, whose spool copies are typed) gets the
    // parameter without holding pointers: warps that hold it don't follow.
    let marked: Vec<DefId> = (0..p.defs.len() as u32)
        .map(DefId)
        .filter(|&d| matches!(p.def(d).kind, DefKind::Warp(_)) && p.def(d).attrs.iter().any(|a| a.name == "region"))
        .collect();
    let mut give: Vec<DefId> = regional.into_iter().chain(marked).collect();
    give.sort();
    let weaves: Vec<DefId> = (0..p.defs.len() as u32)
        .map(DefId)
        .filter(|&d| match &p.def(d).kind {
            DefKind::Weave(wv) => matches!(&wv.target.kind, TyKind::Def { def, .. } if give.contains(def)),
            _ => false,
        })
        .collect();
    for w in give {
        let id = push_hidden_param(p, w);
        if let DefKind::Warp(def) = &mut p.defs[w.index()].kind {
            def.generics.push(id);
        }
    }
    // A `weave W into Expr` holds for `Expr` in every region: its target
    // names the weave's own region parameter.
    for wv in weaves {
        let id = push_hidden_param(p, wv);
        if let DefKind::Weave(weave) = &mut p.defs[wv.index()].kind {
            weave.generics.push(id);
            let target_span = weave.target.span;
            if let TyKind::Def { args, .. } = &mut weave.target.kind {
                args.push(GenericArg::Type(Ty { kind: TyKind::Def { def: id, args: Vec::new() }, span: target_span }));
            }
        }
    }
}

/// A new region parameter of `owner`, named [`HIDDEN`].
fn push_hidden_param(p: &mut Program, owner: DefId) -> DefId {
    let (span, vis, module) = {
        let d = p.def(owner);
        (d.span, d.vis, d.module)
    };
    let id = DefId(p.defs.len() as u32);
    p.defs.push(crate::hir::Def {
        name: HIDDEN.to_string(),
        span,
        vis,
        module,
        parent: Some(owner),
        docs: Vec::new(),
        attrs: Vec::new(),
        is_type: true,
        kind: DefKind::GenericParam(GenericParamDef { kind: GenericParamKind::Region }),
    });
    id
}

fn is_builtin(p: &Program, d: DefId) -> bool {
    p.def(d).attrs.iter().any(|a| a.name == "builtin")
}

/// Whether the fields of warp `w` hold a pointer, or a warp in `regional`.
fn holds_pointer(p: &Program, w: DefId, regional: &HashSet<DefId>) -> bool {
    let DefKind::Warp(def) = &p.def(w).kind else { return false };
    if def.modifier == WarpModifier::Ref {
        return false;
    }
    let field_tys = def.fields.iter().filter_map(|&f| match &p.def(f).kind {
        DefKind::Field(f) => Some(&f.ty),
        _ => None,
    });
    let case_tys = def.cases.iter().flat_map(|&c| match &p.def(c).kind {
        DefKind::Case(c) => c.fields.iter().flatten().map(|f| &f.ty).collect::<Vec<_>>(),
        _ => Vec::new(),
    });
    field_tys.chain(case_tys).any(|t| mentions_pointer(t, regional))
}

fn mentions_pointer(t: &Ty, regional: &HashSet<DefId>) -> bool {
    let any = |ts: &[Ty]| ts.iter().any(|t| mentions_pointer(t, regional));
    match &t.kind {
        TyKind::Pointer(_) => true,
        TyKind::Def { def, args } => {
            regional.contains(def)
                || args.iter().any(|a| match a {
                    GenericArg::Type(t) => mentions_pointer(t, regional),
                    GenericArg::Expr(_) => false,
                })
        }
        TyKind::Tuple(items) | TyKind::Union(items) => any(items),
        TyKind::Array { elem, .. } => mentions_pointer(elem, regional),
        TyKind::Slice(t) | TyKind::Optional(t) | TyKind::RawPointer(t) => mentions_pointer(t, regional),
        TyKind::Region { ty, .. } => mentions_pointer(ty, regional),
        TyKind::Result { ok, err } => mentions_pointer(ok, regional) || mentions_pointer(err, regional),
        _ => false,
    }
}

/// Whether `def` is the hidden region parameter of a warp.
pub fn is_hidden(p: &Program, def: DefId) -> bool {
    let d = p.def(def);
    d.name == HIDDEN && matches!(d.kind, DefKind::GenericParam(_))
}

/// The hidden region parameter of warp `w`, if it has one.
pub fn hidden_param(p: &Program, w: &WarpDef) -> Option<DefId> {
    w.generics.last().copied().filter(|&g| is_hidden(p, g))
}
