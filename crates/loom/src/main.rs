//! The `loom` command-line tool.
//!
//! - `check` parses, resolves and type-checks a program.
//! - `build` compiles it to a Lua bundle (plus a source map), or dumps a
//!   stage with `--emit tokens|ast|hir|lua`.
//! - `run` compiles, writes the bundle to a temporary file, and runs it in a
//!   child process (`loom lua-host`), whose exit code it passes on.
//! - `lua-host` (hidden) runs a bundle and reports how it ended, with Loom
//!   source locations.
//!
//! `test` exists so the CLI has its final shape, and says it isn't
//! implemented yet.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use loom_compiler::hir::{self, Resolved};
use loom_compiler::lower::{self, Bundle};
use loom_compiler::modules::FsLoader;
use loom_compiler::typeck::{self, Typed};
use loom_syntax::diag::{ColorMode, emit_to_stderr};
use loom_syntax::token::dump_tokens;
use loom_syntax::{Diagnostic, FileId, SourceDb, lex, parse, pretty};

/// Exit codes (see design/lua-backend.md).
const EXIT_COMPILE_ERROR: u8 = 2;
const EXIT_INTERNAL: u8 = 70;
const EXIT_UNRAVEL: u8 = 101;

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
    /// Transpile a Loom file and run it with the bundled Lua VM.
    Run {
        file: PathBuf,
        /// Arguments passed to the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Check a Loom file for errors without writing anything.
    Check { file: PathBuf },
    /// Run a file's `#test` functions.
    Test { file: PathBuf },
    /// Run a Lua bundle in the bundled VM (used internally by `loom run`).
    #[command(hide = true)]
    LuaHost {
        bundle: PathBuf,
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
        Command::Run { file, args } => run_file(&file, &args, color),
        Command::Test { .. } => not_yet("loom test"),
        Command::LuaHost { bundle, .. } => lua_host(&bundle),
    }
}

fn not_yet(what: &str) -> ExitCode {
    eprintln!("error: `{what}` isn't implemented yet");
    eprintln!("  = help: `loom run <file>` compiles and runs a program; `loom check <file>` checks one");
    ExitCode::from(EXIT_COMPILE_ERROR)
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
    if !diagnostics.is_empty()
        && let Err(e) = emit_to_stderr(db, diagnostics, color)
    {
        eprintln!("error: couldn't write diagnostics: {e}");
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

/// Resolves and type-checks the program rooted at `path`.
fn typecheck(path: &Path, color: ColorMode) -> Result<(SourceDb, Resolved, Typed), ExitCode> {
    let (db, resolved) = resolve(path, color)?;
    let typed = typeck::check(&resolved.program);
    report(&db, path, &typed.diagnostics, "type-check", color)?;
    Ok((db, resolved, typed))
}

fn check(path: &Path, color: ColorMode) -> ExitCode {
    let run = || -> Result<(), ExitCode> {
        let (_, resolved, _) = typecheck(path, color)?;
        let p = &resolved.program;
        let n = p.root.map_or(0, |root| p.module(root).decls.len());
        println!("{}: {n} item{}, no errors", path.display(), if n == 1 { "" } else { "s" });
        Ok(())
    };
    run().err().unwrap_or(ExitCode::SUCCESS)
}

/// Compiles the program rooted at `path` to a Lua bundle.
fn compile(path: &Path, color: ColorMode) -> Result<(SourceDb, Bundle), ExitCode> {
    let (db, resolved, typed) = typecheck(path, color)?;
    match lower::lower(&db, &resolved.program, &typed) {
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
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
        let bundle_path = std::env::temp_dir().join(format!("loom-{}-{stem}.lua", std::process::id()));
        write_bundle(&bundle_path, &bundle)?;
        let exe = std::env::current_exe().map_err(|e| {
            eprintln!("error: can't find the loom executable: {e}");
            ExitCode::from(EXIT_INTERNAL)
        })?;
        let status = std::process::Command::new(exe).arg("lua-host").arg(&bundle_path).arg("--").args(args).status();
        let _ = std::fs::remove_file(&bundle_path);
        let _ = std::fs::remove_file(map_path(&bundle_path));
        match status {
            Ok(status) => Ok(status.code().map_or(ExitCode::from(EXIT_INTERNAL), |c| ExitCode::from(c as u8))),
            Err(e) => {
                eprintln!("error: can't start the program: {e}");
                Err(ExitCode::from(EXIT_INTERNAL))
            }
        }
    };
    run().unwrap_or_else(|code| code)
}

/// `loom lua-host`: runs a bundle and reports how it ended.
fn lua_host(bundle: &Path) -> ExitCode {
    let lua = match std::fs::read_to_string(bundle) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("error: can't read `{}`: {e}", bundle.display());
            return ExitCode::from(EXIT_INTERNAL);
        }
    };
    let map = std::fs::read_to_string(map_path(bundle)).ok().and_then(|t| loom_host::SourceMap::parse(&t));
    match loom_host::run_bundle(&lua, map.as_ref(), loom_host::Output::Stdout) {
        loom_host::Outcome::Finished => ExitCode::SUCCESS,
        loom_host::Outcome::Unravel { message, location } => {
            eprintln!("unravel: {message}");
            print_location(location.as_ref());
            ExitCode::from(EXIT_UNRAVEL)
        }
        loom_host::Outcome::Crash { message, location } => {
            eprintln!("internal error: {message}");
            print_location(location.as_ref());
            eprintln!("  = note: this is a bug in Loom, not in your program");
            ExitCode::from(EXIT_INTERNAL)
        }
    }
}

/// `  --> file:line:col` and the source line, with a caret under the column.
fn print_location(location: Option<&loom_host::Location>) {
    let Some(at) = location else { return };
    eprintln!("  --> {}:{}:{}", at.file, at.line, at.column);
    let Ok(text) = std::fs::read_to_string(&at.file) else { return };
    let Some(line) = text.lines().nth(at.line.saturating_sub(1) as usize) else { return };
    let number = at.line.to_string();
    let pad = " ".repeat(number.len());
    let caret = " ".repeat(at.column.saturating_sub(1) as usize);
    eprintln!("{pad} |");
    eprintln!("{number} | {line}");
    eprintln!("{pad} | {caret}^");
}
