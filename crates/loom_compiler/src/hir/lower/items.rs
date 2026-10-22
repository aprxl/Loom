//! Lowering items, members, signatures, generics and types.

use loom_syntax::Diagnostic;
use loom_syntax::ast::{self, Ident, MemberKind};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::{FnCtx, ItemDefs, Res, Resolver, RibKind};
use crate::hir::{
    Attr, CaseDef, CaseField, ConstDef, DefId, DefKind, FieldDef, FnDef, FnRet, GenericArg, GenericParamDef,
    GenericParamKind, GivenParam, LocalId, LocalKind, ModuleId, Param, Region, Ty, TyKind, TypeDef, WarpDef, WeaveDef,
    WeftDef, WherePred,
};

/// What a dotted path in a type or pattern resolved to.
pub(super) enum PathRes {
    Module(ModuleId),
    Type(Ty),
    /// A function, constant or other value definition.
    Value(DefId),
    Local(LocalId),
    /// `target` or `heap`.
    Intrinsic,
    /// `Type.name`.
    Assoc(Ty, Ident),
    Error,
}

impl Resolver<'_> {
    /// Lowers one item whose defs were made by `collect_item`. Items don't see
    /// the locals of a function they're nested in.
    pub(super) fn lower_item(&mut self, item: &ast::Item, defs: &ItemDefs) {
        let saved_fx = std::mem::take(&mut self.fx);
        let saved_self = std::mem::replace(&mut self.self_type, false);
        let saved_cond = std::mem::replace(&mut self.in_cond, false);
        self.push_rib(RibKind::Item);

        use ast::ItemKind as K;
        match (&item.kind, defs.def) {
            (K::Use(_), _) => {}
            (K::Const(c), Some(def)) => {
                let kind = self.lower_const(c);
                self.finish_def(def, &item.attrs, kind);
            }
            (K::Type(t), Some(def)) => {
                let kind = self.lower_type_decl(def, t);
                self.finish_def(def, &item.attrs, kind);
            }
            (K::Fn(f), Some(def)) => {
                self.require_body(f, is_builtin(&item.attrs));
                let kind = DefKind::Fn(self.lower_fn(def, f, None));
                self.finish_def(def, &item.attrs, kind);
            }
            (K::ConstExpr(e), Some(def)) => {
                let kind = DefKind::ConstBlock(self.lower_expr(e));
                self.finish_def(def, &item.attrs, kind);
            }
            (K::Warp(w), Some(def)) => self.lower_warp(def, w, &defs.members, &item.attrs),
            (K::Weft(w), Some(def)) => self.lower_weft(def, w, &defs.members, &item.attrs),
            (K::Weave(w), Some(def)) => self.lower_weave(def, w, &defs.members, &item.attrs),
            (K::Extern(block), _) => {
                for (member, member_defs) in block.members.iter().zip(&defs.members) {
                    if let (MemberKind::Fn(f), Some(&def)) = (&member.kind, member_defs.first()) {
                        let kind = DefKind::Fn(self.lower_fn(def, f, Some(block.abi.clone())));
                        self.finish_def(def, &member.attrs, kind);
                    }
                }
            }
            _ => {}
        }

        self.pop_rib();
        self.fx = saved_fx;
        self.self_type = saved_self;
        self.in_cond = saved_cond;
    }

    fn finish_def(&mut self, def: DefId, attrs: &[ast::Attr], kind: DefKind) {
        for a in attrs {
            let name = a.name.name.as_str();
            if PLANNED_ATTRS.contains(&name) {
                self.error(codes::U0102, a.span, format!("`#{name}` isn't implemented yet"), "not implemented yet");
            } else if !KNOWN_ATTRS.contains(&name) {
                self.error(codes::N0101, a.span, format!("unknown attribute `#{name}`"), "no such attribute");
            }
        }
        let attrs =
            attrs.iter().map(|a| Attr { name: a.name.name.clone(), args: a.args.clone(), span: a.span }).collect();
        let d = &mut self.p.defs[def.index()];
        d.attrs = attrs;
        d.kind = kind;
    }

    fn lower_const(&mut self, c: &ast::ConstDecl) -> DefKind {
        let ty = c.ty.as_ref().map(|t| self.lower_ty(t));
        let value = self.lower_expr(&c.value);
        DefKind::Const(ConstDef { ty, value })
    }

    fn lower_type_decl(&mut self, def: DefId, t: &ast::TypeDecl) -> DefKind {
        self.push_rib(RibKind::Normal);
        let generics = self.lower_generics(def, &t.generics);
        let bounds = t.bounds.iter().map(|b| self.lower_ty(b)).collect();
        let value = t.value.as_ref().map(|v| self.lower_ty(v));
        // A gauge's predicate is a hole lambda: `where _ % 2 == 0`.
        let gauge = t.gauge.as_ref().map(|g| self.lower_arg_value(g));
        self.pop_rib();
        DefKind::Type(TypeDef { generics, bounds, value, gauge })
    }

    // ------------------------------------------------------------ warps, wefts, weaves

    /// Binds the member types and constants of a warp, weft or weave, so the
    /// body can name them directly (`Idx`, `MAX`).
    fn bind_member_names(&mut self, members: &[ast::Member], defs: &[Vec<DefId>]) {
        for (member, member_defs) in members.iter().zip(defs) {
            if let (MemberKind::Type(_) | MemberKind::Const(_), Some(&def)) = (&member.kind, member_defs.first()) {
                let name = self.p.def(def).name.clone();
                self.bind(&name, Res::Def(def));
            }
        }
    }

    fn lower_warp(&mut self, def: DefId, w: &ast::WarpDecl, member_defs: &[Vec<DefId>], attrs: &[ast::Attr]) {
        self.push_rib(RibKind::Normal);
        let generics = self.lower_generics(def, &w.generics);
        self.self_type = true;
        let wefts = w.wefts.iter().map(|t| self.lower_ty(t)).collect();
        let where_clause = self.lower_where(&w.where_clause);
        let (mut fields, mut cases, mut members) = (Vec::new(), Vec::new(), Vec::new());
        let tuple = match &w.body {
            ast::WarpBody::Tuple(tuple_fields) => {
                for (field, defs) in tuple_fields.iter().zip(member_defs) {
                    let ty = self.lower_ty(&field.ty);
                    if let Some(&field_def) = defs.first() {
                        self.finish_def(field_def, &[], DefKind::Field(FieldDef { ty, default: None, through: false }));
                        fields.push(field_def);
                    }
                }
                true
            }
            ast::WarpBody::Members(ast_members) => {
                self.bind_member_names(ast_members, member_defs);
                let needs_body = !is_builtin(attrs);
                for (member, defs) in ast_members.iter().zip(member_defs) {
                    self.lower_member(member, defs, &mut fields, &mut cases, &mut members, needs_body);
                }
                false
            }
        };
        self.pop_rib();
        let warp = WarpDef { modifier: w.modifier, generics, wefts, where_clause, tuple, fields, cases, members };
        self.finish_def(def, attrs, DefKind::Warp(warp));
    }

    fn lower_member(
        &mut self,
        member: &ast::Member,
        defs: &[DefId],
        fields: &mut Vec<DefId>,
        cases: &mut Vec<DefId>,
        members: &mut Vec<DefId>,
        needs_body: bool,
    ) {
        match &member.kind {
            MemberKind::Field(f) | MemberKind::Through(f) => {
                let ty = self.lower_ty(&f.ty);
                let default = f.default.as_ref().map(|e| self.lower_expr(e));
                let through = matches!(member.kind, MemberKind::Through(_));
                if let Some(&def) = defs.first() {
                    self.finish_def(def, &member.attrs, DefKind::Field(FieldDef { ty, default, through }));
                    fields.push(def);
                }
            }
            MemberKind::Cases(case_decls) => {
                for (case, &def) in case_decls.iter().zip(defs) {
                    let fields = case.fields.as_ref().map(|fs| fs.iter().map(|f| self.lower_case_field(f)).collect());
                    self.finish_def(def, &member.attrs, DefKind::Case(CaseDef { fields }));
                    cases.push(def);
                }
            }
            MemberKind::Const(c) => {
                if let Some(&def) = defs.first() {
                    let kind = self.lower_const(c);
                    self.finish_def(def, &member.attrs, kind);
                    members.push(def);
                }
            }
            MemberKind::Type(t) => {
                if let Some(&def) = defs.first() {
                    let kind = self.lower_type_decl(def, t);
                    self.finish_def(def, &member.attrs, kind);
                    members.push(def);
                }
            }
            MemberKind::Fn(f) => {
                if let Some(&def) = defs.first() {
                    self.require_body(f, !needs_body || is_builtin(&member.attrs));
                    let kind = DefKind::Fn(self.lower_fn(def, f, None));
                    self.finish_def(def, &member.attrs, kind);
                    members.push(def);
                }
            }
            MemberKind::ConstExpr(e) => {
                if let Some(&def) = defs.first() {
                    let kind = DefKind::ConstBlock(self.lower_expr(e));
                    self.finish_def(def, &member.attrs, kind);
                    members.push(def);
                }
            }
        }
    }

    fn lower_case_field(&mut self, f: &ast::CaseField) -> CaseField {
        CaseField { name: f.name.clone(), absorbs: f.absorbs, ty: self.lower_ty(&f.ty), span: f.span }
    }

    fn lower_weft(&mut self, def: DefId, w: &ast::WeftDecl, member_defs: &[Vec<DefId>], attrs: &[ast::Attr]) {
        self.push_rib(RibKind::Normal);
        let generics = self.lower_generics(def, &w.generics);
        self.self_type = true;
        let supers = w.supers.iter().map(|t| self.lower_ty(t)).collect();
        self.bind_member_names(&w.members, member_defs);
        let members = self.lower_members(&w.members, member_defs, false);
        self.pop_rib();
        self.finish_def(def, attrs, DefKind::Weft(WeftDef { generics, supers, members }));
    }

    fn lower_weave(&mut self, def: DefId, w: &ast::WeaveDecl, member_defs: &[Vec<DefId>], attrs: &[ast::Attr]) {
        self.push_rib(RibKind::Normal);
        let generics = self.lower_generics(def, &w.generics);
        let weft = self.lower_ty(&w.weft);
        let target = self.lower_ty(&w.target);
        let where_clause = self.lower_where(&w.where_clause);
        self.self_type = true;
        self.bind_member_names(&w.members, member_defs);
        let members = self.lower_members(&w.members, member_defs, true);
        self.pop_rib();
        self.finish_def(def, attrs, DefKind::Weave(WeaveDef { generics, weft, target, where_clause, members }));
    }

    /// A function outside a weft or an `extern` block needs a body, unless
    /// the compiler provides it (`#builtin`).
    fn require_body(&mut self, f: &ast::FnDecl, builtin: bool) {
        if f.body.is_some() || builtin {
            return;
        }
        self.report(
            Diagnostic::error(codes::S0213, format!("`{}` has no body", f.name.name))
                .primary(f.name.span, "a body goes after the signature")
                .help(
                    "write it as `fn f() { … }` or `fn f() = …`; only wefts and `extern` blocks list signatures alone",
                ),
        );
    }

    /// The members of a weft or weave: functions and types. A weave's
    /// functions need bodies; a weft's may be signatures alone.
    fn lower_members(
        &mut self,
        ast_members: &[ast::Member],
        member_defs: &[Vec<DefId>],
        needs_body: bool,
    ) -> Vec<DefId> {
        let (mut fields, mut cases, mut members) = (Vec::new(), Vec::new(), Vec::new());
        for (member, defs) in ast_members.iter().zip(member_defs) {
            self.lower_member(member, defs, &mut fields, &mut cases, &mut members, needs_body);
        }
        members
    }

    // ------------------------------------------------------------ functions

    pub(super) fn lower_fn(&mut self, def: DefId, f: &ast::FnDecl, abi: Option<String>) -> FnDef {
        self.push_rib(RibKind::Normal);
        let generics = self.lower_generics(def, &f.generics);

        // Parameters are bound one by one, so a type can use an earlier
        // `const` parameter: `fn zeros(const n: usize) -> [f32; n]`.
        self.push_rib(RibKind::Normal);
        let mut self_local = None;
        let mut params = Vec::with_capacity(f.params.len());
        // A `given`'s name is a region the parameters' types can name.
        let given_locals: Vec<LocalId> = f
            .givens
            .iter()
            .map(|g| {
                let local = self.new_local(&g.name.name, g.name.span, false, LocalKind::Given);
                self.bind(&g.name.name, Res::Local(local));
                local
            })
            .collect();
        for p in &f.params {
            let ty = p.ty.as_ref().map(|t| self.lower_ty(t));
            let default = p.default.as_ref().map(|e| self.lower_expr(e));
            let mutable = p.mode == ast::ParamMode::Mut;
            let (local, is_self) = match &p.name {
                ast::ParamName::SelfValue => {
                    if !self.self_type {
                        self.error(
                            codes::N0106,
                            p.span,
                            "only methods of a warp, weft or weave take `self`",
                            "`self` outside a method",
                        );
                    }
                    let local = self.new_local("self", p.span, mutable, LocalKind::SelfParam);
                    // So a type can name the spool it was called on: `^T@self`.
                    self.bind("self", Res::Local(local));
                    self_local = Some(local);
                    (local, true)
                }
                ast::ParamName::Named(name) => {
                    let local = self.new_local(&name.name, name.span, mutable, LocalKind::Param);
                    self.bind(&name.name, Res::Local(local));
                    (local, false)
                }
            };
            params.push(Param { mode: p.mode, local, is_self, ty, default, span: p.span });
        }
        let givens = f
            .givens
            .iter()
            .zip(given_locals)
            .map(|(g, local)| GivenParam { local, ty: self.lower_ty(&g.ty) })
            .collect();
        let ret = match &f.ret {
            ast::FnRet::Unit => FnRet::Unit,
            ast::FnRet::Value(t) => FnRet::Value(self.lower_ty(t)),
            ast::FnRet::Shuttle { yields, pulls } => FnRet::Shuttle {
                yields: yields.as_ref().map(|y| (y.place, self.lower_ty(&y.ty))),
                pulls: pulls.as_ref().map(|t| self.lower_ty(t)),
            },
        };
        let where_clause = self.lower_where(&f.where_clause);

        let saved = std::mem::replace(
            &mut self.fx,
            FnCtx { self_local, is_shuttle: matches!(ret, FnRet::Shuttle { .. }), ..FnCtx::default() },
        );
        let body = f.body.as_ref().map(|body| match body {
            ast::FnBody::Block(block) => self.block_expr(block),
            ast::FnBody::Expr(e) => self.lower_expr(e),
        });
        self.fx = saved;

        self.pop_rib();
        self.pop_rib();
        FnDef { qualifier: f.qualifier, generics, params, ret, givens, where_clause, body, abi }
    }

    /// Makes defs for generic parameters and binds them. Bounds are lowered
    /// after every parameter is bound, so `[T, U: From[T]]` works.
    fn lower_generics(&mut self, owner: DefId, params: &[ast::GenericParam]) -> Vec<DefId> {
        let module = self.module;
        let defs: Vec<DefId> = params
            .iter()
            .map(|p| {
                let (name, is_type) = match &p.kind {
                    ast::GenericParamKind::Type { name, .. } => (name, true),
                    ast::GenericParamKind::Const { name, .. } | ast::GenericParamKind::Region { name } => (name, false),
                };
                let def = self.new_def(&name.name, name.span, ast::Vis::Private, module, Some(owner), &[], is_type);
                self.bind(&name.name, Res::Def(def));
                def
            })
            .collect();
        for (p, &def) in params.iter().zip(&defs) {
            let kind = match &p.kind {
                ast::GenericParamKind::Type { bounds, .. } => {
                    GenericParamKind::Type { bounds: bounds.iter().map(|b| self.lower_ty(b)).collect() }
                }
                ast::GenericParamKind::Const { ty, .. } => GenericParamKind::Const { ty: self.lower_ty(ty) },
                ast::GenericParamKind::Region { .. } => GenericParamKind::Region,
            };
            self.p.defs[def.index()].kind = DefKind::GenericParam(GenericParamDef { kind });
        }
        defs
    }

    fn lower_where(&mut self, preds: &[ast::WherePred]) -> Vec<WherePred> {
        preds
            .iter()
            .map(|p| WherePred {
                ty: self.lower_ty(&p.ty),
                bounds: p.bounds.iter().map(|b| self.lower_ty(b)).collect(),
                span: p.span,
            })
            .collect()
    }

    // ------------------------------------------------------------ paths

    fn res_to_path(&self, res: Res, span: Span) -> PathRes {
        match res {
            Res::Local(l) => PathRes::Local(l),
            Res::Def(d) if self.p.def(d).is_type => {
                PathRes::Type(Ty { kind: TyKind::Def { def: d, args: Vec::new() }, span })
            }
            Res::Def(d) => PathRes::Value(d),
            Res::Module(m) => PathRes::Module(m),
            Res::Intrinsic(_) => PathRes::Intrinsic,
        }
    }

    /// Resolves `a.b.c` through modules and types, for type and pattern paths.
    pub(super) fn resolve_path(&mut self, segments: &[Ident]) -> PathRes {
        let Some(first) = segments.first() else { return PathRes::Error };
        let mut current = match self.lookup(&first.name) {
            Some(res) => self.res_to_path(res, first.span),
            None => {
                self.unknown_name(first);
                return PathRes::Error;
            }
        };
        let mut span = first.span;
        for segment in &segments[1..] {
            span = span.to(segment.span);
            current = match current {
                PathRes::Module(m) => match self.module_member_or_report(m, segment) {
                    Some(binding) => self.res_to_path(binding.into(), span),
                    None => return PathRes::Error,
                },
                PathRes::Type(ty) => PathRes::Assoc(ty, segment.clone()),
                PathRes::Assoc(base, name) => {
                    let base_span = base.span.to(name.span);
                    let inner = Ty { kind: TyKind::Assoc { base: Box::new(base), name }, span: base_span };
                    PathRes::Assoc(inner, segment.clone())
                }
                PathRes::Value(_) | PathRes::Local(_) | PathRes::Intrinsic => {
                    self.error(
                        codes::N0105,
                        segment.span,
                        format!("`{}` has no members that can be named in a path", segments[0].name),
                        "can't look this up here",
                    );
                    return PathRes::Error;
                }
                PathRes::Error => return PathRes::Error,
            };
        }
        current
    }

    // ------------------------------------------------------------ types

    pub(super) fn lower_ty(&mut self, t: &ast::Ty) -> Ty {
        use ast::TyKind as T;
        let span = t.span;
        let boxed = |this: &mut Self, t: &ast::Ty| Box::new(this.lower_ty(t));
        let kind = match &t.kind {
            T::Prim(p) => TyKind::Prim(*p),
            T::Path { segments, args } => self.lower_ty_path(segments, args, span),
            T::SelfType => {
                if self.self_type {
                    TyKind::SelfType
                } else {
                    self.self_type_error(span);
                    TyKind::Error
                }
            }
            T::TypeType => TyKind::TypeType,
            T::ConstCall { segments, args } => match self.resolve_path(segments) {
                PathRes::Value(def) => {
                    TyKind::ConstCall { def, args: args.iter().map(|a| self.lower_expr(a)).collect() }
                }
                PathRes::Error => TyKind::Error,
                _ => {
                    self.error(codes::N0111, span, "only a `const fn` can be called in a type", "not a `const fn`");
                    TyKind::Error
                }
            },
            T::Unit => TyKind::Unit,
            T::Tuple(tys) => TyKind::Tuple(tys.iter().map(|t| self.lower_ty(t)).collect()),
            T::Array { elem, len } => TyKind::Array { elem: boxed(self, elem), len: Box::new(self.lower_expr(len)) },
            T::Slice(t) => TyKind::Slice(boxed(self, t)),
            T::Optional(t) => TyKind::Optional(boxed(self, t)),
            T::Pointer(t) => TyKind::Pointer(boxed(self, t)),
            T::RawPointer(t) => TyKind::RawPointer(boxed(self, t)),
            T::Region { ty, region } => {
                let ty = boxed(self, ty);
                TyKind::Region { ty, region: self.lower_region(region) }
            }
            T::Result { ok, err } => TyKind::Result { ok: boxed(self, ok), err: boxed(self, err) },
            T::Union(tys) => TyKind::Union(tys.iter().map(|t| self.lower_ty(t)).collect()),
            T::Fn { params, ret } => {
                TyKind::Fn { params: params.iter().map(|t| self.lower_ty(t)).collect(), ret: boxed(self, ret) }
            }
            T::Any(tys) => TyKind::Any(tys.iter().map(|t| self.lower_ty(t)).collect()),
            T::Infer => TyKind::Infer,
            T::Error => TyKind::Error,
        };
        Ty { kind, span }
    }

    fn lower_ty_path(&mut self, segments: &[Ident], args: &[ast::GenericArg], span: Span) -> TyKind {
        let resolved = self.resolve_path(segments);
        let mut lowered_args = self.lower_generic_args(args);
        match resolved {
            PathRes::Type(Ty { kind: TyKind::Def { def, .. }, .. }) => {
                TyKind::Def { def, args: std::mem::take(&mut lowered_args) }
            }
            PathRes::Type(ty) => ty.kind,
            PathRes::Assoc(base, name) => TyKind::Assoc { base: Box::new(base), name },
            PathRes::Local(local) => TyKind::Local(local),
            PathRes::Error => TyKind::Error,
            PathRes::Value(def) => {
                let what = self.p.def(def).kind.describe();
                let name = segments.last().map(|s| s.name.as_str()).unwrap_or_default();
                self.error(codes::N0111, span, format!("`{name}` is a {what}, not a type"), "expected a type");
                TyKind::Error
            }
            PathRes::Module(_) => {
                let name = segments.last().map(|s| s.name.as_str()).unwrap_or_default();
                self.error(codes::N0111, span, format!("`{name}` is a module, not a type"), "expected a type");
                TyKind::Error
            }
            PathRes::Intrinsic => {
                let name = segments.last().map(|s| s.name.as_str()).unwrap_or_default();
                self.error(codes::N0111, span, format!("`{name}` is a value, not a type"), "expected a type");
                TyKind::Error
            }
        }
    }

    pub(super) fn lower_generic_args(&mut self, args: &[ast::GenericArg]) -> Vec<GenericArg> {
        args.iter()
            .map(|a| match a {
                // `Matrix[C, R]`: a name the parser took for a type may be a
                // `const` parameter, an argument that's a value.
                ast::GenericArg::Type(t) if self.names_const_param(t) => {
                    let ast::TyKind::Path { segments, .. } = &t.kind else { unreachable!() };
                    let name = ast::Expr { kind: ast::ExprKind::Name(segments[0].clone()), span: t.span };
                    GenericArg::Expr(self.lower_expr(&name))
                }
                ast::GenericArg::Type(t) => GenericArg::Type(self.lower_ty(t)),
                ast::GenericArg::Expr(e) => GenericArg::Expr(self.lower_expr(e)),
            })
            .collect()
    }

    /// Whether `t` is just the name of a `const` parameter: a generic one
    /// (`const N: usize`) or a function's (`const n: usize`).
    fn names_const_param(&mut self, t: &ast::Ty) -> bool {
        let ast::TyKind::Path { segments, args } = &t.kind else { return false };
        if segments.len() != 1 || !args.is_empty() {
            return false;
        }
        match self.resolve_path(segments) {
            PathRes::Value(def) => matches!(
                &self.p.def(def).kind,
                DefKind::GenericParam(g) if matches!(g.kind, crate::hir::GenericParamKind::Const { .. })
            ),
            PathRes::Local(_) => true,
            _ => false,
        }
    }

    pub(super) fn self_type_error(&mut self, span: Span) {
        self.error(
            codes::N0107,
            span,
            "`Self` only means something inside a warp, weft or weave",
            "`Self` outside a type",
        );
    }

    fn lower_region(&mut self, name: &Ident) -> Region {
        match name.name.as_str() {
            "heap" => return Region::Heap,
            "static" => return Region::Static,
            _ => {}
        }
        match self.lookup(&name.name) {
            Some(Res::Local(local)) => Region::Local(local),
            Some(Res::Def(def))
                if matches!(
                    self.p.def(def).kind,
                    DefKind::GenericParam(GenericParamDef { kind: GenericParamKind::Region })
                ) =>
            {
                Region::Param(def)
            }
            _ => {
                self.report(
                    loom_syntax::Diagnostic::error(
                        codes::N0113,
                        format!("there's no spool or region `{}` here", name.name),
                    )
                    .primary(name.span, "unknown region")
                    .help("a region is a `spool`, a `given` spool, a region parameter `@r`, `heap` or `static`"),
                );
                Region::Error
            }
        }
    }
}

/// Whether `attrs` has `#builtin`.
/// The attributes Loom reads. Any other `#name` is a misspelling.
const KNOWN_ATTRS: &[&str] = &["builtin", "disjoint", "given_default", "region", "spool_put", "spool_reset"];

/// The attributes the design documents that Loom doesn't read yet. Ignoring
/// one would miscompile (`#layout`), so using one is an error.
const PLANNED_ATTRS: &[&str] = &["layout", "inline", "deprecated", "test"];

fn is_builtin(attrs: &[ast::Attr]) -> bool {
    attrs.iter().any(|a| a.name.name == "builtin")
}
