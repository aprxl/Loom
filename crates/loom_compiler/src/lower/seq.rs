//! Lowering arrays, lists and slices.
//!
//! An array and a list are the same at run time: a table with the elements
//! at `1..n` and the length in `n` (never Lua's `#`, since an element can be
//! `nil`). A slice is a view `{t = elements, o = offset, n = length}` of one.
//!
//! Reading or writing `xs[i]` checks `i` against the length (`rt.idx`, which
//! also turns the 0-based index into a Lua one), except when the index's type
//! can't reach past the end of an array: a `u4` into `[u8; 16]`.

use super::lir::{Expr, StmtKind, TableKey};
use super::{Dest, Lowerer};
use crate::hir::{self, ExprKind, GenericArg, PatKind};
use crate::ty::{Ty, TyKind};
use crate::typeck::Seq;

/// A map or a set, with the types it holds.
#[derive(Clone, Copy, Debug)]
pub(super) enum Coll {
    Map(Ty, Ty),
    Set(Ty),
}

impl Lowerer<'_> {
    /// The sequence `ty` is, if it's one.
    pub(super) fn seq(&self, ty: Ty) -> Option<Seq> {
        let tcx = self.tcx();
        match tcx.kind(ty) {
            TyKind::Array(t, n) => Some(Seq::Array(*t, *n)),
            TyKind::Adt(def, args) if args.len() == 1 => {
                let d = self.p.def(*def);
                match (d.name.as_str(), self.p.module(d.module).path.as_str()) {
                    ("List", "std.list") => Some(Seq::List(args[0])),
                    ("Slice", "std.slice") => Some(Seq::Slice(args[0])),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `[a, b, c]`: `{n = 3, a, b, c}`.
    pub(super) fn array_lit(&mut self, items: &[hir::Expr]) -> Expr {
        let items: Vec<&hir::Expr> = items.iter().collect();
        let values = self.lower_owned_operands(&items);
        let mut entries = vec![(TableKey::Name("n".into()), Expr::Int(values.len() as i64))];
        for (i, v) in values.into_iter().enumerate() {
            entries.push((TableKey::Index(i as i64 + 1), v));
        }
        Expr::Record(entries)
    }

    /// `[x; n]`: `n` copies of `x`.
    pub(super) fn array_repeat(&mut self, e: &hir::Expr, value: &hir::Expr) -> Expr {
        let n = match self.tcx().kind(self.ty_of(e)) {
            TyKind::Array(_, n) => *n as i64,
            _ => 0,
        };
        let elem = self.ty_of(value);
        let v = self.lower_owned(value);
        let d = self.desc(elem, e.span);
        Expr::rt("filled", vec![d, v, Expr::Int(n)])
    }

    /// `xs[i]` or `xs[a..b]`, as a value or as a place to assign to.
    pub(super) fn index(&mut self, base: &hir::Expr, args: &[GenericArg]) -> Expr {
        let [GenericArg::Expr(index)] = args else { return Expr::Nil };
        let base_ty = self.ty_of(base);
        if let Some(Coll::Map(k, _)) = self.coll(base_ty) {
            return self.map_index(base, index, k);
        }
        let Some(seq) = self.seq(base_ty) else {
            // `s[a..b]`: the text between two byte positions.
            let ExprKind::Range { lo, hi, inclusive } = &index.kind else { return Expr::Nil };
            let s = self.lower_expr(base);
            let (lo, hi) = self.range_bounds(lo.as_deref(), hi.as_deref(), *inclusive);
            return Expr::rt("substr", vec![s, lo, hi]);
        };
        let b = if self.is_place(base) { self.lower_place(base) } else { self.lower_expr(base) };
        if let ExprKind::Range { lo, hi, inclusive } = &index.kind {
            let (lo, hi) = self.range_bounds(lo.as_deref(), hi.as_deref(), *inclusive);
            return Expr::rt("subview", vec![b, lo, hi]);
        }
        let b = if matches!(seq, Seq::Array(..)) { b } else { self.spill_place(b) };
        let i = self.lower_expr(index);
        let checked = match seq {
            Seq::Array(_, n) if self.index_fits(self.ty_of(index), n) => Expr::bin("+", i, Expr::Int(1)),
            Seq::Array(_, n) => Expr::rt("idx", vec![i, Expr::Int(n as i64)]),
            Seq::List(_) | Seq::Slice(_) => Expr::rt("idx", vec![i, Expr::field(b.clone(), "n")]),
        };
        let at = self.spill(checked);
        match seq {
            Seq::Slice(_) => Expr::index(Expr::field(b.clone(), "t"), Expr::bin("+", Expr::field(b, "o"), at)),
            _ => Expr::index(b, at),
        }
    }

    /// The bounds of `a..b` as a start and an exclusive end: `0` for no
    /// start, `nil` for no end.
    fn range_bounds(&mut self, lo: Option<&hir::Expr>, hi: Option<&hir::Expr>, inclusive: bool) -> (Expr, Expr) {
        let bounds: Vec<&hir::Expr> = lo.iter().chain(hi.iter()).copied().collect();
        let mut vals = self.lower_operands(&bounds).into_iter();
        let start = if lo.is_some() { vals.next().unwrap_or(Expr::Int(0)) } else { Expr::Int(0) };
        let end = match (hi.is_some(), vals.next()) {
            (true, Some(h)) if inclusive => Expr::bin("+", h, Expr::Int(1)),
            (true, Some(h)) => h,
            _ => Expr::Nil,
        };
        (start, end)
    }

    /// Whether every value of index type `ty` is below `n`, so no check is
    /// needed.
    fn index_fits(&self, ty: Ty, n: u64) -> bool {
        self.int_of(ty).is_some_and(|i| !i.signed && (i.max() as u128) < n as u128)
    }

    /// A place used more than once: a local name as it is, anything else in
    /// a temporary.
    fn spill_place(&mut self, v: Expr) -> Expr {
        if matches!(v, Expr::Name(_)) { v } else { self.spill_always(v) }
    }

    /// `for x in xs` over an array, list or slice. The length is read once,
    /// before the loop. `for mut x in xs` makes `x` stand for the element.
    pub(super) fn for_seq(&mut self, e: &hir::Expr, pat: &hir::Pat, iter: &hir::Expr, body: &hir::Block, seq: Seq) {
        let v = if self.is_place(iter) { self.lower_place(iter) } else { self.lower_expr(iter) };
        let s = self.spill_always(v);
        let mut guard = None;
        let (t, lo, hi) = match seq {
            Seq::Array(_, n) => (s, Expr::Int(1), Expr::Int(n as i64)),
            Seq::List(_) => {
                let n = self.spill_always(Expr::field(s.clone(), "n"));
                guard = Some(Expr::rt("same_len", vec![s.clone(), n.clone()]));
                (s, Expr::Int(1), n)
            }
            Seq::Slice(_) => {
                let t = self.spill_always(Expr::field(s.clone(), "t"));
                let len = self.spill_always(Expr::field(t.clone(), "n"));
                guard = Some(Expr::rt("same_len", vec![t.clone(), len]));
                let o = self.spill_always(Expr::field(s.clone(), "o"));
                let hi = Expr::bin("+", o.clone(), Expr::field(s, "n"));
                (t, Expr::bin("+", o, Expr::Int(1)), hi)
            }
        };
        let control = self.new_temp();
        let elem = Expr::index(t, Expr::name(&control));
        let alias = match &pat.kind {
            PatKind::Bind(local) if self.p.local(*local).mutable => Some(*local),
            _ => None,
        };
        let mut body_stmts = self.nested(|this| {
            if let Some(guard) = guard {
                this.push(StmtKind::Call(guard));
            }
            match alias {
                Some(local) => {
                    this.aliases.insert(local, elem.clone());
                }
                None => this.bind_irrefutable(pat, elem.clone()),
            }
        });
        body_stmts.extend(self.loop_body(e.id, Dest::Discard, body));
        if let Some(local) = alias {
            self.aliases.remove(&local);
        }
        self.push(StmtKind::NumFor(control, lo, hi, body_stmts));
        self.after_loop();
    }

    /// A map's key and value types, or a set's element type.
    pub(super) fn coll(&self, ty: Ty) -> Option<Coll> {
        let TyKind::Adt(def, args) = self.tcx().kind(ty) else { return None };
        let d = self.p.def(*def);
        match (d.name.as_str(), self.p.module(d.module).path.as_str(), args.as_slice()) {
            ("Map", "std.map", [k, v]) => Some(Coll::Map(*k, *v)),
            ("Set", "std.set", [t]) => Some(Coll::Set(*t)),
            _ => None,
        }
    }

    /// `[k: v, …]`: `rt.map_of` with the keys and values in turn.
    pub(super) fn map_lit(&mut self, e: &hir::Expr, pairs: &[(hir::Expr, hir::Expr)]) -> Expr {
        let Some(Coll::Map(k, _)) = self.coll(self.ty_of(e)) else { return Expr::Nil };
        let dk = self.desc(k, e.span);
        let items: Vec<&hir::Expr> = pairs.iter().flat_map(|(k, v)| [k, v]).collect();
        let values = self.lower_owned_operands(&items);
        let n = values.len() as i64;
        let mut entries: Vec<(TableKey, Expr)> =
            values.into_iter().enumerate().map(|(i, v)| (TableKey::Index(i as i64 + 1), v)).collect();
        entries.push((TableKey::Name("n".into()), Expr::Int(n)));
        Expr::rt("map_of", vec![dk, Expr::Record(entries)])
    }

    /// `m[key]`: the value itself, unravelling when there's none.
    pub(super) fn map_index(&mut self, base: &hir::Expr, key: &hir::Expr, k: Ty) -> Expr {
        let dk = self.desc(k, key.span);
        let m = if self.is_place(base) { self.lower_place(base) } else { self.lower_expr(base) };
        let m = self.spill(m);
        let key = self.lower_expr(key);
        Expr::rt("map_at", vec![dk, m, key])
    }

    /// `m[key] = value`, and `m[key] op= value`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn map_assign(
        &mut self,
        base: &hir::Expr,
        key: &hir::Expr,
        k: Ty,
        v: Ty,
        op: loom_syntax::ast::AssignOp,
        value: &hir::Expr,
        span: loom_syntax::source::Span,
    ) {
        let dk = self.desc(k, span);
        let m = if self.is_place(base) { self.lower_place(base) } else { self.lower_expr(base) };
        let m = self.spill_always(m);
        let key = self.lower_owned(key);
        let key = self.spill_always(key);
        let new = match op {
            loom_syntax::ast::AssignOp::Assign => self.lower_owned(value),
            loom_syntax::ast::AssignOp::Compound(bin) => {
                let current = Expr::rt("map_at", vec![dk.clone(), m.clone(), key.clone()]);
                let current = self.spill_always(current);
                let operand = self.lower_expr(value);
                self.apply(bin, current, operand, v, span)
            }
        };
        self.push(StmtKind::Call(Expr::rt("map_set", vec![dk, m, key, new])));
    }

    /// `List()`: an empty list.
    pub(super) fn new_list() -> Expr {
        Expr::Record(vec![(TableKey::Name("n".into()), Expr::Int(0))])
    }

    /// The descriptor of a sequence type.
    pub(super) fn seq_desc(&mut self, seq: Seq, span: loom_syntax::source::Span) -> Expr {
        match seq {
            Seq::Array(t, _) | Seq::List(t) => {
                let d = self.desc(t, span);
                Expr::rt("seq", vec![d])
            }
            Seq::Slice(t) => {
                let d = self.desc(t, span);
                Expr::rt("slice", vec![d])
            }
        }
    }
}
