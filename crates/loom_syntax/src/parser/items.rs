//! Items (top-level declarations) and the members of warps, wefts, weaves
//! and extern blocks.

use super::{NameCase, Parser};
use crate::ast::{
    Attr, AttrArg, CaseDecl, CaseField, ConstDecl, ExternBlock, FieldDecl, FnBody, FnDecl, FnQualifier, FnRet,
    GenericParam, GenericParamKind, GivenParam, Item, ItemKind, Member, MemberKind, Module, Param, ParamMode,
    ParamName, PlaceMode, Ty, TyKind, TypeDecl, UseKind, UseTree, Vis, WarpBody, WarpDecl, WarpModifier, WeaveDecl,
    WeftDecl, WherePred, YieldSpec,
};
use crate::diag::{Diagnostic, codes};
use crate::token::{Keyword, Punct, TokenKind};

/// Where a member list appears; decides which members are allowed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberCtx {
    Warp,
    Weft,
    Weave,
    Extern,
}

impl MemberCtx {
    fn expected(self) -> &'static str {
        match self {
            MemberCtx::Warp => "a field, `case`, `through`, `fn`, `const` or `type`",
            MemberCtx::Weft => "`fn` or `type`",
            MemberCtx::Weave => "`fn` or `type`",
            MemberCtx::Extern => "an `fn` declaration",
        }
    }
}

