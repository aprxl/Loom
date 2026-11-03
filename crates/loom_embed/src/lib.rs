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
//! `pub const` for each constant, the unit enums, value warps and handles'
//! warps, the wefts, then the source added with [`Embed::source`].
//!
//! A value warp ([`Embed::warp`]) is a struct of scalars: a host function takes
//! and gives it as the address of a copy, and its methods read and write its
//! fields through a [`Layout`]. A unit enum ([`Embed::unit_enum`]) crosses as the
//! index of its case. [`Embed::constant_warp`] declares a constant of a value
//! warp, [`Embed::weft`] declares a weft, and [`Embed::implement`] makes a value
//! warp satisfy it with its host methods. [`Embed::show_via`] makes a value warp
//! show through one of them.
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
use loom_syntax::diag::codes;
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
pub type Factory = Box<dyn Fn(&[Layout]) -> HostFn + Send>;

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

/// A warp of scalar fields, whose values the host reads and writes by address.
struct ValueWarp {
    name: String,
    fields: Vec<(String, String)>,
    sizes: Vec<u64>,
    methods: Vec<String>,
    wefts: Vec<String>,
    cases: Vec<CaseDecl>,
    woven: Vec<String>,
}

/// A case of a payload enum: its name, its fields (name and Loom type) and their sizes.
struct CaseDecl {
    name: String,
    fields: Vec<(String, String)>,
    sizes: Vec<u64>,
}

/// A weft declared with [`Embed::weft`]: its methods (name, parameter types and
/// result) and its Loom text.
struct WeftDecl {
    name: String,
    methods: Vec<(String, Vec<String>, String)>,
    text: String,
}

/// A value warp declared with [`Embed::warp`]; its methods are registered with
/// [`Embed::warp_method`].
#[derive(Clone, Debug)]
pub struct WarpRef {
    index: usize,
    name: String,
}

/// A field's byte offset and size.
type Field = (usize, usize);

/// Where a value's bytes are: a struct's fields (each field's byte offset and size),
/// or a payload enum's tag and each case's fields, and the bytes the whole value
/// spans. A host closure gets its warp's layout, which the compiler sets, and reads
/// and writes with plain values.
#[derive(Clone, Debug)]
pub struct Layout {
    span: usize,
    fields: Box<[Field]>,
    tag: Field,
    cases: Box<[Box<[Field]>]>,
}

impl Layout {
    fn new(offsets: &[u64], sizes: &[u64]) -> Layout {
        let fields: Box<[(usize, usize)]> =
            offsets.iter().zip(sizes).map(|(at, size)| (*at as usize, *size as usize)).collect();
        let span = fields.iter().map(|(at, size)| at + size).max().unwrap_or(0);
        Layout { span, fields, tag: (0, 0), cases: Box::default() }
    }

    fn cases(tag: (u64, u8), offsets: &[Vec<u64>], sizes: &[Vec<u64>]) -> Layout {
        let cases: Box<[Box<[Field]>]> = offsets
            .iter()
            .zip(sizes)
            .map(|(at, size)| {
                at.iter().zip(size).map(|(at, size)| (*at as usize, *size as usize)).collect::<Box<[_]>>()
            })
            .collect();
        let tag = (tag.0 as usize, tag.1 as usize);
        let span = cases.iter().flat_map(|c| c.iter()).map(|(at, size)| at + size).chain([tag.0 + tag.1]).max();
        Layout { span: span.unwrap_or(0), fields: Box::default(), tag, cases }
    }

    /// The bytes of the whole value at `addr`, read in place: one checked call.
    pub fn bytes<'c>(&self, cx: &'c HostCx<'_>, addr: u64) -> Result<&'c [u8], String> {
        cx.bytes(addr, self.span as u64)
    }

    /// The bytes of the whole value at `addr`, to write in place.
    pub fn bytes_mut<'c>(&self, cx: &'c mut HostCx<'_>, addr: u64) -> Result<&'c mut [u8], String> {
        cx.bytes_mut(addr, self.span as u64)
    }

