//! The `loom` command-line tool.
//!
//! The pipeline currently stops after the lexer: `check` lexes and reports
//! diagnostics, and `build --emit tokens` dumps the token stream. The other
//! commands exist so the CLI has its final shape, and say they aren't
//! implemented yet.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use loom_syntax::diag::{ColorMode, emit_to_stderr};
use loom_syntax::token::dump_tokens;
use loom_syntax::{FileId, SourceDb, lex};

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

fn main() -> ExitCode {
    let cli = Cli::parse();
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
    eprintln!("error: `{what}` isn't implemented yet: the pipeline currently stops after the lexer");
    eprintln!("  = help: `loom check <file>` lexes a file, `loom build <file> --emit tokens` dumps its tokens");
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

/// Lexes `path`, reporting diagnostics. `Err` means errors were reported.
fn lex_file(path: &Path, color: ColorMode) -> Result<(SourceDb, loom_syntax::Lexed), ExitCode> {
    let (db, id) = load(path)?;
    let lexed = lex(&db, id);
    if !lexed.diagnostics.is_empty()
        && let Err(e) = emit_to_stderr(&db, &lexed.diagnostics, color)
    {
        eprintln!("error: couldn't write diagnostics: {e}");
    }
    if lexed.has_errors() {
        let n = lexed.diagnostics.iter().filter(|d| d.is_error()).count();
        eprintln!(
            "error: {} failed to lex ({n} error{})",
            path.display(),
            if n == 1 { "" } else { "s" }
        );
        return Err(ExitCode::from(EXIT_COMPILE_ERROR));
    }
    Ok((db, lexed))
}

fn check(path: &Path, color: ColorMode) -> ExitCode {
    match lex_file(path, color) {
        Ok((_, lexed)) => {
            println!(
                "{}: {} tokens, no errors (the pipeline stops after the lexer for now)",
                path.display(),
                lexed.tokens.len()
            );
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

fn build(path: &Path, out: Option<&Path>, emit: Option<Emit>, color: ColorMode) -> ExitCode {
    let (db, lexed) = match lex_file(path, color) {
        Ok(v) => v,
        Err(code) => return code,
    };
    match emit {
        Some(Emit::Tokens) => {
            let dump = dump_tokens(&db, &lexed.tokens);
            match out {
                None => print!("{dump}"),
                Some(out) => {
                    if let Err(e) = std::fs::write(out, dump) {
                        eprintln!("error: can't write `{}`: {e}", out.display());
                        return ExitCode::from(EXIT_COMPILE_ERROR);
                    }
                }
            }
            ExitCode::SUCCESS
        }
        None => not_yet("loom build (Lua output)"),
    }
}
