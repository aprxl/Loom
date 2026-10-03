//! The `loom` command-line tool.
//!
//! - `check` parses, resolves and type-checks a program.
//! - `build` compiles it to a Lua bundle (plus a source map), or dumps a
//!   stage with `--emit tokens|ast|hir|lua|heddle|bytecode` (the last two
//!   are Treadle's, Loom's own VM).
//! - `run` compiles, writes the bundle to a temporary file, and runs it in a
//!   child process (`loom lua-host`), whose exit code it passes on. With
//!   `--backend vm` it runs the program on Treadle instead, in this process.
//! - `test` compiles a file's tests (and only then are they compiled) and
//!   runs each in a child process the same way, which prints a report.
//! - `docs` opens the documentation, which is built into the binary.
//! - `lua-host` (hidden) runs a bundle, or a test bundle's tests, and
//!   reports how it ended, with Loom source locations.

mod docs;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use loom_compiler::hir::{self, Resolved};
use loom_compiler::lower::{self, Bundle};
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::treadle;
use loom_compiler::typeck::{self, Typed};
use loom_host::{Host, Outcome, Output};
use loom_syntax::diag::{ColorMode, emit_to_stderr};
use loom_syntax::token::dump_tokens;
use loom_syntax::{Diagnostic, FileId, SourceDb, lex, parse, pretty};

/// Exit codes (see design/lua-backend.md).
const EXIT_FAILED: u8 = 1;
const EXIT_COMPILE_ERROR: u8 = 2;
const EXIT_INTERNAL: u8 = 70;
const EXIT_UNRAVEL: u8 = 101;
const EXIT_INTERRUPTED: u8 = 130;

/// How many diagnostics one report shows; the rest are counted. Past the
/// first few, they're mostly the same mistake again.
const MAX_SHOWN: usize = 50;

