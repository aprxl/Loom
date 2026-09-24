//! A compact S-expression rendering of the AST, for `loom build --emit ast`
//! and for parser tests. Precedence and grouping are explicit:
//! `a + b * c` prints as `(+ a (* b c))`.

use std::fmt::Write as _;

use crate::ast::{
    Arg, AssignOp, Attr, Block, CasePath, ExprKind, FieldName, FnBody, FnDecl, FnQualifier, FnRet, GenericArg,
    GenericParamKind, Item, ItemKind, Lit, Member, MemberKind, Module, Param, ParamMode, ParamName, Pat, PatKind,
    PlaceMode, Stmt, StmtKind, TemplatePart, Ty, TyKind, UnaryOp, UseKind, UseTree, Vis, WarpBody, WarpModifier,
    WherePred,
};
use crate::ast::{CaseField, Expr, GenericParam};

// ================================================================ expressions

pub fn expr(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Lit(lit) => lit_str(lit),
        ExprKind::Template(parts) => {
            let mut s = String::from("(template");
            for part in parts {
                match part {
                    TemplatePart::Text(text, _) => {
                        let _ = write!(s, " {text:?}");
                    }
                    TemplatePart::Interp { expr: e, spec: None } => {
                        let _ = write!(s, " {{{}}}", expr(e));
                    }
                    TemplatePart::Interp { expr: e, spec: Some(spec) } => {
                        let _ = write!(s, " {{{}:{spec}}}", expr(e));
                    }
                }
            }
            s + ")"
        }
        ExprKind::Name(name) => name.name.clone(),
        ExprKind::Prim(p) => p.to_string(),
        ExprKind::BuiltinCase(k) => k.to_string(),
        ExprKind::SelfValue => "self".into(),
        ExprKind::SelfType => "Self".into(),
        ExprKind::LeadingDot(FieldName::Named(name)) => format!(".{}", name.name),
        ExprKind::LeadingDot(FieldName::Index(i, _)) => format!(".{i}"),
        ExprKind::Hole => "_".into(),
        ExprKind::Tuple(elems) if elems.is_empty() => "()".into(),
        ExprKind::Tuple(elems) => list("tuple", elems.iter().map(expr)),
        ExprKind::Array(elems) => list("array", elems.iter().map(expr)),
        ExprKind::ArrayRepeat { value, count } => format!("(array-repeat {} {})", expr(value), expr(count)),
        ExprKind::Map(pairs) => list("map", pairs.iter().map(|(k, v)| format!("({} {})", expr(k), expr(v)))),
        ExprKind::Block(b) => block(b),
        ExprKind::If { cond, then, else_, is_const } => {
            let head = if *is_const { "const-if" } else { "if" };
            match else_ {
                Some(e) => format!("({head} {} {} {})", expr(cond), block(then), expr(e)),
                None => format!("({head} {} {})", expr(cond), block(then)),
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            let arms = arms.iter().map(|arm| match &arm.guard {
                Some(g) => format!("(=> {} (if {}) {})", pat(&arm.pat), expr(g), expr(&arm.body)),
                None => format!("(=> {} {})", pat(&arm.pat), expr(&arm.body)),
            });
            let mut s = format!("(match {}", expr(scrutinee));
            for arm in arms {
                s.push(' ');
                s.push_str(&arm);
            }
            s + ")"
        }
        ExprKind::For { label, pat: p, iter, body, is_const, is_loom } => {
            let head = if *is_loom {
                "loom-for"
            } else if *is_const {
                "const-for"
            } else {
                "for"
            };
            format!("({head}{} {} {} {})", label_str(label), pat(p), expr(iter), block(body))
        }
        ExprKind::While { label, cond, body } => format!("(while{} {} {})", label_str(label), expr(cond), block(body)),
        ExprKind::Loop { label, body } => format!("(loop{} {})", label_str(label), block(body)),
        ExprKind::Loom { args, body } => {
            let mut s = String::from("(loom");
            for a in args {
                s.push(' ');
                s.push_str(&arg(a));
            }
            format!("{s} {})", block(body))
        }
        ExprKind::Thread(e) => format!("(thread {})", expr(e)),
        ExprKind::Break { label, value } => {
            let value = value.as_ref().map(|v| format!(" {}", expr(v))).unwrap_or_default();
            format!("(break{}{value})", label_str(label))
        }
        ExprKind::Continue { label } => format!("(continue{})", label_str(label)),
        ExprKind::Return(Some(v)) => format!("(return {})", expr(v)),
        ExprKind::Return(None) => "(return)".into(),
        ExprKind::Fail(e) => format!("(fail {})", expr(e)),
        ExprKind::Yield { place, value } => match place {
            Some(PlaceMode::Mut) => format!("(yield mut {})", expr(value)),
            Some(PlaceMode::Ref) => format!("(yield ref {})", expr(value)),
            None => format!("(yield {})", expr(value)),
        },
        ExprKind::Pull => "pull".into(),
        ExprKind::Unsafe { reason, body } => format!("(unsafe {reason:?} {})", block(body)),
        ExprKind::Const(e) => format!("(const {})", expr(e)),
        ExprKind::Lambda { params, body } => {
            let params: Vec<String> = params
                .iter()
                .map(|p| {
                    let name = if p.take { format!("take {}", p.name.name) } else { p.name.name.clone() };
                    match &p.ty {
                        Some(t) => format!("{name}: {}", ty(t)),
                        None => name,
                    }
                })
                .collect();
            format!("(fn ({}) {})", params.join(", "), expr(body))
        }
        ExprKind::Call { callee, args } => {
            let mut s = format!("(call {}", expr(callee));
            for a in args {
                s.push(' ');
                s.push_str(&arg(a));
            }
            s + ")"
        }
        ExprKind::Index { base, args } => {
            let mut s = format!("(index {}", expr(base));
            for a in args {
                s.push(' ');
                s.push_str(&generic_arg(a));
            }
            s + ")"
        }
        ExprKind::Field { base, field: FieldName::Named(name) } => format!("(. {} {})", expr(base), name.name),
        ExprKind::Field { base, field: FieldName::Index(i, _) } => format!("(. {} {i})", expr(base)),
        ExprKind::ConstField { base, field } => format!("(.() {} {})", expr(base), expr(field)),
        ExprKind::Try(e) => format!("(? {})", expr(e)),
        ExprKind::Deref(e) => format!("(deref {})", expr(e)),
        ExprKind::DotBlock { subject, body } => format!("(dot-block {} {})", expr(subject), block(body)),
        ExprKind::Unary { op, operand } => {
            let op = match op {
                UnaryOp::Neg => "-",
                UnaryOp::BitNot => "~",
                UnaryOp::Not => "not",
                UnaryOp::Alloc => "alloc",
            };
            format!("({op} {})", expr(operand))
        }
        ExprKind::Binary { op, lhs, rhs } => format!("({} {} {})", op.as_str(), expr(lhs), expr(rhs)),
        ExprKind::Chain { first, rest } => {
            let mut s = format!("(chain {}", expr(first));
            for (op, e) in rest {
                let _ = write!(s, " {} {}", op.as_str(), expr(e));
            }
            s + ")"
        }
        ExprKind::Is { expr: e, pat: p } => format!("(is {} {})", expr(e), pat(p)),
        ExprKind::Cast { expr: e, kind, ty: t } => format!("({} {} {})", kind.as_str(), expr(e), ty(t)),
        ExprKind::Range { lo, hi, inclusive } => {
            let op = if *inclusive { "..=" } else { ".." };
            let side = |e: &Option<Box<Expr>>| e.as_ref().map_or("_".to_string(), |e| expr(e));
            format!("({op} {} {})", side(lo), side(hi))
        }
        ExprKind::Mend { expr: e, binding: Some(b), handler } => {
            format!("(mend {} ({} => {}))", expr(e), b.name, expr(handler))
        }
        ExprKind::Mend { expr: e, binding: None, handler } => format!("(mend {} {})", expr(e), expr(handler)),
        ExprKind::Assign { target, op, value } => {
            let op = match op {
                AssignOp::Assign => "=".to_string(),
                AssignOp::Compound(op) => format!("{}=", op.as_str()),
            };
            format!("({op} {} {})", expr(target), expr(value))
        }
        ExprKind::Error => "<error>".into(),
    }
}

