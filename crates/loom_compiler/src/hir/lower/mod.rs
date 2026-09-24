//! Name resolution: AST → HIR.
//!
//! 1. **Load.** Parse the root file and every `std` module. User modules are
//!    loaded when a `use` first reaches them.
//! 2. **Collect.** Give every item and member a [`DefId`] and fill in each
//!    module's namespace, reporting duplicates.
//! 3. **Import.** Resolve `use` declarations to a fixpoint, since a `pub use`
//!    in one module can feed an import in another. Then build the prelude.
//! 4. **Lower.** Walk every item with a stack of scopes, resolving each name
//!    and building the HIR (`items.rs`, `body.rs`).

mod body;
mod items;

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use loom_syntax::ast::{self, Ident, UseKind, UseTree, Vis};
use loom_syntax::diag::codes;
use loom_syntax::source::{FileId, SourceDb, Span};
use loom_syntax::{Diagnostic, parse};

use super::{
    Binding, Def, DefId, DefKind, HirId, Import, Intrinsic, Local, LocalId, LocalKind, Module, ModuleId, Program,
};
use crate::modules::{Loader, Source, UserModule};

/// The result of name resolution.
#[derive(Debug)]
pub struct Resolved {
    pub program: Program,
    /// Parse and resolution diagnostics for every file loaded, in load order.
    pub diagnostics: Vec<Diagnostic>,
}

impl Resolved {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Resolves the program whose root file is `root`, loading `std` and user
/// modules through `loader` (and adding their files to `db`).
pub fn resolve(db: &mut SourceDb, root: FileId, loader: &dyn Loader) -> Resolved {
    let mut r = Resolver::new(db, loader);
    r.run(root);
    Resolved { program: r.p, diagnostics: r.diags }
}

/// What a name refers to while lowering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Res {
    Local(LocalId),
    Def(DefId),
    Module(ModuleId),
    Intrinsic(Intrinsic),
}

