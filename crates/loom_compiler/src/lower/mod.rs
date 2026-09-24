//! Lowering: type-checked HIR → a Lua bundle.
//!
//! A bundle is one self-contained Lua chunk:
//!
//! ```text
//! local rt = (function() <runtime/rt/core.lua> end)()
//! local T, F, C, EXT, VT = {}, {}, {}, {}, {}
//! T[1] = rt.int(32, true)            -- type descriptors
//! EXT[1] = rt.ext["std.io.print"]    -- std externs it calls
//! F.main_7 = function() … end        -- every reachable function
//! VT[1] = {d = T[4], [31] = F.draw_9} -- vtables of values made `any`
//! C.LIMIT_3 = …                      -- constants, in dependency order
//! return F.main_7
//! ```
//!
//! The host loads the chunk and calls what it returns. Functions live in a
//! table rather than in locals because Lua allows at most 200 locals per
//! function. Next to the Lua text, a source map records the Loom line of
//! every Lua line that starts a statement.

mod expr;
mod fallible;
mod lir;
mod seq;
mod warp;

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use loom_host::srcmap::{Mapping, SourceMap};
use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::{SourceDb, Span};

use crate::hir::{DefId, DefKind, FnRet, HirId, LocalId, Program};
use crate::ty::{FloatTy, Ty, TyKind};
use crate::typeck::{Body, BodyId, Typed};
use lir::{Expr, Printer, Stmt, StmtKind};

/// A compiled program.
#[derive(Debug)]
pub struct Bundle {
    pub lua: String,
    pub map: SourceMap,
}

/// The `std` externs the runtime implements (see `rt.ext` in
/// `runtime/rt/core.lua`). Calling any other extern is "not implemented yet".
const EXTERNS: &[&str] = &[
    "std.io.print",
    "std.error.unravel",
    "std.error.assert",
    "std.fmt.Fmt.put",
    "std.list.List.filled",
    "std.list.List.push",
    "std.list.List.pop",
    "std.list.List.insert",
    "std.list.List.remove",
    "std.list.List.clear",
    "std.list.List.truncate",
    "std.list.List.extend",
    "std.list.List.copy",
    "std.slice.Slice.len",
    "std.slice.Slice.is_empty",
    "std.slice.Slice.get",
    "std.slice.Slice.first",
    "std.slice.Slice.last",
    "std.slice.Slice.contains",
    "std.slice.Slice.index_of",
    "std.slice.Slice.copy_from",
    "std.slice.Slice.fill",
    "std.slice.Slice.swap",
    "std.slice.Slice.reverse",
    "std.slice.Slice.sort",
    "std.slice.Slice.to_list",
];

/// Lowers a type-checked program. `typed` must have no errors.
pub fn lower(db: &SourceDb, p: &Program, typed: &Typed) -> Result<Bundle, Vec<Diagnostic>> {
    let mut l = Lowerer::new(p, typed);
    let Some(main) = l.find_main() else { return Err(l.diags) };
    for &id in &typed.results.reachable {
        l.lower_instance(id);
    }
    if !l.diags.is_empty() {
        return Err(l.diags);
    }
    Ok(l.assemble(db, main))
}

/// Where a lowered value goes.
#[derive(Clone, Debug)]
enum Dest {
    /// Evaluated for its effects only.
    Discard,
    /// Assigned to a local (usually a temporary).
    Assign(String),
    /// Returned from the Lua function.
    Return,
}

/// A Loom loop being lowered.
#[derive(Debug)]
struct LoopInfo {
    id: HirId,
    /// Where `break value` delivers its value.
    dest: Dest,
    continue_label: String,
    continue_used: bool,
    break_label: String,
    /// Set when a `break` from an inner loop targets this one, which needs
    /// a `goto` rather than Lua's `break`.
    break_label_used: bool,
}