fn list(head: &str, items: impl Iterator<Item = String>) -> String {
    let mut s = format!("({head}");
    for item in items {
        s.push(' ');
        s.push_str(&item);
    }
    s + ")"
}

fn label_str(label: &Option<crate::ast::Ident>) -> String {
    label.as_ref().map(|l| format!(" '{}", l.name)).unwrap_or_default()
}

fn arg(a: &Arg) -> String {
    let value = expr(&a.value);
    let value = if a.mutable { format!("(mut {value})") } else { value };
    if a.is_given {
        format!("(given: {value})")
    } else if let Some(label) = &a.label {
        format!("({}: {value})", label.name)
    } else {
        value
    }
}

fn generic_arg(a: &GenericArg) -> String {
    match a {
        GenericArg::Type(t) => format!("(type {})", ty(t)),
        GenericArg::Expr(e) => expr(e),
    }
}

/// A literal as source-like text: `42u8`, `"hi"`, `b"\xff"`.
pub fn lit_str(lit: &Lit) -> String {
    match lit {
        Lit::Int(i) => match i.suffix {
            Some(s) => format!("{}{s}", i.value),
            None => i.value.to_string(),
        },
        Lit::Float(f) => match f.suffix {
            Some(s) => format!("{:?}{s}", f.value),
            None => format!("{:?}", f.value),
        },
        Lit::Char(c) => format!("{c:?}"),
        Lit::Str(s) => format!("{s:?}"),
        Lit::Bytes(b) => {
            let escaped: String = b.iter().flat_map(|&c| std::ascii::escape_default(c)).map(char::from).collect();
            format!("b\"{escaped}\"")
        }
        Lit::Bool(b) => b.to_string(),
        Lit::None => "none".into(),
    }
}