impl From<Binding> for Res {
    fn from(b: Binding) -> Self {
        match b {
            Binding::Def(d) => Res::Def(d),
            Binding::Module(m) => Res::Module(m),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RibKind {
    /// A block, lambda, arm or parameter list: locals are visible inside.
    Normal,
    /// The start of an item: the locals of enclosing functions aren't visible past it.
    Item,
}

#[derive(Debug)]
struct Rib {
    kind: RibKind,
    names: HashMap<String, Res>,
}

/// The [`DefId`]s made for one AST item, so lowering can find them again.
#[derive(Clone, Debug, Default)]
struct ItemDefs {
    /// `None` for `use` and extern blocks.
    def: Option<DefId>,
    /// Per member (or tuple field, or extern fn), the defs it declared.
    members: Vec<Vec<DefId>>,
}

/// One name a `use` brings in, flattened out of groups.
#[derive(Clone, Debug)]
struct PendingUse {
    module: ModuleId,
    path: Vec<Ident>,
    alias: Option<Ident>,
    public: bool,
}

enum UseOutcome {
    Done(Binding),
    /// The name may still appear once another module's imports resolve.
    Retry,
    Failed(Diagnostic),
}

/// A loop that `break` and `continue` can target.
#[derive(Clone, Debug)]
struct LoopFrame {
    label: Option<String>,
    id: HirId,
}

/// Per-function state, saved and reset at lambda and item boundaries.
#[derive(Clone, Debug, Default)]
struct FnCtx {
    self_local: Option<LocalId>,
    is_shuttle: bool,
    loops: Vec<LoopFrame>,
    /// Hole-lambda parameters being collected, innermost last.
    hole_frames: Vec<Vec<LocalId>>,
}

struct Resolver<'a> {
    db: &'a mut SourceDb,
    loader: &'a dyn Loader,
    p: Program,
    diags: Vec<Diagnostic>,
    /// Diagnostics of `const if` branches being lowered, innermost last.
    deferred: Vec<Vec<Diagnostic>>,
    asts: Vec<Option<ast::Module>>,
    item_defs: Vec<Vec<ItemDefs>>,
    pending_uses: Vec<PendingUse>,
    /// The names being imported in the current round of `resolve_imports`.
    in_flight: Vec<(ModuleId, String)>,
    std_root: ModuleId,
    user_modules: HashMap<Vec<String>, ModuleId>,
    prelude: IndexMap<String, Binding>,
    /// `print` → `std.io.print`, for "add a `use`" hints.
    std_index: HashMap<String, String>,
    /// Every function def, known from collection on (before it's lowered), so
    /// `f[T]` can be told apart from indexing.
    fn_defs: HashSet<DefId>,

    // Lowering state.
    module: ModuleId,
    ribs: Vec<Rib>,
    fx: FnCtx,
    /// Whether `Self` means something here.
    self_type: bool,
    /// Inside the condition of an `if`, `while` or guard, where `is` bindings
    /// flow on into the `and` chain and the body.
    in_cond: bool,
}

impl<'a> Resolver<'a> {
    fn new(db: &'a mut SourceDb, loader: &'a dyn Loader) -> Self {
        let mut p = Program::default();
        let std_root = ModuleId(0);
        p.modules.push(empty_module("std".into(), None, None));
        Resolver {
            db,
            loader,
            p,
            diags: Vec::new(),
            deferred: Vec::new(),
            asts: vec![None],
            item_defs: vec![Vec::new()],
            pending_uses: Vec::new(),
            in_flight: Vec::new(),
            std_root,
            user_modules: HashMap::new(),
            prelude: IndexMap::new(),
            std_index: HashMap::new(),
            fn_defs: HashSet::new(),
            module: std_root,
            ribs: Vec::new(),
            fx: FnCtx::default(),
            self_type: false,
            in_cond: false,
        }
    }

    fn run(&mut self, root: FileId) {
        let root_name = self.db.file(root).name().to_string();
        let stem = std::path::Path::new(&root_name).file_stem().and_then(|s| s.to_str()).unwrap_or("main").to_string();
        let root_module = self.add_file_module(stem, None, root);
        self.p.root = Some(root_module);

        for (path, source) in self.loader.std_modules() {
            self.load_std_module(&path, source);
        }

        self.resolve_imports();
        self.build_prelude();

        let mut m = 0;
        while m < self.p.modules.len() {
            self.lower_module(ModuleId(m as u32));
            m += 1;
        }
    }

    // ------------------------------------------------------------ diagnostics

    fn report(&mut self, diag: Diagnostic) {
        match self.deferred.last_mut() {
            Some(buffer) => buffer.push(diag),
            None => self.diags.push(diag),
        }
    }

    fn error(&mut self, code: loom_syntax::diag::Code, span: Span, message: impl Into<String>, label: &str) {
        self.report(Diagnostic::error(code, message).primary(span, label));
    }

    // ------------------------------------------------------------ ids

    fn next_hir_id(&mut self) -> HirId {
        let id = HirId(self.p.hir_ids);
        self.p.hir_ids += 1;
        id
    }

    fn new_local(&mut self, name: &str, span: Span, mutable: bool, kind: LocalKind) -> LocalId {
        let id = LocalId(self.p.locals.len() as u32);
        self.p.locals.push(Local { name: name.to_string(), span, mutable, kind });
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn new_def(
        &mut self,
        name: &str,
        span: Span,
        vis: Vis,
        module: ModuleId,
        parent: Option<DefId>,
        docs: &[String],
        is_type: bool,
    ) -> DefId {
        let id = DefId(self.p.defs.len() as u32);
        self.p.defs.push(Def {
            name: name.to_string(),
            span,
            vis,
            module,
            parent,
            docs: docs.to_vec(),
            attrs: Vec::new(),
            is_type,
            kind: DefKind::Pending,
        });
        id
    }

    // ------------------------------------------------------------ loading

    fn add_module(&mut self, path: String, file: Option<FileId>, parent: Option<ModuleId>, name: &str) -> ModuleId {
        let id = ModuleId(self.p.modules.len() as u32);
        self.p.modules.push(empty_module(path, file, parent));
        self.asts.push(None);
        self.item_defs.push(Vec::new());
        if let Some(parent) = parent {
            self.p.modules[parent.index()].children.insert(name.to_string(), id);
        }
        id
    }

    /// Parses `file` as a new module and collects its items.
    fn add_file_module(&mut self, path: String, parent: Option<ModuleId>, file: FileId) -> ModuleId {
        let name = path.rsplit('.').next().unwrap_or(&path).to_string();
        let id = self.add_module(path, Some(file), parent, &name);
        let parsed = parse(self.db, file);
        self.diags.extend(parsed.diagnostics);
        let defs = self.collect_module(id, &parsed.module);
        self.asts[id.index()] = Some(parsed.module);
        self.item_defs[id.index()] = defs;
        id
    }

    fn load_std_module(&mut self, path: &[String], source: Source) {
        let mut parent = self.std_root;
        let mut dotted = String::from("std");
        for (i, segment) in path.iter().enumerate() {
            dotted.push('.');
            dotted.push_str(segment);
            if i + 1 == path.len() {
                let file = self.db.add(source.name.clone(), source.text.clone());
                self.add_file_module(dotted.clone(), Some(parent), file);
            } else {
                parent = match self.p.module(parent).children.get(segment) {
                    Some(&child) => child,
                    None => self.add_module(dotted.clone(), None, Some(parent), segment),
                };
            }
        }
    }

    /// The user module at `path`, loading it (and queuing its imports) on first use.
    fn user_module(&mut self, path: &[String]) -> Option<ModuleId> {
        if let Some(&id) = self.user_modules.get(path) {
            return Some(id);
        }
        let parent = match path.len() {
            1 => None,
            n => Some(self.user_module(&path[..n - 1])?),
        };
        let dotted = path.join(".");
        let name = path.last().cloned().unwrap_or_default();
        let id = match self.loader.user_module(path)? {
            UserModule::Dir => self.add_module(dotted, None, parent, &name),
            UserModule::File(source) => {
                let file = self.db.add(source.name, source.text);
                self.add_file_module(dotted, parent, file)
            }
        };
        self.user_modules.insert(path.to_vec(), id);
        Some(id)
    }

    // ------------------------------------------------------------ collection

    fn collect_module(&mut self, m: ModuleId, module: &ast::Module) -> Vec<ItemDefs> {
        let mut out = Vec::with_capacity(module.items.len());
        for item in &module.items {
            if let ast::ItemKind::Use(tree) = &item.kind {
                let mut prefix = Vec::new();
                self.queue_uses(m, tree, &mut prefix, item.vis == Vis::Pub);
                out.push(ItemDefs::default());
                continue;
            }
            let defs = self.collect_item(m, item);
            if let Some(def) = defs.def {
                self.p.modules[m.index()].decls.push(def);
                self.declare_in_module(m, def);
            }
            if matches!(item.kind, ast::ItemKind::Extern(_)) {
                for def in defs.members.iter().flatten() {
                    self.p.modules[m.index()].decls.push(*def);
                    self.declare_in_module(m, *def);
                }
            }
            out.push(defs);
        }
        out
    }

    fn queue_uses(&mut self, m: ModuleId, tree: &UseTree, prefix: &mut Vec<Ident>, public: bool) {
        let depth = prefix.len();
        prefix.extend(tree.path.iter().cloned());
        match &tree.kind {
            UseKind::Single { alias } => {
                self.pending_uses.push(PendingUse { module: m, path: prefix.clone(), alias: alias.clone(), public })
            }
            UseKind::Group(trees) => {
                for tree in trees {
                    self.queue_uses(m, tree, prefix, public);
                }
            }
        }
        prefix.truncate(depth);
    }

    fn declare_in_module(&mut self, m: ModuleId, def: DefId) {
        let name = self.p.def(def).name.clone();
        if name.is_empty() {
            return;
        }
        if let Some(&first) = self.p.module(m).items.get(&name) {
            let (first_span, span) = (self.p.def(first).span, self.p.def(def).span);
            self.report(
                Diagnostic::error(codes::N0104, format!("`{name}` is defined twice"))
                    .primary(span, "defined again here")
                    .secondary(first_span, "first defined here"),
            );
            return;
        }
        self.p.modules[m.index()].items.insert(name, def);
    }

    /// Makes the defs of one item (and its members). Nothing is resolved yet.
    fn collect_item(&mut self, m: ModuleId, item: &ast::Item) -> ItemDefs {
        use ast::ItemKind as K;
        let docs = &item.docs;
        match &item.kind {
            K::Use(_) => ItemDefs::default(),
            K::Const(c) => self.one(m, &c.name.name, c.name.span, item.vis, docs, false),
            K::Type(t) => self.one(m, &t.name.name, t.name.span, item.vis, docs, true),
            K::Fn(f) => {
                let defs = self.one(m, &f.name.name, f.name.span, item.vis, docs, false);
                self.fn_defs.extend(defs.def);
                defs
            }
            K::ConstExpr(e) => self.one(m, "", e.span, Vis::Private, docs, false),
            K::Warp(w) => {
                let def = self.new_def(&w.name.name, w.name.span, item.vis, m, None, docs, true);
                let members = match &w.body {
                    ast::WarpBody::Members(members) => self.collect_members(m, def, members),
                    ast::WarpBody::Tuple(fields) => fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let vis = item.vis;
                            vec![self.new_def(&i.to_string(), f.span, vis, m, Some(def), &[], false)]
                        })
                        .collect(),
                };
                ItemDefs { def: Some(def), members }
            }
            K::Weft(w) => {
                let def = self.new_def(&w.name.name, w.name.span, item.vis, m, None, docs, true);
                let members = self.collect_members(m, def, &w.members);
                ItemDefs { def: Some(def), members }
            }
            K::Weave(w) => {
                let def = self.new_def("", w.weft.span, item.vis, m, None, docs, false);
                let members = self.collect_members(m, def, &w.members);
                ItemDefs { def: Some(def), members }
            }
            K::Extern(block) => {
                let members = block
                    .members
                    .iter()
                    .map(|member| match &member.kind {
                        ast::MemberKind::Fn(f) => {
                            let def = self.new_def(&f.name.name, f.name.span, member.vis, m, None, &member.docs, false);
                            self.fn_defs.insert(def);
                            vec![def]
                        }
                        _ => Vec::new(),
                    })
                    .collect();
                ItemDefs { def: None, members }
            }
        }
    }

    fn one(&mut self, m: ModuleId, name: &str, span: Span, vis: Vis, docs: &[String], is_type: bool) -> ItemDefs {
        let def = self.new_def(name, span, vis, m, None, docs, is_type);
        ItemDefs { def: Some(def), members: Vec::new() }
    }

    fn collect_members(&mut self, m: ModuleId, parent: DefId, members: &[ast::Member]) -> Vec<Vec<DefId>> {
        use ast::MemberKind as K;
        let mut seen: HashMap<String, Span> = HashMap::new();
        let mut out = Vec::with_capacity(members.len());
        for member in members {
            let names: Vec<&Ident> = match &member.kind {
                K::Field(f) | K::Through(f) => vec![&f.name],
                K::Cases(cases) => cases.iter().map(|c| &c.name).collect(),
                K::Const(c) => vec![&c.name],
                K::Type(t) => vec![&t.name],
                K::Fn(f) => vec![&f.name],
                K::ConstExpr(_) => Vec::new(),
            };
            let is_type = matches!(member.kind, K::Type(_));
            let mut defs = Vec::new();
            for name in names {
                if let Some(&first) = seen.get(&name.name) {
                    self.report(
                        Diagnostic::error(codes::N0104, format!("`{}` is defined twice", name.name))
                            .primary(name.span, "defined again here")
                            .secondary(first, "first defined here"),
                    );
                } else {
                    seen.insert(name.name.clone(), name.span);
                }
                let def = self.new_def(&name.name, name.span, member.vis, m, Some(parent), &member.docs, is_type);
                if matches!(member.kind, K::Fn(_)) {
                    self.fn_defs.insert(def);
                }
                defs.push(def);
            }
            if let K::ConstExpr(e) = &member.kind {
                defs.push(self.new_def("", e.span, Vis::Private, m, Some(parent), &[], false));
            }
            out.push(defs);
        }
        out
    }

    // ------------------------------------------------------------ imports

    fn resolve_imports(&mut self) {
        loop {
            let pending = std::mem::take(&mut self.pending_uses);
            if pending.is_empty() {
                return;
            }
            // Everything unresolved as this round starts: a use that fails to
            // find a name must retry if one of these could still bind it.
            self.in_flight = pending.iter().map(|u| (u.module, binding_name(u).name.clone())).collect();
            let mut progress = false;
            let mut retry = Vec::new();
            for u in pending {
                match self.resolve_use(&u) {
                    UseOutcome::Done(binding) => {
                        progress = true;
                        self.bind_import(&u, binding);
                    }
                    UseOutcome::Retry => retry.push(u),
                    UseOutcome::Failed(diag) => {
                        progress = true;
                        self.report(diag);
                    }
                }
            }
            // Loading a user module may have queued more uses.
            progress |= !self.pending_uses.is_empty();
            self.pending_uses.extend(retry);
            if !progress {
                for u in std::mem::take(&mut self.pending_uses) {
                    let (module, name) = self.use_target_for_error(&u);
                    let diag = Diagnostic::error(codes::N0105, format!("there's no `{}` in `{module}`", name.name))
                        .primary(name.span, "not found");
                    self.report(diag);
                }
                return;
            }
        }
    }

    fn use_target_for_error<'u>(&self, u: &'u PendingUse) -> (String, &'u Ident) {
        let name = u.path.last().expect("use paths are never empty");
        let module = u.path[..u.path.len() - 1].iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(".");
        (module, name)
    }