struct Lowerer<'a> {
    p: &'a Program,
    typed: &'a Typed,
    diags: Vec<Diagnostic>,
    /// Type descriptors: the Lua expression that builds each, in order.
    descs: IndexMap<Ty, Expr>,
    externs: IndexMap<String, usize>,
    /// Vtables, by type and wefts, with the type's descriptor.
    vtables: IndexMap<(Ty, Vec<DefId>), Expr>,
    /// Lowered functions and constants, in the order they were reached.
    fns: Vec<Stmt>,
    consts: IndexMap<DefId, (Stmt, Vec<DefId>)>,

    // Per-body state.
    /// The body being lowered: where its types and decisions are recorded.
    cur: BodyId,
    out: Vec<Stmt>,
    next_temp: u32,
    next_label: u32,
    loops: Vec<LoopInfo>,
    span: Option<Span>,
    /// Typed constants this body reads, for ordering constant initializers.
    const_refs: Vec<DefId>,
    /// The Lua local holding each dot-block's subject.
    subjects: HashMap<HirId, String>,
    /// The `mend`s whose left operand is being lowered, innermost last.
    mend_labels: Vec<fallible::MendLabels>,
    /// For `main ! E`: the descriptor of `E`.
    main_error: Option<Expr>,
    /// Locals that stand for a place: `x` in `for mut x in xs` is `xs[i]`.
    aliases: HashMap<LocalId, Expr>,
    reported: HashSet<String>,
}

impl<'a> Lowerer<'a> {
    fn new(p: &'a Program, typed: &'a Typed) -> Self {
        Lowerer {
            p,
            typed,
            diags: Vec::new(),
            descs: IndexMap::new(),
            externs: IndexMap::new(),
            vtables: IndexMap::new(),
            fns: Vec::new(),
            consts: IndexMap::new(),
            cur: BodyId(0),
            out: Vec::new(),
            next_temp: 0,
            next_label: 0,
            loops: Vec::new(),
            span: None,
            const_refs: Vec::new(),
            subjects: HashMap::new(),
            mend_labels: Vec::new(),
            main_error: None,
            aliases: HashMap::new(),
            reported: HashSet::new(),
        }
    }

    fn unsupported(&mut self, span: Span, what: &str) {
        if self.reported.insert(what.to_string()) {
            self.diags.push(
                Diagnostic::error(codes::U0102, format!("not implemented yet: {what}"))
                    .primary(span, "can't be compiled to Lua yet"),
            );
        }
    }

    fn find_main(&mut self) -> Option<DefId> {
        let p = self.p;
        let root = p.root?;
        let main = p.module(root).items.get("main").copied();
        let Some(def) = main.filter(|&d| matches!(p.def(d).kind, DefKind::Fn(_))) else {
            let span = p.module(root).file.map(|f| Span::new(f, 0, 0));
            let mut diag = Diagnostic::error(codes::N0116, "there's no `main` function to run")
                .help("add `fn main() { … }`, where the program starts");
            if let Some(span) = span {
                diag = diag.primary(span, "in this file");
            }
            self.diags.push(diag);
            return None;
        };
        let DefKind::Fn(f) = &p.def(def).kind else { return None };
        let span = p.def(def).span;
        // `main ! E`: a failure is shown, and the program exits with 1.
        let ret =
            self.typed.results.find(&crate::typeck::Instance::of(def)).and_then(|id| self.typed.results.sigs.get(&id));
        let fails = ret.and_then(|sig| match self.tcx().kind(sig.ret) {
            TyKind::Result(ok, err) if *ok == self.tcx().unit => Some(*err),
            _ => None,
        });
        if !f.params.is_empty() || (!matches!(f.ret, FnRet::Unit) && fails.is_none()) {
            self.unsupported(span, "`main` with parameters, or a return type other than `! E`");
            return None;
        }
        self.main_error = fails.map(|err| self.desc(err, span));
        Some(def)
    }

    // ------------------------------------------------------------ names

    fn local_name(&self, local: LocalId) -> String {
        format!("{}_{}", ident(&self.p.local(local).name), local.0)
    }

    fn def_name(&self, def: DefId) -> String {
        format!("{}_{}", ident(&self.p.def(def).name), def.0)
    }

