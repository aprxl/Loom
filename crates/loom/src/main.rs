//! The `loom` command-line tool.
//!
//! - `check` parses, resolves and type-checks a program.
//! - `build` compiles it to a Lua bundle (plus a source map), or dumps a
//!   stage with `--emit tokens|ast|hir|lua|heddle|bytecode` (the last two
//!   are Treadle's, Loom's own VM).
//! - `run` compiles the program for Treadle and runs it in this process. With
//!   `--backend lua` it writes the Lua bundle to a temporary file and runs it
//!   in a child process (`loom lua-host`) instead, whose exit code it passes on.
//! - `test` compiles a file's tests (and only then are they compiled) and
//!   runs each on Treadle, in one VM; with `--backend lua`, in a child
//!   process, which prints a report.
//! - `docs` opens the documentation, which is built into the binary.
//! - `lua-host` (hidden) runs a bundle, or a test bundle's tests, and
//!   reports how it ended, with Loom source locations.

mod alloc;
mod docs;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::{self, Resolved};
use loom_compiler::lower::{self, Bundle};
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::treadle;
use loom_compiler::typeck::{self, Typed};
use loom_host::{Host, Output};
use loom_runner::{EXIT_FAILED, EXIT_INTERNAL, describe, exit_code, worker_count};
use loom_syntax::diag::{ColorMode, MAX_SHOWN, emit_to_stderr, report_tail};
use loom_syntax::token::dump_tokens;
use loom_syntax::{Diagnostic, FileId, SourceDb, lex, parse, pretty};

#[global_allocator]
static ALLOCATOR: alloc::Classes = alloc::Classes;

/// Exit codes (see design/lua-backend.md).
const EXIT_COMPILE_ERROR: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "loom",
    version,
    about = "The Loom compiler: runs programs on Treadle, its own VM, or builds them for Lua 5.4"
)]
struct Cli {
    /// When to color diagnostics.
    #[arg(long, value_enum, default_value_t = Color::Auto, global = true)]
    color: Color,

