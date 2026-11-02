//! The program generator (design/vm.md §15): random, well-typed programs
//! from a seed, run on Lua and on Treadle (the checked Treadle with
//! `--features vm-checked`), which must print the same and end the same
//! way, unravels included.
//!
//! A program has a record, a sum and an error type, a few functions (some
//! fallible), and a `main`, using integers of odd widths under every
//! overflow policy, casts, floats, `match`, loops, lists, optionals, `?`,
//! `mend` and `defer`; and the values that own memory (`String`s, lists of
//! them, maps of `String`s to integers and the other way, lambdas capturing
//! locals), where drops, copies and moves are hardest; and shuttles:
//! generators and duplex shuttles whose `yield`s land anywhere in their
//! bodies, gone through in place, held, by stream methods, and pushed and
//! closed. A generator with a `defer` is only ever closed (by its loop) or
//! run to its end: one dropped while suspended runs its `defer`s on
//! Treadle and not on Lua (design/vm.md §14, P2). `cargo test` runs 300
//! seeds; `cargo test -p loom --test gen -- --ignored` runs 20 000. A failing seed is shrunk (the
//! statements of `main` it doesn't need are left out) and printed, to keep
//! as a `tests/run` fixture.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{lower, treadle, typeck};
use loom_host::{Host, Outcome, run_bundle};
use loom_syntax::SourceDb;
use loom_syntax::diag::render_plain;

// ---------------------------------------------------------------- randomness

/// SplitMix64: small, and the same everywhere.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// True `percent` times in 100.
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Int {
    signed: bool,
    bits: u32,
}

impl Int {
    fn name(self) -> String {
        format!("{}{}", if self.signed { "i" } else { "u" }, self.bits)
    }

    fn min(self) -> i128 {
        if self.signed { -(1i128 << (self.bits - 1)) } else { 0 }
    }

