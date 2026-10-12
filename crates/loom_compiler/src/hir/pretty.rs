//! An S-expression rendering of the HIR, for `loom build --emit hir` and
//! for tests. It looks like the AST dump, except that names are resolved:
//!
//! - a local prints as `name#n`, numbered in order of first appearance in
//!   the item, so `let x = x + 1` shows which `x` is which;
//! - a definition prints as its path (`std.io.print`, `Shape.Circle`), with
//!   the root module's own prefix left off;
//! - a type used as a value prints as `(type T)`, and `Type.name` as `(:: T name)`.

use std::collections::HashMap;
use std::fmt::Write as _;

use loom_syntax::ast::{AssignOp, FieldName, FnQualifier, ParamMode, PlaceMode, UnaryOp, Vis, WarpModifier};
use loom_syntax::pretty::lit_str;

use super::{
    Arg, Block, CasePath, DefId, DefKind, Expr, ExprKind, FnDef, FnRet, GenericArg, GenericParamKind, HirId, Intrinsic,
    LocalId, ModuleId, Pat, PatKind, Program, Region, Stmt, StmtKind, TemplatePart, Ty, TyKind, WherePred,
};

/// Every declaration of `module`, one per paragraph.
pub fn module(p: &Program, module: ModuleId) -> String {
    let mut out = String::new();
    for &def in &p.module(module).decls {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&def_str(p, def));
        out.push('\n');
    }
    out
}

/// One definition and everything inside it.
pub fn def_str(p: &Program, def: DefId) -> String {
    let mut printer = Printer { p, locals: HashMap::new(), loops: HashMap::new() };
    printer.def(def, 0)
}

struct Printer<'p> {
    p: &'p Program,
    locals: HashMap<LocalId, usize>,
    loops: HashMap<HirId, usize>,
}