    fn resolve_use(&mut self, u: &PendingUse) -> UseOutcome {
        let first = &u.path[0];
        let mut current = if first.name == "std" {
            Binding::Module(self.std_root)
        } else {
            match self.user_module(std::slice::from_ref(&first.name)) {
                Some(m) => Binding::Module(m),
                None => {
                    return UseOutcome::Failed(
                        Diagnostic::error(codes::N0102, format!("there's no module `{}`", first.name))
                            .primary(first.span, "unknown module")
                            .help(format!(
                                "`use` paths start with `std` or with a file next to this one (`{}.loom`)",
                                first.name
                            )),
                    );
                }
            }
        };
        let mut user_path = vec![first.name.clone()];
        for segment in &u.path[1..] {
            let Binding::Module(m) = current else {
                let what = match current {
                    Binding::Def(d) => self.p.def(d).kind.describe(),
                    Binding::Module(_) => "module",
                };
                return UseOutcome::Failed(
                    Diagnostic::error(codes::N0105, format!("`use` can only import from modules, not from a {what}"))
                        .primary(segment.span, "can't import this"),
                );
            };
            user_path.push(segment.name.clone());
            if let Some(&child) = self.p.module(m).children.get(&segment.name) {
                current = Binding::Module(child);
                continue;
            }
            if first.name != "std"
                && self.p.module(m).file.is_none()
                && let Some(child) = self.user_module(&user_path)
            {
                current = Binding::Module(child);
                continue;
            }
            match self.module_member(m, &segment.name, u.module) {
                Ok(Some(binding)) => current = binding,
                Ok(None) => {
                    if self.pending_uses_define(m, &segment.name) {
                        return UseOutcome::Retry;
                    }
                    let path = self.p.module(m).path.clone();
                    return UseOutcome::Failed(
                        Diagnostic::error(codes::N0105, format!("there's no `{}` in `{path}`", segment.name))
                            .primary(segment.span, "not found"),
                    );
                }
                Err(def) => return UseOutcome::Failed(self.private_error(def, segment.span)),
            }
        }
        UseOutcome::Done(current)
    }