impl Parser<'_> {
    pub(super) fn module(&mut self) -> Module {
        let mut items = Vec::new();
        loop {
            while self.eat_punct(Punct::Semi).is_some() {}
            if self.at_eof() {
                break;
            }
            let before = self.pos;
            let saved = self.stmt_indent;
            self.stmt_indent = self.peek().line_indent;
            match self.item() {
                Some(item) => {
                    items.push(item);
                    self.stmt_terminator();
                }
                None => self.recover_to_item(),
            }
            self.stmt_indent = saved;
            if self.pos == before {
                self.bump();
            }
        }
        Module { items, span: self.file_span() }
    }

    /// Skips to the next line that starts an item.
    fn recover_to_item(&mut self) {
        if !self.at_eof() {
            self.bump();
        }
        while !self.at_eof() && !(self.peek().nl_before && self.at_item_start()) {
            self.bump();
        }
    }

    /// Whether the current token starts an item (so a statement position
    /// holds a nested declaration).
    pub(super) fn at_item_start(&self) -> bool {
        match self.kind() {
            TokenKind::Keyword(
                Keyword::Fn
                | Keyword::Use
                | Keyword::Warp
                | Keyword::Weft
                | Keyword::Weave
                | Keyword::Extern
                | Keyword::Pub,
            )
            | TokenKind::Punct(Punct::Hash) => true,
            TokenKind::Keyword(Keyword::Type) => {
                matches!(self.nth_kind(1), TokenKind::TypeIdent | TokenKind::Ident | TokenKind::ConstIdent)
            }
            TokenKind::Keyword(Keyword::Ref) => self.nth_is_kw(1, Keyword::Warp),
            TokenKind::Keyword(Keyword::Unsafe) => self.nth_is_kw(1, Keyword::Fn),
            TokenKind::Keyword(Keyword::Const) => self.nth_is_kw(1, Keyword::Fn) || self.at_const_decl(),
            TokenKind::Ident => self.at_contextual("once") && self.nth_is_kw(1, Keyword::Warp),
            _ => false,
        }
    }

    /// `const NAME =` or `const NAME:` (as opposed to compile-time code).
    fn at_const_decl(&self) -> bool {
        matches!(self.nth_kind(1), TokenKind::ConstIdent | TokenKind::TypeIdent | TokenKind::Ident)
            && (self.nth_is_punct(2, Punct::Eq) || self.nth_is_punct(2, Punct::Colon))
    }

    pub(super) fn item(&mut self) -> Option<Item> {
        let docs = self.take_docs();
        let start = self.peek().span;
        let attrs = self.attrs();
        let vis = self.vis(false);
        let kind = self.item_kind()?;
        Some(Item { docs, attrs, vis, kind, span: self.span_from(start) })
    }

    fn item_kind(&mut self) -> Option<ItemKind> {
        Some(match self.kind() {
            TokenKind::Keyword(Keyword::Use) => {
                self.bump();
                ItemKind::Use(self.use_tree())
            }
            TokenKind::Keyword(Keyword::Const) if self.nth_is_kw(1, Keyword::Fn) => {
                ItemKind::Fn(self.fn_decl(NameCase::Value))
            }
            TokenKind::Keyword(Keyword::Const) if self.at_const_decl() => ItemKind::Const(self.const_decl()),
            TokenKind::Keyword(Keyword::Const) => ItemKind::ConstExpr(self.expr()),
            TokenKind::Keyword(Keyword::Unsafe) if self.nth_is_kw(1, Keyword::Fn) => {
                ItemKind::Fn(self.fn_decl(NameCase::Value))
            }
            TokenKind::Keyword(Keyword::Fn) => ItemKind::Fn(self.fn_decl(NameCase::Value)),
            TokenKind::Keyword(Keyword::Type) => ItemKind::Type(self.type_decl()),
            TokenKind::Keyword(Keyword::Warp) => ItemKind::Warp(self.warp(WarpModifier::None)),
            TokenKind::Keyword(Keyword::Ref) if self.nth_is_kw(1, Keyword::Warp) => {
                self.bump();
                ItemKind::Warp(self.warp(WarpModifier::Ref))
            }
            TokenKind::Ident if self.at_contextual("once") && self.nth_is_kw(1, Keyword::Warp) => {
                self.bump();
                ItemKind::Warp(self.warp(WarpModifier::Once))
            }
            TokenKind::Keyword(Keyword::Weft) => ItemKind::Weft(self.weft()),
            TokenKind::Keyword(Keyword::Weave) => ItemKind::Weave(self.weave()),
            TokenKind::Keyword(Keyword::Extern) => ItemKind::Extern(self.extern_block()),
            _ => {
                self.error_expected("an item (`fn`, `warp`, `weft`, `weave`, `use`, `const` or `type`)");
                return None;
            }
        })
    }

    /// `#name` / `#name(args)`, possibly several.
    fn attrs(&mut self) -> Vec<Attr> {
        let mut attrs = Vec::new();
        while let Some(start) = self.eat_punct(Punct::Hash) {
            let name = self.ident(NameCase::Value, "attribute name");
            let args = if self.at_punct(Punct::LParen) && !self.at_line_start() {
                let open = self.bump().span;
                let args = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::attr_arg));
                self.expect_close(Punct::RParen, open);
                args
            } else {
                Vec::new()
            };
            attrs.push(Attr { name, args, span: self.span_from(start) });
        }
        attrs
    }

    /// `value` or `key = value`.
    fn attr_arg(&mut self) -> AttrArg {
        let key = (matches!(self.kind(), TokenKind::Ident) && self.nth_is_punct(1, Punct::Eq)).then(|| {
            let key = self.ident(NameCase::Value, "attribute argument");
            self.bump();
            key
        });
        AttrArg { key, value: self.expr() }
    }

    /// `pub`, `pub(get)`, or nothing.
    fn vis(&mut self, allow_pub_get: bool) -> Vis {
        let Some(start) = self.eat_kw(Keyword::Pub) else {
            return Vis::Private;
        };
        let pub_get = self.at_punct(Punct::LParen)
            && !self.at_line_start()
            && self.nth_is_contextual(1, "get")
            && self.nth_is_punct(2, Punct::RParen);
        if !pub_get {
            return Vis::Pub;
        }
        self.bump();
        self.bump();
        self.bump();
        if !allow_pub_get {
            let span = self.span_from(start);
            self.error_at(codes::S0209, span, "`pub(get)` only applies to fields", "not a field");
            return Vis::Pub;
        }
        Vis::PubGet
    }

    // ------------------------------------------------------------ use / const / type

    fn use_tree(&mut self) -> UseTree {
        self.nested(|span| UseTree { path: Vec::new(), kind: UseKind::Group(Vec::new()), span }, Self::use_tree_inner)
    }

    fn use_tree_inner(&mut self) -> UseTree {
        let start = self.peek().span;
        let mut path = vec![self.ident(NameCase::Any, "module or item name")];
        loop {
            if !self.at_punct(Punct::Dot) || self.at_line_start() {
                break;
            }
            if self.nth_is_punct(1, Punct::LBrace) {
                self.bump();
                let open = self.bump().span;
                let group = self.with_nl(false, |p| p.comma_list(Punct::RBrace, Self::use_tree));
                self.expect_close(Punct::RBrace, open);
                return UseTree { path, kind: UseKind::Group(group), span: self.span_from(start) };
            }
            self.bump();
            path.push(self.ident(NameCase::Any, "module or item name"));
        }
        let alias = self.eat_kw(Keyword::As).map(|_| self.ident(NameCase::Any, "alias"));
        UseTree { path, kind: UseKind::Single { alias }, span: self.span_from(start) }
    }

    fn const_decl(&mut self) -> ConstDecl {
        self.bump();
        let name = self.ident(NameCase::Const, "constant");
        let ty = self.eat_punct(Punct::Colon).map(|_| self.ty());
        self.expect_punct(Punct::Eq);
        let value = self.expr();
        ConstDecl { name, ty, value }
    }

    /// `type Name[G] = Ty where gauge`, or an associated type `type Name: Bounds`.
    fn type_decl(&mut self) -> TypeDecl {
        self.bump();
        let name = self.ident(NameCase::Type, "type");
        let generics = self.generic_params();
        let bounds = if self.eat_punct(Punct::Colon).is_some() { self.bounds() } else { Vec::new() };
        let value = self.eat_punct(Punct::Eq).map(|_| self.ty());
        let gauge = if value.is_some() && self.at_contextual("where") && !self.at_line_start() {
            self.bump();
            Some(self.expr())
        } else {
            None
        };
        TypeDecl { name, generics, bounds, value, gauge }
    }

    // ------------------------------------------------------------ warps, wefts, weaves

    fn warp(&mut self, modifier: WarpModifier) -> WarpDecl {
        self.bump();
        let name = self.ident(NameCase::Type, "type");
        let generics = self.generic_params();
        let mut wefts = Vec::new();
        if self.eat_punct(Punct::Colon).is_some() {
            wefts.push(self.ty_cast());
            while self.eat_punct(Punct::Comma).is_some() {
                wefts.push(self.ty_cast());
            }
        }
        let where_clause = self.where_clause();
        let body = if self.at_punct(Punct::LParen) {
            let open = self.bump().span;
            let fields = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::case_field));
            self.expect_close(Punct::RParen, open);
            WarpBody::Tuple(fields)
        } else {
            WarpBody::Members(self.members(MemberCtx::Warp))
        };
        WarpDecl { modifier, name, generics, wefts, where_clause, body }
    }

    fn weft(&mut self) -> WeftDecl {
        self.bump();
        let name = self.ident(NameCase::Type, "weft");
        let generics = self.generic_params();
        let mut supers = Vec::new();
        if self.eat_punct(Punct::Colon).is_some() {
            supers.push(self.ty_cast());
            while self.eat_punct(Punct::Plus).is_some() || self.eat_punct(Punct::Comma).is_some() {
                supers.push(self.ty_cast());
            }
        }
        let members = self.members(MemberCtx::Weft);
        WeftDecl { name, generics, supers, members }
    }

    fn weave(&mut self) -> WeaveDecl {
        self.bump();
        let generics = self.generic_params();
        let weft = self.ty_cast();
        self.expect_kw(Keyword::Into);
        let target = self.ty_cast();
        let where_clause = self.where_clause();
        let members = self.members(MemberCtx::Weave);
        WeaveDecl { generics, weft, target, where_clause, members }
    }

    fn extern_block(&mut self) -> ExternBlock {
        self.bump();
        let abi = match self.kind().clone() {
            TokenKind::Str(abi) => {
                self.bump();
                abi
            }
            _ => {
                self.error_expected("an ABI string, like `\"C\"` or `\"lua\"`");
                String::new()
            }
        };
        let members = self.members(MemberCtx::Extern);
        ExternBlock { abi, members }
    }

    fn members(&mut self, ctx: MemberCtx) -> Vec<Member> {
        let open = self.expect_punct(Punct::LBrace);
        let outer_indent = self.stmt_indent;
        let mut closed = true;
        let members = self.with_nl(true, |p| {
            let mut members = Vec::new();
            loop {
                while p.eat_punct(Punct::Comma).is_some() || p.eat_punct(Punct::Semi).is_some() {}
                if p.at_punct(Punct::RBrace) || p.at_eof() {
                    break;
                }
                // `warp`, `use`, `let`, … no deeper than the declaration
                // itself: its `}` is missing, and the next item starts here.
                if p.at_line_start() && p.peek().line_indent <= outer_indent && p.at_stmt_keyword() {
                    p.report_unclosed(open, Punct::RBrace);
                    closed = false;
                    break;
                }
                let before = p.pos;
                let saved = p.stmt_indent;
                p.stmt_indent = p.peek().line_indent;
                match p.member(ctx) {
                    Some(member) => {
                        members.push(member);
                        if p.eat_punct(Punct::Comma).is_none() {
                            p.stmt_terminator();
                        }
                    }
                    None => p.recover_to_stmt_end(),
                }
                p.stmt_indent = saved;
                if p.pos == before {
                    p.skip_bad_stmt();
                }
            }
            members
        });
        if closed {
            self.expect_close(Punct::RBrace, open);
        }
        members
    }

    fn member(&mut self, ctx: MemberCtx) -> Option<Member> {
        let docs = self.take_docs();
        let start = self.peek().span;
        let attrs = self.attrs();
        let vis = self.vis(ctx == MemberCtx::Warp);
        let is_fn = self.at_kw(Keyword::Fn)
            || (matches!(self.kind(), TokenKind::Keyword(Keyword::Const | Keyword::Unsafe))
                && self.nth_is_kw(1, Keyword::Fn));
        let field_like = matches!(self.kind(), TokenKind::Ident | TokenKind::TypeIdent | TokenKind::ConstIdent)
            && self.nth_is_punct(1, Punct::Colon);
        let kind = match self.kind() {
            _ if is_fn => {
                let case = if ctx == MemberCtx::Extern { NameCase::Any } else { NameCase::Value };
                MemberKind::Fn(self.fn_decl(case))
            }
            TokenKind::Keyword(Keyword::Case) if ctx == MemberCtx::Warp => {
                self.bump();
                let mut cases = vec![self.case_decl()];
                while self.eat_punct(Punct::Comma).is_some() {
                    cases.push(self.case_decl());
                }
                MemberKind::Cases(cases)
            }
            TokenKind::Keyword(Keyword::Through) if ctx == MemberCtx::Warp => {
                self.bump();
                let name = self.ident(NameCase::Value, "field");
                self.expect_punct(Punct::Colon);
                let ty = self.ty();
                MemberKind::Through(FieldDecl { name, ty, default: None })
            }
            TokenKind::Keyword(Keyword::Const) if ctx == MemberCtx::Warp && self.at_const_decl() => {
                MemberKind::Const(self.const_decl())
            }
            TokenKind::Keyword(Keyword::Const) if ctx == MemberCtx::Warp => MemberKind::ConstExpr(self.expr()),
            TokenKind::Keyword(Keyword::Type) if ctx != MemberCtx::Extern => MemberKind::Type(self.type_decl()),
            _ if field_like && ctx == MemberCtx::Warp => {
                let name = self.ident(NameCase::Value, "field");
                self.bump();
                let ty = self.ty();
                let default = self.eat_punct(Punct::Eq).map(|_| self.expr());
                MemberKind::Field(FieldDecl { name, ty, default })
            }
            _ => {
                self.error_expected(ctx.expected());
                return None;
            }
        };
        if vis == Vis::PubGet && !matches!(kind, MemberKind::Field(_)) {
            let span = self.span_from(start);
            self.error_at(codes::S0209, span, "`pub(get)` only applies to fields", "not a field");
        }
        Some(Member { docs, attrs, vis, kind, span: self.span_from(start) })
    }

    fn case_decl(&mut self) -> CaseDecl {
        let start = self.peek().span;
        let name = self.ident(NameCase::Type, "case");
        let fields = if self.at_punct(Punct::LParen) && !self.at_line_start() {
            let open = self.bump().span;
            let fields = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::case_field));
            self.expect_close(Punct::RParen, open);
            Some(fields)
        } else {
            None
        };
        CaseDecl { name, fields, span: self.span_from(start) }
    }

    /// `name: Ty`, `Ty`, or `from Ty`.
    fn case_field(&mut self) -> CaseField {
        let start = self.peek().span;
        if matches!(self.kind(), TokenKind::Ident) && self.nth_is_punct(1, Punct::Colon) {
            let name = self.ident(NameCase::Value, "field");
            self.bump();
            let ty = self.ty();
            return CaseField { name: Some(name), absorbs: false, ty, span: self.span_from(start) };
        }
        let absorbs = self.at_contextual("from")
            && !matches!(self.nth_kind(1), TokenKind::Punct(Punct::Comma | Punct::RParen | Punct::Colon));
        if absorbs {
            self.bump();
        }
        let ty = self.ty();
        CaseField { name: None, absorbs, ty, span: self.span_from(start) }
    }

    // ------------------------------------------------------------ functions

    fn fn_decl(&mut self, name_case: NameCase) -> FnDecl {
        let qualifier = if self.eat_kw(Keyword::Const).is_some() {
            FnQualifier::Const
        } else if self.eat_kw(Keyword::Unsafe).is_some() {
            FnQualifier::Unsafe
        } else {
            FnQualifier::None
        };
        self.expect_kw(Keyword::Fn);
        let name = self.ident(name_case, "function");
        let generics = self.generic_params();
        let params = self.params();
        let ret = self.fn_ret();
        let mut givens = Vec::new();
        if self.at_kw(Keyword::Given) && !self.at_line_start() {
            self.bump();
            loop {
                let name = self.ident(NameCase::Value, "given name");
                self.expect_punct(Punct::Colon);
                let ty = self.ty();
                givens.push(GivenParam { name, ty });
                if self.eat_punct(Punct::Comma).is_none() {
                    break;
                }
            }
        }
        let where_clause = self.where_clause();
        let body = if self.at_punct(Punct::LBrace) {
            Some(FnBody::Block(self.block()))
        } else if self.eat_punct(Punct::Eq).is_some() {
            Some(FnBody::Expr(self.expr()))
        } else {
            None
        };
        FnDecl { qualifier, name, generics, params, ret, givens, where_clause, body }
    }

    fn params(&mut self) -> Vec<Param> {
        let open = self.expect_punct(Punct::LParen);
        let params = self.with_nl(false, |p| p.comma_list(Punct::RParen, Self::param));
        self.expect_close(Punct::RParen, open);
        params
    }

    fn param(&mut self) -> Param {
        let start = self.peek().span;
        let names_follows =
            |p: &Self| matches!(p.nth_kind(1), TokenKind::Ident | TokenKind::Keyword(Keyword::SelfValue));
        let mut mode = if self.at_kw(Keyword::Mut) && names_follows(self) {
            self.bump();
            ParamMode::Mut
        } else if self.at_contextual("take") && names_follows(self) {
            self.bump();
            ParamMode::Take
        } else if self.at_kw(Keyword::Const) {
            self.bump();
            ParamMode::Const
        } else {
            ParamMode::Read
        };
        if self.eat_kw(Keyword::SelfValue).is_some() {
            return Param { mode, name: ParamName::SelfValue, ty: None, default: None, span: self.span_from(start) };
        }
        let case = if mode == ParamMode::Const { NameCase::Any } else { NameCase::Value };
        let name = self.ident(case, "parameter");
        self.expect_punct(Punct::Colon);

        // `c: mut Canvas` is a common slip for `mut c: Canvas`.
        let misplaced = if self.at_kw(Keyword::Mut) {
            Some(ParamMode::Mut)
        } else if self.at_contextual("take") && !matches!(self.nth_kind(1), TokenKind::Punct(_)) {
            Some(ParamMode::Take)
        } else {
            None
        };
        if let Some(actual) = misplaced {
            let word = self.bump();
            let keyword = self.text(word.span);
            let fixed = format!("{keyword} {}: …", name.name);
            self.report(
                Diagnostic::error(codes::S0208, format!("`{keyword}` goes before the parameter name"))
                    .primary(word.span, "parameter modes are written first")
                    .help(format!("write `{fixed}`")),
            );
            mode = actual;
        }

        let ty = self.ty();
        let default = self.eat_punct(Punct::Eq).map(|_| self.expr());
        Param { mode, name: ParamName::Named(name), ty: Some(ty), default, span: self.span_from(start) }
    }

    fn fn_ret(&mut self) -> FnRet {
        if self.at_line_start() {
            return FnRet::Unit;
        }
        match self.kind() {
            TokenKind::Punct(Punct::Arrow) => {
                self.bump();
                FnRet::Value(self.ty())
            }
            TokenKind::Punct(Punct::Bang) => {
                let bang = self.bump().span;
                let err = self.err_ty();
                let unit = Ty { kind: TyKind::Unit, span: bang };
                let span = bang.to(err.span);
                FnRet::Value(Ty { kind: TyKind::Result { ok: Box::new(unit), err: Box::new(err) }, span })
            }
            TokenKind::Punct(Punct::YieldArrow) => {
                self.bump();
                let place = if self.eat_kw(Keyword::Mut).is_some() {
                    Some(PlaceMode::Mut)
                } else if self.eat_kw(Keyword::Ref).is_some() {
                    Some(PlaceMode::Ref)
                } else {
                    None
                };
                let ty = self.ty();
                let pulls = self.eat_punct(Punct::PullArrow).map(|_| self.ty());
                FnRet::Shuttle { yields: Some(YieldSpec { place, ty }), pulls }
            }
            TokenKind::Punct(Punct::PullArrow) => {
                self.bump();
                FnRet::Shuttle { yields: None, pulls: Some(self.ty()) }
            }
            _ => FnRet::Unit,
        }
    }

    /// `[T: Show, const N: usize, @r]`, if present.
    fn generic_params(&mut self) -> Vec<GenericParam> {
        if !self.at_punct(Punct::LBracket) || self.at_line_start() {
            return Vec::new();
        }
        let open = self.bump().span;
        let params = self.with_nl(false, |p| {
            p.comma_list(Punct::RBracket, |p| {
                let start = p.peek().span;
                let kind = if p.eat_kw(Keyword::Const).is_some() {
                    let name = p.ident(NameCase::Any, "const parameter");
                    p.expect_punct(Punct::Colon);
                    GenericParamKind::Const { name, ty: p.ty() }
                } else if p.eat_punct(Punct::At).is_some() {
                    GenericParamKind::Region { name: p.ident(NameCase::Value, "region") }
                } else {
                    let name = p.ident(NameCase::Type, "type parameter");
                    let bounds = if p.eat_punct(Punct::Colon).is_some() { p.bounds() } else { Vec::new() };
                    GenericParamKind::Type { name, bounds }
                };
                GenericParam { kind, span: p.span_from(start) }
            })
        });
        self.expect_close(Punct::RBracket, open);
        params
    }

    /// `where T: A + B, U: C`, if present.
    fn where_clause(&mut self) -> Vec<WherePred> {
        if !self.at_contextual("where") || self.at_line_start() {
            return Vec::new();
        }
        self.bump();
        let mut preds = Vec::new();
        loop {
            let start = self.peek().span;
            let ty = self.ty_cast();
            self.expect_punct(Punct::Colon);
            let bounds = self.bounds();
            preds.push(WherePred { ty, bounds, span: self.span_from(start) });
            if self.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        preds
    }
}
