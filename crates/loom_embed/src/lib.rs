//! Loom as a library, on Treadle: a Rust host registers functions, constants
//! and handles, compiles Loom source that uses them and runs its `pub`
//! functions by name.
//!
//! ```
//! use loom_embed::Embed;
//!
//! let mut embed = Embed::new("editor");
//! embed.fun("add", |a: i64, b: i64| a + b)?;
//! embed.fun("cat", |s: &str, n: i64| s.repeat(n as usize))?;
//! embed.raw_fun("tick", &[], None, |_cx, _r: &mut [u64]| -> Result<(), String> { Ok(()) })?;
//! embed.constant("TAB_WIDTH", 4_i64)?;
//! embed.source("pub fun width() -> i64 { TAB_WIDTH * 2 }")?;
//! let program = embed
//!     .compile("use editor.{add, width}\npub fun calc(a: i64, b: i64) -> i64 { add(a, b) + width() }")
//!     .map_err(|d| format!("{d:?}"))?;
//! let mut run = program.start(loom_sys::Host::stdout(vec!["main".into()])).map_err(|o| format!("{o:?}"))?;
//! assert_eq!(run.call("calc", &[2, 3]).map_err(|o| format!("{o:?}"))?, [13]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! What a registration declares becomes a Loom module of the given name
//! that the program `use`s: an `extern "host"` block for the functions, a
//! `pub const` for each constant, the handles' warps, then the source added
//! with [`Embed::source`].
//!
//! A typed closure takes scalars (integers, `bool`, `char`, `f32`, `f64`) and
//! `&str`, and returns a scalar or a `String`; its annotated parameter types
//! pick the registration, so no turbofish is needed. A scalar is one register,
//! a `&str` is an address and a length, and a `String` result is written by
//! the host into a slot that comes first. [`Embed::raw_fun`] gives the
//! registers themselves, with a [`HostCx`].
//!
//! A [`Program`] can be started any number of times. Each registration keeps
//! a factory that clones the closure, so each run owns its closures and a
//! call is one indirect call. A closure's captured state must be `Clone`. A
//! handle's values are shared by every run of its program and live until the
//! program is dropped: v1 has no way to release one.

mod scalar;
mod typed;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::{MemoryLoader, Source};
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_isa::Image;
use loom_syntax::{Diagnostic, SourceDb, TokenKind};
use loom_sys::{Host, Outcome};
use loom_vm::{Config, HostCx, HostFn, HostTable, Vm};

pub use scalar::{Out, Scalar};
pub use typed::{IntoHost, IntoMethod, Num, Text};

/// A registration that can't be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbedError(pub String);

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EmbedError {}

/// Makes a run's own copy of a host function.
#[doc(hidden)]
pub type Factory = Box<dyn Fn() -> HostFn + Send>;

struct Registration {
    name: String,
    params: Vec<String>,
    ret: Option<String>,
    factory: Factory,
}

/// A handle's warp, and the Loom methods declared in it.
struct Warp {
    name: String,
    methods: Vec<String>,
}

/// What a handle's warp keeps: the values the host made, by id.
#[doc(hidden)]
pub struct HandleTable<T> {
    next: u64,
    pub(crate) items: HashMap<u64, T>,
}

impl<T> fmt::Debug for HandleTable<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandleTable").field("next", &self.next).finish_non_exhaustive()
    }
}

impl<T> HandleTable<T> {
    fn insert(&mut self, value: T) -> u64 {
        let id = self.next;
        self.next += 1;
        self.items.insert(id, value);
        id
    }
}

/// A handle type declared with [`Embed::handle`]: its values are kept by the
/// host, and the Loom warp holds only their id.
pub struct Handle<T> {
    index: usize,
    warp: String,
    table: Arc<Mutex<HandleTable<T>>>,
}

impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle").field("warp", &self.warp).finish_non_exhaustive()
    }
}