    /// The name of a function instance in `F`. A function checked as written
    /// is named after its definition; one checked for particular types (a
    /// `Self`, generic arguments) also gets its instance number.
    fn fn_name(&self, id: BodyId) -> String {
        let inst = self.typed.results.instance(id);
        if inst.self_ty.is_none() && inst.args.is_empty() && !inst.forward {
            self.def_name(inst.def)
        } else {
            format!("{}_{}", self.def_name(inst.def), id.0)
        }
    }

    /// The tables of the body being lowered.
    fn tables(&self) -> &'a Body {
        self.typed.results.body(self.cur)
    }

    /// Runs `f` reading the tables of body `id`: for a default value, which
    /// was checked with its function or warp rather than with the caller.
    fn in_body<T>(&mut self, id: BodyId, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.cur, id);
        let out = f(self);
        self.cur = saved;
        out
    }

    fn new_temp(&mut self) -> String {
        self.next_temp += 1;
        format!("_t{}", self.next_temp)
    }

    fn new_label(&mut self, what: &str) -> String {
        self.next_label += 1;
        format!("_{what}{}", self.next_label)
    }

    /// The Lua expression for a std extern, registering it.
    fn extern_fn(&mut self, def: DefId, span: Span) -> Option<Expr> {
        let path = self.p.def_path(def);
        if !EXTERNS.contains(&path.as_str()) {
            self.unsupported(span, &format!("`{path}` in the Lua runtime"));
            return None;
        }
        let next = self.externs.len() + 1;
        let index = *self.externs.entry(path).or_insert(next);
        Some(Expr::index(Expr::name("EXT"), Expr::Int(index as i64)))
    }

    // ------------------------------------------------------------ type descriptors

    fn tcx(&self) -> &'a crate::ty::TyCtx {
        &self.typed.tcx
    }

    /// `T[n]`, the runtime descriptor of `ty`, registering it (and what it's
    /// made of) first.
    fn desc(&mut self, ty: Ty, span: Span) -> Expr {
        // `str` and `String` are both Lua strings: one descriptor serves both.
        let ty = match self.tcx().kind(ty) {
            TyKind::Adt(def, _) if self.is_string(*def) => self.tcx().str,
            _ => ty,
        };
        if let Some(i) = self.descs.get_index_of(&ty) {
            return Expr::index(Expr::name("T"), Expr::Int(i as i64 + 1));
        }
        let tcx = self.tcx();
        let build = match tcx.kind(ty) {
            TyKind::Int(int) => {
                Expr::rt("int", vec![Expr::Int(int.bits as i64), Expr::Bool(int.signed), Expr::Bool(int.size)])
            }
            TyKind::Float(FloatTy::F64) => Expr::field(Expr::name("rt"), "f64"),
            TyKind::Float(FloatTy::F32) => Expr::field(Expr::name("rt"), "f32t"),
            TyKind::Bool => Expr::field(Expr::name("rt"), "bool"),
            TyKind::Char => Expr::field(Expr::name("rt"), "char"),
            TyKind::Str => Expr::field(Expr::name("rt"), "str"),
            TyKind::Unit => Expr::field(Expr::name("rt"), "unit"),
            // The value carries its own type's description in its vtable.
            TyKind::Any(_) | TyKind::Union(_) => Expr::field(Expr::name("rt"), "anyd"),
            TyKind::Array(..) | TyKind::Adt(..) if self.seq(ty).is_some() => {
                let seq = self.seq(ty).expect("checked");
                self.seq_desc(seq, span)
            }
            TyKind::Adt(..) => {
                // Field types depend on a generic warp's arguments.
                match self.typed.results.adts.get(&ty).cloned() {
                    Some(info) => self.warp_desc(&info, span),
                    None => {
                        let shown = self.tcx().display(self.p, ty);
                        self.unsupported(span, &format!("runtime descriptions of `{shown}`"));
                        Expr::Nil
                    }
                }
            }
            TyKind::Tuple(items) => {
                let items = items.clone();
                let items = items.into_iter().map(|t| self.desc(t, span)).collect();
                Expr::rt("tuple", vec![Expr::Table(items)])
            }
            TyKind::Optional(inner) => {
                let inner = *inner;
                let boxed = self.opt_boxed(inner);
                let inner = self.desc(inner, span);
                Expr::rt("opt", vec![inner, Expr::Bool(boxed)])
            }
            _ => {
                let shown = self.tcx().display(self.p, ty);
                self.unsupported(span, &format!("runtime descriptions of `{shown}`"));
                Expr::Nil
            }
        };
        let (i, _) = self.descs.insert_full(ty, build);
        Expr::index(Expr::name("T"), Expr::Int(i as i64 + 1))
    }

    /// `VT[n]`, the vtable of `ty` made into `any` of `wefts`, registering
    /// it. It's written out after the functions it points to.
    fn vtable(&mut self, ty: Ty, wefts: Vec<DefId>, span: Span) -> Expr {
        let desc = self.desc(ty, span);
        let (i, _) = self.vtables.insert_full((ty, wefts), desc);
        Expr::index(Expr::name("VT"), Expr::Int(i as i64 + 1))
    }

    /// `rt.record`, `rt.sum` or `rt.enum` for a warp, with its fields'
    /// descriptors (registered first, so they're built first).
    fn warp_desc(&mut self, info: &crate::typeck::WarpInfo, span: Span) -> Expr {
        let name = Expr::Str(info.name.clone().into_bytes());
        let field = |this: &mut Self, key: &crate::typeck::Key, name: Option<&str>, ty: Ty| {
            let key = match key {
                crate::typeck::Key::Name(n) => Expr::Str(n.clone().into_bytes()),
                crate::typeck::Key::Pos(i) => Expr::Int(*i as i64),
            };
            let name = name.map_or(Expr::Nil, |n| Expr::Str(n.as_bytes().to_vec()));
            Expr::Table(vec![key, name, this.desc(ty, span)])
        };
        if info.unit_only() {
            let names = info.cases.iter().map(|c| Expr::Str(c.name.clone().into_bytes())).collect();
            return Expr::rt("enum", vec![name, Expr::Table(names)]);
        }
        let fields: Vec<Expr> = info.fields.iter().map(|f| field(self, &f.key, Some(&f.name), f.ty)).collect();
        if !info.is_sum() {
            return Expr::rt("record", vec![name, Expr::Table(fields), Expr::Bool(info.tuple)]);
        }
        let cases = info
            .cases
            .iter()
            .map(|c| {
                let mut entries = vec![(lir::TableKey::Name("name".into()), Expr::Str(c.name.clone().into_bytes()))];
                if let Some(fs) = &c.fields {
                    let fs = fs.iter().map(|f| field(self, &f.key, f.name.as_deref(), f.ty)).collect();
                    entries.push((lir::TableKey::Name("fields".into()), Expr::Table(fs)));
                }
                Expr::Record(entries)
            })
            .collect();
        Expr::rt("sum", vec![name, Expr::Table(cases), Expr::Table(fields)])
    }

    fn is_string(&self, def: DefId) -> bool {
        let d = self.p.def(def);
        d.name == "String" && self.p.module(d.module).path == "std.string"
    }

    /// Whether `?T` must box `some(v)` as `{v}`: when `nil` is already one
    /// of T's own values.
    fn opt_boxed(&self, inner: Ty) -> bool {
        matches!(self.tcx().kind(inner), TyKind::Unit | TyKind::Optional(_))
    }

    // ------------------------------------------------------------ items

    fn lower_instance(&mut self, id: BodyId) {
        let p = self.p;
        let def = self.typed.results.instance(id).def;
        let d = p.def(def);
        self.cur = id;
        match &d.kind {
            DefKind::Fn(f) if f.abi.is_some() => {}
            DefKind::Fn(f) if self.typed.results.instance(id).forward => {
                // A weft method a `through` field provides: call the field's.
                let Some(fwd) = self.typed.results.forwards.get(&id) else { return };
                let span = d.span;
                let params: Vec<String> = f.params.iter().map(|param| self.local_name(param.local)).collect();
                let mut args: Vec<Expr> = params.iter().map(|p| Expr::name(p.clone())).collect();
                if let Some(first) = args.first_mut() {
                    let this = std::mem::replace(first, Expr::Nil);
                    *first = fwd.path.iter().fold(this, Self::access);
                }
                let call = Expr::call(Expr::field(Expr::name("F"), &self.fn_name(fwd.target)), args);
                let body = vec![Stmt { kind: StmtKind::Return(Some(call)), span: Some(span) }];
                let func = Expr::Function(params, body);
                let target = Expr::field(Expr::name("F"), &self.fn_name(id));
                self.fns.push(Stmt { kind: StmtKind::Assign(vec![target], vec![func]), span: Some(span) });
            }
            DefKind::Fn(f) if f.body.is_none() => {
                // A `#builtin` weft method, for a type that doesn't write its own.
                let Some(self_ty) = self.typed.results.instance(id).self_ty else { return };
                let span = d.span;
                let params: Vec<String> = f.params.iter().map(|param| self.local_name(param.local)).collect();
                let Some(body) = self.builtin_method(def, self_ty, &params, span) else { return };
                let func = Expr::Function(params, body);
                let target = Expr::field(Expr::name("F"), &self.fn_name(id));
                self.fns.push(Stmt { kind: StmtKind::Assign(vec![target], vec![func]), span: Some(span) });
            }
            DefKind::Fn(f) => {
                let span = d.span;
                let params: Vec<String> = f.params.iter().map(|param| self.local_name(param.local)).collect();
                let body_expr = f.body.as_ref().expect("checked above");
                let unit = matches!(f.ret, FnRet::Unit);
                let mut body = self.body(span, |this| {
                    this.lower_into(body_expr, if unit { &Dest::Discard } else { &Dest::Return });
                });
                // Scalar `mut` parameters go back to the caller after the result.
                let outs = self.copy_outs(id, f);
                if !outs.is_empty() {
                    body = with_copy_outs(body, &outs);
                    let mut values = vec![Expr::Nil];
                    values.extend(outs.iter().cloned());
                    body.push(Stmt { kind: StmtKind::ReturnMany(values), span: None });
                }
                let func = Expr::Function(params, body);
                let target = Expr::field(Expr::name("F"), &self.fn_name(id));
                self.fns.push(Stmt { kind: StmtKind::Assign(vec![target], vec![func]), span: Some(span) });
            }
            DefKind::Const(c) if !self.typed.results.untyped_consts.contains_key(&def) => {
                let span = d.span;
                self.const_refs.clear();
                let body = self.body(span, |this| this.lower_into(&c.value, &Dest::Return));
                let deps = std::mem::take(&mut self.const_refs);
                let init = Expr::call(Expr::Function(Vec::new(), body), Vec::new());
                let target = Expr::field(Expr::name("C"), &self.def_name(def));
                let stmt = Stmt { kind: StmtKind::Assign(vec![target], vec![init]), span: Some(span) };
                self.consts.insert(def, (stmt, deps));
            }
            _ => {}
        }
    }

    /// The body of a method std leaves to the compiler: a derived `show`,
    /// `eq`, `lt` or `message`, which work through the type's runtime
    /// description.
    fn builtin_method(&mut self, def: DefId, self_ty: Ty, params: &[String], span: Span) -> Option<Vec<Stmt>> {
        let path = self.p.def_path(def);
        let d = self.desc(self_ty, span);
        let arg = |i: usize| Expr::name(params[i].clone());
        let ret = |e: Expr| vec![Stmt { kind: StmtKind::Return(Some(e)), span: None }];
        Some(match path.as_str() {
            "std.fmt.Show.show" => {
                let call = Expr::rt("write_show", vec![d, arg(0), arg(1)]);
                vec![Stmt { kind: StmtKind::Call(call), span: None }]
            }
            "std.ops.Eq.eq" => ret(Expr::rt("eq", vec![d, arg(0), arg(1)])),
            "std.ops.Ord.lt" => ret(Expr::bin("<", Expr::rt("cmp", vec![d, arg(0), arg(1)]), Expr::Int(0))),
            "std.error.Error.message" => ret(Expr::rt("show", vec![d, arg(0), Expr::Bool(false)])),
            _ => {
                self.unsupported(span, &format!("deriving `{path}`"));
                return None;
            }
        })
    }

    /// The parameters of function instance `id` that are copied back to the
    /// caller: `mut` ones whose values aren't tables (numbers, strings,
    /// enums), which the callee can't change in place.
    fn copy_outs(&self, id: BodyId, f: &crate::hir::FnDef) -> Vec<Expr> {
        let Some(sig) = self.typed.results.sigs.get(&id) else { return Vec::new() };
        f.params
            .iter()
            .zip(&sig.params)
            .filter(|(_, p)| p.mode == loom_syntax::ast::ParamMode::Mut && !self.is_table(p.ty))
            .map(|(param, _)| Expr::name(self.local_name(param.local)))
            .collect()
    }

    /// Lowers one function body with fresh per-body state.
    fn body(&mut self, span: Span, f: impl FnOnce(&mut Self)) -> Vec<Stmt> {
        let saved_out = std::mem::take(&mut self.out);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_span = self.span.replace(span);
        f(self);
        self.span = saved_span;
        self.loops = saved_loops;
        std::mem::replace(&mut self.out, saved_out)
    }

    // ------------------------------------------------------------ the bundle

    fn assemble(mut self, db: &SourceDb, main: DefId) -> Bundle {
        let preamble = format!(
            "-- Generated by loom {}. Don't edit: rebuild from the .loom source.\nlocal rt = (function()\n{}end)()\n",
            env!("CARGO_PKG_VERSION"),
            loom_host::RT_CORE
        );
        let mut printer = Printer::new(&preamble);
        printer.raw("local T, F, C, EXT, VT = {}, {}, {}, {}, {}");
        let descs = std::mem::take(&mut self.descs);
        for (i, (_, build)) in descs.iter().enumerate() {
            let target = Expr::index(Expr::name("T"), Expr::Int(i as i64 + 1));
            printer.stmt(&Stmt { kind: StmtKind::Assign(vec![target], vec![build.clone()]), span: None });
        }
        for (path, &i) in &self.externs {
            let source = Expr::index(Expr::field(Expr::name("rt"), "ext"), Expr::Str(path.clone().into_bytes()));
            let target = Expr::index(Expr::name("EXT"), Expr::Int(i as i64));
            printer.stmt(&Stmt { kind: StmtKind::Assign(vec![target], vec![source]), span: None });
        }
        for stmt in &self.fns {
            printer.stmt(stmt);
        }
        for (i, ((ty, wefts), desc)) in self.vtables.iter().enumerate() {
            let mut entries = vec![(lir::TableKey::Name("d".into()), desc.clone())];
            let key = (*ty, wefts.clone());
            for &(method, id) in self.typed.results.vtables.get(&key).into_iter().flatten() {
                let f = Expr::field(Expr::name("F"), &self.fn_name(id));
                entries.push((lir::TableKey::Index(method.0 as i64), f));
            }
            let target = Expr::index(Expr::name("VT"), Expr::Int(i as i64 + 1));
            printer.stmt(&Stmt { kind: StmtKind::Assign(vec![target], vec![Expr::Record(entries)]), span: None });
        }
        for (i, ty) in descs.keys().enumerate() {
            let Some(fns) = self.typed.results.op_fns.get(ty) else { continue };
            for (slot, id) in [("show", fns.show), ("eq", fns.eq), ("lt", fns.lt)] {
                let Some(id) = id else { continue };
                let target = Expr::field(Expr::index(Expr::name("T"), Expr::Int(i as i64 + 1)), slot);
                let value = Expr::field(Expr::name("F"), &self.fn_name(id));
                printer.stmt(&Stmt { kind: StmtKind::Assign(vec![target], vec![value]), span: None });
            }
        }
        for stmt in self.const_order() {
            printer.stmt(&stmt);
        }
        match &self.main_error {
            // The host gets the error's text back, or nothing.
            Some(desc) => {
                let stmt = |kind| Stmt { kind, span: None };
                let r = Expr::name("r");
                let call = Expr::call(Expr::field(Expr::name("F"), &self.def_name(main)), Vec::new());
                let shown =
                    Expr::rt("show", vec![desc.clone(), Expr::index(r.clone(), Expr::Int(2)), Expr::Bool(false)]);
                let failed = Expr::Unary("not", Box::new(Expr::index(r, Expr::Int(1))));
                let body = vec![
                    stmt(StmtKind::Local(vec!["r".into()], vec![call])),
                    stmt(StmtKind::If(vec![(failed, vec![stmt(StmtKind::Return(Some(shown)))])], None)),
                ];
                printer.stmt(&stmt(StmtKind::Return(Some(Expr::Function(Vec::new(), body)))));
            }
            None => printer.raw(&format!("return F.{}", self.def_name(main))),
        }
        let printed = printer.finish();
        Bundle { lua: printed.text, map: source_map(db, &printed.lines) }
    }

    /// Constant initializers ordered so each runs after the ones it reads.
    fn const_order(&mut self) -> Vec<Stmt> {
        let mut consts = std::mem::take(&mut self.consts);
        let mut done = HashSet::new();
        let mut out = Vec::new();
        let keys: Vec<DefId> = consts.keys().copied().collect();
        fn visit(
            def: DefId,
            consts: &mut IndexMap<DefId, (Stmt, Vec<DefId>)>,
            done: &mut HashSet<DefId>,
            out: &mut Vec<Stmt>,
        ) {
            if !done.insert(def) {
                return;
            }
            let Some((stmt, deps)) = consts.get(&def).cloned() else { return };
            for dep in deps {
                visit(dep, consts, done, out);
            }
            out.push(stmt);
        }
        for def in keys {
            visit(def, &mut consts, &mut done, &mut out);
        }
        out
    }
}

