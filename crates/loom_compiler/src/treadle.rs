//! Compiling for Treadle, Loom's own VM (design/vm.md): the typed program
//! becomes Heddle, and Heddle becomes an image of bytecode.

use loom_isa::Image;
use loom_syntax::Diagnostic;
use loom_syntax::diag::codes;
use loom_syntax::source::SourceDb;

use crate::hir::Program;
use crate::typeck::Typed;
use crate::{codegen, heddle, lower};

/// A program compiled for Treadle.
#[derive(Debug)]
pub struct Compiled {
    pub heddle: heddle::Module,
    pub image: Image,
}

/// Compiles a program type-checked for [`Target::VM`](crate::target::Target::VM)
/// with no errors, after running its compile-time code.
pub fn compile(db: &SourceDb, p: &Program, typed: &Typed) -> Result<Compiled, Vec<Diagnostic>> {
    lower::compile_time(db, p, typed, false)?;
    let heddle = heddle::build(p, typed)?;
    let image = codegen::generate(db, &heddle).map_err(|e| {
        vec![Diagnostic::error(codes::U0102, format!("not implemented yet: {e}")).note("Treadle can't run this yet")]
    })?;
    Ok(Compiled { heddle, image })
}
