//! Calls, members, and what a name after a dot means.
//!
//! - `f(x)`: a function. `Point(1, 2)`: a constructor.
//! - `x.f(y)`: a method of `x`'s type (see `method.rs`), or else a free
//!   function in scope whose first parameter takes `x` (UFCS).
//! - `Type.name`: a case, associated function, method, or constant.
//! - `Weft.f(x, y)`: the method of the weft, for `x`'s type.
//! - `.name`: a member of the dot-block's subject, of `self` in a method,
//!   or of the expected type (see 01-syntax, "The leading dot").

use loom_syntax::Diagnostic;
use loom_syntax::ast::{FieldName, ParamMode, Vis};
use loom_syntax::diag::codes;
use loom_syntax::source::Span;

use super::generic::{self, Patch, Pending};
use super::method::{Found, Lookup};
use super::warp::{Key, Member, WarpInfo};
use super::{ArgSource, AssocValue, Call, Checker, FieldAccess, Instance, Receiver, SigParam};
use crate::hir::{self, Arg, Binding, DefId, DefKind, Expr, ExprKind, HirId};
use crate::ty::{Ty, TyKind};

/// What a leading `.name` found.
enum Leading {
    Field(Key, Ty, Receiver),
    Method(Found, Receiver),
    Case(WarpInfo, DefId),
    AssocFn(DefId),
    Const(DefId),
}

/// The value a method is called on, as far as argument checking cares.
struct Recv<'e> {
    receiver: Receiver,
    ty: Ty,
    /// The expression before the dot, for coercions and place checks.
    expr: Option<&'e Expr>,
    /// Whether it's a place that can be changed (for `mut self`).
    mutable: bool,
}

