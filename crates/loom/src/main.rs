//! The `loom` command-line tool.
//!
//! The pipeline currently stops after type checking: `check` parses,
//! resolves and type-checks a program and reports diagnostics, and `build
//! --emit tokens|ast|hir` dumps a stage. The other commands exist so the CLI has its
//! final shape, and say they aren't implemented yet.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use loom_compiler::hir::{self, Resolved};
use loom_compiler::modules::FsLoader;
use loom_compiler::typeck::{self, Typed};
use loom_syntax::diag::{ColorMode, emit_to_stderr};
use loom_syntax::token::dump_tokens;
use loom_syntax::{Diagnostic, FileId, SourceDb, lex, parse, pretty};

/// Exit code for compile errors and usage errors (see design/lua-backend.md).
const EXIT_COMPILE_ERROR: u8 = 2;

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
        Command::Run { .. } => not_yet("loom run"),
        Command::Test { .. } => not_yet("loom test"),
        Command::LuaHost { .. } => not_yet("loom lua-host"),
    }
}

fn not_yet(what: &str) -> ExitCode {
    eprintln!("error: `{what}` isn't implemented yet: the pipeline currently stops after type checking");
    eprintln!("  = help: `loom check <file>` checks a file; `loom build <file> --emit tokens|ast|hir` dumps a stage");
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
        println!(
            "{}: {n} item{}, no errors (the pipeline stops after type checking for now)",
            path.display(),
            if n == 1 { "" } else { "s" }
        );
        Ok(())
    };
    run().err().unwrap_or(ExitCode::SUCCESS)
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
            None => {
                typecheck(path, color)?;
                return Err(not_yet("loom build (Lua output)"));
            }
        };
        match out {
            None => print!("{text}"),
            Some(out) => std::fs::write(out, text).map_err(|e| {
                eprintln!("error: can't write `{}`: {e}", out.display());
                ExitCode::from(EXIT_COMPILE_ERROR)
            })?,
        }
        Ok(())
    };
    run().err().unwrap_or(ExitCode::SUCCESS)
}