/// What a program can call, collected before it is compiled.
pub struct Embed {
    module: String,
    funs: Vec<Registration>,
    consts: Vec<(String, String, String)>,
    warps: Vec<Warp>,
    functions: Vec<String>,
    source: String,
    names: HashSet<String>,
}

impl fmt::Debug for Embed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embed").field("module", &self.module).field("names", &self.names).finish_non_exhaustive()
    }
}

/// Whether `name` lexes as one token of `kind`.
fn lexes_as(name: &str, kind: &TokenKind) -> bool {
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    let mut db = SourceDb::new();
    let file = db.add("<name>", name);
    let lexed = loom_syntax::lex(&db, file);
    let kinds: Vec<&TokenKind> = lexed.tokens.iter().map(|t| &t.kind).collect();
    !lexed.has_errors() && matches!(kinds.as_slice(), [k, TokenKind::Eof] if *k == kind)
}

/// `a0: i64, a1: f64`, for the parameters of a generated Loom function.
fn params_text(types: &[&str]) -> String {
    types.iter().enumerate().map(|(i, t)| format!("a{i}: {t}")).collect::<Vec<_>>().join(", ")
}

/// `a0, a1`, the arguments of a generated Loom function.
fn args_text(count: usize) -> String {
    (0..count).map(|i| format!("a{i}")).collect::<Vec<_>>().join(", ")
}

macro_rules! constructors {
    ($($method:ident($($ty:ident => $i:tt),*);)*) => {$(
        /// Registers a constructor of `h`'s warp: `f` makes the value, which the table keeps under the id the Loom function `name` returns.
        pub fn $method<T: Send + 'static, $($ty: Scalar,)* F>(&mut self, h: &Handle<T>, name: &str, f: F) -> Result<(), EmbedError>
        where
            F: FnMut($($ty),*) -> T + Clone + Send + 'static,
        {
            let ext = format!("{}_{name}", h.warp.to_lowercase());
            self.claim(name, &TokenKind::Ident)?;
            let table = Arc::clone(&h.table);
            let types: Vec<&str> = vec![$(<$ty as Scalar>::LOOM),*];
            self.add_factory(&ext, types.clone(), Some("u64"), Box::new(move || {
                let mut f = f.clone();
                let table = Arc::clone(&table);
                Box::new(move |_cx: &mut HostCx<'_>, r: &mut [u64]| {
                    let value = f($(<$ty as Scalar>::from_reg(r[$i])),*);
                    r[0] = table.lock().map_err(|_| "a handle's table is poisoned".to_string())?.insert(value);
                    Ok(())
                })
            }))?;
            let (params, args) = (params_text(&types), args_text(types.len()));
            self.functions.push(format!(
                "pub fun {name}({params}) -> {warp} {{\n    {warp}(id: {ext}({args}))\n}}\n",
                warp = h.warp,
            ));
            Ok(())
        }
    )*};
}

impl Embed {
    /// A module named `module`, which programs `use`.
    pub fn new(module: &str) -> Embed {
        Embed {
            module: module.to_string(),
            funs: Vec::new(),
            consts: Vec::new(),
            warps: Vec::new(),
            functions: Vec::new(),
            source: String::new(),
            names: HashSet::new(),
        }
    }

    fn claim(&mut self, name: &str, kind: &TokenKind) -> Result<(), EmbedError> {
        if !lexes_as(&self.module, &TokenKind::Ident) {
            return Err(EmbedError(format!("`{}` is not a module name", self.module)));
        }
        if !lexes_as(name, kind) {
            return Err(EmbedError(format!("`{name}` is not a Loom name of the right case")));
        }
        if !self.names.insert(name.to_string()) {
            return Err(EmbedError(format!("`{name}` is registered twice")));
        }
        Ok(())
    }