    /// Field `field`, decoded from the bytes of [`bytes`](Self::bytes). Panics if
    /// `field` is not one of the warp's fields.
    pub fn read<T: Scalar>(&self, bytes: &[u8], field: usize) -> T {
        let (at, size) = self.fields[field];
        T::from_reg(read_le(&bytes[at..at + size], size))
    }

    /// Writes field `field` into the bytes of [`bytes_mut`](Self::bytes_mut).
    pub fn write<T: Scalar>(&self, bytes: &mut [u8], field: usize, value: T) {
        let (at, size) = self.fields[field];
        write_le(&mut bytes[at..at + size], size, value.to_reg());
    }

    /// The case of a payload enum's value, from the bytes of [`bytes`](Self::bytes).
    pub fn case(&self, bytes: &[u8]) -> usize {
        let (at, size) = self.tag;
        read_le(&bytes[at..at + size], size) as usize
    }

    /// Makes a payload enum's value the case `case`, by its tag.
    pub fn set_case(&self, bytes: &mut [u8], case: usize) {
        let (at, size) = self.tag;
        write_le(&mut bytes[at..at + size], size, case as u64);
    }

    /// Field `field` of case `case` of a payload enum's value, read as `T`. Panics
    /// if there is no such case or field.
    pub fn read_case<T: Scalar>(&self, bytes: &[u8], case: usize, field: usize) -> T {
        let (at, size) = self.cases[case][field];
        T::from_reg(read_le(&bytes[at..at + size], size))
    }

    /// Writes field `field` of case `case` of a payload enum's value.
    pub fn write_case<T: Scalar>(&self, bytes: &mut [u8], case: usize, field: usize, value: T) {
        let (at, size) = self.cases[case][field];
        write_le(&mut bytes[at..at + size], size, value.to_reg());
    }
}

/// The `size` bytes of `raw` as a little-endian register, zero-extended.
fn read_le(raw: &[u8], size: usize) -> u64 {
    match size {
        1 => u64::from(raw[0]),
        2 => u64::from(u16::from_le_bytes(raw.try_into().unwrap_or_default())),
        4 => u64::from(u32::from_le_bytes(raw.try_into().unwrap_or_default())),
        _ => u64::from_le_bytes(raw.try_into().unwrap_or_default()),
    }
}

/// The low `size` bytes of `value`, little-endian, into `raw`.
fn write_le(raw: &mut [u8], size: usize, value: u64) {
    match size {
        1 => raw[0] = value as u8,
        2 => raw.copy_from_slice(&(value as u16).to_le_bytes()),
        4 => raw.copy_from_slice(&(value as u32).to_le_bytes()),
        _ => raw.copy_from_slice(&value.to_le_bytes()),
    }
}

/// A scalar's value as a Loom literal, for [`Embed::constant_warp`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Literal {
    ty: &'static str,
    text: String,
}

impl Literal {
    /// `value` as a literal of its Loom type; a float with no literal (NaN,
    /// infinity) is an error.
    pub fn of<T: Scalar>(value: T) -> Result<Literal, EmbedError> {
        let text = value.literal().ok_or_else(|| EmbedError(format!("a {} has no Loom literal", T::LOOM)))?;
        Ok(Literal { ty: T::LOOM, text })
    }
}

/// The size in bytes of a scalar type, if it is one.
fn scalar_size(ty: &str) -> Option<u64> {
    Some(match ty {
        "i8" | "u8" | "bool" => 1,
        "i16" | "u16" => 2,
        "i32" | "u32" | "f32" | "char" => 4,
        "i64" | "u64" | "f64" => 8,
        _ => return None,
    })
}