    /// Whether a not-yet-resolved `use` in `m` could still bind `name`.
    fn pending_uses_define(&self, m: ModuleId, name: &str) -> bool {
        self.in_flight.iter().any(|(module, n)| *module == m && n == name)
            || self.pending_uses.iter().any(|u| u.module == m && binding_name(u).name == name)
    }

    /// `name` inside module `m`, as seen from module `from`. `Err` holds a
    /// private definition that exists but can't be used from there.
    fn module_member(&self, m: ModuleId, name: &str, from: ModuleId) -> Result<Option<Binding>, DefId> {
        let module = self.p.module(m);
        if let Some(&child) = module.children.get(name) {
            return Ok(Some(Binding::Module(child)));
        }
        if let Some(&def) = module.items.get(name) {
            if m != from && self.p.def(def).vis == Vis::Private {
                return Err(def);
            }
            return Ok(Some(Binding::Def(def)));
        }
        if let Some(import) = module.imports.get(name)
            && (import.public || m == from)
        {
            return Ok(Some(import.target));
        }
        Ok(None)
    }

    fn private_error(&self, def: DefId, span: Span) -> Diagnostic {
        let d = self.p.def(def);
        Diagnostic::error(codes::N0103, format!("`{}` is private to `{}`", d.name, self.p.module(d.module).path))
            .primary(span, "private")
            .secondary(d.span, "declared here without `pub`")
    }

