//! Patterns: tests as branches, and bindings.

use loom_syntax::ast::BinaryOp;

use super::{BlockId, Builder};
use crate::heddle::{BitOp, InstKind, VTy, Value};
use crate::hir::{self, PatKind};
use crate::ty::{IntTy, Ty};

/// `u64`, for picking bits out of a value: its shifts never trap for an
/// amount below 64, and never wrap.
const BITS: IntTy = IntTy { signed: false, bits: 64, size: false };

impl<'a> Builder<'a> {
    /// Jumps to `yes` if `pat` matches `vals` (a value of type `ty`), and to
    /// `no` if it doesn't.
    pub(super) fn test_pat(
        &mut self,
        pat: &'a hir::Pat,
        vals: &[Value],
        ty: Ty,
        yes: BlockId,
        no: BlockId,
    ) -> Option<()> {
        match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Error => self.jump(yes),
            PatKind::Lit { lit, negative } => {
                let v = self.lit(lit, *negative, ty, pat.span)?;
                let c = self.compare(BinaryOp::Eq, vals, &v, ty, pat.span)?;
                self.br(c, yes, no);
            }
            PatKind::Range { lo, hi, inclusive } => {
                let bound = |this: &mut Self, p: &Option<Box<hir::Pat>>| -> Option<Option<Vec<Value>>> {
                    match p.as_ref().map(|p| &p.kind) {
                        None => Some(None),
                        Some(PatKind::Lit { lit, negative }) => Some(Some(this.lit(lit, *negative, ty, pat.span)?)),
                        Some(_) => this.unsupported(pat.span, "this range pattern"),
                    }
                };
                let (lo, hi) = (bound(self, lo)?, bound(self, hi)?);
                if let Some(lo) = lo {
                    let c = self.compare(BinaryOp::Le, &lo, vals, ty, pat.span)?;
                    let next = self.new_block();
                    self.br(c, next, no);
                    self.enter(next);
                }
                match hi {
                    Some(hi) => {
                        let op = if *inclusive { BinaryOp::Le } else { BinaryOp::Lt };
                        let c = self.compare(op, vals, &hi, ty, pat.span)?;
                        self.br(c, yes, no);
                    }
                    None => self.jump(yes),
                }
            }
            PatKind::Tuple(pats) => {
                let parts = self.tuple_parts(ty, pat.span)?;
                let mut at = 0;
                for (p, (item_ty, n)) in pats.iter().zip(parts) {
                    let next = self.new_block();
                    self.test_pat(p, &vals[at..at + n], item_ty, next, no)?;
                    self.enter(next);
                    at += n;
                }
                self.jump(yes);
            }
            PatKind::Or(alts) => {
                let (last, rest) = alts.split_last()?;
                for alt in rest {
                    let next = self.new_block();
                    self.test_pat(alt, vals, ty, yes, next)?;
                    self.enter(next);
                }
                self.test_pat(last, vals, ty, yes, no)?;
            }
            PatKind::Twill { pattern, .. } => {
                if pattern.mask == 0 {
                    self.jump(yes);
                } else {
                    let mask = self.iconst(pattern.mask as u64);
                    let masked = self.emit1(InstKind::Bit { op: BitOp::And, a: vals[0], b: mask }, VTy::Int);
                    let want = self.iconst(pattern.value as u64);
                    let c = self.emit1(
                        InstKind::ICmp { cmp: crate::heddle::Cmp::Eq, unsigned: true, a: masked, b: want },
                        VTy::Int,
                    );
                    self.br(c, yes, no);
                }
            }
            _ => return self.unsupported(pat.span, "this pattern"),
        }
        Some(())
    }

    /// Binds the names of a pattern that matched `vals`.
    pub(super) fn bind_pat(&mut self, pat: &'a hir::Pat, vals: &[Value], ty: Ty) -> Option<()> {
        match &pat.kind {
            PatKind::Bind(l) => {
                let vars = self.local_vars(*l, pat.span)?;
                self.write_all(&vars, vals);
            }
            PatKind::Tuple(pats) => {
                let parts = self.tuple_parts(ty, pat.span)?;
                let mut at = 0;
                for (p, (item_ty, n)) in pats.iter().zip(parts) {
                    self.bind_pat(p, &vals[at..at + n], item_ty)?;
                    at += n;
                }
            }
            PatKind::Twill { pattern, captures } => {
                for (capture, &l) in pattern.captures.iter().zip(captures) {
                    let value = self.twill_capture(vals[0], capture);
                    let vars = self.local_vars(l, pat.span)?;
                    self.write_all(&vars, &[value]);
                }
            }
            _ => {}
        }
        Some(())
    }

    /// The bits of `v` a twill capture names, most significant first.
    fn twill_capture(&mut self, v: Value, capture: &loom_syntax::twill::TwillCapture) -> Value {
        let bits = |this: &mut Self, shift: u32, width: u32| {
            let shifted = if shift == 0 {
                v
            } else {
                let n = this.iconst(u64::from(shift));
                this.emit1(InstKind::Shr { ty: BITS, a: v, b: n }, VTy::Int)
            };
            let mask = this.iconst(((1u128 << width) - 1) as u64);
            this.emit1(InstKind::Bit { op: BitOp::And, a: shifted, b: mask }, VTy::Int)
        };
        if let Some((shift, width)) = capture.contiguous() {
            return bits(self, shift, width);
        }
        // Scattered bits, gathered from the most significant.
        let n = capture.bits.len();
        let mut acc: Option<Value> = None;
        for (i, &bit) in capture.bits.iter().enumerate() {
            let one = bits(self, bit, 1);
            let place = (n - 1 - i) as u64;
            let placed = if place == 0 {
                one
            } else {
                let k = self.iconst(place);
                self.emit1(InstKind::Shl { ty: BITS, a: one, b: k }, VTy::Int)
            };
            acc = Some(match acc {
                None => placed,
                Some(prev) => self.emit1(InstKind::Bit { op: BitOp::Or, a: prev, b: placed }, VTy::Int),
            });
        }
        acc.unwrap_or_else(|| self.iconst(0))
    }

    /// Binds a pattern that can't fail (`let`, `for`).
    pub(super) fn bind_irrefutable(&mut self, pat: &'a hir::Pat, vals: &[Value], ty: Ty) -> Option<()> {
        match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Tuple(_) | PatKind::Twill { .. } | PatKind::Error => {
                self.bind_pat(pat, vals, ty)
            }
            _ => self.unsupported(pat.span, "this pattern in a `let`"),
        }
    }
}

/// Whether a pattern binds any names.
pub(super) fn binds_anything(pat: &hir::Pat) -> bool {
    match &pat.kind {
        PatKind::Bind(_) | PatKind::Rest(Some(_)) | PatKind::TypeTest { .. } => true,
        PatKind::Tuple(pats) | PatKind::Slice(pats) | PatKind::Or(pats) => pats.iter().any(binds_anything),
        PatKind::Case { fields, .. } => fields.iter().flatten().any(|f| binds_anything(&f.pat)),
        PatKind::Twill { captures, .. } => !captures.is_empty(),
        PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => binds_anything(p),
        _ => false,
    }
}