// ================================================================ statements

pub fn block(b: &Block) -> String {
    list("block", b.stmts.iter().map(stmt))
}

pub fn stmt(s: &Stmt) -> String {
    match &s.kind {
        StmtKind::Let { mutable, pat: p, ty: t, init } => {
            let head = if *mutable { "var" } else { "let" };
            match t {
                Some(t) => format!("({head} {}: {} {})", pat(p), ty(t), expr(init)),
                None => format!("({head} {} {})", pat(p), expr(init)),
            }
        }
        StmtKind::Given { name: Some(n), value } => format!("(given {} {})", n.name, expr(value)),
        StmtKind::Given { name: None, value } => format!("(given {})", expr(value)),
        StmtKind::Spool { name, init } => format!("(spool {} {})", name.name, expr(init)),
        StmtKind::Defer { binding: Some(b), body } => format!("(defer {} => {})", b.name, expr(body)),
        StmtKind::Defer { binding: None, body } => format!("(defer {})", expr(body)),
        StmtKind::Item(item_) => item(item_).trim_end().to_string(),
        StmtKind::Expr(e) => expr(e),
    }
}

// ================================================================ types

pub fn ty(t: &Ty) -> String {
    match &t.kind {
        TyKind::Prim(p) => p.to_string(),
        TyKind::Path { segments, args } => {
            let path: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
            let mut s = path.join(".");
            if !args.is_empty() {
                let args: Vec<String> = args
                    .iter()
                    .map(|a| match a {
                        GenericArg::Type(t) => ty(t),
                        GenericArg::Expr(e) => expr(e),
                    })
                    .collect();
                let _ = write!(s, "[{}]", args.join(", "));
            }
            s
        }
        TyKind::SelfType => "Self".into(),
        TyKind::TypeType => "type".into(),
        TyKind::ConstCall { segments, args } => {
            let path: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
            let args: Vec<String> = args.iter().map(expr).collect();
            format!("{}({})", path.join("."), args.join(", "))
        }
        TyKind::Unit => "()".into(),
        TyKind::Tuple(elems) => format!("({})", elems.iter().map(ty).collect::<Vec<_>>().join(", ")),
        TyKind::Array { elem, len } => format!("[{}; {}]", ty(elem), expr(len)),
        TyKind::Slice(elem) => format!("[{}]", ty(elem)),
        TyKind::Optional(inner) => format!("?{}", ty(inner)),
        TyKind::Pointer(inner) => format!("^{}", ty(inner)),
        TyKind::RawPointer(inner) => format!("*{}", ty(inner)),
        TyKind::Region { ty: inner, region } => format!("{}@{}", ty(inner), region.name),
        TyKind::Result { ok, err } => format!("({} ! {})", ty(ok), ty(err)),
        TyKind::Union(members) => format!("({})", members.iter().map(ty).collect::<Vec<_>>().join(" | ")),
        TyKind::Fn { params, ret } => {
            format!("(({}) => {})", params.iter().map(ty).collect::<Vec<_>>().join(", "), ty(ret))
        }
        TyKind::Any(wefts) => format!("any {}", wefts.iter().map(ty).collect::<Vec<_>>().join(" + ")),
        TyKind::Infer => "_".into(),
        TyKind::Error => "<error>".into(),
    }
}