    fn bind_import(&mut self, u: &PendingUse, target: Binding) {
        let name = binding_name(u).clone();
        let module = self.p.module(u.module);
        if let Some(&def) = module.items.get(&name.name) {
            let diag = Diagnostic::error(codes::N0110, format!("`{}` is already defined in this module", name.name))
                .primary(name.span, "imported here")
                .secondary(self.p.def(def).span, "defined here")
                .help("import it under another name with `as`");
            self.report(diag);
            return;
        }
        if let Some(existing) = module.imports.get(&name.name) {
            if existing.target != target {
                let diag = Diagnostic::error(codes::N0110, format!("`{}` is imported twice", name.name))
                    .primary(name.span, "imported again here")
                    .secondary(existing.span, "first imported here")
                    .help("import one of them under another name with `as`");
                self.report(diag);
            }
            return;
        }
        let import = Import { target, span: name.span, public: u.public };
        self.p.modules[u.module.index()].imports.insert(name.name, import);
    }

    fn build_prelude(&mut self) {
        let Some(&prelude) = self.p.module(self.std_root).children.get("prelude") else { return };
        let module = self.p.module(prelude);
        let mut names = IndexMap::new();
        for (name, &def) in &module.items {
            if self.p.def(def).vis == Vis::Pub {
                names.insert(name.clone(), Binding::Def(def));
            }
        }
        for (name, import) in &module.imports {
            if import.public {
                names.insert(name.clone(), import.target);
            }
        }
        self.prelude = names;

        // Where each public std item lives, for "add a `use`" hints.
        for module in &self.p.modules {
            if !module.path.starts_with("std.") || module.path == "std.prelude" {
                continue;
            }
            for (name, &def) in &module.items {
                if self.p.def(def).vis == Vis::Pub {
                    self.std_index.entry(name.clone()).or_insert_with(|| format!("{}.{name}", module.path));
                }
            }
        }
    }