    /// How much Treadle's code is optimized: `-O0` not at all (the fastest
    /// compile, every call kept), `-O2` fully (design/vm.md §7).
    #[arg(short = 'O', value_enum, default_value_t = Opt::O2, global = true)]
    opt: Opt,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Transpile a Loom file to Lua (or dump an intermediate stage with --emit).
    Build {
        file: PathBuf,
        /// Where to write the output (default: stdout for --emit, `<file>.lua` otherwise).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Stop after a stage and print it instead of writing Lua.
        #[arg(long, value_enum, conflicts_with_all = ["image", "exe"])]
        emit: Option<Emit>,
        /// Write Treadle's compiled image (`<file>.lmi`, which `loom run` runs) instead of Lua.
        #[arg(long, conflicts_with = "exe")]
        image: bool,
        /// Write a standalone executable: Treadle and the compiled image, without the compiler.
        #[arg(long)]
        exe: bool,
    },
    /// Compile a Loom file and run it (on Treadle, or with `--backend lua` on the bundled Lua VM).
    Run {
        file: PathBuf,
        /// What runs the program: Treadle, Loom's own VM, or Lua.
        #[arg(long, value_enum, default_value_t = Backend::Vm)]
        backend: Backend,
        /// Arguments passed to the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Check a Loom file for errors without writing anything.
    Check { file: PathBuf },
    /// Run a file's tests (`test name() { … }`).
    Test {
        file: PathBuf,
        /// What runs the tests: Treadle, Loom's own VM, or Lua.
        #[arg(long, value_enum, default_value_t = Backend::Vm)]
        backend: Backend,
    },
    /// Open the documentation (the language and std) in your browser. It's built in, so it works offline.
    Docs {
        /// Only print where the docs are; don't open a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Run a Lua bundle in the bundled VM (used internally by `loom run` and `loom test`).
    #[command(hide = true)]
    LuaHost {
        bundle: PathBuf,
        /// The bundle holds tests: run each and print a report.
        #[arg(long)]
        test: bool,
        /// The program's name, the first of `env.args()`.
        #[arg(long)]
        program: Option<String>,
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Emit {
    /// The token stream, one token per line.
    Tokens,
    /// The syntax tree, as S-expressions.
    Ast,
    /// The program after name resolution, as S-expressions.
    Hir,
    /// The Lua bundle.
    Lua,
    /// Treadle's IR, Heddle (design/vm.md).
    Heddle,
    /// Treadle's bytecode.
    Bytecode,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    /// Treadle, Loom's own VM (the default).
    Vm,
    /// Lua 5.4, bundled.
    Lua,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Opt {
    #[value(name = "0")]
    O0,
    #[value(name = "2")]
    O2,
}

impl From<Opt> for Level {
    fn from(o: Opt) -> Self {
        match o {
            Opt::O0 => Level::O0,
            Opt::O2 => Level::O2,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Color {
    Auto,
    Always,
    Never,
}

impl From<Color> for ColorMode {
    fn from(c: Color) -> Self {
        match c {
            Color::Auto => ColorMode::Auto,
            Color::Always => ColorMode::Always,
            Color::Never => ColorMode::Never,
        }
    }
}

/// Stack for the thread that runs the compiler. The front end is recursive
/// descent, and the main thread's default stack (1 MiB on Windows) is too
/// small for input nested up to the parser's limit in debug builds. This is
/// reserved address space; pages are only committed as they're used.
const COMPILER_STACK: usize = 64 * 1024 * 1024;

fn main() -> ExitCode {
    // A panic is a bug in Loom: the terminal goes back as it was found (the
    // thread's `Term` isn't dropped by a panic that ends it first), the
    // message is printed, and the exit code is the internal error's.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        loom_sys::term::restore_terminal();
        default_hook(info);
    }));
    let cli = Cli::parse();
    let spawned = std::thread::Builder::new().name("loom".into()).stack_size(COMPILER_STACK).spawn(move || run(cli));
    match spawned {
        // A panic has already been printed by the panic hook.
        Ok(handle) => handle.join().unwrap_or(ExitCode::from(EXIT_INTERNAL)),
        Err(e) => {
            eprintln!("error: couldn't start the compiler thread: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> ExitCode {
    let color = ColorMode::from(cli.color);
    let level = cli.opt.into();
    match cli.command {
        Command::Build { file, out, emit, image, exe } => {
            let product = if exe {
                Product::Exe
            } else if image {
                Product::Image
            } else {
                Product::Lua
            };
            build(&file, out.as_deref(), emit, product, color, level)
        }
        Command::Check { file } => check(&file, color),
        Command::Run { file, backend: Backend::Lua, args } => run_file(&file, &args, color),
        Command::Run { file, backend: Backend::Vm, args } if file.extension().is_some_and(|e| e == IMAGE_EXTENSION) => {
            run_image(&file, args)
        }
        Command::Run { file, backend: Backend::Vm, args } => run_vm(&file, &args, color, level),
        Command::Test { file, backend: Backend::Lua } => test_file(&file, color),
        Command::Test { file, backend: Backend::Vm } => test_vm(&file, color, level),
        Command::Docs { no_open } => docs::docs(!no_open),
        Command::LuaHost { bundle, test: true, .. } => lua_host_tests(&bundle),
        Command::LuaHost { bundle, test: false, program, args } => lua_host(&bundle, program, args),
    }
}

/// Loads `path` into a fresh source database.
fn load(path: &Path) -> Result<(SourceDb, FileId), ExitCode> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mut db = SourceDb::new();
            let id = db.add(path.display().to_string(), text);
            Ok((db, id))
        }
        Err(e) => {
            eprintln!("error: can't read `{}`: {e}", path.display());
            Err(ExitCode::from(EXIT_COMPILE_ERROR))
        }
    }
}

/// Reports diagnostics; `Err` if any of them is an error.
fn report(
    db: &SourceDb,
    path: &Path,
    diagnostics: &[Diagnostic],
    stage: &str,
    color: ColorMode,
) -> Result<(), ExitCode> {
    let shown = &diagnostics[..diagnostics.len().min(MAX_SHOWN)];
    if !shown.is_empty()
        && let Err(e) = emit_to_stderr(db, shown, color)
    {
        eprintln!("error: couldn't write diagnostics: {e}");
    }
    let (tail, failed) = report_tail(&path.display().to_string(), diagnostics, stage);
    eprint!("{tail}");
    if failed { Err(ExitCode::from(EXIT_COMPILE_ERROR)) } else { Ok(()) }
}

/// Parses and resolves the program rooted at `path`. When a file has syntax
/// errors only those are reported, since resolving broken code would mostly
/// add noise.
fn resolve(path: &Path, color: ColorMode) -> Result<(SourceDb, Resolved), ExitCode> {
    let (mut db, id) = load(path)?;
    let resolved = hir::resolve(&mut db, id, &FsLoader::new(path));
    let syntax_errors = resolved.diagnostics.iter().any(|d| d.is_error() && d.code.id.starts_with('S'));
    if syntax_errors {
        let syntax: Vec<Diagnostic> =
            resolved.diagnostics.iter().filter(|d| d.code.id.starts_with('S')).cloned().collect();
        report(&db, path, &syntax, "parse", color)?;
    }
    report(&db, path, &resolved.diagnostics, "resolve", color)?;
    Ok((db, resolved))
}

/// Resolves and type-checks the program rooted at `path`, for Lua.
fn typecheck(path: &Path, color: ColorMode) -> Result<(SourceDb, Resolved, Typed), ExitCode> {
    typecheck_for(path, color, Target::LUA)
}

/// Resolves and type-checks the program rooted at `path` for `target`.
fn typecheck_for(path: &Path, color: ColorMode, target: Target) -> Result<(SourceDb, Resolved, Typed), ExitCode> {
    let (db, resolved) = resolve(path, color)?;
    let typed = typeck::check_for(&resolved.program, target);
    report(&db, path, &typed.diagnostics, "type-check", color)?;
    Ok((db, resolved, typed))
}

fn check(path: &Path, color: ColorMode) -> ExitCode {
    let run = || -> Result<(), ExitCode> {
        let (db, resolved, typed) = typecheck(path, color)?;
        let p = &resolved.program;
        report(&db, path, &lower::check(&db, p, &typed), "compile", color)?;
        let n = p.root.map_or(0, |root| p.module(root).decls.len());
        println!("{}: {n} item{}, no errors", path.display(), if n == 1 { "" } else { "s" });
        Ok(())
    };
    run().err().unwrap_or(ExitCode::SUCCESS)
}

/// Compiles the program rooted at `path` to a Lua bundle.
fn compile(path: &Path, color: ColorMode) -> Result<(SourceDb, Bundle), ExitCode> {
    compile_with(path, color, lower::lower)
}

type Lowering = fn(&SourceDb, &hir::Program, &Typed) -> Result<Bundle, Vec<Diagnostic>>;

fn compile_with(path: &Path, color: ColorMode, lowering: Lowering) -> Result<(SourceDb, Bundle), ExitCode> {
    let (db, resolved, typed) = typecheck(path, color)?;
    match lowering(&db, &resolved.program, &typed) {
        Ok(bundle) => Ok((db, bundle)),
        Err(diagnostics) => {
            report(&db, path, &diagnostics, "compile", color)?;
            Err(ExitCode::from(EXIT_COMPILE_ERROR))
        }
    }
}

fn build(
    path: &Path,
    out: Option<&Path>,
    emit: Option<Emit>,
    product: Product,
    color: ColorMode,
    level: Level,
) -> ExitCode {
    let run = || -> Result<(), ExitCode> {
        let (db, id) = load(path)?;
        let text = match emit {
            Some(Emit::Tokens) => {
                let lexed = lex(&db, id);
                report(&db, path, &lexed.diagnostics, "lex", color)?;
                dump_tokens(&db, &lexed.tokens)
            }
            Some(Emit::Ast) => {
                let parsed = parse(&db, id);
                report(&db, path, &parsed.diagnostics, "parse", color)?;
                pretty::module(&parsed.module)
            }
            Some(Emit::Hir) => {
                let (_, resolved) = resolve(path, color)?;
                let p = &resolved.program;
                p.root.map(|root| hir::pretty::module(p, root)).unwrap_or_default()
            }
            Some(Emit::Lua) => compile(path, color)?.1.lua,
            Some(Emit::Heddle) => loom_compiler::heddle::print(&compile_vm(path, color, level)?.heddle),
            Some(Emit::Bytecode) => loom_isa::disassemble(&compile_vm(path, color, level)?.image),
            None if product != Product::Lua => {
                let compiled = compile_vm(path, color, level)?;
                let image = loom_isa::encode(&compiled.image);
                let out = match product {
                    Product::Exe => exe_output(path, out),
                    _ => out.map(Path::to_path_buf).unwrap_or_else(|| image_output(path)),
                };
                return write_output(&out, &image, product);
            }
            None => {
                // A bundle and its source map.
                let (_, bundle) = compile(path, color)?;
                let out = out.map(Path::to_path_buf).unwrap_or_else(|| default_output(path));
                write_bundle(&out, &bundle)?;
                return Ok(());
            }
        };
        match out {
            None => print!("{text}"),
            Some(out) => write_file(out, &text)?,
        }
        Ok(())
    };
    run().err().unwrap_or(ExitCode::SUCCESS)
}

/// What `loom build` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Product {
    /// A Lua bundle and its source map.
    Lua,
    /// Treadle's image.
    Image,
    /// A standalone executable.
    Exe,
}

/// The extension of an image file, which `loom run` runs as it is.
const IMAGE_EXTENSION: &str = "lmi";

/// `tour.loom` → `tour.lmi` in the current directory.
fn image_output(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    PathBuf::from(format!("{stem}.{IMAGE_EXTENSION}"))
}

/// `tour.loom` → `tour` (`tour.exe` on Windows) in the current directory; a
/// name given gets the suffix if it lacks it.
fn exe_output(path: &Path, out: Option<&Path>) -> PathBuf {
    let suffix = std::env::consts::EXE_SUFFIX;
    let mut name = match out {
        Some(out) => out.as_os_str().to_os_string(),
        None => path.file_stem().unwrap_or(std::ffi::OsStr::new("out")).to_os_string(),
    };
    if !suffix.is_empty() && !name.to_string_lossy().ends_with(suffix) {
        name.push(suffix);
    }
    PathBuf::from(name)
}

/// Writes `image` to `path`: as it is, or after the runner stub, as an
/// executable.
fn write_output(path: &Path, image: &[u8], product: Product) -> Result<(), ExitCode> {
    let write = |bytes: &[u8]| {
        std::fs::write(path, bytes).map_err(|e| {
            eprintln!("error: can't write `{}`: {e}", path.display());
            ExitCode::from(EXIT_COMPILE_ERROR)
        })
    };
    if product != Product::Exe {
        return write(image);
    }
    let fail = |what: String| {
        eprintln!("error: {what}");
        ExitCode::from(EXIT_FAILED)
    };
    let here = std::env::current_exe().map_err(|e| fail(format!("can't find the loom executable: {e}")))?;
    let runner = here.with_file_name(format!("loom_runner{}", std::env::consts::EXE_SUFFIX));
    let stub = std::fs::read(&runner).map_err(|e| {
        fail(format!("can't read the runner `{}` (build it with `cargo build -p loom_runner`): {e}", runner.display()))
    })?;
    write(&loom_runner::exe_bytes(&stub, image))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| fail(format!("can't make `{}` executable: {e}", path.display())))?;
    }
    Ok(())
}

/// `loom run x.lmi`: runs an image `loom build --image` wrote.
fn run_image(path: &Path, args: Vec<String>) -> ExitCode {
    let image =
        std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| loom_isa::decode(&b).map_err(|e| e.to_string()));
    match image {
        Ok(image) => loom_runner::run(&image, path.display().to_string(), args, true),
        Err(e) => {
            eprintln!("loom: {}: {e}", path.display());
            ExitCode::from(EXIT_FAILED)
        }
    }
}

/// `tour.loom` → `tour.lua` in the current directory.
fn default_output(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    PathBuf::from(format!("{stem}.lua"))
}

fn map_path(bundle: &Path) -> PathBuf {
    let mut name = bundle.as_os_str().to_os_string();
    name.push(".map");
    PathBuf::from(name)
}

fn write_file(path: &Path, text: &str) -> Result<(), ExitCode> {
    std::fs::write(path, text).map_err(|e| {
        eprintln!("error: can't write `{}`: {e}", path.display());
        ExitCode::from(EXIT_COMPILE_ERROR)
    })
}

fn write_bundle(path: &Path, bundle: &Bundle) -> Result<(), ExitCode> {
    write_file(path, &bundle.lua)?;
    write_file(&map_path(path), &bundle.map.to_text())
}

// ------------------------------------------------------------ running

/// `loom run`: compile, then run the bundle in a child process so the
/// program can't take the compiler down with it. Its stdin, stdout and
/// stderr are this process's; its exit code becomes ours.
fn run_file(path: &Path, args: &[String], color: ColorMode) -> ExitCode {
    let run = || -> Result<ExitCode, ExitCode> {
        let (_, bundle) = compile(path, color)?;
        let program = ["--program".to_string(), path.display().to_string()];
        spawn_host(path, &bundle, &program, args)
    };
    run().unwrap_or_else(|code| code)
}

/// Compiles the program rooted at `path` for Treadle.
fn compile_vm(path: &Path, color: ColorMode, level: Level) -> Result<treadle::Compiled, ExitCode> {
    let (db, resolved, typed) = typecheck_for(path, color, Target::VM)?;
    match treadle::compile_at(&db, &resolved.program, &typed, level) {
        Ok(compiled) => Ok(compiled),
        Err(diagnostics) => {
            report(&db, path, &diagnostics, "compile", color)?;
            Err(ExitCode::from(EXIT_COMPILE_ERROR))
        }
    }
}

/// `loom run --backend vm`: compile for Treadle and run the program here.
fn run_vm(path: &Path, args: &[String], color: ColorMode, level: Level) -> ExitCode {
    let run = || -> Result<ExitCode, ExitCode> {
        let compiled = compile_vm(path, color, level)?;
        Ok(loom_runner::run(&compiled.image, path.display().to_string(), args.to_vec(), false))
    };
    run().unwrap_or_else(|code| code)
}

/// `loom test`: compile the tests, then run them in a child process, which
/// prints the report.
fn test_file(path: &Path, color: ColorMode) -> ExitCode {
    let run = || -> Result<ExitCode, ExitCode> {
        let (_, bundle) = compile_with(path, color, lower::lower_tests)?;
        spawn_host(path, &bundle, &["--test".to_string()], &[])
    };
    run().unwrap_or_else(|code| code)
}

/// Writes `bundle` to a temporary file and runs `loom lua-host` on it with
/// `flags`, passing `args` to the program.
fn spawn_host(path: &Path, bundle: &Bundle, flags: &[String], args: &[String]) -> Result<ExitCode, ExitCode> {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
    let bundle_path = std::env::temp_dir().join(format!("loom-{}-{stem}.lua", std::process::id()));
    write_bundle(&bundle_path, bundle)?;
    let exe = std::env::current_exe().map_err(|e| {
        eprintln!("error: can't find the loom executable: {e}");
        ExitCode::from(EXIT_INTERNAL)
    })?;
    let status =
        std::process::Command::new(exe).arg("lua-host").arg(&bundle_path).args(flags).arg("--").args(args).status();
    let _ = std::fs::remove_file(&bundle_path);
    let _ = std::fs::remove_file(map_path(&bundle_path));
    match status {
        Ok(status) => Ok(status.code().map_or(ExitCode::from(EXIT_INTERNAL), |c| ExitCode::from(c as u8))),
        Err(e) => {
            eprintln!("error: can't start the program: {e}");
            Err(ExitCode::from(EXIT_INTERNAL))
        }
    }
}

/// Reads a bundle and its source map, if it has one.
fn read_bundle(bundle: &Path) -> Result<(String, Option<loom_host::SourceMap>), ExitCode> {
    let lua = std::fs::read_to_string(bundle).map_err(|e| {
        eprintln!("error: can't read `{}`: {e}", bundle.display());
        ExitCode::from(EXIT_INTERNAL)
    })?;
    let map = std::fs::read_to_string(map_path(bundle)).ok().and_then(|t| loom_host::SourceMap::parse(&t));
    Ok((lua, map))
}

/// `loom lua-host`: runs a bundle and reports how it ended.
fn lua_host(bundle: &Path, program: Option<String>, args: Vec<String>) -> ExitCode {
    let (lua, map) = match read_bundle(bundle) {
        Ok(read) => read,
        Err(code) => return code,
    };
    let program = program.unwrap_or_else(|| bundle.display().to_string());
    let host = Host { output: Output::Stdout, sandbox: false, args: std::iter::once(program).chain(args).collect() };
    loom_sys::interrupt::install();
    let outcome = loom_host::run_bundle(&lua, map.as_ref(), host);
    if let Some(text) = describe(&outcome) {
        eprint!("{text}");
    }
    exit_code(&outcome)
}

/// `loom lua-host --test`: runs a test bundle's tests and prints a report.
/// It fails if any test does.
fn lua_host_tests(bundle: &Path) -> ExitCode {
    let (lua, map) = match read_bundle(bundle) {
        Ok(read) => read,
        Err(code) => return code,
    };
    let runs = match loom_host::run_tests(&lua, map.as_ref(), false) {
        Ok(runs) => runs,
        Err(outcome) => {
            eprint!("{}", describe(&outcome).unwrap_or_default());
            return exit_code(&outcome);
        }
    };
    test_report(&runs)
}

/// `loom test --backend vm`: compile the tests for Treadle, and run each
/// here, on a machine of its own, its output kept for the report.
fn test_vm(path: &Path, color: ColorMode, level: Level) -> ExitCode {
    let run = || -> Result<ExitCode, ExitCode> {
        let (db, resolved, typed) = typecheck_for(path, color, Target::VM)?;
        let (compiled, tests) = match treadle::compile_tests(&db, &resolved.program, &typed, level) {
            Ok(done) => done,
            Err(diagnostics) => {
                report(&db, path, &diagnostics, "compile", color)?;
                return Err(ExitCode::from(EXIT_COMPILE_ERROR));
            }
        };
        let mut runs = Vec::with_capacity(tests.len());
        for (name, func) in tests {
            let buffer = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let host = Host::captured(buffer.clone());
            let outcome = match loom_vm::Vm::new(
                &compiled.image,
                host,
                &loom_vm::Config { workers: worker_count(), ..loom_vm::Config::default() },
            ) {
                Ok(mut vm) => {
                    let result = vm.call(func, &[]);
                    vm.ended(result)
                }
                Err(outcome) => outcome,
            };
            let output = String::from_utf8_lossy(&buffer.borrow()).into_owned();
            runs.push(loom_host::TestRun { name, outcome, output });
        }
        Ok(test_report(&runs))
    };
    run().unwrap_or_else(|code| code)
}

/// Prints what each test did, and how it went overall.
fn test_report(runs: &[loom_host::TestRun]) -> ExitCode {
    let (report, code) = loom_runner::test_report(runs, &loom_runner::read_source);
    print!("{report}");
    ExitCode::from(code)
}
