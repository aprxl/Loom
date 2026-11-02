//! Compiling for Treadle, Loom's own VM (design/vm.md): the typed program
//! becomes Heddle, and Heddle becomes an image of bytecode.

use loom_isa::Image;
use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::SourceDb;

use crate::heddle::opt::Level;
use crate::hir::Program;
use crate::typeck::Typed;
use crate::{codegen, heddle};

/// What codegen's failure says: over a limit of Treadle's (U0103), or a
/// program it can't run yet (U0102).
pub(crate) fn codegen_diagnostics(error: codegen::CodegenError) -> Vec<Diagnostic> {
    vec![match error {
        codegen::CodegenError::Limit(message) => {
            Diagnostic::error(codes::U0103, message).note("the Lua backend has no such limit: `loom run --backend lua`")
        }
        codegen::CodegenError::Bug(message) => {
            Diagnostic::error(codes::U0102, format!("not implemented yet: {message}"))
                .note("Treadle can't run this yet")
        }
    }]
}

/// `module` optimized at `level`, or the internal error a pass made.
fn optimized(module: heddle::Module, level: Level) -> Result<heddle::Module, Vec<Diagnostic>> {
    heddle::opt::optimize(module, level).map_err(|e| {
        vec![Diagnostic::error(codes::U0102, format!("internal error: the optimizer made invalid Heddle: {e}"))]
    })
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
    let baked = crate::consteval::evaluate(db, p, typed, true)?;
    let (heddle, tests) = heddle::build_tests_with(p, typed, &baked)?;
    let heddle = optimized(heddle, level)?;
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
    // `LOOM_PASS_TIMES=1` prints how long each phase took (the passes
    // themselves are timed in `heddle::opt`).
    let timing = std::env::var_os("LOOM_PASS_TIMES").is_some();
    let lap = |what: &str, since: &mut std::time::Instant| {
        if timing {
            eprintln!("phase {what:12} {:>9.2?}", since.elapsed());
        }
        *since = std::time::Instant::now();
    };
    let mut t = std::time::Instant::now();
    let baked = crate::consteval::evaluate(db, p, typed, false)?;
    lap("const code", &mut t);
    let built = heddle::build_with(p, typed, &baked)?;
    lap("build", &mut t);
    let heddle = optimized(built, level)?;
    lap("optimize", &mut t);
    let image = codegen::generate(db, &heddle).map_err(codegen_diagnostics)?;
    lap("codegen", &mut t);
    Ok(Compiled { heddle, image })
}