    // ------------------------------------------------------------ scopes

    fn push_rib(&mut self, kind: RibKind) {
        self.ribs.push(Rib { kind, names: HashMap::new() });
    }

    fn pop_rib(&mut self) {
        self.ribs.pop();
    }

    fn bind(&mut self, name: &str, res: Res) {
        if let Some(rib) = self.ribs.last_mut() {
            rib.names.insert(name.to_string(), res);
        }
    }

    /// Looks a name up through the scopes, then the module, its imports, the
    /// prelude, and finally the names built into the compiler.
    fn lookup(&self, name: &str) -> Option<Res> {
        let mut past_item = false;
        for rib in self.ribs.iter().rev() {
            if let Some(&res) = rib.names.get(name)
                && !(past_item && matches!(res, Res::Local(_)))
            {
                return Some(res);
            }
            past_item |= rib.kind == RibKind::Item;
        }
        let module = self.p.module(self.module);
        if let Some(&def) = module.items.get(name) {
            return Some(Res::Def(def));
        }
        if let Some(import) = module.imports.get(name) {
            return Some(import.target.into());
        }
        if let Some(&binding) = self.prelude.get(name) {
            return Some(binding.into());
        }
        match name {
            "std" => Some(Res::Module(self.std_root)),
            "target" => Some(Res::Intrinsic(Intrinsic::Target)),
            "heap" => Some(Res::Intrinsic(Intrinsic::Heap)),
            _ => None,
        }
    }