impl Checker<'_> {
    pub(super) fn check_call(&mut self, call: &Expr, callee: &Expr, args: &[Arg], expected: Option<Ty>) -> Ty {
        self.b.expr_ty.insert(callee.id, self.tcx.error);
        match &callee.kind {
            ExprKind::Def { def, args: generic_args } if matches!(self.p.def(*def).kind, DefKind::Fn(_)) => {
                let mut explicit = Vec::with_capacity(generic_args.len());
                for arg in generic_args {
                    match arg {
                        hir::GenericArg::Type(t) => explicit.push(self.lower_ty(t)),
                        hir::GenericArg::Expr(e) => return self.unsupported(e.span, "value generic arguments"),
                    }
                }
                self.call_fn_with(call, Instance::of(*def), &explicit, args, None)
            }
            ExprKind::Field { base, field: FieldName::Named(name) } => {
                self.method_call(call, callee, base, &name.name, args)
            }
            ExprKind::Type(ty) => {
                self.infer_args = true;
                let ty = self.lower_ty(ty);
                self.infer_args = false;
                match self.adt_info(ty) {
                    Some(info) => self.construct(call.id, &info, None, Some(args), call.span),
                    None if self.tcx.is_error(ty) => self.tcx.error,
                    None => {
                        let shown = self.show(ty);
                        self.unsupported(callee.span, &format!("calling `{shown}` like a function"))
                    }
                }
            }
            ExprKind::Assoc { base, name } => {
                if let hir::TyKind::Def { def, args: weft_args } = &base.kind
                    && matches!(self.p.def(*def).kind, DefKind::Weft(_))
                {
                    if !weft_args.is_empty() {
                        return self.unsupported(base.span, "generic wefts");
                    }
                    return self.weft_call(call, *def, &name.name, name.span, args);
                }
                self.infer_args = true;
                let ty = self.lower_ty(base);
                self.infer_args = false;
                self.assoc_call(call, ty, &name.name, name.span, args)
            }
            ExprKind::LeadingDot(FieldName::Named(name)) => {
                match self.resolve_leading(&name.name, callee.span, expected, true) {
                    Some(Leading::Method(found, receiver)) => {
                        let recv = Recv { receiver, ty: found.self_ty, expr: None, mutable: true };
                        self.call_found(call, found, args, recv)
                    }
                    Some(Leading::Case(info, case)) => {
                        self.construct(call.id, &info, Some(case), Some(args), call.span)
                    }
                    Some(Leading::AssocFn(def)) => self.call_fn(call, Instance::of(def), args, None),
                    Some(Leading::Field(..) | Leading::Const(_)) => {
                        self.error(
                            codes::T0106,
                            callee.span,
                            format!("`.{}` isn't a function", name.name),
                            "not callable",
                        );
                        self.check_args_alone(args)
                    }
                    None => self.check_args_alone(args),
                }
            }
            ExprKind::BuiltinCase(_) => self.unsupported(callee.span, "`some`, `ok` and `err`"),
            ExprKind::Local(_) => self.unsupported(callee.span, "calling function values"),
            ExprKind::Def { .. } => self.unsupported(callee.span, "calling this kind of value"),
            _ => {
                let ty = self.check_expr(callee, None);
                if !self.tcx.is_error(ty) {
                    let shown = self.show(ty);
                    self.error(codes::T0106, callee.span, format!("`{shown}` can't be called"), "not a function");
                }
                self.check_args_alone(args)
            }
        }
    }

    fn check_args_alone(&mut self, args: &[Arg]) -> Ty {
        for arg in args {
            self.check_expr(&arg.value, None);
        }
        self.tcx.error
    }

    /// Calls a method `find_method` found, following any `through` fields
    /// from the receiver to the method's `self`.
    fn call_found(&mut self, call: &Expr, found: Found, args: &[Arg], recv: Recv<'_>) -> Ty {
        let ty = self.call_fn(call, found.inst, args, Some(recv));
        if !found.path.is_empty()
            && let Some(c) = self.b.calls.get_mut(&call.id)
        {
            c.through = found.path;
        }
        ty
    }

    fn receiver_ty(&self, receiver: Receiver) -> Ty {
        match receiver {
            Receiver::Local(l) => self.b.local_ty.get(&l).copied().unwrap_or(self.tcx.error),
            Receiver::Subject(id) => self.subjects.iter().find(|(s, _)| *s == id).map_or(self.tcx.error, |(_, t)| *t),
            Receiver::Expr => self.tcx.error,
        }
    }

    /// Calls a function instance. With a receiver, it feeds the first
    /// parameter and `args` the rest.
    fn call_fn(&mut self, call: &Expr, inst: Instance, args: &[Arg], recv: Option<Recv<'_>>) -> Ty {
        self.call_fn_with(call, inst, &[], args, recv)
    }

    /// `call_fn`, with the generic arguments the call gives (`f[u8](x)`).
    /// A generic function's instance is known once the body is finished.
    fn call_fn_with(
        &mut self,
        call: &Expr,
        inst: Instance,
        explicit: &[Ty],
        args: &[Arg],
        recv: Option<Recv<'_>>,
    ) -> Ty {
        let def = inst.def;
        let generic = !explicit.is_empty() || self.needs_inference(&inst);
        let (sig, id, pending) = if generic {
            let Some((sig, full, bounds)) = self.generic_sig(inst, explicit, call.span) else {
                return self.check_args_alone(args);
            };
            (sig, generic::UNKNOWN, Some(Pending { inst: full, patch: Patch::Call(call.id), span: call.span, bounds }))
        } else {
            let id = self.instance(inst);
            let Some(sig) = self.fn_sig(id) else { return self.check_args_alone(args) };
            (sig, id, None)
        };
        self.call_with_sig(call, def, sig, id, None, args, recv, pending)
    }

    /// Checks a call's receiver and arguments against `sig`, and records
    /// the call.
    #[allow(clippy::too_many_arguments)]
    fn call_with_sig(
        &mut self,
        call: &Expr,
        def: DefId,
        sig: super::FnSig,
        callee: super::BodyId,
        dynamic: Option<DefId>,
        args: &[Arg],
        recv: Option<Recv<'_>>,
        pending: Option<Pending>,
    ) -> Ty {
        let name = self.p.def(def).name.clone();
        let mut params: &[SigParam] = &sig.params;
        let receiver = match recv {
            Some(recv) => {
                let Some((first, rest)) = sig.params.split_first() else {
                    self.error(codes::T0104, call.span, format!("`{name}` takes no arguments to call it on"), "here");
                    return self.check_args_alone(args);
                };
                match recv.expr {
                    Some(e) => {
                        let saved = std::mem::replace(&mut self.in_argument, true);
                        self.coerce(e, recv.ty, first.ty);
                        self.in_argument = saved;
                    }
                    None => {
                        if !self.unify(recv.ty, first.ty) {
                            self.mismatch(call.span, first.ty, recv.ty);
                        }
                    }
                }
                if first.mode == ParamMode::Mut && !recv.mutable {
                    let span = recv.expr.map_or(call.span, |e| e.span);
                    self.diags.push(
                        Diagnostic::error(codes::T0107, format!("`{name}` changes the value it's called on"))
                            .primary(span, "this can't be changed")
                            .help("call it on a variable declared with `var`, or a `mut` parameter"),
                    );
                }
                params = rest;
                Some(recv.receiver)
            }
            None => None,
        };
        let Some(mut sources) = self.match_args(&name, params, args, call.span) else { return sig.ret };
        if receiver.is_some() {
            sources.insert(0, ArgSource::Receiver);
        }
        self.b.calls.insert(call.id, Call { callee, args: sources, receiver, through: Vec::new(), dynamic });
        if let Some(pending) = pending {
            self.pending.push(pending);
        }
        sig.ret
    }

    /// Whether calling `inst` needs generic arguments inferred: the function
    /// has its own generic parameters (or weft-typed ones), or it's a method
    /// of a generic warp whose arguments aren't all known yet.
    fn needs_inference(&self, inst: &Instance) -> bool {
        let DefKind::Fn(f) = &self.p.def(inst.def).kind else { return false };
        if !f.generics.is_empty() || self.is_generic(inst.def) && inst.args.is_empty() {
            return true;
        }
        inst.args.iter().any(|&t| self.has_vars(t))
    }

    /// Matches arguments to parameters, positional first, then by name, and
    /// checks each. `None` if the call can't be made (already reported).
    fn match_args(&mut self, name: &str, params: &[SigParam], args: &[Arg], span: Span) -> Option<Vec<ArgSource>> {
        let mut slots: Vec<Option<usize>> = vec![None; params.len()];
        let mut ok = true;
        let mut next_positional = 0;
        for (i, arg) in args.iter().enumerate() {
            if arg.is_given {
                self.unsupported(arg.span, "`given:` arguments");
                ok = false;
                continue;
            }
            let slot = match &arg.label {
                None => {
                    let slot = next_positional;
                    next_positional += 1;
                    if slot >= params.len() {
                        let n = params.len();
                        self.error(
                            codes::T0104,
                            arg.span,
                            format!("`{name}` takes {n} argument{}", if n == 1 { "" } else { "s" }),
                            "one too many",
                        );
                        ok = false;
                        continue;
                    }
                    slot
                }
                Some(label) => match params.iter().position(|p| p.name == label.name) {
                    Some(slot) => slot,
                    None => {
                        self.error(
                            codes::T0105,
                            label.span,
                            format!("`{name}` has no parameter named `{}`", label.name),
                            "unknown name",
                        );
                        ok = false;
                        continue;
                    }
                },
            };
            if slots[slot].is_some() {
                self.error(codes::T0105, arg.span, format!("`{}` is given twice", params[slot].name), "already given");
                ok = false;
                continue;
            }
            slots[slot] = Some(i);
            let saved = std::mem::replace(&mut self.in_argument, true);
            match self.prechecked.remove(&arg.value.id) {
                Some(ty) => {
                    self.coerce(&arg.value, ty, params[slot].ty);
                }
                None => {
                    self.check_coerce(&arg.value, params[slot].ty);
                }
            }
            self.in_argument = saved;
            self.check_mut_arg(arg, &params[slot]);
        }
        let mut sources = Vec::with_capacity(slots.len());
        for (slot, param) in slots.iter().zip(params) {
            match slot {
                Some(i) => sources.push(ArgSource::Arg(*i)),
                None if param.has_default => sources.push(ArgSource::Default),
                None => {
                    self.error(
                        codes::T0104,
                        span,
                        format!("`{name}` needs an argument for `{}`", param.name),
                        format!("`{}` is missing", param.name),
                    );
                    ok = false;
                }
            }
        }
        // Arguments that went nowhere still need types.
        for (i, arg) in args.iter().enumerate() {
            self.prechecked.remove(&arg.value.id);
            if !slots.contains(&Some(i)) && !self.b.expr_ty.contains_key(&arg.value.id) {
                self.check_expr(&arg.value, None);
            }
        }
        ok.then_some(sources)
    }

    /// `f(mut x)` must meet `mut x: T`, and `x` must be changeable.
    fn check_mut_arg(&mut self, arg: &Arg, param: &SigParam) {
        match (arg.mutable, param.mode == ParamMode::Mut) {
            (true, true) => {
                if self.place(&arg.value) != Some(true) {
                    self.diags.push(
                        Diagnostic::error(codes::T0107, "a `mut` argument must be something that can change")
                            .primary(arg.value.span, "can't be changed")
                            .help("pass a variable declared with `var`, or a `mut` parameter"),
                    );
                }
            }
            (false, true) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::T0112,
                        format!("`{}` changes this argument, so the call must say so", param.name),
                    )
                    .primary(arg.value.span, "add `mut` before it")
                    .note("a call that changes a variable is marked at the call site: `sort(mut scores)`"),
                );
            }
            (true, false) => {
                self.diags.push(
                    Diagnostic::error(codes::T0112, format!("`{}` doesn't change its argument", param.name))
                        .primary(arg.span, "remove `mut`"),
                );
            }
            (false, false) => {}
        }
    }

    // ------------------------------------------------------------ methods

    fn method_call(&mut self, call: &Expr, callee: &Expr, base: &Expr, name: &str, args: &[Arg]) -> Ty {
        let base_ty = self.check_expr(base, None);
        let base_ty = self.resolve(base_ty);
        if self.tcx.is_error(base_ty) {
            return self.check_args_alone(args);
        }
        let mutable = self.place(base) == Some(true) || self.place(base).is_none();
        let recv = |ty: Ty| Recv { receiver: Receiver::Expr, ty, expr: Some(base), mutable };
        // A method of an `any` value is found in its vtable at run time.
        if let TyKind::Any(wefts) = self.tcx.kind(base_ty).clone() {
            let Some((method, sig)) = self.dyn_method(base_ty, &wefts, name, callee.span) else {
                return self.check_args_alone(args);
            };
            return self.call_with_sig(
                call,
                method,
                sig,
                generic::UNKNOWN,
                Some(method),
                args,
                Some(recv(base_ty)),
                None,
            );
        }
        if let Some(def) = self.warp_def(base_ty)
            && let Some(Member::AssocFn(_)) = self.member(def, name)
        {
            let warp = self.p.def(def).name.clone();
            self.diags.push(
                Diagnostic::error(codes::T0111, format!("`{name}` doesn't take `self`"))
                    .primary(callee.span, "not a method")
                    .help(format!("call it on the type: `{warp}.{name}(…)`")),
            );
            return self.check_args_alone(args);
        }
        // 1–3. A method of the type: its own, a weave's, or a `through` field's.
        match self.find_method(base_ty, name, callee.span) {
            Lookup::One(found) => {
                let r = recv(found.self_ty);
                return self.call_found(call, found, args, r);
            }
            Lookup::Ambiguous => return self.check_args_alone(args),
            Lookup::None => {}
        }
        // 4. A free function in scope whose first parameter takes the value.
        let module = self.current.map(|c| self.p.def(c).module);
        if let Some(Binding::Def(def)) = module.and_then(|m| self.p.module_name(m, name))
            && matches!(self.p.def(def).kind, DefKind::Fn(_))
        {
            return self.call_fn(call, Instance::of(def), args, Some(recv(base_ty)));
        }
        let shown = self.show(base_ty);
        if self.adt_info(base_ty).is_some() {
            self.diags.push(
                Diagnostic::error(codes::T0111, format!("`{shown}` has no method `{name}`"))
                    .primary(callee.span, "no such method")
                    .help(format!("a free function `{name}` whose first parameter is a `{shown}` would work too")),
            );
            return self.check_args_alone(args);
        }
        // Methods of built-in types come from std, which doesn't declare them yet.
        self.unsupported(callee.span, &format!("methods on `{shown}`"));
        self.check_args_alone(args)
    }

    fn assoc_call(&mut self, call: &Expr, ty: Ty, name: &str, span: Span, args: &[Arg]) -> Ty {
        if self.tcx.is_error(ty) {
            return self.check_args_alone(args);
        }
        let Some(info) = self.adt_info(ty) else {
            // `bool.to_json(b)`: a method a weave gives a built-in type.
            match self.find_method(ty, name, span) {
                Lookup::One(found) => return self.call_fn(call, found.inst, args, None),
                Lookup::Ambiguous => return self.check_args_alone(args),
                Lookup::None => {}
            }
            let shown = self.show(ty);
            return self.unsupported(span, &format!("associated functions of `{shown}`"));
        };
        match self.member(info.def, name) {
            Some(Member::Case(case)) => self.construct(call.id, &info, Some(case), Some(args), call.span),
            Some(Member::AssocFn(def) | Member::Method(def)) => self.call_fn(call, Instance::of(def), args, None),
            Some(Member::Const(_)) => {
                self.error(
                    codes::T0106,
                    span,
                    format!("`{}.{name}` is a constant, not a function", info.name),
                    "not callable",
                );
                self.check_args_alone(args)
            }
            // `Rect.area(r)`: a weave's method, called through the type. A
            // method a `through` field has needs a value to follow the
            // field from, so it's called on one instead.
            None => match self.find_method(ty, name, span) {
                Lookup::One(found) if found.path.is_empty() => self.call_fn(call, found.inst, args, None),
                Lookup::Ambiguous => self.check_args_alone(args),
                Lookup::One(_) | Lookup::None => {
                    self.error(codes::T0111, span, format!("`{}` has no `{name}`", info.name), "no such member");
                    self.check_args_alone(args)
                }
            },
        }
    }

    /// `Weft.name(x, …)`: the weft's method, for the type of `x`.
    fn weft_call(&mut self, call: &Expr, weft: DefId, name: &str, span: Span, args: &[Arg]) -> Ty {
        let weft_name = self.p.def(weft).name.clone();
        let Some(method) = self.weft_methods(weft).into_iter().find(|&m| self.p.def(m).name == name) else {
            self.error(codes::T0111, span, format!("`{weft_name}` has no method `{name}`"), "no such method");
            return self.check_args_alone(args);
        };
        let Some(first) = args.first().filter(|a| a.label.is_none() && !a.is_given) else {
            self.error(
                codes::T0104,
                call.span,
                format!("`{weft_name}.{name}` needs the value to call it on first"),
                format!("for example `{weft_name}.{name}(x)`"),
            );
            return self.check_args_alone(args);
        };
        let ty = self.check_expr(&first.value, None);
        let ty = self.resolve(ty);
        if self.tcx.is_error(ty) {
            return self.check_args_alone(&args[1..]);
        }
        let inst = match self.weaving(weft, ty) {
            Some(m) => self.impl_of(m.w, method, ty, 0).map(|imp| self.impl_instance(imp, ty, &m.bindings)),
            None => None,
        };
        let Some(inst) = inst else {
            let shown = self.show(ty);
            self.error(
                codes::T0108,
                first.value.span,
                format!("`{shown}` doesn't weave `{weft_name}`"),
                format!("this is `{shown}`"),
            );
            return self.check_args_alone(&args[1..]);
        };
        self.prechecked.insert(first.value.id, ty);
        self.call_fn(call, inst, args, None)
    }

    /// The warp a type names, built-in ones included.
    pub(super) fn warp_def(&self, ty: Ty) -> Option<DefId> {
        match *self.tcx.kind(self.resolve(ty)) {
            TyKind::Adt(def, _) if matches!(self.p.def(def).kind, DefKind::Warp(_)) => Some(def),
            _ => None,
        }
    }

    // ------------------------------------------------------------ values

    /// `x.name` or `x.0` as a value.
    pub(super) fn check_field(&mut self, e: &Expr, base: &Expr, field: &FieldName) -> Ty {
        let base_ty = self.check_expr(base, None);
        let base_ty = self.resolve(base_ty);
        if self.tcx.is_error(base_ty) {
            return self.tcx.error;
        }
        if let (TyKind::Tuple(items), FieldName::Index(i, span)) = (self.tcx.kind(base_ty).clone(), field) {
            return match items.get(*i as usize) {
                Some(&ty) => {
                    self.b.fields.insert(e.id, FieldAccess { key: Key::Pos(*i + 1), base: Receiver::Expr });
                    ty
                }
                None => {
                    let shown = self.show(base_ty);
                    let n = items.len();
                    self.error(
                        codes::T0111,
                        *span,
                        format!("`{shown}` has {n} element{}, so there's no `.{i}`", if n == 1 { "" } else { "s" }),
                        "no such element",
                    );
                    self.tcx.error
                }
            };
        }
        let Some(info) = self.adt_info(base_ty) else {
            let shown = self.show(base_ty);
            return self.unsupported(e.span, &format!("fields of `{shown}`"));
        };
        let name = match field {
            FieldName::Named(n) => n.name.clone(),
            FieldName::Index(i, _) => i.to_string(),
        };
        match self.field_of(&info, &name, e.span) {
            Some((key, ty)) => {
                self.b.fields.insert(e.id, FieldAccess { key, base: Receiver::Expr });
                ty
            }
            None => self.tcx.error,
        }
    }

    /// A field of `info` by name, checking visibility.
    fn field_of(&mut self, info: &WarpInfo, name: &str, span: Span) -> Option<(Key, Ty)> {
        if let Some(f) = info.field(name) {
            if f.vis == Vis::Private && !self.can_see_private(info.def) {
                self.diags.push(
                    Diagnostic::error(codes::N0103, format!("`{name}` is private to `{}`'s module", info.name))
                        .primary(span, "private field"),
                );
            }
            return Some((f.key.clone(), f.ty));
        }
        let owner = info.cases.iter().find(|c| c.fields.iter().flatten().any(|f| f.name.as_deref() == Some(name)));
        match owner {
            Some(case) => self.diags.push(
                Diagnostic::error(codes::T0111, format!("`{name}` belongs to the `{}` case only", case.name))
                    .primary(span, "not every case has it")
                    .help(format!("match on the value first: `if x is .{}({name}) {{ … }}`", case.name)),
            ),
            None => {
                let mut diag = Diagnostic::error(codes::T0111, format!("`{}` has no field `{name}`", info.name))
                    .primary(span, "no such field");
                // `through` forwards methods, never fields: name the field it's in.
                for through in info.fields.iter().filter(|f| f.through) {
                    if self.adt_info(through.ty).is_some_and(|inner| inner.field(name).is_some()) {
                        diag = diag
                            .help(format!("`through` forwards methods, not fields: write `.{}.{name}`", through.name));
                        break;
                    }
                }
                self.diags.push(diag);
            }
        }
        None
    }

    /// `Type.name` as a value: a unit case, a constant, or a primitive's bound.
    pub(super) fn check_assoc_value(&mut self, e: &Expr, ty: Ty, name: &str, span: Span) -> Ty {
        if self.tcx.is_error(ty) {
            return ty;
        }
        if let Some(int) = self.tcx.as_int(ty) {
            let value = match name {
                "MAX" => Some(int.max()),
                "MIN" => Some(int.min()),
                _ => None,
            };
            if let Some(value) = value {
                self.b.assoc_values.insert(e.id, AssocValue::Int(value));
                return ty;
            }
        }
        let Some(info) = self.adt_info(ty) else {
            let shown = self.show(ty);
            return self.unsupported(span, &format!("members of `{shown}`"));
        };
        match self.member(info.def, name) {
            Some(Member::Case(case)) => self.construct(e.id, &info, Some(case), None, e.span),
            Some(Member::Const(def)) => {
                self.b.assoc_values.insert(e.id, AssocValue::Const(def));
                self.const_ty(def)
            }
            Some(Member::AssocFn(_) | Member::Method(_)) => self.unsupported(span, "functions used as values"),
            None => {
                self.error(codes::T0111, span, format!("`{}` has no `{name}`", info.name), "no such member");
                self.tcx.error
            }
        }
    }

    /// A leading `.name` or `.0` as a value.
    pub(super) fn check_leading_value(&mut self, e: &Expr, field: &FieldName, expected: Option<Ty>) -> Ty {
        let name = match field {
            FieldName::Named(n) => n.name.clone(),
            FieldName::Index(i, _) => i.to_string(),
        };
        match self.resolve_leading(&name, e.span, expected, false) {
            Some(Leading::Field(key, ty, receiver)) => {
                self.b.fields.insert(e.id, FieldAccess { key, base: receiver });
                ty
            }
            Some(Leading::Case(info, case)) => self.construct(e.id, &info, Some(case), None, e.span),
            Some(Leading::Const(def)) => {
                self.b.leading_consts.insert(e.id, def);
                self.const_ty(def)
            }
            Some(Leading::Method(..) | Leading::AssocFn(_)) => self.unsupported(e.span, "functions used as values"),
            None => self.tcx.error,
        }
    }

    /// What a leading `.name` means here: a member of the dot-block's subject
    /// or of `self` (rules 1 and 2), or of the expected type (rule 3). When
    /// both find something, that's an error: it's ambiguous.
    fn resolve_leading(&mut self, name: &str, span: Span, expected: Option<Ty>, call: bool) -> Option<Leading> {
        let context = match self.subjects.last() {
            Some(&(id, ty)) => Some((Receiver::Subject(id), ty)),
            None => self.self_local.map(|l| (Receiver::Local(l), self.receiver_ty(Receiver::Local(l)))),
        };
        let mut from_context = None;
        if let Some((receiver, ty)) = context {
            if let TyKind::Tuple(items) = self.tcx.kind(self.resolve(ty)).clone()
                && let Ok(i) = name.parse::<usize>()
                && let Some(&item) = items.get(i)
            {
                from_context = Some(Leading::Field(Key::Pos(i as u32 + 1), item, receiver));
            } else if call {
                from_context = match self.find_method(ty, name, span) {
                    Lookup::One(found) => Some(Leading::Method(found, receiver)),
                    Lookup::Ambiguous => return None,
                    Lookup::None => None,
                };
            } else if let Some(info) = self.adt_info(ty)
                && let Some(f) = info.field(name)
            {
                from_context = Some(Leading::Field(f.key.clone(), f.ty, receiver));
            }
        }
        let expected = expected.map(|t| self.resolve(t)).map(|t| match self.tcx.kind(t) {
            TyKind::Optional(inner) => self.resolve(*inner),
            _ => t,
        });
        let mut from_type = None;
        if let Some(info) = expected.and_then(|t| self.adt_info(t)) {
            from_type = match self.member(info.def, name) {
                Some(Member::Case(case)) => Some(Leading::Case(info, case)),
                Some(Member::AssocFn(def)) if call => Some(Leading::AssocFn(def)),
                Some(Member::Const(def)) if !call => Some(Leading::Const(def)),
                _ => None,
            };
        }
        match (from_context, from_type) {
            (Some(_), Some(_)) => {
                let ty = expected.map(|t| self.show(t)).unwrap_or_default();
                self.diags.push(
                    Diagnostic::error(codes::T0111, format!("`.{name}` is ambiguous here"))
                        .primary(span, "could mean two things")
                        .help(format!("write `self.{name}` for the member, or `{ty}.{name}` for the type's")),
                );
                None
            }
            (Some(found), None) | (None, Some(found)) => Some(found),
            (None, None) => {
                let mut diag = Diagnostic::error(codes::T0111, format!("`.{name}` doesn't name anything here"))
                    .primary(span, "nothing called this in context");
                diag = match expected {
                    Some(t) => {
                        let shown = self.show(t);
                        diag.note(format!("the expected type here is `{shown}`"))
                    }
                    None => diag.help("a leading dot needs a method's `self`, a dot-block, or an expected type"),
                };
                self.diags.push(diag);
                None
            }
        }
    }

    // ------------------------------------------------------------ places

    /// `Some(changeable)` if `e` is a place (a variable, or a field of one),
    /// `None` if it's a value.
    pub(super) fn place(&self, e: &Expr) -> Option<bool> {
        match &e.kind {
            ExprKind::Local(l) => Some(self.p.local(*l).mutable),
            ExprKind::Field { base, .. } => self.place(base),
            ExprKind::LeadingDot(_) => match self.b.fields.get(&e.id).map(|f| f.base) {
                Some(Receiver::Local(l)) => Some(self.p.local(l).mutable),
                Some(Receiver::Subject(id)) => Some(self.subject_mutable(id)),
                _ => None,
            },
            _ => None,
        }
    }

    /// A dot-block's subject can be changed inside it: in place when the
    /// subject is a changeable place, and otherwise it's a fresh copy.
    fn subject_mutable(&self, _id: HirId) -> bool {
        true
    }
}