    fn max(self) -> i128 {
        if self.signed { (1i128 << (self.bits - 1)) - 1 } else { (1i128 << self.bits) - 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Int(Int),
    Bool,
    F64,
    /// The record `R { a: A, b: B, f: f64 }`.
    Rec,
    /// The sum `S { case P(x: A), case Q, case W(y: B, z: bool) }`.
    Sum,
    List(Int),
    Opt(Int),
    /// A `String`.
    Str,
    /// A `List[String]`, never empty.
    StrList,
    /// A `Map[String, iN]`.
    MapSI(Int),
    /// A `Map[iN, String]`.
    MapIS(Int),
}

#[derive(Clone, Debug)]
struct Func {
    name: String,
    params: Vec<Ty>,
    ret: Ty,
    fallible: bool,
}

/// A shuttle the program has: what it takes, yields, and pulls (a
/// generator pulls nothing), and whether it has `defer`s.
#[derive(Clone, Debug)]
struct Shut {
    name: String,
    params: Vec<Ty>,
    item: Ty,
    pulls: Option<Int>,
    defers: bool,
}

#[derive(Clone, Debug)]
struct Var {
    name: String,
    ty: Ty,
    mutable: bool,
}

// ---------------------------------------------------------------- the generator

struct Gen {
    rng: Rng,
    /// Where the constants' random numbers come from: not `rng`, so that
    /// each seed makes the program it did before they were added, and what
    /// was checked with them stays checked.
    const_rng: Rng,
    /// The record's `a` and `b`, the sum's `x` and `y`.
    a: Int,
    b: Int,
    funcs: Vec<Func>,
    scopes: Vec<Vec<Var>>,
    next: u32,
    /// Whether the function being made can `fail`, and use `?`.
    fallible: bool,
    shuttles: Vec<Shut>,
    /// In a shuttle's body: what it yields.
    yields: Option<Ty>,
    /// Whether the body being made may have `defer`s.
    defers_ok: bool,
}

const WIDTHS: [u32; 14] = [8, 16, 32, 64, 1, 3, 5, 7, 12, 13, 24, 31, 48, 63];

impl Gen {
    fn new(seed: u64) -> Gen {
        let mut rng = Rng(seed);
        let a = Self::int_of(&mut rng);
        let b = Self::int_of(&mut rng);
        Gen {
            rng,
            const_rng: Rng(seed ^ 0x00C0_FFEE_5EED_0001),
            a,
            b,
            funcs: Vec::new(),
            scopes: vec![Vec::new()],
            next: 0,
            fallible: false,
            shuttles: Vec::new(),
            yields: None,
            defers_ok: true,
        }
    }

    fn int_of(rng: &mut Rng) -> Int {
        let bits = rng.pick(&WIDTHS);
        // A signed type needs two bits.
        let signed = bits > 1 && rng.chance(50);
        Int { signed, bits }
    }

    fn int(&mut self) -> Int {
        Self::int_of(&mut self.rng)
    }

    fn fresh(&mut self, stem: &str) -> String {
        self.next += 1;
        format!("{stem}_{}", self.next)
    }

    fn ty_name(&self, t: Ty) -> String {
        match t {
            Ty::Int(i) => i.name(),
            Ty::Bool => "bool".into(),
            Ty::F64 => "f64".into(),
            Ty::Rec => "R".into(),
            Ty::Sum => "S".into(),
            Ty::List(i) => format!("List[{}]", i.name()),
            Ty::Opt(i) => format!("?{}", i.name()),
            Ty::Str => "String".into(),
            Ty::StrList => "List[String]".into(),
            Ty::MapSI(i) => format!("Map[String, {}]", i.name()),
            Ty::MapIS(i) => format!("Map[{}, String]", i.name()),
        }
    }

    /// A key for a map of Strings: few, so keys come again.
    fn str_key(&mut self) -> String {
        format!("{:?}", self.rng.pick(&["a", "b", "c", "d", "é", "zz", ""]))
    }

    fn vars(&self, want: impl Fn(&Var) -> bool) -> Vec<Var> {
        self.scopes.iter().flatten().filter(|v| want(v)).cloned().collect()
    }

    fn bind(&mut self, name: &str, ty: Ty, mutable: bool) {
        self.scopes.last_mut().expect("a scope").push(Var { name: name.into(), ty, mutable });
    }

    // ------------------------------------------------------------ literals

    fn int_lit(&mut self, i: Int) -> String {
        let v: i128 = match self.rng.below(8) {
            0 => 0,
            1 => 1.min(i.max()),
            2 => i.max(),
            // The least i64 can't be written as a literal: one more.
            3 => i.min() + i128::from(i.bits == 64 && i.signed),
            4 if i.signed => -1,
            _ => {
                let span = (i.max() - i.min()) as u128;
                let small = (self.rng.below(20) as i128).min(i.max());
                if self.rng.chance(50) { small } else { i.min() + (u128::from(self.rng.next()) % (span + 1)) as i128 }
            }
        };
        let v = v.clamp(i.min() + i128::from(i.bits == 64 && i.signed), i.max());
        if v < 0 { format!("({v}{})", i.name()) } else { format!("{v}{}", i.name()) }
    }

    fn f64_lit(&mut self) -> String {
        let s = self.rng.pick(&["0.0", "1.0", "1.5", "0.1", "2.25", "1000000.0", "0.333", "1e300", "7.0", "123.456"]);
        if self.rng.chance(30) { format!("(-{s})") } else { s.to_string() }
    }

    // ------------------------------------------------------------ expressions

    fn expr(&mut self, t: Ty, depth: u32) -> String {
        match t {
            Ty::Int(i) => self.int_expr(i, depth),
            Ty::Bool => self.bool_expr(depth),
            Ty::F64 => self.f64_expr(depth),
            Ty::Rec => {
                let (a, b) = (self.a, self.b);
                let (ea, eb, ef) = (
                    self.int_expr(a, depth.saturating_sub(1)),
                    self.int_expr(b, depth.saturating_sub(1)),
                    self.f64_expr(depth.saturating_sub(1)),
                );
                format!("R(a: {ea}, b: {eb}, f: {ef})")
            }
            Ty::Sum => match self.rng.below(3) {
                0 => {
                    let a = self.a;
                    format!("S.P(x: {})", self.int_expr(a, depth.saturating_sub(1)))
                }
                1 => "S.Q".into(),
                _ => {
                    let b = self.b;
                    let y = self.int_expr(b, depth.saturating_sub(1));
                    format!("S.W(y: {y}, z: {})", self.bool_expr(depth.saturating_sub(1)))
                }
            },
            Ty::List(i) => {
                let n = 1 + self.rng.below(4);
                let items: Vec<String> = (0..n).map(|_| self.int_expr(i, depth.saturating_sub(1))).collect();
                format!("[{}]", items.join(", "))
            }
            Ty::Opt(i) => self.opt_expr(i, depth),
            Ty::Str => self.str_expr(depth),
            Ty::StrList => {
                let n = 1 + self.rng.below(4);
                let items: Vec<String> = (0..n).map(|_| self.str_expr(depth.saturating_sub(1))).collect();
                format!("[{}]", items.join(", "))
            }
            Ty::MapSI(i) => {
                let n = self.rng.below(4);
                if n == 0 {
                    return "[:]".into();
                }
                let pairs: Vec<String> = (0..n)
                    .map(|_| {
                        let k = self.str_key();
                        format!("{k}: {}", self.int_expr(i, depth.saturating_sub(1)))
                    })
                    .collect();
                format!("[{}]", pairs.join(", "))
            }
            Ty::MapIS(i) => {
                let n = self.rng.below(4);
                if n == 0 {
                    return "[:]".into();
                }
                let pairs: Vec<String> = (0..n)
                    .map(|_| {
                        let k = self.int_lit(i);
                        format!("{k}: {}", self.str_expr(depth.saturating_sub(1)))
                    })
                    .collect();
                format!("[{}]", pairs.join(", "))
            }
        }
    }

    /// A `String`: a literal or a template made into one, or a copy of (or
    /// something made from) a `String` variable. Variables are only ever
    /// copied from, so none is moved.
    fn str_expr(&mut self, depth: u32) -> String {
        let words = ["", "a", "ab", "héllo", "x y", "Zed", "  pad  ", "line\n", "q\"q", "é"];
        let vars = self.vars(|v| v.ty == Ty::Str);
        let var = |this: &mut Self| vars[this.rng.below(vars.len() as u64) as usize].name.clone();
        let lists = self.vars(|v| v.ty == Ty::StrList);
        let maps = self.vars(|v| matches!(v.ty, Ty::MapIS(_)));
        match self.rng.below(if depth == 0 { 3 } else { 11 }) {
            10 if !maps.is_empty() => {
                let m = &maps[self.rng.below(maps.len() as u64) as usize];
                let Ty::MapIS(k) = m.ty else { unreachable!("a map of Strings") };
                let name = m.name.clone();
                let key = self.int_lit(k);
                format!("({name}.get({key}) mend \"none\".to_string())")
            }
            9 if !lists.is_empty() => {
                let l = &lists[self.rng.below(lists.len() as u64) as usize].name;
                let from = self.int();
                let ix = self.int_expr(from, 0);
                format!("{l}[(({ix} as% u32) as usize) % {l}.len()].copy()")
            }
            0 => format!("{:?}.to_string()", self.rng.pick(&words)),
            1 if !vars.is_empty() => format!("{}.copy()", var(self)),
            2 | 1 => {
                // A template of variables and literals (no braces inside).
                let (i, j) = (self.int(), self.int());
                let (a, b) = (self.int_expr(i, 0), self.int_expr(j, 0));
                format!("\"<{{{a}}}|{{{b}}}>\"")
            }
            3 if !vars.is_empty() => format!("{}.to_upper()", var(self)),
            4 if !vars.is_empty() => format!("{}.to_lower()", var(self)),
            5 if !vars.is_empty() => {
                let n = self.rng.below(3);
                format!("{}.repeat({n})", var(self))
            }
            6 if !vars.is_empty() => {
                let (from, to) = (self.rng.pick(&["a", "é", "", " "]), self.rng.pick(&["", "xx", "-"]));
                format!("{}.replace({from:?}, {to:?})", var(self))
            }
            7 if !vars.is_empty() => format!("{}.trim().to_string()", var(self)),
            _ => format!("{:?}.to_string()", self.rng.pick(&words)),
        }
    }

    fn opt_expr(&mut self, i: Int, depth: u32) -> String {
        let d = depth.saturating_sub(1);
        match self.rng.below(3) {
            0 => {
                let op = self.rng.pick(&["+?", "-?", "*?"]);
                format!("({} {op} {})", self.int_expr(i, d), self.int_expr(i, d))
            }
            1 => {
                let from = self.int();
                format!("({} as? {})", self.int_expr(from, d), i.name())
            }
            _ => match self.vars(|v| v.ty == Ty::Opt(i)).first() {
                Some(v) => v.name.clone(),
                None => format!("({} +? {})", self.int_expr(i, d), self.int_lit(i)),
            },
        }
    }

    fn int_expr(&mut self, i: Int, depth: u32) -> String {
        let d = depth.saturating_sub(1);
        let choice = if depth == 0 { self.rng.below(2) } else { self.rng.below(16) };
        match choice {
            0 => self.int_lit(i),
            1 => {
                let vs = self.vars(|v| v.ty == Ty::Int(i));
                if vs.is_empty() { self.int_lit(i) } else { vs[self.rng.below(vs.len() as u64) as usize].name.clone() }
            }
            2 | 3 => {
                let op = self.rng.pick(&["+", "-", "*"]);
                let policy = match self.rng.below(10) {
                    0 => "",
                    1..=5 => "%",
                    _ => "|",
                };
                format!("({} {op}{policy} {})", self.int_expr(i, d), self.int_expr(i, d))
            }
            4 => {
                // A divisor that isn't zero.
                let op = self.rng.pick(&["/", "%"]);
                format!("({} {op} ({} | 1{}))", self.int_expr(i, d), self.int_expr(i, d), i.name())
            }
            5 => {
                let op = self.rng.pick(&["&", "|", "~"]);
                format!("({} {op} {})", self.int_expr(i, d), self.int_expr(i, d))
            }
            6 => {
                let op = self.rng.pick(&["<<", ">>"]);
                let k = self.rng.below(u64::from(i.bits));
                format!("({} {op} {k})", self.int_expr(i, d))
            }
            7 | 8 => {
                let from = self.int();
                let e = self.int_expr(from, d);
                match self.rng.below(10) {
                    0 => format!("({e} as {})", i.name()),
                    1..=4 => format!("({e} as% {})", i.name()),
                    5..=7 => format!("({e} as| {})", i.name()),
                    _ => format!("(({e} as? {}) mend {})", i.name(), self.int_lit(i)),
                }
            }
            9 => format!("(if {} {{ {} }} else {{ {} }})", self.bool_expr(d), self.int_expr(i, d), self.int_expr(i, d)),
            10 => {
                let fs: Vec<Func> = self.funcs.iter().filter(|f| f.ret == Ty::Int(i)).cloned().collect();
                if fs.is_empty() {
                    return self.int_lit(i);
                }
                let f = fs[self.rng.below(fs.len() as u64) as usize].clone();
                let args: Vec<String> = f.params.iter().map(|&p| self.expr(p, d.min(1))).collect();
                let call = format!("{}({})", f.name, args.join(", "));
                if !f.fallible {
                    call
                } else if self.fallible && self.rng.chance(50) {
                    format!("{call}?")
                } else {
                    format!("({call} mend {})", self.int_lit(i))
                }
            }
            11 => {
                let fields: Vec<String> = self
                    .vars(|v| v.ty == Ty::Rec)
                    .into_iter()
                    .flat_map(|v| {
                        let mut out = Vec::new();
                        if self.a == i {
                            out.push(format!("{}.a", v.name));
                        }
                        if self.b == i {
                            out.push(format!("{}.b", v.name));
                        }
                        out
                    })
                    .collect();
                if fields.is_empty() {
                    self.int_lit(i)
                } else {
                    fields[self.rng.below(fields.len() as u64) as usize].clone()
                }
            }
            12 => {
                let ls = self.vars(|v| v.ty == Ty::List(i));
                match ls.first() {
                    Some(l) => {
                        let from = self.int();
                        let ix = self.int_expr(from, d);
                        format!("{}[(({ix} as% u32) as usize) % {}.len()]", l.name, l.name)
                    }
                    None => self.int_lit(i),
                }
            }
            13 => format!("({} as| {})", self.f64_expr(d), i.name()),
            14 => {
                let maps = self.vars(|v| v.ty == Ty::MapSI(i));
                if !maps.is_empty() && self.rng.chance(50) {
                    let name = maps[self.rng.below(maps.len() as u64) as usize].name.clone();
                    let key = self.str_key();
                    return format!("({name}.get({key}) mend {})", self.int_lit(i));
                }
                match self.vars(|v| matches!(v.ty, Ty::Str | Ty::StrList | Ty::MapSI(_) | Ty::MapIS(_))).first() {
                    Some(s) => format!("({}.len() as% {})", s.name, i.name()),
                    None => self.int_lit(i),
                }
            }
            _ => format!("({} mend {})", self.opt_expr(i, d), self.int_lit(i)),
        }
    }

    fn bool_expr(&mut self, depth: u32) -> String {
        let d = depth.saturating_sub(1);
        let choice = if depth == 0 { self.rng.below(2) } else { self.rng.below(7) };
        match choice {
            0 => self.rng.pick(&["true", "false"]).to_string(),
            1 => {
                let vs = self.vars(|v| v.ty == Ty::Bool);
                if vs.is_empty() { "true".into() } else { vs[self.rng.below(vs.len() as u64) as usize].name.clone() }
            }
            2 | 3 => {
                let i = self.int();
                let op = self.rng.pick(&["==", "!=", "<", "<=", ">", ">="]);
                format!("({} {op} {})", self.int_expr(i, d), self.int_expr(i, d))
            }
            4 => {
                let op = self.rng.pick(&["==", "<", ">="]);
                format!("({} {op} {})", self.f64_expr(d), self.f64_expr(d))
            }
            5 => {
                let maps = self.vars(|v| matches!(v.ty, Ty::MapSI(_) | Ty::MapIS(_)));
                if !maps.is_empty() && self.rng.chance(50) {
                    let m = maps[self.rng.below(maps.len() as u64) as usize].clone();
                    let key = match m.ty {
                        Ty::MapIS(k) => self.int_lit(k),
                        _ => self.str_key(),
                    };
                    return format!("{}.has({key})", m.name);
                }
                let op = self.rng.pick(&["and", "or"]);
                format!("({} {op} {})", self.bool_expr(d), self.bool_expr(d))
            }
            _ => format!("(not {})", self.bool_expr(d)),
        }
    }

    fn f64_expr(&mut self, depth: u32) -> String {
        let d = depth.saturating_sub(1);
        let choice = if depth == 0 { self.rng.below(2) } else { self.rng.below(7) };
        match choice {
            0 => self.f64_lit(),
            1 => {
                let vs = self.vars(|v| v.ty == Ty::F64);
                if vs.is_empty() { self.f64_lit() } else { vs[self.rng.below(vs.len() as u64) as usize].name.clone() }
            }
            2 | 3 => {
                let op = self.rng.pick(&["+", "-", "*", "/"]);
                format!("({} {op} {})", self.f64_expr(d), self.f64_expr(d))
            }
            4 => {
                let i = self.int();
                format!("({} as f64)", self.int_expr(i, d))
            }
            5 => format!("(if {} {{ {} }} else {{ {} }})", self.bool_expr(d), self.f64_expr(d), self.f64_expr(d)),
            _ => match self.vars(|v| v.ty == Ty::Rec).first() {
                Some(r) => format!("{}.f", r.name),
                None => self.f64_lit(),
            },
        }
    }

    // ------------------------------------------------------------ statements

    /// A type for a new local.
    fn local_ty(&mut self) -> Ty {
        match self.rng.below(15) {
            0..=3 => Ty::Int(self.int()),
            4 => Ty::Bool,
            5 => Ty::F64,
            6 => Ty::Rec,
            7 => Ty::Sum,
            8 => Ty::List(self.int()),
            9 => Ty::Opt(self.int()),
            10 => Ty::Str,
            11 => Ty::StrList,
            12 => Ty::MapSI(self.int()),
            _ => Ty::MapIS(self.int()),
        }
    }

    /// Statements, each at `indent`, one per string (a statement may span
    /// lines).
    fn stmts(&mut self, n: u32, depth: u32, indent: usize) -> Vec<String> {
        (0..n).map(|_| self.stmt(depth, indent)).collect()
    }

    fn block(&mut self, n: u32, depth: u32, indent: usize) -> String {
        self.scopes.push(Vec::new());
        let body = self.stmts(n, depth, indent + 1);
        self.scopes.pop();
        let pad = "    ".repeat(indent);
        let mut out = String::from("{\n");
        for s in body {
            out.push_str(&s);
        }
        out.push_str(&pad);
        out.push('}');
        out
    }

    fn stmt(&mut self, depth: u32, indent: usize) -> String {
        let pad = "    ".repeat(indent);
        // In a shuttle: a `yield`, wherever a statement goes, and now and
        // then an early `return`.
        if let Some(t) = self.yields {
            if self.rng.chance(15) {
                return format!("{pad}yield {}\n", self.expr(t, 2));
            }
            if self.rng.chance(4) {
                return format!("{pad}if {} {{ return }}\n", self.bool_expr(1));
            }
        }
        let choice = if depth == 0 { self.rng.below(4) } else { self.rng.below(14) };
        match choice {
            0 | 1 => {
                let t = self.local_ty();
                let e = self.expr(t, 3);
                let mutable = self.rng.chance(40);
                let name = self.fresh("v");
                let s = format!("{pad}{} {name}: {} = {e}\n", if mutable { "var" } else { "let" }, self.ty_name(t));
                self.bind(&name, t, mutable);
                s
            }
            2 => self.print(indent),
            3 => {
                // Change a `var`.
                let vs = self.vars(|v| {
                    v.mutable
                        && matches!(
                            v.ty,
                            Ty::Int(_)
                                | Ty::F64
                                | Ty::Bool
                                | Ty::Rec
                                | Ty::List(_)
                                | Ty::Str
                                | Ty::StrList
                                | Ty::MapSI(_)
                                | Ty::MapIS(_)
                        )
                });
                if vs.is_empty() {
                    return self.print(indent);
                }
                let v = vs[self.rng.below(vs.len() as u64) as usize].clone();
                match v.ty {
                    Ty::MapSI(i) => {
                        let key = self.str_key();
                        match self.rng.below(3) {
                            0 => format!("{pad}{}[{key}] = {}\n", v.name, self.int_expr(i, 2)),
                            1 => format!("{pad}print(\"removed {{{}.remove({key})}}\")\n", v.name),
                            _ => {
                                let val = self.int_expr(i, 1);
                                format!(
                                    "{pad}print(\"inserted {{{}.insert({key}, {val})}}\")
",
                                    v.name
                                )
                            }
                        }
                    }
                    Ty::MapIS(k) => {
                        let key = self.int_lit(k);
                        match self.rng.below(3) {
                            0 => format!("{pad}{}[{key}] = {}\n", v.name, self.str_expr(2)),
                            1 => format!("{pad}print(\"removed {{{}.remove({key})}}\")\n", v.name),
                            _ => format!("{pad}{}[{key}] = {}\n", v.name, self.str_expr(1)),
                        }
                    }
                    Ty::StrList => {
                        let e = self.str_expr(2);
                        match self.rng.below(3) {
                            0 => format!("{pad}{}.push({e})\n", v.name),
                            1 => {
                                let from = self.int();
                                let ix = self.int_expr(from, 1);
                                format!("{pad}{0}[(({ix} as% u32) as usize) % {0}.len()] = {e}\n", v.name)
                            }
                            // Out one, which stays non-empty.
                            _ => {
                                let t = self.fresh("t");
                                format!(
                                    "{pad}if {0}.len() > 1 {{\n{pad}    let {t} = {0}.remove(0)\n{pad}    print(\"removed {{{t}}}\")\n{pad}}}\n",
                                    v.name
                                )
                            }
                        }
                    }
                    Ty::Str if self.rng.chance(60) => {
                        let piece = self.rng.pick(&["a", "é", "xyz", " ", ""]);
                        format!("{pad}{}.push_str({piece:?})\n", v.name)
                    }
                    Ty::Rec => {
                        let a = self.a;
                        // Through the projection that lends `a`, or directly.
                        match self.rng.below(4) {
                            0 => format!("{pad}lend_a(mut {}) = {}\n", v.name, self.int_expr(a, 2)),
                            1 => format!("{pad}lend_a(mut {}) +%= {}\n", v.name, self.int_expr(a, 1)),
                            _ => format!("{pad}{}.a = {}\n", v.name, self.int_expr(a, 2)),
                        }
                    }
                    Ty::List(i) => {
                        if self.rng.chance(50) {
                            format!("{pad}{}.push({})\n", v.name, self.int_expr(i, 2))
                        } else {
                            let from = self.int();
                            let ix = self.int_expr(from, 1);
                            let e = self.int_expr(i, 2);
                            format!("{pad}{}[(({ix} as% u32) as usize) % {}.len()] = {e}\n", v.name, v.name)
                        }
                    }
                    t => format!("{pad}{} = {}\n", v.name, self.expr(t, 2)),
                }
            }
            4 => format!(
                "{pad}if {} {} else {}\n",
                self.bool_expr(2),
                self.block(2, depth - 1, indent),
                self.block(1, depth - 1, indent)
            ),
            5 => {
                let i = self.fresh("i");
                let n = self.rng.below(4);
                self.scopes.push(vec![Var {
                    name: i.clone(),
                    ty: Ty::Int(Int { signed: false, bits: 8 }),
                    mutable: false,
                }]);
                let body = self.block(2, depth - 1, indent);
                self.scopes.pop();
                format!("{pad}for {i} in 0..{n}u8 {body}\n")
            }
            6 => {
                let w = self.fresh("w");
                let n = self.rng.below(4);
                let body = self.block(2, depth - 1, indent + 1);
                let pad1 = "    ".repeat(indent + 1);
                format!(
                    "{pad}{{\n{pad1}var {w} = 0u8\n{pad1}while {w} < {n}u8 {{\n{pad1}    {w} +%= 1\n{pad1}    {body}\n{pad1}}}\n{pad}}}\n"
                )
            }
            7 if self.defers_ok => {
                let n = self.fresh("d");
                format!("{pad}defer print(\"defer {n}\")\n")
            }
            12 if !self.shuttles.is_empty() => self.consume(depth, indent),
            8 => self.int_match(indent),
            9 => self.sum_match(indent),
            10 if self.rng.chance(30) => {
                // A map, read in a loop, in the order things went in.
                let (m, x, k, acc) = (self.fresh("m"), self.fresh("v"), self.fresh("k"), self.fresh("t"));
                if self.rng.chance(50) {
                    let i = self.int();
                    let items = self.expr(Ty::MapSI(i), 2);
                    format!(
                        "{pad}let {m}: Map[String, {0}] = {items}\n{pad}var {acc} = 0usize\n{pad}for ({k}, {x}) in {m} {{ {acc} = {acc} +% {k}.len() +% 1usize\n{pad}    print(\"{{{k}}}={{{x}}}\") }}\n{pad}print(\"{{{acc}}} {{{m}}}\")\n",
                        i.name()
                    )
                } else {
                    let i = self.int();
                    let items = self.expr(Ty::MapIS(i), 2);
                    format!(
                        "{pad}let {m}: Map[{0}, String] = {items}\n{pad}var {acc} = 0usize\n{pad}for ({k}, {x}) in {m} {{ {acc} = {acc} +% {x}.len() +% 1usize\n{pad}    print(\"{{{k}}}={{{x}}}\") }}\n{pad}print(\"{{{acc}}} {{{m}}}\")\n",
                        i.name()
                    )
                }
            }
            10 if self.rng.chance(40) => {
                // A list of Strings, read in a loop.
                let l = self.fresh("l");
                let items = self.expr(Ty::StrList, 2);
                let (x, acc) = (self.fresh("x"), self.fresh("t"));
                format!(
                    "{pad}let {l}: List[String] = {items}\n{pad}var {acc} = 0usize\n{pad}for {x} in {l} {{ {acc} = {acc} +% {x}.len() }}\n{pad}print(\"{{{acc}}} {{{l}}}\")\n"
                )
            }
            10 => {
                // A list, read in a loop.
                let i = self.int();
                let l = self.fresh("l");
                let items = self.expr(Ty::List(i), 2);
                let x = self.fresh("x");
                let acc = self.fresh("t");
                format!(
                    "{pad}var {l}: List[{0}] = {items}\n{pad}var {acc} = 0{0}\n{pad}for {x} in {l} {{ {acc} = {acc} +% {x} }}\n{pad}print(\"{{{acc}}} {{{l}.len()}}\")\n",
                    i.name()
                )
            }
            11 => {
                // A fallible call, mended.
                let fs: Vec<Func> = self.funcs.iter().filter(|f| f.fallible).cloned().collect();
                let Some(f) = fs.first().cloned() else { return self.print(indent) };
                let args: Vec<String> = f.params.iter().map(|&p| self.expr(p, 1)).collect();
                let name = self.fresh("r");
                let fallback = self.expr(f.ret, 0);
                let s = format!(
                    "{pad}let {name}: {} = {}({}) mend e => {{\n{pad}    print(\"failed: {{e}}\")\n{pad}    {fallback}\n{pad}}}\n",
                    self.ty_name(f.ret),
                    f.name,
                    args.join(", ")
                );
                self.bind(&name, f.ret, false);
                s
            }
            13 => self.lambda_stmt(indent),
            _ => {
                if self.fallible && self.rng.chance(50) {
                    format!(
                        "{pad}if {} {{ fail .Bad({}) }}\n",
                        self.bool_expr(2),
                        self.int_expr(Int { signed: true, bits: 64 }, 1)
                    )
                } else {
                    self.print(indent)
                }
            }
        }
    }

    /// Goes through a shuttle: a generator in a `for` loop (called there,
    /// or held in a variable first, and maybe left early), collected, or
    /// taken from; a duplex shuttle pushed a few inputs, then closed.
    fn consume(&mut self, depth: u32, indent: usize) -> String {
        let pad = "    ".repeat(indent);
        let sh = self.shuttles[self.rng.below(self.shuttles.len() as u64) as usize].clone();
        let args: Vec<String> = sh.params.iter().map(|&p| self.expr(p, 1)).collect();
        let call = format!("{}({})", sh.name, args.join(", "));
        if let Some(k) = sh.pulls {
            let (d, v) = (self.fresh("d"), self.fresh("v"));
            let n = 1 + self.rng.below(4);
            let inputs: Vec<String> = (0..n).map(|_| self.int_lit(k)).collect();
            return format!(
                "{pad}var {d} = {call}\n{pad}for {v} in [{}] {{\n{pad}    print(\"pushed: {{{d}.push({v}).collect()}}\")\n{pad}}}\n{pad}print(\"closed: {{{d}.close().collect()}}\")\n",
                inputs.join(", ")
            );
        }
        match self.rng.below(if sh.defers { 3 } else { 4 }) {
            0 | 1 => {
                let x = self.fresh("x");
                let (before, over) = if self.rng.chance(50) {
                    let g = self.fresh("g");
                    (format!("{pad}let {g} = {call}\n"), g)
                } else {
                    (String::new(), call)
                };
                self.scopes.push(vec![Var { name: x.clone(), ty: sh.item, mutable: false }]);
                let n = 1 + self.rng.below(2) as u32;
                let body = self.stmts(n, depth.saturating_sub(1), indent + 1);
                let leave = if self.rng.chance(40) {
                    format!("{pad}    if {} {{ break }}\n", self.bool_expr(1))
                } else {
                    String::new()
                };
                self.scopes.pop();
                format!(
                    "{before}{pad}for {x} in {over} {{\n{pad}    print(\"item {{{x}}}\")\n{}{leave}{pad}}}\n",
                    body.concat()
                )
            }
            2 => format!("{pad}print({call}.collect())\n"),
            _ => format!("{pad}print({call}.take({}).collect())\n", self.rng.below(4)),
        }
    }

    /// A shuttle: a generator, or a duplex shuttle pulling integers, whose
    /// `yield`s the statements put anywhere.
    fn shuttle(&mut self) -> String {
        let duplex = self.rng.chance(35);
        let item = if self.rng.chance(30) { Ty::Str } else { Ty::Int(self.int()) };
        let defers = self.rng.chance(50);
        let name = self.fresh(if duplex { "dup" } else { "gen" });
        self.scopes = vec![Vec::new()];
        let mut sig = Vec::new();
        let params: Vec<Ty> = (0..self.rng.below(3))
            .map(|_| match self.rng.below(4) {
                0 => Ty::Bool,
                1 => Ty::F64,
                _ => Ty::Int(self.int()),
            })
            .collect();
        for &p in &params {
            let pn = self.fresh("p");
            sig.push(format!("{pn}: {}", self.ty_name(p)));
            self.bind(&pn, p, false);
        }
        self.yields = Some(item);
        self.defers_ok = defers;
        let n = 1 + self.rng.below(3) as u32;
        let mut body = self.stmts(n, 2, 1);
        let pulls = duplex.then(|| self.int());
        if let Some(k) = pulls {
            let x = self.fresh("x");
            self.scopes.push(vec![Var { name: x.clone(), ty: Ty::Int(k), mutable: false }]);
            let n = 1 + self.rng.below(3) as u32;
            let inner = self.stmts(n, 1, 2);
            let last = self.expr(item, 2);
            self.scopes.pop();
            body.push(format!("    while pull is some({x}) {{\n{}        yield {last}\n    }}\n", inner.concat()));
            let n = self.rng.below(2) as u32;
            body.extend(self.stmts(n, 1, 1));
        }
        body.push(format!("    yield {}\n", self.expr(item, 2)));
        self.yields = None;
        self.defers_ok = true;
        let pulled = pulls.map(|k| format!(" <~ {}", k.name())).unwrap_or_default();
        let mut s = format!("fun {name}({}) ~> {}{pulled} {{\n", sig.join(", "), self.ty_name(item));
        for b in body {
            s.push_str(&b);
        }
        s.push_str("}\n\n");
        self.shuttles.push(Shut { name, params, item, pulls, defers });
        s
    }

    /// Lambdas that capture locals, given to `map`, `filter` and `each` of a
    /// list (`each` changes a captured `var`).
    fn lambda_stmt(&mut self, indent: usize) -> String {
        let pad = "    ".repeat(indent);
        let strs = self.vars(|v| v.ty == Ty::StrList);
        let ints = self.vars(|v| matches!(v.ty, Ty::List(_)));
        let (x, r, acc) = (self.fresh("x"), self.fresh("r"), self.fresh("n"));
        if !strs.is_empty() && (ints.is_empty() || self.rng.chance(40)) {
            let l = strs[self.rng.below(strs.len() as u64) as usize].name.clone();
            let capture = self.vars(|v| v.ty == Ty::Str).first().map(|v| v.name.clone());
            return match (self.rng.below(4), capture) {
                (0, _) => {
                    format!("{pad}let {r}: List[String] = {l}.map({x} => {x}.to_upper())\n{pad}print(\"{{{r}}}\")\n")
                }
                (1, Some(c)) => {
                    format!(
                        "{pad}let {r}: List[String] = {l}.map({x} => \"{{{x}}}{{{c}}}\")\n{pad}print(\"{{{r}}}\")\n"
                    )
                }
                (1 | 2, _) => {
                    let n = self.rng.below(3);
                    format!(
                        "{pad}let {r}: List[String] = {l}.filter({x} => {x}.len() > {n})\n{pad}print(\"{{{r}}}\")\n"
                    )
                }
                _ => format!(
                    "{pad}var {acc} = 0usize\n{pad}{l}.each({x} => {acc} = {acc} +% {x}.len())\n{pad}print(\"{{{acc}}}\")\n"
                ),
            };
        }
        let Some(Var { name: l, ty: Ty::List(i), .. }) =
            ints.get(self.rng.below(ints.len().max(1) as u64) as usize).cloned()
        else {
            return self.print(indent);
        };
        let captured = self.vars(|v| v.ty == Ty::Int(i)).first().map(|v| v.name.clone());
        let cap = captured.unwrap_or_else(|| self.int_lit(i));
        let t = i.name();
        match self.rng.below(5) {
            0 => format!("{pad}let {r}: List[{t}] = {l}.map({x} => ({x} +% {cap}))\n{pad}print(\"{{{r}}}\")\n"),
            1 => format!("{pad}let {r}: List[{t}] = {l}.map({x} => ({x} *| {cap}))\n{pad}print(\"{{{r}}}\")\n"),
            2 | 3 => format!("{pad}let {r}: List[{t}] = {l}.filter({x} => {x} > {cap})\n{pad}print(\"{{{r}}}\")\n"),
            _ => format!(
                "{pad}var {acc} = 0{t}\n{pad}{l}.each({x} => {acc} = {acc} +% {x})\n{pad}print(\"{{{acc}}}\")\n"
            ),
        }
    }

    fn print(&mut self, indent: usize) -> String {
        let pad = "    ".repeat(indent);
        let shown: Vec<Var> = self.vars(|v| !matches!(v.ty, Ty::Sum));
        if shown.is_empty() {
            return format!("{pad}print(\"-\")\n");
        }
        let n = 1 + self.rng.below(3);
        let mut parts = Vec::new();
        for _ in 0..n {
            let v = &shown[self.rng.below(shown.len() as u64) as usize];
            match v.ty {
                Ty::Rec => parts.push(format!("{{{0}.a}} {{{0}.b}} {{{0}.f}}", v.name)),
                _ => parts.push(format!("{{{}}}", v.name)),
            }
        }
        format!("{pad}print(\"{}\")\n", parts.join(" "))
    }

    fn int_match(&mut self, indent: usize) -> String {
        let pad = "    ".repeat(indent);
        let (on, to) = (self.int(), self.int());
        let scrutinee = self.int_expr(on, 2);
        // A literal, a range not holding it, and the rest.
        let lit: i128 = self.rng.below(4) as i128;
        let lit = lit.clamp(on.min(), on.max());
        let lo = (lit + 1).min(on.max());
        let hi = (lo + self.rng.below(5) as i128).min(on.max());
        let name = self.fresh("m");
        let mut s = format!("{pad}let {name}: {} = match {scrutinee} {{\n", to.name());
        let _ = writeln!(s, "{pad}    {lit} => {}", self.int_expr(to, 1));
        if lo > lit && lo <= hi {
            let _ = writeln!(s, "{pad}    {lo}..={hi} => {}", self.int_expr(to, 1));
        }
        let _ = writeln!(s, "{pad}    _ => {}", self.int_expr(to, 1));
        let _ = writeln!(s, "{pad}}}");
        self.bind(&name, Ty::Int(to), false);
        s
    }

    fn sum_match(&mut self, indent: usize) -> String {
        let pad = "    ".repeat(indent);
        let sum = self.expr(Ty::Sum, 2);
        let s_name = self.fresh("s");
        let to = self.int();
        let (x, y, z) = (self.fresh("x"), self.fresh("y"), self.fresh("z"));
        let (a, b) = (self.a, self.b);
        let mut s = format!("{pad}let {s_name} = {sum}\n");
        let name = self.fresh("m");
        let _ = writeln!(s, "{pad}let {name}: {} = match {s_name} {{", to.name());
        self.scopes.push(vec![Var { name: x.clone(), ty: Ty::Int(a), mutable: false }]);
        let _ = writeln!(s, "{pad}    .P({x}) => {}", self.int_expr(to, 2));
        self.scopes.pop();
        let _ = writeln!(s, "{pad}    .Q => {}", self.int_expr(to, 1));
        self.scopes.push(vec![
            Var { name: y.clone(), ty: Ty::Int(b), mutable: false },
            Var { name: z.clone(), ty: Ty::Bool, mutable: false },
        ]);
        let _ = writeln!(s, "{pad}    .W({y}, {z}) => {}", self.int_expr(to, 2));
        self.scopes.pop();
        let _ = writeln!(s, "{pad}}}");
        self.bind(&name, Ty::Int(to), false);
        s
    }

    // ------------------------------------------------------------ programs

    fn func(&mut self) -> String {
        let n = self.rng.below(3) + 1;
        let params: Vec<Ty> = (0..n)
            .map(|_| match self.rng.below(4) {
                0 => Ty::Bool,
                1 => Ty::F64,
                _ => Ty::Int(self.int()),
            })
            .collect();
        let ret = if self.rng.chance(80) { Ty::Int(self.int()) } else { Ty::F64 };
        let fallible = self.rng.chance(35);
        let name = self.fresh("f");
        self.scopes = vec![Vec::new()];
        let mut sig = Vec::new();
        for &p in &params {
            let pn = self.fresh("p");
            sig.push(format!("{pn}: {}", self.ty_name(p)));
            self.bind(&pn, p, false);
        }
        self.fallible = fallible;
        let n = 1 + self.rng.below(4) as u32;
        let body = self.stmts(n, 2, 1);
        let result = self.expr(ret, 3);
        self.fallible = false;
        let mut s = format!(
            "fun {name}({}) -> {}{} {{\n",
            sig.join(", "),
            self.ty_name(ret),
            if fallible { " ! E" } else { "" }
        );
        for b in body {
            s.push_str(&b);
        }
        let _ = writeln!(s, "    {result}\n}}\n");
        self.funcs.push(Func { name, params, ret, fallible });
        s
    }

    /// The program: everything before `main`'s statements, `main`'s
    /// statements, and what closes it.
    fn program(&mut self) -> (String, Vec<String>, String) {
        let (a, b) = (self.a.name(), self.b.name());
        let mut head = format!(
            "use std.io.print\n\nwarp R {{\n    a: {a}\n    b: {b}\n    f: f64\n}}\n\nwarp S {{\n    case P(x: {a})\n    case Q\n    case W(y: {b}, z: bool)\n}}\n\nwarp E: Error, Show {{\n    case Bad(i64)\n    case Worse\n}}\n\nfun lend_a(mut r: R) ~> mut {a} {{\n    yield mut r.a\n    print(\"lent a: {{r.a}}\")\n}}\n\n"
        );
        for _ in 0..1 + self.rng.below(4) {
            head.push_str(&self.func());
        }
        for _ in 0..self.rng.below(4) {
            head.push_str(&self.shuttle());
        }
        // Constants (baked in, V4), each also made by a function from the same
        // text: `main` says if they show differently.
        self.scopes = vec![Vec::new()];
        let mut checks = Vec::new();
        let main_rng = std::mem::replace(&mut self.rng, self.const_rng.clone());
        let next = self.next;
        for k in 0..self.rng.below(4) {
            let ty = match self.rng.below(8) {
                0 => Ty::Int(self.int()),
                1 => Ty::Bool,
                2 => Ty::F64,
                3 => Ty::Rec,
                4 => Ty::List(self.int()),
                5 => Ty::Opt(self.int()),
                6 => Ty::Str,
                _ => Ty::StrList,
            };
            let (text, name) = (self.expr(ty, 0), self.ty_name(ty));
            let _ = write!(head, "const KC_{k}: {name} = {text}\n\nfun mk_{k}() -> {name} {{\n    {text}\n}}\n\n");
            checks.push(format!(
                "    if \"{{KC_{k}:?}}\" != \"{{mk_{k}():?}}\" {{ print(\"CONST MISMATCH {k}\") }}\n    print(\"{{KC_{k}:?}}\")\n"
            ));
        }
        self.rng = main_rng;
        self.next = next;
        head.push_str("fun main() {\n");
        self.scopes = vec![Vec::new()];
        let n = 10 + self.rng.below(20) as u32;
        let mut main = checks;
        main.extend(self.stmts(n, 3, 1));
        (head, main, "}\n".into())
    }
}

// ---------------------------------------------------------------- running

/// How a run on one backend went: its output and how it ended, or why it
/// didn't compile.
fn run_lua(text: &str) -> Result<(String, String), String> {
    let path = Path::new("gen.loom");
    let mut db = SourceDb::new();
    let root = db.add("gen.loom".to_string(), text.to_string());
    let resolved = resolve(&mut db, root, &FsLoader::new(path));
    if resolved.has_errors() {
        return Err(rendered(&db, &resolved.diagnostics));
    }
    let typed = typeck::check(&resolved.program);
    if typed.has_errors() {
        return Err(rendered(&db, &typed.diagnostics));
    }
    let bundle = lower::lower(&db, &resolved.program, &typed).map_err(|d| rendered(&db, &d))?;
    let buffer = Rc::new(RefCell::new(Vec::new()));
    let outcome = run_bundle(&bundle.lua, Some(&bundle.map), Host::captured(buffer.clone()));
    let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
    Ok((out, ending(outcome)))
}

fn run_vm(text: &str, level: Level) -> Result<(String, String), String> {
    let path = Path::new("gen.loom");
    let mut db = SourceDb::new();
    let root = db.add("gen.loom".to_string(), text.to_string());
    let resolved = resolve(&mut db, root, &FsLoader::new(path));
    if resolved.has_errors() {
        return Err(rendered(&db, &resolved.diagnostics));
    }
    let typed = typeck::check_for(&resolved.program, Target::VM);
    if typed.has_errors() {
        return Err(rendered(&db, &typed.diagnostics));
    }
    let compiled = treadle::compile_at(&db, &resolved.program, &typed, level).map_err(|d| rendered(&db, &d))?;
    let buffer = Rc::new(RefCell::new(Vec::new()));
    let config = loom_vm::Config { stack_limit: 64 << 20, ..loom_vm::Config::default() };
    let outcome = loom_vm::run(&compiled.image, Host::captured(buffer.clone()), &config);
    let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
    Ok((out, ending(outcome)))
}

fn rendered(db: &SourceDb, diags: &[loom_syntax::Diagnostic]) -> String {
    diags.iter().map(|d| render_plain(db, d)).collect()
}

fn ending(outcome: Outcome) -> String {
    match outcome {
        Outcome::Finished => "finished".into(),
        Outcome::Unravel { message, location, .. } => {
            format!("unravel {message:?} at {:?}", location.map(|l| (l.line, l.column)))
        }
        Outcome::Failed { message } => format!("failed {message:?}"),
        other => format!("{other:?}"),
    }
}

/// What's wrong with `text`: it doesn't compile, or the backends (Lua, and
/// Treadle at `-O2` and `-O0`) disagree.
fn problem(text: &str) -> Option<String> {
    let lua = match run_lua(text) {
        Ok(r) => r,
        Err(e) => return Some(format!("doesn't compile (a generator bug):\n{e}")),
    };
    if lua.0.contains("CONST MISMATCH") {
        return Some(format!("a constant isn't what its function makes (Lua)\n{}", lua.0));
    }
    for level in [Level::O2, Level::O0] {
        let vm = match run_vm(text, level) {
            Ok(r) => r,
            Err(e) => return Some(format!("doesn't compile for Treadle at {level:?}:\n{e}")),
        };
        if vm.0.contains("CONST MISMATCH") {
            return Some(format!("a constant isn't what its function makes (Treadle at {level:?})\n{}", vm.0));
        }
        if lua != vm {
            return Some(format!(
                "the backends disagree\n--- Lua ({})\n{}--- Treadle at {level:?} ({})\n{}",
                lua.1, lua.0, vm.1, vm.0
            ));
        }
    }
    None
}

/// `main`'s statements, as few as still show the problem (and compile).
fn shrink(head: &str, mut main: Vec<String>, tail: &str) -> Vec<String> {
    let still = |main: &[String]| {
        let text = format!("{head}{}{tail}", main.concat());
        problem(&text).is_some_and(|p| p.starts_with("the backends disagree"))
    };
    let mut i = main.len();
    while i > 0 {
        i -= 1;
        let mut fewer = main.clone();
        fewer.remove(i);
        if still(&fewer) {
            main = fewer;
        }
    }
    main
}

fn check_seeds(seeds: std::ops::Range<u64>) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16) as u64;
    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let seeds = seeds.clone();
                std::thread::Builder::new()
                    .stack_size(64 << 20)
                    .spawn_scoped(scope, move || {
                        let mut out = Vec::new();
                        for seed in seeds.filter(|s| s % threads == t) {
                            let (head, main, tail) = Gen::new(seed).program();
                            let text = format!("{head}{}{tail}", main.concat());
                            if let Some(p) = problem(&text) {
                                let main = if p.starts_with("the backends disagree") {
                                    shrink(&head, main, &tail)
                                } else {
                                    main
                                };
                                let text = format!("{head}{}{tail}", main.concat());
                                let p = problem(&text).unwrap_or(p);
                                out.push(format!("seed {seed}: {p}\n--- program\n{text}"));
                            }
                        }
                        out
                    })
                    .expect("a thread")
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("no panics")).collect()
    });
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds.end - seeds.start,
        failures.join("\n\n")
    );
}

