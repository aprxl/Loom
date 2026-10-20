//! Deriving `Default`: a warp that lists `Default` in its header and doesn't
//! write `default` gets one, made here from the declaration, before names are
//! resolved. Each field without a default of its own takes its type's:
//!
//! ```text
//! warp Config: Default { host: String = "localhost", port: u16 }
//! // gets:  pub fn default() -> Self = Self(port: std.default.of[u16]())
//! ```
//!
//! A warp with cases takes its first case, if that has no payload; for any
//! other warp the missing `default` is reported where the weft asks for it.

use loom_syntax::ast::{
    self, Arg, Expr, ExprKind, FieldName, FnBody, FnDecl, FnQualifier, FnRet, GenericArg, Ident, ItemKind, Member,
    MemberKind, Ty, TyKind, Vis, WarpBody,
};
use loom_syntax::source::Span;

/// Adds the derived `default` to every warp of `module` that needs one.
pub(super) fn derive_defaults(module: &mut ast::Module) {
    for item in &mut module.items {
        let ItemKind::Warp(w) = &mut item.kind else { continue };
        if !w.wefts.iter().any(is_default) || has_default(&w.body) {
            continue;
        }
        let span = item.span;
        let Some(body) = default_body(&w.body, span) else { continue };
        let member = Member {
            docs: vec!["The value every field's default makes.".to_string()],
            attrs: Vec::new(),
            vis: Vis::Pub,
            kind: MemberKind::Fn(FnDecl {
                qualifier: FnQualifier::None,
                name: ident("default", span),
                generics: Vec::new(),
                params: Vec::new(),
                ret: FnRet::Value(Ty { kind: TyKind::SelfType, span }),
                givens: Vec::new(),
                where_clause: Vec::new(),
                body: Some(FnBody::Expr(body)),
            }),
            span,
        };
        if let WarpBody::Members(members) = &mut w.body {
            members.push(member);
        }
    }
}

fn is_default(ty: &Ty) -> bool {
    matches!(&ty.kind, TyKind::Path { segments, args } if args.is_empty()
        && segments.last().is_some_and(|s| s.name == "Default"))
}

fn has_default(body: &WarpBody) -> bool {
    match body {
        WarpBody::Members(members) => members
            .iter()
            .any(|m| matches!(&m.kind, MemberKind::Fn(f) if f.name.name == "default" && f.params.is_empty())),
        WarpBody::Tuple(_) => false,
    }
}

fn ident(name: &str, span: Span) -> Ident {
    Ident { name: name.to_string(), span }
}

fn expr(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

/// `std.default.of[ty]()`.
fn default_of(ty: &Ty, span: Span) -> Expr {
    let mut path = expr(ExprKind::Name(ident("std", span)), span);
    for part in ["default", "of"] {
        path = expr(ExprKind::Field { base: Box::new(path), field: FieldName::Named(ident(part, span)) }, span);
    }
    let callee = expr(ExprKind::Index { base: Box::new(path), args: vec![GenericArg::Type(ty.clone())] }, span);
    expr(ExprKind::Call { callee: Box::new(callee), args: Vec::new() }, span)
}

/// The expression `default` returns, if the warp's shape allows one.
fn default_body(body: &WarpBody, span: Span) -> Option<Expr> {
    let WarpBody::Members(members) = body else { return None };
    let mut args = Vec::new();
    let mut first_case: Option<&ast::CaseDecl> = None;
    for m in members {
        match &m.kind {
            MemberKind::Field(f) | MemberKind::Through(f) => {
                if f.default.is_none() {
                    args.push(Arg {
                        label: Some(f.name.clone()),
                        is_given: false,
                        mutable: false,
                        value: default_of(&f.ty, span),
                        span,
                    });
                }
            }
            MemberKind::Cases(cases) if first_case.is_none() => first_case = cases.first(),
            _ => {}
        }
    }
    if let Some(case) = first_case {
        // A warp with cases: its first case, when it carries nothing.
        if case.fields.is_some() {
            return None;
        }
        let this = expr(ExprKind::SelfType, span);
        return Some(expr(ExprKind::Field { base: Box::new(this), field: FieldName::Named(case.name.clone()) }, span));
    }
    let callee = expr(ExprKind::SelfType, span);
    Some(expr(ExprKind::Call { callee: Box::new(callee), args }, span))
}
