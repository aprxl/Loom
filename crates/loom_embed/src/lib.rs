//! Loom as a library, on Treadle: a Rust host registers functions and
//! constants, compiles Loom source that uses them and runs its `pub`
//! functions by name.
//!
//! ```
//! use loom_embed::Embed;
//!
//! let mut embed = Embed::new("editor");
//! embed.fun2("add", |a: i64, b: i64| a + b)?;
//! embed.fun("tick", &[], None, |_cx, _r: &mut [u64]| -> Result<(), String> { Ok(()) })?;
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
//! `pub const` for each constant, then the source added with
//! [`Embed::source`]. Only integers, `bool` and `char` cross, one register
//! each (see [`Scalar`]).
//!
//! A [`Program`] can be started any number of times. Each registration
//! keeps a factory that clones the closure, so each run owns its closures
//! and a call is one indirect call. A closure's captured state must be
//! `Clone`; state the runs share goes in an `Arc<Mutex<…>>`.

mod scalar;

use std::collections::HashSet;
use std::fmt;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::{MemoryLoader, Source};
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_isa::Image;
use loom_syntax::{Diagnostic, SourceDb, TokenKind};
use loom_sys::{Host, Outcome};
use loom_vm::{Config, HostCx, HostFn, HostTable, Vm};

pub use scalar::Scalar;

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
type Factory = Box<dyn Fn() -> HostFn + Send>;

struct Registration {
    name: String,
    params: Vec<String>,
    ret: Option<String>,
    factory: Factory,
}

/// What a program can call, collected before it is compiled.
pub struct Embed {
    module: String,
    funs: Vec<Registration>,
    consts: Vec<(String, String, String)>,
    source: String,
    names: HashSet<String>,
}

impl fmt::Debug for Embed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embed").field("module", &self.module).field("names", &self.names).finish_non_exhaustive()
    }
}

/// Whether `name` is one Loom name of the right case: `snake_case` for a
/// function, `SCREAMING_CASE` for a constant. No keyword, type or space.
fn is_name(name: &str, constant: bool) -> bool {
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    let mut db = SourceDb::new();
    let file = db.add("<name>", name);
    let lexed = loom_syntax::lex(&db, file);
    let kinds: Vec<&TokenKind> = lexed.tokens.iter().map(|t| &t.kind).collect();
    let wanted = if constant { TokenKind::ConstIdent } else { TokenKind::Ident };
    !lexed.has_errors() && matches!(kinds.as_slice(), [kind, TokenKind::Eof] if **kind == wanted)
}

macro_rules! typed_funs {
    ($($method:ident($($ty:ident => $i:tt),*);)*) => {$(
        /// Registers a function of scalars; its closure may keep state, which each run gets a copy of.
        pub fn $method<$($ty: Scalar,)* R: Scalar, F>(&mut self, name: &str, f: F) -> Result<(), EmbedError>
        where
            F: FnMut($($ty),*) -> R + Clone + Send + 'static,
        {
            self.add(name, vec![$($ty::LOOM),*], Some(R::LOOM), move || {
                let mut f = f.clone();
                Box::new(move |_cx: &mut HostCx<'_>, r: &mut [u64]| {
                    r[0] = f($($ty::from_reg(r[$i])),*).to_reg();
                    Ok(())
                })
            })
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
            source: String::new(),
            names: HashSet::new(),
        }
    }

    fn claim(&mut self, name: &str, constant: bool) -> Result<(), EmbedError> {
        if !is_name(&self.module, false) {
            return Err(EmbedError(format!("`{}` is not a module name", self.module)));
        }
        if !is_name(name, constant) {
            return Err(EmbedError(format!("`{name}` is not a Loom name")));
        }
        if !self.names.insert(name.to_string()) {
            return Err(EmbedError(format!("`{name}` is registered twice")));
        }
        Ok(())
    }

    fn add<I>(&mut self, name: &str, params: Vec<&str>, ret: Option<&str>, make: I) -> Result<(), EmbedError>
    where
        I: Fn() -> HostFn + Send + 'static,
    {
        if let Some(bad) = params.iter().find(|t| !scalar::TYPES.contains(t) && **t != "str") {
            return Err(EmbedError(format!("`{bad}` is not a type that crosses into Loom")));
        }
        if let Some(bad) = ret.filter(|t| !scalar::TYPES.contains(t) && *t != "String") {
            return Err(EmbedError(format!("`{bad}` is not a type that crosses into Loom")));
        }
        self.claim(name, false)?;
        let params = params.into_iter().map(str::to_string).collect();
        self.funs.push(Registration {
            name: name.to_string(),
            params,
            ret: ret.map(str::to_string),
            factory: Box::new(make),
        });
        Ok(())
    }

    typed_funs! {
        fun0();
        fun1(A => 0);
        fun2(A => 0, B => 1);
        fun3(A => 0, B => 1, C => 2);
    }

    /// Registers a function over registers: it gets one for each of `params`
    /// (Loom type names) and writes its result, if `ret` names one, to the first.
    pub fn fun<F>(&mut self, name: &str, params: &[&str], ret: Option<&str>, f: F) -> Result<(), EmbedError>
    where
        F: FnMut(&mut HostCx<'_>, &mut [u64]) -> Result<(), String> + Clone + Send + 'static,
    {
        self.add(name, params.to_vec(), ret, move || Box::new(f.clone()))
    }

    /// Declares `pub const name: T = value` in the module.
    pub fn constant<T: Scalar>(&mut self, name: &str, value: T) -> Result<(), EmbedError> {
        self.claim(name, true)?;
        self.consts.push((name.to_string(), T::LOOM.to_string(), scalar::literal(value)));
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
                let params: Vec<String> = f.params.iter().enumerate().map(|(i, t)| format!("a{i}: {t}")).collect();
                let ret = f.ret.as_ref().map_or(String::new(), |t| format!(" -> {t}"));
                text.push_str(&format!("    pub fun {}({}){ret}\n", f.name, params.join(", ")));
            }
            text.push_str("}\n\n");
        }
        for (name, ty, value) in &self.consts {
            text.push_str(&format!("pub const {name}: {ty} = {value}\n"));
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