#[derive(Debug, Parser)]
#[command(name = "loom", version, about = "The Loom compiler: Loom → Lua 5.4")]
struct Cli {
    /// When to color diagnostics.
    #[arg(long, value_enum, default_value_t = Color::Auto, global = true)]
    color: Color,

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
        #[arg(long, value_enum)]
        emit: Option<Emit>,
    },
    /// Compile a Loom file and run it (on the bundled Lua VM, or on Treadle).
    Run {
        file: PathBuf,
        /// What runs the program: Lua, or Treadle, Loom's own VM.
        #[arg(long, value_enum, default_value_t = Backend::Lua)]
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
        /// What runs the tests: Lua, or Treadle, Loom's own VM.
        #[arg(long, value_enum, default_value_t = Backend::Lua)]
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
    /// Lua 5.4, bundled.
    Lua,
    /// Treadle, Loom's own VM.
    Vm,
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
    let cli = Cli::parse();
    let spawned = std::thread::Builder::new().name("loom".into()).stack_size(COMPILER_STACK).spawn(move || run(cli));
    match spawned {
        // A panic has already been printed by the panic hook.
        Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
        Err(e) => {
            eprintln!("error: couldn't start the compiler thread: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> ExitCode {
    let color = ColorMode::from(cli.color);
    match cli.command {
        Command::Build { file, out, emit } => build(&file, out.as_deref(), emit, color),
        Command::Check { file } => check(&file, color),
        Command::Run { file, backend: Backend::Lua, args } => run_file(&file, &args, color),
        Command::Run { file, backend: Backend::Vm, args } => run_vm(&file, &args, color),
        Command::Test { file, backend: Backend::Lua } => test_file(&file, color),
        Command::Test { file, backend: Backend::Vm } => test_vm(&file, color),
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
    let hidden = diagnostics.len() - shown.len();
    if hidden > 0 {
        eprintln!("note: {hidden} more not shown\n");
    }
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    if errors == 0 {
        return Ok(());
    }
    let plural = if errors == 1 { "" } else { "s" };
    eprintln!("error: {} failed to {stage} ({errors} error{plural})", path.display());
    Err(ExitCode::from(EXIT_COMPILE_ERROR))
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

fn build(path: &Path, out: Option<&Path>, emit: Option<Emit>, color: ColorMode) -> ExitCode {
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
            Some(Emit::Heddle) => loom_compiler::heddle::print(&compile_vm(path, color)?.heddle),
            Some(Emit::Bytecode) => loom_isa::disassemble(&compile_vm(path, color)?.image),
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
fn compile_vm(path: &Path, color: ColorMode) -> Result<treadle::Compiled, ExitCode> {
    let (db, resolved, typed) = typecheck_for(path, color, Target::VM)?;
    match treadle::compile(&db, &resolved.program, &typed) {
        Ok(compiled) => Ok(compiled),
        Err(diagnostics) => {
            report(&db, path, &diagnostics, "compile", color)?;
            Err(ExitCode::from(EXIT_COMPILE_ERROR))
        }
    }
}

/// `loom run --backend vm`: compile for Treadle and run the program here.
fn run_vm(path: &Path, args: &[String], color: ColorMode) -> ExitCode {
    let run = || -> Result<ExitCode, ExitCode> {
        let compiled = compile_vm(path, color)?;
        let args = std::iter::once(path.display().to_string()).chain(args.iter().cloned()).collect();
        let host = Host { output: Output::Stdout, sandbox: false, args };
        let mut trace = Vec::new();
        let mut gap = None;
        let config = loom_vm::Config { interrupt: Some(loom_sys::interrupt::install()), ..loom_vm::Config::default() };
        let outcome = match (compiled.image.entry, loom_vm::Vm::new(&compiled.image, host, &config)) {
            (Some(entry), Ok(mut vm)) => {
                let outcome = match vm.call(entry, &[]) {
                    Ok(_) => vm.leaks().unwrap_or(Outcome::Finished),
                    Err(failed @ Outcome::Failed { .. }) => vm.leaks().unwrap_or(failed),
                    Err(outcome) => outcome,
                };
                trace = vm.backtrace().to_vec();
                gap = vm.backtrace_gap();
                outcome
            }
            (None, _) => Outcome::Crash { message: "the image has no entry function".into(), location: None },
            (_, Err(outcome)) => outcome,
        };
        if let Some(text) = describe(&outcome) {
            eprint!("{text}");
        }
        if matches!(outcome, Outcome::Unravel { .. }) && std::env::var_os("LOOM_BACKTRACE").is_some_and(|v| v == "1") {
            eprint!("{}", backtrace_text(&trace, gap));
        }
        Ok(exit_code(&outcome))
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
fn test_vm(path: &Path, color: ColorMode) -> ExitCode {
    let run = || -> Result<ExitCode, ExitCode> {
        let (db, resolved, typed) = typecheck_for(path, color, Target::VM)?;
        let (compiled, tests) = match treadle::compile_tests(&db, &resolved.program, &typed) {
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
            let outcome = match loom_vm::Vm::new(&compiled.image, host, &loom_vm::Config::default()) {
                Ok(mut vm) => match vm.call(func, &[]) {
                    Ok(_) => vm.leaks().unwrap_or(Outcome::Finished),
                    Err(failed @ Outcome::Failed { .. }) => vm.leaks().unwrap_or(failed),
                    Err(outcome) => outcome,
                },
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
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    println!("running {} test{}", runs.len(), plural(runs.len()));
    for run in runs {
        let verdict = if run.outcome == Outcome::Finished { "ok" } else { "FAILED" };
        println!("test {} ... {verdict}", run.name);
    }
    let failed: Vec<&loom_host::TestRun> = runs.iter().filter(|r| r.outcome != Outcome::Finished).collect();
    if !failed.is_empty() {
        println!("\nfailures:");
        for run in &failed {
            println!("\n---- {} ----", run.name);
            print!("{}", run.output);
            if !run.output.is_empty() && !run.output.ends_with('\n') {
                println!();
            }
            print!("{}", describe(&run.outcome).unwrap_or_default());
        }
    }
    let verdict = if failed.is_empty() { "ok" } else { "FAILED" };
    println!("\ntest result: {verdict}. {} passed; {} failed", runs.len() - failed.len(), failed.len());
    // A crash is Loom's bug, whatever else happened.
    if let Some(crash) = failed.iter().find(|r| matches!(r.outcome, Outcome::Crash { .. })) {
        return exit_code(&crash.outcome);
    }
    if failed.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(EXIT_FAILED) }
}

fn exit_code(outcome: &Outcome) -> ExitCode {
    ExitCode::from(match outcome {
        Outcome::Finished => 0,
        Outcome::Unravel { .. } => EXIT_UNRAVEL,
        Outcome::Failed { .. } => EXIT_FAILED,
        Outcome::Crash { .. } => EXIT_INTERNAL,
        Outcome::Exit(code) => *code,
        Outcome::Interrupted => EXIT_INTERRUPTED,
    })
}

/// What went wrong, for a run that didn't finish: the message, and where.
fn describe(outcome: &Outcome) -> Option<String> {
    Some(match outcome {
        Outcome::Finished | Outcome::Exit(_) => return None,
        Outcome::Interrupted => "interrupted (Ctrl-C)\n".into(),
        Outcome::Unravel { message, location } => {
            format!("unravel: {message}\n{}", location_text(location.as_ref()))
        }
        Outcome::Failed { message } => format!("error: {message}\n"),
        Outcome::Crash { message, location } => format!(
            "internal error: {message}\n{}  = note: this is a bug in Loom, not in your program\n",
            location_text(location.as_ref())
        ),
    })
}

/// `  --> file:line:col` and the source line, with a caret under the column.
/// Every frame an unravel happened in (`LOOM_BACKTRACE=1`, on Treadle),
/// innermost first.
fn backtrace_text(trace: &[(String, Option<loom_host::Location>)], gap: Option<(usize, usize)>) -> String {
    let mut out = String::from("backtrace:\n");
    for (i, (name, at)) in trace.iter().enumerate() {
        if let Some((before, skipped)) = gap
            && i == before
        {
            out.push_str(&format!("  ... {skipped} more frames ...\n"));
        }
        match at {
            Some(at) => out.push_str(&format!("  {i}: {name} at {}:{}:{}\n", at.file, at.line, at.column)),
            None => out.push_str(&format!("  {i}: {name}\n")),
        }
    }
    out
}

fn location_text(location: Option<&loom_host::Location>) -> String {
    let Some(at) = location else { return String::new() };
    let mut out = format!("  --> {}:{}:{}\n", at.file, at.line, at.column);
    let Ok(text) = std::fs::read_to_string(&at.file) else { return out };
    let Some(line) = text.lines().nth(at.line.saturating_sub(1) as usize) else { return out };
    let number = at.line.to_string();
    let pad = " ".repeat(number.len());
    let caret = " ".repeat(at.column.saturating_sub(1) as usize);
    out.push_str(&format!("{pad} |\n{number} | {line}\n{pad} | {caret}^\n"));
    out
}