// ================================================================ patterns

pub fn pat(p: &Pat) -> String {
    match &p.kind {
        PatKind::Wild => "_".into(),
        PatKind::Bind { name, mutable: false } => name.name.clone(),
        PatKind::Bind { name, mutable: true } => format!("(mut {})", name.name),
        PatKind::Lit { lit, negative } => format!("{}{}", if *negative { "-" } else { "" }, lit_str(lit)),
        PatKind::Range { lo, hi, inclusive } => {
            let op = if *inclusive { "..=" } else { ".." };
            let side = |p: &Option<Box<Pat>>| p.as_ref().map_or("_".to_string(), |p| pat(p));
            format!("({op} {} {})", side(lo), side(hi))
        }
        PatKind::Tuple(elems) if elems.is_empty() => "()".into(),
        PatKind::Tuple(elems) => list("tuple", elems.iter().map(pat)),
        PatKind::Slice(elems) => format!("[{}]", elems.iter().map(pat).collect::<Vec<_>>().join(" ")),
        PatKind::Rest(Some(name)) => format!("..{}", name.name),
        PatKind::Rest(None) => "..".into(),
        PatKind::Case { path, fields, rest } => {
            let head = match path {
                CasePath::Dot(name) => format!(".{}", name.name),
                CasePath::Path(segments) => segments.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join("."),
            };
            let Some(fields) = fields else { return head };
            let mut s = format!("({head}");
            for f in fields {
                match &f.label {
                    Some(label) => {
                        let _ = write!(s, " ({}: {})", label.name, pat(&f.pat));
                    }
                    None => {
                        let _ = write!(s, " {}", pat(&f.pat));
                    }
                }
            }
            if *rest {
                s.push_str(" ..");
            }
            s + ")"
        }
        PatKind::ConstCase(e) => format!("(.() {})", expr(e)),
        PatKind::Some(inner) => format!("(some {})", pat(inner)),
        PatKind::None => "none".into(),
        PatKind::Ok(inner) => format!("(ok {})", pat(inner)),
        PatKind::Err(inner) => format!("(err {})", pat(inner)),
        PatKind::TypeTest { name, ty: t } => format!("({}: {})", name.name, ty(t)),
        PatKind::Or(alts) => list("|", alts.iter().map(pat)),
        PatKind::Twill(t) => format!("(twill {})", t.summary()),
        PatKind::Error => "<error>".into(),
    }
}

// ================================================================ items

/// Renders a whole module, one item per paragraph.
pub fn module(m: &Module) -> String {
    let mut out = String::new();
    for i in &m.items {
        out.push_str(&item(i));
        out.push('\n');
    }
    out
}

fn vis_prefix(vis: Vis) -> &'static str {
    match vis {
        Vis::Private => "",
        Vis::Pub => "pub ",
        Vis::PubGet => "pub(get) ",
    }
}