/// `stmts` with every `return` also returning `outs` (see `copy_outs`).
fn with_copy_outs(stmts: Vec<Stmt>, outs: &[Expr]) -> Vec<Stmt> {
    stmts
        .into_iter()
        .map(|stmt| {
            let kind = match stmt.kind {
                StmtKind::Return(v) => {
                    let mut values = vec![v.unwrap_or(Expr::Nil)];
                    values.extend(outs.iter().cloned());
                    StmtKind::ReturnMany(values)
                }
                StmtKind::If(arms, else_) => StmtKind::If(
                    arms.into_iter().map(|(c, b)| (c, with_copy_outs(b, outs))).collect(),
                    else_.map(|b| with_copy_outs(b, outs)),
                ),
                StmtKind::While(c, b) => StmtKind::While(c, with_copy_outs(b, outs)),
                StmtKind::NumFor(v, lo, hi, b) => StmtKind::NumFor(v, lo, hi, with_copy_outs(b, outs)),
                StmtKind::Do(b) => StmtKind::Do(with_copy_outs(b, outs)),
                other => other,
            };
            Stmt { kind, span: stmt.span }
        })
        .collect()
}

fn source_map(db: &SourceDb, lines: &[(u32, Span)]) -> SourceMap {
    let mut files: IndexMap<u32, String> = IndexMap::new();
    let mut map = SourceMap::default();
    let mut ids: HashMap<loom_syntax::FileId, u32> = HashMap::new();
    for &(lua_line, span) in lines {
        let file = *ids.entry(span.file).or_insert_with(|| {
            let id = files.len() as u32;
            files.insert(id, db.file(span.file).name().to_string());
            id
        });
        let at = db.line_col(span);
        map.lines.push(Mapping { lua_line, file, line: at.line, column: at.col });
    }
    map.files = files.into_values().collect();
    map
}

/// A Loom name as a Lua identifier: ASCII letters, digits and `_` stay, and
/// anything else becomes `_uXXXX`. Every use adds a `_N` suffix, so Lua
/// keywords can't collide.
fn ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push_str(&format!("_u{:04X}", c as u32));
        }
    }
    if out.is_empty() { "_".into() } else { out }
}