    fn add_factory(
        &mut self,
        name: &str,
        params: Vec<&str>,
        ret: Option<&str>,
        factory: Factory,
    ) -> Result<(), EmbedError> {
        if let Some(bad) = params.iter().find(|t| !scalar::TYPES.contains(t) && **t != "str") {
            return Err(EmbedError(format!("`{bad}` is not a type that crosses into Loom")));
        }
        if let Some(bad) = ret.filter(|t| !scalar::TYPES.contains(t) && *t != "String") {
            return Err(EmbedError(format!("`{bad}` is not a type that crosses into Loom")));
        }
        self.claim(name, &TokenKind::Ident)?;
        self.funs.push(Registration {
            name: name.to_string(),
            params: params.into_iter().map(str::to_string).collect(),
            ret: ret.map(str::to_string),
            factory,
        });
        Ok(())
    }

    /// Registers a function from a closure: its annotated parameters are its
    /// Loom parameters (`i64`, `f32`, `&str` and the like), its return value
    /// its result (a scalar, or a `String`).
    ///
    /// An unannotated `&str` parameter is not inferred, so the closure needs
    /// its type:
    ///
    /// ```compile_fail
    /// let mut embed = loom_embed::Embed::new("m");
    /// embed.fun("len", |s| s.len() as u64).ok();
    /// ```
    pub fn fun<M, F: IntoHost<M>>(&mut self, name: &str, f: F) -> Result<(), EmbedError> {
        self.add_factory(name, F::params(), Some(F::ret()), f.factory())
    }

    /// Registers a function over registers: it gets one for each of `params`
    /// (Loom type names), reads a `str` as two registers (see [`HostCx::text`]),
    /// and writes its result, if `ret` names one, to the first register; a
    /// `String` result is written into a slot that is the first register and
    /// the parameters follow it.
    pub fn raw_fun<F>(&mut self, name: &str, params: &[&str], ret: Option<&str>, f: F) -> Result<(), EmbedError>
    where
        F: FnMut(&mut HostCx<'_>, &mut [u64]) -> Result<(), String> + Clone + Send + 'static,
    {
        self.add_factory(name, params.to_vec(), ret, Box::new(move || Box::new(f.clone())))
    }

    constructors! {
        constructor0();
        constructor1(A => 0);
        constructor2(A => 0, B => 1);
    }

    /// Declares a handle type `name` (a `PascalCase` Loom warp): its values
    /// are kept by the host, and the Loom warp holds only their id.
    pub fn handle<T: Send + 'static>(&mut self, name: &str) -> Result<Handle<T>, EmbedError> {
        self.claim(name, &TokenKind::TypeIdent)?;
        self.warps.push(Warp { name: name.to_string(), methods: Vec::new() });
        Ok(Handle {
            index: self.warps.len() - 1,
            warp: name.to_string(),
            table: Arc::new(Mutex::new(HandleTable { next: 1, items: HashMap::new() })),
        })
    }

    /// Registers a method of `h`'s warp from a closure whose first parameter
    /// is the value the id names; a stale id is an error.
    pub fn method<T, M, F: IntoMethod<T, M>>(&mut self, h: &Handle<T>, name: &str, f: F) -> Result<(), EmbedError> {
        let ext = format!("{}_{name}", h.warp.to_lowercase());
        let params = F::params();
        let mut types: Vec<&str> = vec!["u64"];
        types.extend(params.iter().copied());
        self.add_factory(&ext, types, Some(F::ret()), f.factory(Arc::clone(&h.table)))?;
        let sep = if params.is_empty() { "" } else { ", " };
        let text = format!(
            "    pub fun {name}(self{sep}{}) -> {} {{\n        {ext}(self.id{sep}{})\n    }}\n",
            params_text(&params),
            F::ret(),
            args_text(params.len()),
        );
        self.warps[h.index].methods.push(text);
        Ok(())
    }

    /// Declares `pub const name: T = value` in the module.
    pub fn constant<T: Scalar>(&mut self, name: &str, value: T) -> Result<(), EmbedError> {
        let literal = value.literal().ok_or_else(|| EmbedError(format!("`{name}` has no Loom literal")))?;
        self.claim(name, &TokenKind::ConstIdent)?;
        self.consts.push((name.to_string(), T::LOOM.to_string(), literal));
        Ok(())
    }