/// What a program can call, collected before it is compiled.
pub struct Embed {
    module: String,
    funs: Vec<Registration>,
    consts: Vec<(String, String, String)>,
    warps: Vec<Warp>,
    value_warps: Vec<ValueWarp>,
    enums: Vec<(String, Vec<String>)>,
    wefts: Vec<WeftDecl>,
    weaves: Vec<String>,
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
            self.add_factory(&ext, types.clone(), Some("u64"), Box::new(move |_: &[Layout]| {
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
            value_warps: Vec::new(),
            enums: Vec::new(),
            wefts: Vec::new(),
            weaves: Vec::new(),
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

    /// Whether a type may cross: a scalar, `str` (or `String` as a result), or a value warp.
    fn crosses(&self, t: &str, result: bool) -> bool {
        scalar::TYPES.contains(&t)
            || if result { t == "String" } else { t == "str" }
            || self.value_warps.iter().any(|w| w.name == t)
            || self.enums.iter().any(|(name, _)| name == t)
    }

    fn add_factory(
        &mut self,
        name: &str,
        params: Vec<&str>,
        ret: Option<&str>,
        factory: Factory,
    ) -> Result<(), EmbedError> {
        if let Some(bad) = params.iter().find(|t| !self.crosses(t, false)) {
            return Err(EmbedError(format!("`{bad}` is not a type that crosses into Loom")));
        }
        if let Some(bad) = ret.filter(|t| !self.crosses(t, true)) {
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
        self.add_factory(name, params.to_vec(), ret, Box::new(move |_: &[Layout]| Box::new(f.clone())))
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

    /// Declares a value warp `name` with scalar `fields` (name and Loom type),
    /// whose values the host reads and writes by address (see [`Layout`]).
    pub fn warp(&mut self, name: &str, fields: &[(&str, &str)]) -> Result<WarpRef, EmbedError> {
        let mut sizes = Vec::with_capacity(fields.len());
        for (field, ty) in fields {
            if !lexes_as(field, &TokenKind::Ident) {
                return Err(EmbedError(format!("`{field}` is not a field name")));
            }
            sizes.push(scalar_size(ty).ok_or_else(|| EmbedError(format!("`{ty}` is not a scalar field type")))?);
        }
        self.claim(name, &TokenKind::TypeIdent)?;
        let fields = fields.iter().map(|(f, t)| (f.to_string(), t.to_string())).collect();
        self.value_warps.push(ValueWarp {
            name: name.to_string(),
            fields,
            sizes,
            methods: Vec::new(),
            wefts: Vec::new(),
            cases: Vec::new(),
            woven: Vec::new(),
        });
        Ok(WarpRef { index: self.value_warps.len() - 1, name: name.to_string() })
    }

    /// Declares a payload enum `name` (`pub warp name { case … }`), whose values the
    /// host reads and writes by address, like a value warp. Each case is a name and
    /// its fields (name and scalar type). Its methods come from
    /// [`Embed::warp_method`], and the closures read it with [`Layout::case`] and
    /// [`Layout::read_case`].
    pub fn enum_warp(&mut self, name: &str, cases: &[(&str, &[(&str, &str)])]) -> Result<WarpRef, EmbedError> {
        if cases.is_empty() {
            return Err(EmbedError(format!("`{name}` has no cases")));
        }
        let mut decls = Vec::with_capacity(cases.len());
        for (case, fields) in cases {
            if !lexes_as(case, &TokenKind::TypeIdent) {
                return Err(EmbedError(format!("`{case}` is not a case name")));
            }
            let mut sizes = Vec::with_capacity(fields.len());
            for (field, ty) in *fields {
                if !lexes_as(field, &TokenKind::Ident) {
                    return Err(EmbedError(format!("`{field}` is not a field name")));
                }
                sizes.push(scalar_size(ty).ok_or_else(|| EmbedError(format!("`{ty}` is not a scalar field type")))?);
            }
            let fields = fields.iter().map(|(f, t)| (f.to_string(), t.to_string())).collect();
            decls.push(CaseDecl { name: case.to_string(), fields, sizes });
        }
        self.claim(name, &TokenKind::TypeIdent)?;
        self.value_warps.push(ValueWarp {
            name: name.to_string(),
            fields: Vec::new(),
            sizes: Vec::new(),
            methods: Vec::new(),
            wefts: Vec::new(),
            cases: decls,
            woven: Vec::new(),
        });
        Ok(WarpRef { index: self.value_warps.len() - 1, name: name.to_string() })
    }

    /// Registers a method of the value warp `w`: the Loom method `name` on `self`,
    /// passing `self` and `params` (their addresses for a warp) to the host. A warp
    /// result is written to a slot the host gets as `r[0]`, and `self` is `r[1]`,
    /// the parameters after it (raw registers, as in [`Embed::raw_fun`]). The
    /// closure gets the warp's [`Layout`].
    pub fn warp_method<F>(
        &mut self,
        w: &WarpRef,
        name: &str,
        params: &[&str],
        ret: &str,
        f: F,
    ) -> Result<(), EmbedError>
    where
        F: FnMut(&mut HostCx<'_>, &mut [u64], &Layout) -> Result<(), String> + Clone + Send + 'static,
    {
        let ext = format!("{}_{name}", w.name.to_lowercase());
        let mut types: Vec<&str> = vec![w.name.as_str()];
        types.extend(params.iter().copied());
        let index = w.index;
        let make = move |layouts: &[Layout]| -> HostFn {
            let layout = layouts[index].clone();
            let mut f = f.clone();
            Box::new(move |cx: &mut HostCx<'_>, r: &mut [u64]| f(cx, r, &layout))
        };
        self.add_factory(&ext, types, Some(ret), Box::new(make))?;
        let sep = if params.is_empty() { "" } else { ", " };
        let text = format!(
            "    pub fun {name}(self{sep}{}) -> {ret} {{\n        {ext}(self{sep}{})\n    }}\n",
            params_text(params),
            args_text(params.len()),
        );
        self.value_warps[w.index].methods.push(text);
        Ok(())
    }

    /// Declares `pub warp name { case … }` in the module: a unit enum that a host
    /// function takes and gives as its tag, the index of the case. The Rust type
    /// of the same name implements [`Scalar`], with `LOOM` `name` and `to_reg`
    /// the index.
    pub fn unit_enum(&mut self, name: &str, cases: &[&str]) -> Result<(), EmbedError> {
        if cases.is_empty() {
            return Err(EmbedError(format!("`{name}` has no cases")));
        }
        if let Some(bad) = cases.iter().find(|c| !lexes_as(c, &TokenKind::TypeIdent)) {
            return Err(EmbedError(format!("`{bad}` is not a case name")));
        }
        self.claim(name, &TokenKind::TypeIdent)?;
        self.enums.push((name.to_string(), cases.iter().map(|c| c.to_string()).collect()));
        Ok(())
    }

    /// Declares `pub const name: W = W(field: value, …)` in the module, for a value
    /// warp `w`: `values` gives each of its fields a [`Literal`] of the field's type.
    pub fn constant_warp(&mut self, w: &WarpRef, name: &str, values: &[(&str, Literal)]) -> Result<(), EmbedError> {
        let warp = &self.value_warps[w.index];
        let mut parts = Vec::with_capacity(values.len());
        for (field, literal) in values {
            let Some((_, ty)) = warp.fields.iter().find(|(f, _)| f == field) else {
                return Err(EmbedError(format!("`{}` has no field `{field}`", warp.name)));
            };
            if ty.as_str() != literal.ty {
                return Err(EmbedError(format!("field `{field}` of `{}` is {ty}, not {}", warp.name, literal.ty)));
            }
            parts.push(format!("{field}: {}", literal.text));
        }
        if warp.fields.iter().any(|(f, _)| !values.iter().any(|(v, _)| v == f)) {
            return Err(EmbedError(format!("a value of `{}` needs every field", warp.name)));
        }
        let value = format!("{}({})", warp.name, parts.join(", "));
        self.claim(name, &TokenKind::ConstIdent)?;
        self.consts.push((name.to_string(), w.name.clone(), value));
        Ok(())
    }

    /// Declares `pub weft name { fun … }` in the module, with a method for each of
    /// `methods`: its name, its parameter types (Loom names) and its result (`""`
    /// for none). A value warp that implements it (see [`Embed::implement`]) gives
    /// each method with [`Embed::warp_method`].
    pub fn weft(&mut self, name: &str, methods: &[(&str, &[&str], &str)]) -> Result<(), EmbedError> {
        for (method, params, ret) in methods {
            if !lexes_as(method, &TokenKind::Ident) {
                return Err(EmbedError(format!("`{method}` is not a method name")));
            }
            if let Some(bad) = params.iter().find(|t| !self.crosses(t, false)) {
                return Err(EmbedError(format!("`{bad}` is not a type that crosses into Loom")));
            }
            if !ret.is_empty() && !self.crosses(ret, true) {
                return Err(EmbedError(format!("`{ret}` is not a type that crosses into Loom")));
            }
        }
        self.claim(name, &TokenKind::TypeIdent)?;
        let mut text = format!("pub weft {name} {{\n");
        for (method, params, ret) in methods {
            let sep = if params.is_empty() { "" } else { ", " };
            let ret = if ret.is_empty() { String::new() } else { format!(" -> {ret}") };
            text.push_str(&format!("    fun {method}(self{sep}{}){ret}\n", params_text(params)));
        }
        text.push_str("}\n\n");
        let methods = methods
            .iter()
            .map(|(m, params, ret)| (m.to_string(), params.iter().map(|t| t.to_string()).collect(), ret.to_string()))
            .collect();
        self.wefts.push(WeftDecl { name: name.to_string(), methods, text });
        Ok(())
    }

    /// Makes the value warp `w` implement the weft `weft`: its methods are the ones
    /// [`Embed::warp_method`] gave it, and an `any` of the weft takes it.
    pub fn implement(&mut self, w: &WarpRef, weft: &str) -> Result<(), EmbedError> {
        if !self.wefts.iter().any(|d| d.name == weft) {
            return Err(EmbedError(format!("`{weft}` is not a weft of this module")));
        }
        let warp = &mut self.value_warps[w.index];
        if warp.wefts.iter().any(|n| n == weft) {
            return Err(EmbedError(format!("`{}` already implements `{weft}`", warp.name)));
        }
        warp.wefts.push(weft.to_string());
        Ok(())
    }

    /// Makes the value warp `w` a weave of the weft `weft` through host methods:
    /// `methods` maps each of the weft's methods to the warp method that gives it
    /// (see [`Embed::warp_method`]). The weave is `weave weft into w`, and each of
    /// its methods calls its target.
    pub fn weave(&mut self, w: &WarpRef, weft: &str, methods: &[(&str, &str)]) -> Result<(), EmbedError> {
        let Some(decl) = self.wefts.iter().find(|d| d.name == weft) else {
            return Err(EmbedError(format!("`{weft}` is not a weft of this module")));
        };
        let warp = &self.value_warps[w.index];
        if warp.wefts.iter().chain(&warp.woven).any(|n| n == weft) {
            return Err(EmbedError(format!("`{}` already implements `{weft}`", warp.name)));
        }
        let mut text = format!("weave {weft} into {} {{\n", warp.name);
        for (method, params, ret) in &decl.methods {
            let Some((_, target)) = methods.iter().find(|(m, _)| *m == method.as_str()) else {
                return Err(EmbedError(format!("`{weft}.{method}` has no host method to give it")));
            };
            let types: Vec<&str> = params.iter().map(String::as_str).collect();
            let sep = if types.is_empty() { "" } else { ", " };
            let ret = if ret.is_empty() { String::new() } else { format!(" -> {ret}") };
            text.push_str(&format!(
                "    fun {method}(self{sep}{}){ret} {{\n        self.{target}({})\n    }}\n",
                params_text(&types),
                args_text(types.len()),
            ));
        }
        text.push_str("}\n\n");
        self.value_warps[w.index].woven.push(weft.to_string());
        self.weaves.push(text);
        Ok(())
    }

    /// Makes the value warp `w` show through its host method `method`, a method
    /// that gives a `String` (see [`Embed::warp_method`]): `Show`'s `show` writes
    /// that text, so `{w}` in a template and `print(w)` show it.
    pub fn show_via(&mut self, w: &WarpRef, method: &str) -> Result<(), EmbedError> {
        if !lexes_as(method, &TokenKind::Ident) {
            return Err(EmbedError(format!("`{method}` is not a method name")));
        }
        let warp = &mut self.value_warps[w.index];
        if warp.wefts.iter().any(|n| n == "Show") {
            return Err(EmbedError(format!("`{}` already shows", warp.name)));
        }
        warp.wefts.push("Show".to_string());
        warp.methods.push(format!("    fun show(self, mut f: Fmt) {{\n        f.put(self.{method}())\n    }}\n"));
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
        if self.value_warps.iter().any(|w| w.wefts.iter().any(|n| n == "Show")) {
            text.push_str("use std.fmt.{Fmt, Show}\n\n");
        }
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
        for weft in &self.wefts {
            text.push_str(&weft.text);
        }
        for (name, cases) in &self.enums {
            let cases: String = cases.iter().map(|c| format!("    case {c}\n")).collect();
            text.push_str(&format!("pub warp {name} {{\n{cases}}}\n\n"));
        }
        for warp in &self.value_warps {
            let header = if warp.wefts.is_empty() { String::new() } else { format!(": {}", warp.wefts.join(", ")) };
            let methods: String = warp.methods.concat();
            if warp.cases.is_empty() {
                let fields: Vec<String> = warp.fields.iter().map(|(f, t)| format!("    pub {f}: {t}")).collect();
                let sep = if methods.is_empty() { "\n" } else { ",\n" };
                text.push_str(&format!(
                    "pub warp {}{header} {{\n{}{sep}{methods}}}\n\n",
                    warp.name,
                    fields.join(",\n")
                ));
            } else {
                let cases: String = warp
                    .cases
                    .iter()
                    .map(|c| {
                        let fields: Vec<String> = c.fields.iter().map(|(f, t)| format!("{f}: {t}")).collect();
                        if fields.is_empty() {
                            format!("    case {}\n", c.name)
                        } else {
                            format!("    case {}({})\n", c.name, fields.join(", "))
                        }
                    })
                    .collect();
                text.push_str(&format!("pub warp {}{header} {{\n{cases}{methods}}}\n\n", warp.name));
            }
        }
        for woven in &self.weaves {
            text.push_str(woven);
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
        let mut layouts = Vec::with_capacity(self.value_warps.len());
        for w in &self.value_warps {
            let no_layout = || vec![Diagnostic::error(codes::U0102, format!("`{}` has no layout to read by", w.name))];
            if w.cases.is_empty() {
                let offsets = treadle::warp_field_offsets(&resolved.program, &typed, &self.module, &w.name)
                    .ok_or_else(no_layout)?;
                layouts.push(Layout::new(&offsets, &w.sizes));
            } else {
                let case_offsets = treadle::warp_case_offsets(&resolved.program, &typed, &self.module, &w.name)
                    .ok_or_else(no_layout)?;
                let sizes: Vec<Vec<u64>> = w.cases.iter().map(|c| c.sizes.clone()).collect();
                layouts.push(Layout::cases(case_offsets.tag, &case_offsets.cases, &sizes));
            }
        }
        let image = treadle::compile_exports(&db, &resolved.program, &typed, Level::O2)?.image;
        let hosts = self.funs.into_iter().map(|f| (format!("{}.{}", self.module, f.name), f.factory)).collect();
        Ok(Program { image, hosts, layouts })
    }
}

/// A compiled program, which can be started any number of times.
pub struct Program {
    image: Image,
    hosts: Vec<(String, Factory)>,
    layouts: Vec<Layout>,
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

    /// A new run, with a fresh copy of every registered closure.
    pub fn start(&self, host: Host) -> Result<Running<'_>, Outcome> {
        let mut table = HostTable::default();
        for (key, make) in &self.hosts {
            table.register(key, make(&self.layouts)).map_err(|message| Outcome::Crash { message, location: None })?;
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