impl Printer<'_> {
    fn local(&mut self, id: LocalId) -> String {
        let next = self.locals.len() + 1;
        let n = *self.locals.entry(id).or_insert(next);
        format!("{}#{n}", self.p.local(id).name)
    }

    fn def_ref(&self, id: DefId) -> String {
        let path = self.p.def_path(id);
        let def = self.p.def(id);
        if Some(def.module) == self.p.root {
            let prefix = format!("{}.", self.p.module(def.module).path);
            if let Some(rest) = path.strip_prefix(&prefix) {
                return rest.to_string();
            }
        }
        path
    }

    fn loop_label(&mut self, id: HirId) -> String {
        let next = self.loops.len() + 1;
        format!("'L{}", *self.loops.entry(id).or_insert(next))
    }

    // ------------------------------------------------------------ definitions

    fn def(&mut self, id: DefId, indent: usize) -> String {
        let p = self.p;
        let d = p.def(id);
        let pad = "  ".repeat(indent);
        let vis = match d.vis {
            Vis::Private => "",
            Vis::Pub => "pub ",
            Vis::PubGet => "pub(get) ",
        };
        let mut attrs = String::new();
        for a in &d.attrs {
            let _ = writeln!(attrs, "{pad}#{}", a.name);
        }
        let body = match &d.kind {
            DefKind::Fn(f) => self.fn_def(&d.name, f),
            DefKind::Const(c) => {
                let ty = c.ty.as_ref().map(|t| format!(": {}", self.ty(t))).unwrap_or_default();
                format!("(const {}{ty} {})", d.name, self.expr(&c.value))
            }
            DefKind::Type(t) => {
                let mut s = format!("(type {}{}", d.name, self.generics(&t.generics));
                if !t.bounds.is_empty() {
                    let _ = write!(s, ": {}", self.tys(&t.bounds, " + "));
                }
                if let Some(v) = &t.value {
                    let _ = write!(s, " = {}", self.ty(v));
                }
                if let Some(g) = &t.gauge {
                    let _ = write!(s, " where {}", self.expr(g));
                }
                s + ")"
            }
            // The expression is already a `(const …)`.
            DefKind::ConstBlock(e) => self.expr(e),
            DefKind::Field(f) => {
                let head = if f.through { "through" } else { "field" };
                let default = f.default.as_ref().map(|e| format!(" = {}", self.expr(e))).unwrap_or_default();
                format!("({head} {}: {}{default})", d.name, self.ty(&f.ty))
            }
            DefKind::Case(c) => match &c.fields {
                None => format!("(case {})", d.name),
                Some(fields) => {
                    let fields: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            let from = if f.absorbs { "from " } else { "" };
                            match &f.name {
                                Some(n) => format!("{}: {from}{}", n.name, self.ty(&f.ty)),
                                None => format!("{from}{}", self.ty(&f.ty)),
                            }
                        })
                        .collect();
                    format!("(case {}({}))", d.name, fields.join(", "))
                }
            },
            DefKind::Warp(w) => {
                let modifier = match w.modifier {
                    WarpModifier::None => "",
                    WarpModifier::Once => "once ",
                    WarpModifier::Ref => "ref ",
                };
                let mut s = format!("({modifier}warp {}{}", d.name, self.generics(&w.generics));
                if !w.wefts.is_empty() {
                    let _ = write!(s, ": {}", self.tys(&w.wefts, ", "));
                }
                s.push_str(&self.where_str(&w.where_clause));
                for &member in w.fields.iter().chain(&w.cases).chain(&w.members) {
                    let _ = write!(s, "\n{}", self.def(member, indent + 1));
                }
                s + ")"
            }
            DefKind::Weft(w) => {
                let mut s = format!("(weft {}{}", d.name, self.generics(&w.generics));
                if !w.supers.is_empty() {
                    let _ = write!(s, ": {}", self.tys(&w.supers, ", "));
                }
                for &member in &w.members {
                    let _ = write!(s, "\n{}", self.def(member, indent + 1));
                }
                s + ")"
            }
            DefKind::Weave(w) => {
                let mut s = format!(
                    "(weave{} {} into {}{}",
                    self.generics(&w.generics),
                    self.ty(&w.weft),
                    self.ty(&w.target),
                    self.where_str(&w.where_clause)
                );
                for &member in &w.members {
                    let _ = write!(s, "\n{}", self.def(member, indent + 1));
                }
                s + ")"
            }
            DefKind::GenericParam(_) => d.name.clone(),
            DefKind::Pending => format!("(pending {})", d.name),
        };
        format!("{attrs}{pad}{vis}{body}")
    }

    fn generics(&mut self, generics: &[DefId]) -> String {
        if generics.is_empty() {
            return String::new();
        }
        let p = self.p;
        let parts: Vec<String> = generics
            .iter()
            .map(|&g| {
                let d = p.def(g);
                match &d.kind {
                    DefKind::GenericParam(gp) => match &gp.kind {
                        GenericParamKind::Type { bounds } if bounds.is_empty() => d.name.clone(),
                        GenericParamKind::Type { bounds } => format!("{}: {}", d.name, self.tys(bounds, " + ")),
                        GenericParamKind::Const { ty } => format!("const {}: {}", d.name, self.ty(ty)),
                        GenericParamKind::Region => format!("@{}", d.name),
                    },
                    _ => d.name.clone(),
                }
            })
            .collect();
        format!("[{}]", parts.join(", "))
    }

    fn where_str(&mut self, preds: &[WherePred]) -> String {
        if preds.is_empty() {
            return String::new();
        }
        let parts: Vec<String> =
            preds.iter().map(|w| format!("{}: {}", self.ty(&w.ty), self.tys(&w.bounds, " + "))).collect();
        format!(" where {}", parts.join(", "))
    }

    fn fn_def(&mut self, name: &str, f: &FnDef) -> String {
        let mut s = String::from("(");
        if let Some(abi) = &f.abi {
            let _ = write!(s, "extern {abi:?} ");
        }
        let keyword = if f.qualifier == FnQualifier::Test { "test" } else { "fn" };
        let _ = write!(s, "{keyword} {name}{}(", self.generics(&f.generics));
        let params: Vec<String> = f
            .params
            .iter()
            .map(|param| {
                let mode = match param.mode {
                    ParamMode::Read => "",
                    ParamMode::Mut => "mut ",
                    ParamMode::Take => "take ",
                    ParamMode::Const => "const ",
                };
                let name = if param.is_self { "self".to_string() } else { self.local(param.local) };
                let ty = param.ty.as_ref().map(|t| format!(": {}", self.ty(t))).unwrap_or_default();
                let default = param.default.as_ref().map(|e| format!(" = {}", self.expr(e))).unwrap_or_default();
                format!("{mode}{name}{ty}{default}")
            })
            .collect();
        s.push_str(&params.join(", "));
        s.push(')');
        match &f.ret {
            FnRet::Unit => {}
            FnRet::Value(t) => {
                let _ = write!(s, " -> {}", self.ty(t));
            }
            FnRet::Shuttle { yields, pulls } => {
                if let Some((place, t)) = yields {
                    let place = match place {
                        Some(PlaceMode::Mut) => "mut ",
                        Some(PlaceMode::Ref) => "ref ",
                        None => "",
                    };
                    let _ = write!(s, " ~> {place}{}", self.ty(t));
                }
                if let Some(t) = pulls {
                    let _ = write!(s, " <~ {}", self.ty(t));
                }
            }
        }
        if !f.givens.is_empty() {
            let givens: Vec<String> =
                f.givens.iter().map(|g| format!("{}: {}", self.local(g.local), self.ty(&g.ty))).collect();
            let _ = write!(s, " given {}", givens.join(", "));
        }
        s.push_str(&self.where_str(&f.where_clause));
        if let Some(body) = &f.body {
            let _ = write!(s, " {}", self.expr(body));
        }
        s + ")"
    }

    // ------------------------------------------------------------ types

    fn tys(&mut self, tys: &[Ty], sep: &str) -> String {
        tys.iter().map(|t| self.ty(t)).collect::<Vec<_>>().join(sep)
    }

    fn ty(&mut self, t: &Ty) -> String {
        match &t.kind {
            TyKind::Prim(p) => p.to_string(),
            TyKind::Def { def, args } if args.is_empty() => self.def_ref(*def),
            TyKind::Def { def, args } => {
                let args: Vec<String> = args.iter().map(|a| self.generic_arg(a)).collect();
                format!("{}[{}]", self.def_ref(*def), args.join(", "))
            }
            TyKind::Local(l) => self.local(*l),
            TyKind::SelfType => "Self".into(),
            TyKind::Assoc { base, name } => format!("{}.{}", self.ty(base), name.name),
            TyKind::TypeType => "type".into(),
            TyKind::ConstCall { def, args } => {
                let args: Vec<String> = args.iter().map(|a| self.expr(a)).collect();
                format!("{}({})", self.def_ref(*def), args.join(", "))
            }
            TyKind::Unit => "()".into(),
            TyKind::Tuple(tys) => format!("({})", self.tys(tys, ", ")),
            TyKind::Array { elem, len } => format!("[{}; {}]", self.ty(elem), self.expr(len)),
            TyKind::Slice(t) => format!("[{}]", self.ty(t)),
            TyKind::Optional(t) => format!("?{}", self.ty(t)),
            TyKind::Pointer(t) => format!("^{}", self.ty(t)),
            TyKind::RawPointer(t) => format!("*{}", self.ty(t)),
            TyKind::Region { ty, region } => {
                let region = match region {
                    Region::Local(l) => self.local(*l),
                    Region::Param(d) => self.def_ref(*d),
                    Region::Heap => "heap".into(),
                    Region::Static => "static".into(),
                    Region::Error => "<error>".into(),
                };
                format!("{}@{region}", self.ty(ty))
            }
            TyKind::Result { ok, err } => format!("({} ! {})", self.ty(ok), self.ty(err)),
            TyKind::Union(tys) => format!("({})", self.tys(tys, " | ")),
            TyKind::Fn { params, ret } => format!("(({}) => {})", self.tys(params, ", "), self.ty(ret)),
            TyKind::Any(tys) => format!("any {}", self.tys(tys, " + ")),
            TyKind::Infer => "_".into(),
            TyKind::Error => "<error>".into(),
        }
    }

    fn generic_arg(&mut self, a: &GenericArg) -> String {
        match a {
            GenericArg::Type(t) => self.ty(t),
            GenericArg::Expr(e) => self.expr(e),
        }
    }

    // ------------------------------------------------------------ expressions

    fn list(&mut self, head: &str, items: Vec<String>) -> String {
        let mut s = format!("({head}");
        for item in items {
            s.push(' ');
            s.push_str(&item);
        }
        s + ")"
    }

    fn exprs(&mut self, es: &[Expr]) -> Vec<String> {
        es.iter().map(|e| self.expr(e)).collect()
    }

    fn block(&mut self, b: &Block) -> String {
        let stmts: Vec<String> = b.stmts.iter().map(|s| self.stmt(s)).collect();
        self.list("block", stmts)
    }

    fn stmt(&mut self, s: &Stmt) -> String {
        match &s.kind {
            StmtKind::Let { mutable, pat, ty, init } => {
                let head = if *mutable { "var" } else { "let" };
                let init = self.expr(init);
                let ty = ty.as_ref().map(|t| format!(": {}", self.ty(t))).unwrap_or_default();
                format!("({head} {}{ty} {init})", self.pat(pat))
            }
            StmtKind::Given { local, value } => {
                let value = self.expr(value);
                match local {
                    Some(l) => format!("(given {} {value})", self.local(*l)),
                    None => format!("(given {value})"),
                }
            }
            StmtKind::Spool { local, init } => {
                let init = self.expr(init);
                format!("(spool {} {init})", self.local(*local))
            }
            StmtKind::Defer { binding, body } => match binding {
                Some(l) => format!("(defer {} => {})", self.local(*l), self.expr(body)),
                None => format!("(defer {})", self.expr(body)),
            },
            StmtKind::Item(def) => self.def(*def, 0),
            StmtKind::Expr(e) => self.expr(e),
        }
    }

    fn arg(&mut self, a: &Arg) -> String {
        let value = self.expr(&a.value);
        let value = if a.mutable { format!("(mut {value})") } else { value };
        if a.is_given {
            format!("(given: {value})")
        } else if let Some(label) = &a.label {
            format!("({}: {value})", label.name)
        } else {
            value
        }
    }

    fn expr(&mut self, e: &Expr) -> String {
        match &e.kind {
            ExprKind::Lit(lit) => lit_str(lit),
            ExprKind::Template(parts) => {
                let mut s = String::from("(template");
                for part in parts {
                    match part {
                        TemplatePart::Text(text) => {
                            let _ = write!(s, " {text:?}");
                        }
                        TemplatePart::Interp { expr, spec } => {
                            let inner = self.expr(expr);
                            match spec {
                                Some(spec) => {
                                    let _ = write!(s, " {{{inner}:{spec}}}");
                                }
                                None => {
                                    let _ = write!(s, " {{{inner}}}");
                                }
                            }
                        }
                    }
                }
                s + ")"
            }
            ExprKind::Local(l) => self.local(*l),
            ExprKind::Def { def, args } if args.is_empty() => self.def_ref(*def),
            ExprKind::Def { def, args } => {
                let args: Vec<String> = args.iter().map(|a| self.generic_arg(a)).collect();
                format!("{}[{}]", self.def_ref(*def), args.join(", "))
            }
            ExprKind::Type(t) => format!("(type {})", self.ty(t)),
            ExprKind::Assoc { base, name } => format!("(:: {} {})", self.ty(base), name.name),
            ExprKind::BuiltinCase(k) => k.to_string(),
            ExprKind::Intrinsic(Intrinsic::Target) => "<target>".into(),
            ExprKind::Intrinsic(Intrinsic::Heap) => "<heap>".into(),
            ExprKind::LeadingDot(FieldName::Named(n)) => format!(".{}", n.name),
            ExprKind::LeadingDot(FieldName::Index(i, _)) => format!(".{i}"),
            ExprKind::Tuple(items) if items.is_empty() => "()".into(),
            ExprKind::Tuple(items) => {
                let items = self.exprs(items);
                self.list("tuple", items)
            }
            ExprKind::Array(items) => {
                let items = self.exprs(items);
                self.list("array", items)
            }
            ExprKind::ArrayRepeat { value, count } => {
                format!("(array-repeat {} {})", self.expr(value), self.expr(count))
            }
            ExprKind::Map(pairs) => {
                let pairs: Vec<String> =
                    pairs.iter().map(|(k, v)| format!("({} {})", self.expr(k), self.expr(v))).collect();
                self.list("map", pairs)
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::If { cond, then, else_, deferred } => {
                let head = if deferred.is_some() { "const-if" } else { "if" };
                let cond = self.expr(cond);
                let then = self.block(then);
                match else_ {
                    Some(e) => format!("({head} {cond} {then} {})", self.expr(e)),
                    None => format!("({head} {cond} {then})"),
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let mut s = format!("(match {}", self.expr(scrutinee));
                for arm in arms {
                    let pat = self.pat(&arm.pat);
                    let arm_str = match &arm.guard {
                        Some(g) => format!("(=> {pat} (if {}) {})", self.expr(g), self.expr(&arm.body)),
                        None => format!("(=> {pat} {})", self.expr(&arm.body)),
                    };
                    s.push(' ');
                    s.push_str(&arm_str);
                }
                s + ")"
            }
            ExprKind::For { pat, iter, body, is_const, is_loom } => {
                let head = if *is_loom {
                    "loom-for"
                } else if *is_const {
                    "const-for"
                } else {
                    "for"
                };
                let label = self.loop_label(e.id);
                let iter = self.expr(iter);
                format!("({head} {label} {} {iter} {})", self.pat(pat), self.block(body))
            }
            ExprKind::While { cond, body } => {
                let label = self.loop_label(e.id);
                format!("(while {label} {} {})", self.expr(cond), self.block(body))
            }
            ExprKind::Loop { body } => {
                let label = self.loop_label(e.id);
                format!("(loop {label} {})", self.block(body))
            }
            ExprKind::Loom { args, body, .. } => {
                let mut s = String::from("(loom");
                for a in args {
                    let a = self.arg(a);
                    s.push(' ');
                    s.push_str(&a);
                }
                format!("{s} {})", self.block(body))
            }
            ExprKind::Thread { body, .. } => format!("(thread {})", self.expr(body)),
            ExprKind::Break { target, value } => {
                let label = self.loop_label(*target);
                match value {
                    Some(v) => format!("(break {label} {})", self.expr(v)),
                    None => format!("(break {label})"),
                }
            }
            ExprKind::Continue { target } => format!("(continue {})", self.loop_label(*target)),
            ExprKind::Return(Some(v)) => format!("(return {})", self.expr(v)),
            ExprKind::Return(None) => "(return)".into(),
            ExprKind::Fail(v) => format!("(fail {})", self.expr(v)),
            ExprKind::Yield { place, value } => match place {
                Some(PlaceMode::Mut) => format!("(yield mut {})", self.expr(value)),
                Some(PlaceMode::Ref) => format!("(yield ref {})", self.expr(value)),
                None => format!("(yield {})", self.expr(value)),
            },
            ExprKind::Pull => "pull".into(),
            ExprKind::Unsafe { reason, body } => format!("(unsafe {reason:?} {})", self.block(body)),
            ExprKind::Const(v) => format!("(const {})", self.expr(v)),
            ExprKind::Lambda { params, body } => {
                let params: Vec<String> = params
                    .iter()
                    .map(|param| {
                        let name = self.local(param.local);
                        let name = if param.take { format!("take {name}") } else { name };
                        match &param.ty {
                            Some(t) => format!("{name}: {}", self.ty(t)),
                            None => name,
                        }
                    })
                    .collect();
                format!("(fn ({}) {})", params.join(", "), self.expr(body))
            }
            ExprKind::Call { callee, args } => {
                let mut s = format!("(call {}", self.expr(callee));
                for a in args {
                    let a = self.arg(a);
                    s.push(' ');
                    s.push_str(&a);
                }
                s + ")"
            }
            ExprKind::Index { base, args } => {
                let mut s = format!("(index {}", self.expr(base));
                for a in args {
                    let a = match a {
                        GenericArg::Type(t) => format!("(type {})", self.ty(t)),
                        GenericArg::Expr(e) => self.expr(e),
                    };
                    s.push(' ');
                    s.push_str(&a);
                }
                s + ")"
            }
            ExprKind::Field { base, field: FieldName::Named(n) } => format!("(. {} {})", self.expr(base), n.name),
            ExprKind::Field { base, field: FieldName::Index(i, _) } => format!("(. {} {i})", self.expr(base)),
            ExprKind::ConstField { base, field } => format!("(.() {} {})", self.expr(base), self.expr(field)),
            ExprKind::Try(v) => format!("(? {})", self.expr(v)),
            ExprKind::Deref(v) => format!("(deref {})", self.expr(v)),
            ExprKind::DotBlock { subject, body } => {
                format!("(dot-block {} {})", self.expr(subject), self.block(body))
            }
            ExprKind::Unary { op, operand } => {
                let op = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::BitNot => "~",
                    UnaryOp::Not => "not",
                    UnaryOp::Alloc => "alloc",
                };
                format!("({op} {})", self.expr(operand))
            }
            ExprKind::Binary { op, lhs, rhs } => format!("({} {} {})", op.as_str(), self.expr(lhs), self.expr(rhs)),
            ExprKind::Chain { first, rest } => {
                let mut s = format!("(chain {}", self.expr(first));
                for (op, e) in rest {
                    let e = self.expr(e);
                    let _ = write!(s, " {} {e}", op.as_str());
                }
                s + ")"
            }
            ExprKind::Is { expr, pat } => format!("(is {} {})", self.expr(expr), self.pat(pat)),
            ExprKind::Cast { expr, kind, ty } => format!("({} {} {})", kind.as_str(), self.expr(expr), self.ty(ty)),
            ExprKind::Range { lo, hi, inclusive } => {
                let op = if *inclusive { "..=" } else { ".." };
                let lo = lo.as_ref().map_or("_".to_string(), |e| self.expr(e));
                let hi = hi.as_ref().map_or("_".to_string(), |e| self.expr(e));
                format!("({op} {lo} {hi})")
            }
            ExprKind::Mend { expr, binding: Some(b), handler } => {
                let expr = self.expr(expr);
                format!("(mend {expr} ({} => {}))", self.local(*b), self.expr(handler))
            }
            ExprKind::Mend { expr, binding: None, handler } => {
                format!("(mend {} {})", self.expr(expr), self.expr(handler))
            }
            ExprKind::Assign { target, op, value } => {
                let op = match op {
                    AssignOp::Assign => "=".to_string(),
                    AssignOp::Compound(op) => format!("{}=", op.as_str()),
                };
                format!("({op} {} {})", self.expr(target), self.expr(value))
            }
            ExprKind::Error => "<error>".into(),
        }
    }

    // ------------------------------------------------------------ patterns

    fn pat(&mut self, p: &Pat) -> String {
        match &p.kind {
            PatKind::Wild => "_".into(),
            PatKind::Bind(l) => {
                let name = self.local(*l);
                if self.p.local(*l).mutable { format!("(mut {name})") } else { name }
            }
            PatKind::Lit { lit, negative } => format!("{}{}", if *negative { "-" } else { "" }, lit_str(lit)),
            PatKind::Range { lo, hi, inclusive } => {
                let op = if *inclusive { "..=" } else { ".." };
                let lo = lo.as_ref().map_or("_".to_string(), |p| self.pat(p));
                let hi = hi.as_ref().map_or("_".to_string(), |p| self.pat(p));
                format!("({op} {lo} {hi})")
            }
            PatKind::Tuple(pats) => {
                let pats: Vec<String> = pats.iter().map(|p| self.pat(p)).collect();
                format!("({})", pats.join(", "))
            }
            PatKind::Slice(pats) => {
                let pats: Vec<String> = pats.iter().map(|p| self.pat(p)).collect();
                format!("[{}]", pats.join(", "))
            }
            PatKind::Rest(None) => "..".into(),
            PatKind::Rest(Some(l)) => format!("..{}", self.local(*l)),
            PatKind::Case { path, fields, rest } => {
                let head = match path {
                    CasePath::Dot(n) => format!(".{}", n.name),
                    CasePath::Type(t) => self.ty(t),
                    CasePath::Const(d) => self.def_ref(*d),
                    CasePath::Assoc { base, name } => format!("{}.{}", self.ty(base), name.name),
                };
                match fields {
                    None => head,
                    Some(fields) => {
                        let mut parts: Vec<String> = fields
                            .iter()
                            .map(|f| {
                                let pat = self.pat(&f.pat);
                                match &f.label {
                                    Some(l) => format!("{}: {pat}", l.name),
                                    None => pat,
                                }
                            })
                            .collect();
                        if *rest {
                            parts.push("..".into());
                        }
                        format!("({head} {})", parts.join(" "))
                    }
                }
            }
            PatKind::ConstCase(e) => format!(".({})", self.expr(e)),
            PatKind::Some(p) => format!("(some {})", self.pat(p)),
            PatKind::None => "none".into(),
            PatKind::Ok(p) => format!("(ok {})", self.pat(p)),
            PatKind::Err(p) => format!("(err {})", self.pat(p)),
            PatKind::TypeTest { local, ty } => format!("({}: {})", self.local(*local), self.ty(ty)),
            PatKind::Or(alts) => {
                let alts: Vec<String> = alts.iter().map(|p| self.pat(p)).collect();
                format!("(| {})", alts.join(" "))
            }
            PatKind::Twill { pattern, captures } => {
                let captures: Vec<String> = captures.iter().map(|&c| self.local(c)).collect();
                format!("(twill {} [{}])", pattern.summary(), captures.join(" "))
            }
            PatKind::Error => "<error>".into(),
        }
    }
}
