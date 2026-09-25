//! Compile-time code at the top of a module. So far that's `const assert`
//! whose condition the constant folder can work out: literals, constants,
//! arithmetic and comparisons, and the length of an array. Anything else is
//! "not implemented yet" (it needs the Lua sandbox, see
//! design/06-compile-time.md).

use loom_syntax::Diagnostic;
use loom_syntax::ast::Lit;
use loom_syntax::diag::codes;

use super::gauge::Val;
use super::{BodyId, Checker};
use crate::hir::{Expr, ExprKind};

impl Checker<'_> {
    /// `const …` as an item: `e` is the whole `const` expression.
    pub(super) fn check_const_block(&mut self, id: BodyId, e: &Expr) {
        let ExprKind::Const(inner) = &e.kind else {
            self.unsupported(e.span, "compile-time code");
            return;
        };
        let assert = match &inner.kind {
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::Def { def, .. } if self.p.def_path(*def) == "std.error.assert" => Some(args),
                _ => None,
            },
            _ => None,
        };
        let Some(args) = assert else {
            self.unsupported(e.span, "compile-time code");
            return;
        };
        self.in_body(id, |this| {
            let first_var = this.vars.len();
            let unit = this.tcx.unit;
            let mark = this.diags.len();
            this.check_coerce(inner, unit);
            this.finish_body(first_var);
            if this.diags.len() > mark {
                return;
            }
            let Some(cond) = args.first() else { return };
            let title = match args.get(1).map(|a| &a.value.kind) {
                None => Some("const assertion failed".to_string()),
                Some(ExprKind::Lit(Lit::Str(s))) => Some(format!("const assertion failed: {s}")),
                Some(_) => None,
            };
            match (this.fold(&cond.value, None), title) {
                (Some(Val::Bool(true)), _) => {}
                (Some(Val::Bool(false)), Some(title)) => {
                    this.diags.push(Diagnostic::error(codes::K0101, title).primary(cond.value.span, "this is false"))
                }
                (Some(Val::Bool(false)), None) => {
                    this.unsupported(args[1].value.span, "a `const assert` message that isn't a string literal");
                }
                _ => {
                    this.unsupported(cond.value.span, "evaluating this at compile time");
                }
            }
        });
    }
}