pub fn item(i: &Item) -> String {
    let mut out = String::new();
    for a in &i.attrs {
        let _ = writeln!(out, "{}", attr(a));
    }
    let vis = vis_prefix(i.vis);
    match &i.kind {
        ItemKind::Use(tree) => {
            let _ = writeln!(out, "({vis}use {})", use_tree(tree));
        }
        ItemKind::Const(c) => {
            let t = c.ty.as_ref().map(|t| format!(": {}", ty(t))).unwrap_or_default();
            let _ = writeln!(out, "({vis}const {}{t} {})", c.name.name, expr(&c.value));
        }
        ItemKind::Type(t) => {
            let _ = writeln!(out, "({vis}{})", type_decl(t));
        }
        ItemKind::Warp(w) => {
            let modifier = match w.modifier {
                WarpModifier::None => "",
                WarpModifier::Once => "once ",
                WarpModifier::Ref => "ref ",
            };
            let wefts = if w.wefts.is_empty() {
                String::new()
            } else {
                format!(": {}", w.wefts.iter().map(ty).collect::<Vec<_>>().join(", "))
            };
            let _ = write!(
                out,
                "({vis}{modifier}warp {}{}{wefts}{}",
                w.name.name,
                generics(&w.generics),
                where_str(&w.where_clause)
            );
            match &w.body {
                WarpBody::Tuple(fields) => {
                    let _ = writeln!(out, " ({}))", case_fields(fields));
                }
                WarpBody::Members(members) => {
                    out.push('\n');
                    push_members(&mut out, members);
                    out.push_str(")\n");
                }
            }
        }
        ItemKind::Weft(w) => {
            let supers = if w.supers.is_empty() {
                String::new()
            } else {
                format!(": {}", w.supers.iter().map(ty).collect::<Vec<_>>().join(" + "))
            };
            let _ = writeln!(out, "({vis}weft {}{}{supers}", w.name.name, generics(&w.generics));
            push_members(&mut out, &w.members);
            out.push_str(")\n");
        }
        ItemKind::Weave(w) => {
            let _ = writeln!(
                out,
                "({vis}weave{} {} into {}{}",
                generics(&w.generics),
                ty(&w.weft),
                ty(&w.target),
                where_str(&w.where_clause)
            );
            push_members(&mut out, &w.members);
            out.push_str(")\n");
        }
        ItemKind::Fn(f) => {
            let _ = writeln!(out, "({vis}{})", fn_decl(f));
        }
        ItemKind::Extern(e) => {
            let _ = writeln!(out, "({vis}extern {:?}", e.abi);
            push_members(&mut out, &e.members);
            out.push_str(")\n");
        }
        ItemKind::ConstExpr(e) => {
            let _ = writeln!(out, "{}", expr(e));
        }
    }
    out
}

fn push_members(out: &mut String, members: &[Member]) {
    for m in members {
        for a in &m.attrs {
            let _ = writeln!(out, "  {}", attr(a));
        }
        let _ = writeln!(out, "  {}", member(m));
    }
}

/// `#name(args)`; a keyed argument prints as `(= key value)`.
fn attr(a: &Attr) -> String {
    let args: Vec<String> = a
        .args
        .iter()
        .map(|arg| match &arg.key {
            Some(key) => format!("(= {} {})", key.name, expr(&arg.value)),
            None => expr(&arg.value),
        })
        .collect();
    format!("#{}({})", a.name.name, args.join(" "))
}

fn member(m: &Member) -> String {
    let vis = vis_prefix(m.vis);
    match &m.kind {
        MemberKind::Field(f) => {
            let default = f.default.as_ref().map(|d| format!(" = {}", expr(d))).unwrap_or_default();
            format!("({vis}field {}: {}{default})", f.name.name, ty(&f.ty))
        }
        MemberKind::Through(f) => format!("({vis}through {}: {})", f.name.name, ty(&f.ty)),
        MemberKind::Cases(cases) => {
            let cases: Vec<String> = cases
                .iter()
                .map(|c| match &c.fields {
                    Some(fields) => format!("{}({})", c.name.name, case_fields(fields)),
                    None => c.name.name.clone(),
                })
                .collect();
            format!("(case {})", cases.join(", "))
        }
        MemberKind::Const(c) => {
            let t = c.ty.as_ref().map(|t| format!(": {}", ty(t))).unwrap_or_default();
            format!("({vis}const {}{t} {})", c.name.name, expr(&c.value))
        }
        MemberKind::Type(t) => format!("({vis}{})", type_decl(t)),
        MemberKind::Fn(f) => format!("({vis}{})", fn_decl(f)),
        MemberKind::ConstExpr(e) => expr(e),
    }
}