    /// Adds Loom source to the module, after the host functions and constants.
    pub fn source(&mut self, text: &str) -> Result<(), EmbedError> {
        self.source.push_str(text);
        self.source.push('\n');
        Ok(())
    }

    fn module_text(&self) -> String {
        let mut text = String::new();
        if !self.funs.is_empty() {
            text.push_str("extern \"host\" {\n");
            for f in &self.funs {
                let types: Vec<&str> = f.params.iter().map(String::as_str).collect();
                let ret = f.ret.as_ref().map_or(String::new(), |t| format!(" -> {t}"));
                text.push_str(&format!("    pub fun {}({}){ret}\n", f.name, params_text(&types)));
            }
            text.push_str("}\n\n");
        }
        for (name, ty, value) in &self.consts {
            text.push_str(&format!("pub const {name}: {ty} = {value}\n"));
        }
        for warp in &self.warps {
            text.push_str(&format!("pub warp {} {{\n    id: u64\n", warp.name));
            for method in &warp.methods {
                text.push_str(method);
            }
            text.push_str("}\n\n");
        }
        for function in &self.functions {
            text.push_str(function);
            text.push('\n');
        }
        text.push('\n');
        text.push_str(&self.source);
        text
    }

    /// Compiles `user`, a Loom program with no `main` whose `pub` functions
    /// are called by [`Running::call`], for Treadle. It may `use` the module.
    pub fn compile(self, user: &str) -> Result<Program, Vec<Diagnostic>> {
        let module = Source { name: format!("<{}>", self.module), text: self.module_text() };
        let loader = MemoryLoader { user: vec![(vec![self.module.clone()], module)] };
        let mut db = SourceDb::new();
        let file = db.add("<program>", user);
        let resolved = resolve(&mut db, file, &loader);
        if resolved.has_errors() {
            return Err(resolved.diagnostics);
        }
        let typed = typeck::check_for(&resolved.program, Target::VM);
        if typed.has_errors() {
            return Err(typed.diagnostics);
        }
        let image = treadle::compile_exports(&db, &resolved.program, &typed, Level::O2)?.image;
        let hosts = self.funs.into_iter().map(|f| (format!("{}.{}", self.module, f.name), f.factory)).collect();
        Ok(Program { image, hosts })
    }
}

/// A compiled program, which can be started any number of times.
pub struct Program {
    image: Image,
    hosts: Vec<(String, Factory)>,
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program").field("functions", &self.image.funcs.len()).finish_non_exhaustive()
    }
}

impl Program {
    /// The bytecode.
    pub fn image(&self) -> &Image {
        &self.image
    }

    /// A new run, with a copy of every registered closure.
    pub fn start(&self, host: Host) -> Result<Running<'_>, Outcome> {
        let mut table = HostTable::default();
        for (key, make) in &self.hosts {
            table.register(key, make()).map_err(|message| Outcome::Crash { message, location: None })?;
        }
        let vm = Vm::new_hosted(&self.image, host, &Config::default(), table)?;
        Ok(Running { vm, image: &self.image })
    }
}

/// A started program, whose functions can be called one after another.
#[derive(Debug)]
pub struct Running<'p> {
    vm: Vm<'p>,
    image: &'p Image,
}

impl Running<'_> {
    /// Calls the plain, non-generic function `name` with `args` (one
    /// register each, see [`Scalar::to_reg`]) and gives its results.
    pub fn call(&mut self, name: &str, args: &[u64]) -> Result<Vec<u64>, Outcome> {
        let crash = |message: String| Outcome::Crash { message, location: None };
        let found = self.image.funcs.iter().position(|f| f.name == name && !f.name.contains('['));
        let Some(index) = found else { return Err(crash(format!("no function `{name}`"))) };
        let params = usize::from(self.image.funcs[index].params);
        if args.len() != params {
            return Err(crash(format!("`{name}` takes {params} arguments, not {}", args.len())));
        }
        self.vm.call(index as u32, args)
    }
}
