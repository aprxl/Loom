//! Compiling for Treadle, Loom's own VM (design/vm.md): the typed program
//! becomes Heddle, and Heddle becomes an image of bytecode.

use loom_isa::Image;
use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::SourceDb;

use crate::heddle::opt::Level;
use crate::hir::Program;
use crate::typeck::Typed;
use crate::{codegen, heddle, lower};

/// What codegen's failure says: over a limit of Treadle's (U0103), or a
/// program it can't run yet (U0102).
fn codegen_diagnostics(error: codegen::CodegenError) -> Vec<Diagnostic> {
    vec![match error {
        codegen::CodegenError::Limit(message) => Diagnostic::error(codes::U0103, message)
            .note("the Lua backend has no such limit: `loom run` without `--backend vm`"),
        codegen::CodegenError::Bug(message) => {
            Diagnostic::error(codes::U0102, format!("not implemented yet: {message}"))
                .note("Treadle can't run this yet")
        }
    }]
}

/// A program compiled for Treadle.
#[derive(Debug)]
pub struct Compiled {
    pub heddle: heddle::Module,
    pub image: Image,
}

/// Compiles the tests of a program type-checked for Treadle: the image,
/// and each test's name and function.
#[allow(clippy::type_complexity)]
pub fn compile_tests(
    db: &SourceDb,
    p: &Program,
    typed: &Typed,
    level: Level,
) -> Result<(Compiled, Vec<(String, u32)>), Vec<Diagnostic>> {
    lower::compile_time(db, p, typed, true)?;
    let (heddle, tests) = heddle::build_tests(p, typed)?;
    let heddle = heddle::opt::optimize(heddle, level);
    let tests = tests.into_iter().map(|(name, f)| (name, f.0)).collect();
    if heddle.funcs.is_empty() {
        let image = Image::default();
        return Ok((Compiled { heddle, image }, tests));
    }
    let image = codegen::generate(db, &heddle).map_err(codegen_diagnostics)?;
    Ok((Compiled { heddle, image }, tests))
}

/// Compiles a program type-checked for [`Target::VM`](crate::target::Target::VM)
/// with no errors, after running its compile-time code, at `-O2`.
pub fn compile(db: &SourceDb, p: &Program, typed: &Typed) -> Result<Compiled, Vec<Diagnostic>> {
    compile_at(db, p, typed, Level::O2)
}

/// [`compile`], optimized at `level`.
pub fn compile_at(db: &SourceDb, p: &Program, typed: &Typed, level: Level) -> Result<Compiled, Vec<Diagnostic>> {
    lower::compile_time(db, p, typed, false)?;
    let heddle = heddle::opt::optimize(heddle::build(p, typed)?, level);
    let image = codegen::generate(db, &heddle).map_err(codegen_diagnostics)?;
    Ok(Compiled { heddle, image })
}