#[test]
fn generated_programs_agree() {
    check_seeds(0..300);
}

#[test]
#[ignore = "long: 20 000 seeds"]
fn many_generated_programs_agree() {
    check_seeds(300..20_300);
}

/// The generator makes what it's meant to (a kind of value it never made
/// would test nothing).
#[test]
fn the_generator_covers_its_kinds() {
    let mut text = String::new();
    for seed in 0..300 {
        let (head, main, tail) = Gen::new(seed).program();
        text.push_str(&format!("{head}{}{tail}", main.concat()));
    }
    for (what, needle) in [
        ("a String variable", ": String = "),
        ("push_str", ".push_str("),
        ("a list of Strings", ": List[String] = "),
        ("removing a String", ".remove(0)"),
        ("a map of Strings to integers", ": Map[String, "),
        ("a map of integers to Strings", "String] = "),
        ("a lambda over Strings", ".to_upper())"),
        ("a lambda changing a captured var", ".each("),
        ("a shuttle", ") ~> "),
        ("a duplex shuttle", " <~ "),
        ("a yield", "    yield "),
        ("a generator gone through in place", "for x_"),
        ("a generator held, then gone through", "let g_"),
        ("a generator taken from", ".take("),
        ("a push", ".push(v_"),
        ("a projection", "lend_a(mut "),
    ] {
        assert!(text.matches(needle).count() >= 10, "{what}: {} in 300 programs", text.matches(needle).count());
    }
}