fn case_fields(fields: &[CaseField]) -> String {
    fields
        .iter()
        .map(|f| {
            let from = if f.absorbs { "from " } else { "" };
            match &f.name {
                Some(name) => format!("{}: {from}{}", name.name, ty(&f.ty)),
                None => format!("{from}{}", ty(&f.ty)),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn type_decl(t: &crate::ast::TypeDecl) -> String {
    let mut s = format!("type {}{}", t.name.name, generics(&t.generics));
    if !t.bounds.is_empty() {
        let _ = write!(s, ": {}", t.bounds.iter().map(ty).collect::<Vec<_>>().join(" + "));
    }
    if let Some(v) = &t.value {
        let _ = write!(s, " = {}", ty(v));
    }
    if let Some(g) = &t.gauge {
        let _ = write!(s, " where {}", expr(g));
    }
    s
}

fn generics(params: &[GenericParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let params: Vec<String> = params
        .iter()
        .map(|p| match &p.kind {
            GenericParamKind::Type { name, bounds } if bounds.is_empty() => name.name.clone(),
            GenericParamKind::Type { name, bounds } => {
                format!("{}: {}", name.name, bounds.iter().map(ty).collect::<Vec<_>>().join(" + "))
            }
            GenericParamKind::Const { name, ty: t } => format!("const {}: {}", name.name, ty(t)),
            GenericParamKind::Region { name } => format!("@{}", name.name),
        })
        .collect();
    format!("[{}]", params.join(", "))
}

fn where_str(preds: &[WherePred]) -> String {
    if preds.is_empty() {
        return String::new();
    }
    let preds: Vec<String> = preds
        .iter()
        .map(|p| format!("{}: {}", ty(&p.ty), p.bounds.iter().map(ty).collect::<Vec<_>>().join(" + ")))
        .collect();
    format!(" where {}", preds.join(", "))
}

fn fn_decl(f: &FnDecl) -> String {
    let qualifier = match f.qualifier {
        FnQualifier::None => "",
        FnQualifier::Const => "const ",
        FnQualifier::Unsafe => "unsafe ",
    };
    let params: Vec<String> = f.params.iter().map(param).collect();
    let mut s = format!("{qualifier}fn {}{}({})", f.name.name, generics(&f.generics), params.join(", "));
    match &f.ret {
        FnRet::Unit => {}
        FnRet::Value(t) => {
            let _ = write!(s, " -> {}", ty(t));
        }
        FnRet::Shuttle { yields, pulls } => {
            if let Some(y) = yields {
                let place = match y.place {
                    Some(PlaceMode::Mut) => "mut ",
                    Some(PlaceMode::Ref) => "ref ",
                    None => "",
                };
                let _ = write!(s, " ~> {place}{}", ty(&y.ty));
            }
            if let Some(p) = pulls {
                let _ = write!(s, " <~ {}", ty(p));
            }
        }
    }
    if !f.givens.is_empty() {
        let givens: Vec<String> = f.givens.iter().map(|g| format!("{}: {}", g.name.name, ty(&g.ty))).collect();
        let _ = write!(s, " given {}", givens.join(", "));
    }
    s.push_str(&where_str(&f.where_clause));
    match &f.body {
        Some(FnBody::Block(b)) => {
            let _ = write!(s, " {}", block(b));
        }
        Some(FnBody::Expr(e)) => {
            let _ = write!(s, " = {}", expr(e));
        }
        None => {}
    }
    s
}

fn param(p: &Param) -> String {
    let mode = match p.mode {
        ParamMode::Read => "",
        ParamMode::Mut => "mut ",
        ParamMode::Take => "take ",
        ParamMode::Const => "const ",
    };
    let name = match &p.name {
        ParamName::SelfValue => "self".to_string(),
        ParamName::Named(n) => n.name.clone(),
    };
    let t = p.ty.as_ref().map(|t| format!(": {}", ty(t))).unwrap_or_default();
    let default = p.default.as_ref().map(|d| format!(" = {}", expr(d))).unwrap_or_default();
    format!("{mode}{name}{t}{default}")
}

fn use_tree(t: &UseTree) -> String {
    let path: Vec<&str> = t.path.iter().map(|s| s.name.as_str()).collect();
    let path = path.join(".");
    match &t.kind {
        UseKind::Single { alias: Some(a) } => format!("{path} as {}", a.name),
        UseKind::Single { alias: None } => path,
        UseKind::Group(trees) => {
            format!("{path}.{{{}}}", trees.iter().map(use_tree).collect::<Vec<_>>().join(", "))
        }
    }
}