    /// Reports an unknown name, with a hint when `std` has something by that name.
    fn unknown_name(&mut self, ident: &Ident) {
        let mut diag = Diagnostic::error(codes::N0101, format!("there's no `{}` in scope", ident.name))
            .primary(ident.span, "unknown name");
        if let Some(path) = self.std_index.get(&ident.name) {
            diag = diag.help(format!("it's in `std`: add `use {path}`"));
        }
        self.report(diag);
    }

    /// A member of a module, as seen from the module being lowered; loads user
    /// modules on demand and reports what's missing or private.
    fn module_member_or_report(&mut self, m: ModuleId, ident: &Ident) -> Option<Binding> {
        if self.p.module(m).file.is_none()
            && !self.p.module(m).path.starts_with("std")
            && !self.p.module(m).children.contains_key(&ident.name)
        {
            let mut path: Vec<String> = self.p.module(m).path.split('.').map(str::to_string).collect();
            path.push(ident.name.clone());
            if let Some(child) = self.user_module(&path) {
                self.resolve_imports();
                return Some(Binding::Module(child));
            }
        }
        match self.module_member(m, &ident.name, self.module) {
            Ok(Some(binding)) => Some(binding),
            Ok(None) => {
                let path = self.p.module(m).path.clone();
                self.error(codes::N0105, ident.span, format!("there's no `{}` in `{path}`", ident.name), "not found");
                None
            }
            Err(def) => {
                let diag = self.private_error(def, ident.span);
                self.report(diag);
                None
            }
        }
    }

    // ------------------------------------------------------------ modules

    fn lower_module(&mut self, m: ModuleId) {
        let Some(module) = self.asts[m.index()].take() else { return };
        let defs = std::mem::take(&mut self.item_defs[m.index()]);
        self.module = m;
        for (item, defs) in module.items.iter().zip(&defs) {
            self.lower_item(item, defs);
        }
    }
}

/// The name a `use` binds: its alias, or the last segment.
fn binding_name(u: &PendingUse) -> &Ident {
    u.alias.as_ref().or(u.path.last()).expect("use paths are never empty")
}

fn empty_module(path: String, file: Option<FileId>, parent: Option<ModuleId>) -> Module {
    Module {
        path,
        file,
        parent,
        children: IndexMap::new(),
        items: IndexMap::new(),
        imports: IndexMap::new(),
        decls: Vec::new(),
    }
}
