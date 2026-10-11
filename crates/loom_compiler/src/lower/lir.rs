//! LIR: a Lua-shaped IR, and its printer.
//!
//! Lowering builds LIR rather than text so that it can still insert
//! statements (spilling operands into temporaries) and so the printer can
//! record which Loom span each Lua line came from.

use loom_syntax::source::Span;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Name(String),
    /// `t[k]`.
    Index(Box<Expr>, Box<Expr>),
    /// `t.name`.
    Field(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    /// `{a, b, c}`.
    Table(Vec<Expr>),
    /// `{x = 1, [2] = y}`: a record, or a case with its `tag`.
    Record(Vec<(TableKey, Expr)>),
    Function(Vec<String>, Vec<Stmt>),
}

impl Expr {
    pub fn name(s: impl Into<String>) -> Expr {
        Expr::Name(s.into())
    }

    pub fn call(callee: Expr, args: Vec<Expr>) -> Expr {
        Expr::Call(Box::new(callee), args)
    }

    pub fn field(base: Expr, name: &str) -> Expr {
        Expr::Field(Box::new(base), name.to_string())
    }

    pub fn bin(op: &'static str, a: Expr, b: Expr) -> Expr {
        Expr::Binary(op, Box::new(a), Box::new(b))
    }

    pub fn index(base: Expr, key: Expr) -> Expr {
        Expr::Index(Box::new(base), Box::new(key))
    }

    /// `rt.<name>(args…)`.
    pub fn rt(name: &str, args: Vec<Expr>) -> Expr {
        Expr::call(Expr::field(Expr::name("rt"), name), args)
    }

    /// A constant: evaluating it has no effect and can't be affected by
    /// anything, so it never needs spilling.
    pub fn is_constant(&self) -> bool {
        matches!(self, Expr::Nil | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_))
    }

    /// Whether this nests calls, tables and operators more than `n` deep
    /// (not counting what's inside functions, which start again).
    pub fn deeper_than(&self, n: u32) -> bool {
        let Some(n) = n.checked_sub(1) else { return true };
        match self {
            Expr::Index(a, b) | Expr::Binary(_, a, b) => a.deeper_than(n) || b.deeper_than(n),
            Expr::Field(a, _) | Expr::Unary(_, a) => a.deeper_than(n),
            Expr::Call(callee, args) => callee.deeper_than(n) || args.iter().any(|a| a.deeper_than(n)),
            Expr::Table(items) => items.iter().any(|a| a.deeper_than(n)),
            Expr::Record(entries) => entries.iter().any(|(_, v)| v.deeper_than(n)),
            Expr::Function(..)
            | Expr::Nil
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Name(_) => false,
        }
    }
}

/// A key in a keyed table constructor.
#[derive(Clone, Debug, PartialEq)]
pub enum TableKey {
    Name(String),
    Index(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    /// The Loom code this statement comes from.
    pub span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// `local a, b = x, y` (no values: just the declaration).
    Local(Vec<String>, Vec<Expr>),
    /// `local name <close> = value`: closed when its block ends, however it
    /// ends (a `defer`).
    LocalClose(String, Expr),
    Assign(Vec<Expr>, Vec<Expr>),
    /// A call evaluated for its effects.
    Call(Expr),
    /// `if c1 then … elseif c2 then … else … end`.
    If(Vec<(Expr, Vec<Stmt>)>, Option<Vec<Stmt>>),
    While(Expr, Vec<Stmt>),
    /// `for var = start, stop do … end`.
    NumFor(String, Expr, Expr, Vec<Stmt>),
    /// `for a, b in f do … end`.
    GenFor(Vec<String>, Expr, Vec<Stmt>),
    Do(Vec<Stmt>),
    Goto(String),
    Label(String),
    /// Always printed as `do return … end`, so it can go anywhere.
    Return(Option<Expr>),
    /// `do return a, b, c end`: a result, then the final values of the
    /// function's scalar `mut` parameters.
    ReturnMany(Vec<Expr>),
    Break,
}

/// Lua source text plus, for each line that starts a statement with a span,
/// that span.
#[derive(Debug, Default)]
pub struct Printed {
    pub text: String,
    /// `(1-based Lua line, Loom span)`, in line order.
    pub lines: Vec<(u32, Span)>,
}

pub struct Printer {
    out: Printed,
    line: u32,
    indent: usize,
}

impl Printer {
    /// Starts printing after `preamble`, whose lines count toward positions.
    pub fn new(preamble: &str) -> Self {
        let line = 1 + preamble.matches('\n').count() as u32;
        Printer { out: Printed { text: preamble.to_string(), lines: Vec::new() }, line, indent: 0 }
    }

    pub fn finish(self) -> Printed {
        self.out
    }

    /// A line of raw text, with no span.
    pub fn raw(&mut self, text: &str) {
        self.out.text.push_str(text);
        self.out.text.push('\n');
        self.line += 1 + text.matches('\n').count() as u32;
    }

    fn line_start(&mut self, span: Option<Span>) {
        if let Some(span) = span {
            self.out.lines.push((self.line, span));
        }
        for _ in 0..self.indent {
            self.out.text.push_str("  ");
        }
    }

    fn end_line(&mut self) {
        self.out.text.push('\n');
        self.line += 1;
    }

    pub fn stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn block(&mut self, stmts: &[Stmt]) {
        self.indent += 1;
        self.stmts(stmts);
        self.indent -= 1;
    }

    pub fn stmt(&mut self, s: &Stmt) {
        self.line_start(s.span);
        match &s.kind {
            StmtKind::Local(names, values) => {
                self.write(&format!("local {}", names.join(", ")));
                if !values.is_empty() {
                    self.write(" = ");
                    self.exprs(values);
                }
                self.end_line();
            }
            StmtKind::LocalClose(name, value) => {
                self.write(&format!("local {name} <close> = "));
                self.expr(value, 0);
                self.end_line();
            }
            StmtKind::Assign(targets, values) => {
                self.exprs(targets);
                self.write(" = ");
                self.exprs(values);
                self.end_line();
            }
            StmtKind::Call(e) => {
                self.expr(e, 0);
                self.end_line();
            }
            StmtKind::If(arms, else_) => {
                for (i, (cond, body)) in arms.iter().enumerate() {
                    if i > 0 {
                        self.line_start(None);
                    }
                    self.write(if i == 0 { "if " } else { "elseif " });
                    self.expr(cond, 0);
                    self.write(" then");
                    self.end_line();
                    self.block(body);
                }
                if let Some(body) = else_ {
                    self.line_start(None);
                    self.write("else");
                    self.end_line();
                    self.block(body);
                }
                self.line_start(None);
                self.write("end");
                self.end_line();
            }
            StmtKind::While(cond, body) => {
                self.write("while ");
                self.expr(cond, 0);
                self.write(" do");
                self.end_line();
                self.block(body);
                self.line_start(None);
                self.write("end");
                self.end_line();
            }
            StmtKind::NumFor(var, start, stop, body) => {
                self.write(&format!("for {var} = "));
                self.expr(start, 0);
                self.write(", ");
                self.expr(stop, 0);
                self.write(" do");
                self.end_line();
                self.block(body);
                self.line_start(None);
                self.write("end");
                self.end_line();
            }
            StmtKind::GenFor(vars, f, body) => {
                self.write(&format!("for {} in ", vars.join(", ")));
                self.expr(f, 0);
                self.write(" do");
                self.end_line();
                self.block(body);
                self.line_start(None);
                self.write("end");
                self.end_line();
            }
            StmtKind::Do(body) => {
                self.write("do");
                self.end_line();
                self.block(body);
                self.line_start(None);
                self.write("end");
                self.end_line();
            }
            StmtKind::Goto(label) => {
                self.write(&format!("goto {label}"));
                self.end_line();
            }
            StmtKind::Label(label) => {
                self.write(&format!("::{label}::"));
                self.end_line();
            }
            StmtKind::Return(value) => {
                self.write("do return");
                if let Some(v) = value {
                    self.write(" ");
                    // A runtime helper isn't tail-called: one that unravels
                    // would report at the caller, its frame gone.
                    // (`O[n]` is an operation made for a type: the same.)
                    let helper = matches!(v, Expr::Call(callee, _)
                        if matches!(&**callee, Expr::Field(base, _) if matches!(&**base, Expr::Name(n) if n == "rt"))
                            || matches!(&**callee, Expr::Index(base, _) if matches!(&**base, Expr::Name(n) if n == "O")));
                    if helper {
                        self.write("(");
                    }
                    self.expr(v, 0);
                    if helper {
                        self.write(")");
                    }
                }
                self.write(" end");
                self.end_line();
            }
            StmtKind::ReturnMany(values) => {
                self.write("do return ");
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.expr(v, 0);
                }
                self.write(" end");
                self.end_line();
            }
            StmtKind::Break => {
                self.write("break");
                self.end_line();
            }
        }
    }

    fn write(&mut self, s: &str) {
        self.out.text.push_str(s);
    }

    fn exprs(&mut self, es: &[Expr]) {
        for (i, e) in es.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.expr(e, 0);
        }
    }

    /// Prints `e`, parenthesized if its operator binds looser than `min`.
    fn expr(&mut self, e: &Expr, min: u8) {
        match e {
            Expr::Nil => self.write("nil"),
            Expr::Bool(b) => self.write(if *b { "true" } else { "false" }),
            Expr::Int(i) if *i == i64::MIN => self.write("math.mininteger"),
            Expr::Int(i) if *i < 0 => {
                // A negative literal is a unary minus in Lua.
                let text = format!("-{}", i.unsigned_abs());
                if min > UNARY_PREC { self.write(&format!("({text})")) } else { self.write(&text) }
            }
            Expr::Int(i) => self.write(&i.to_string()),
            Expr::Float(f) => {
                let text = float_literal(*f);
                if text.starts_with('-') && min > UNARY_PREC {
                    self.write(&format!("({text})"))
                } else {
                    self.write(&text)
                }
            }
            Expr::Str(bytes) => {
                let lit = lua_string(bytes);
                self.write(&lit);
            }
            Expr::Name(n) => self.write(n),
            Expr::Index(base, key) => {
                self.prefix_expr(base);
                self.write("[");
                self.expr(key, 0);
                self.write("]");
            }
            Expr::Field(base, name) => {
                self.prefix_expr(base);
                if is_lua_name(name) {
                    self.write(".");
                    self.write(name);
                } else {
                    let key = lua_string(name.as_bytes());
                    self.write(&format!("[{key}]"));
                }
            }
            Expr::Call(callee, args) => {
                self.prefix_expr(callee);
                self.write("(");
                self.exprs(args);
                self.write(")");
            }
            Expr::Binary(op, a, b) => {
                let (prec, right_assoc) = binary_prec(op);
                let paren = prec < min;
                if paren {
                    self.write("(");
                }
                // Left-associative operators need a tighter right operand, and
                // right-associative ones (`..`, `^`) a tighter left one.
                let (lmin, rmin) = if right_assoc { (prec + 1, prec) } else { (prec, prec + 1) };
                self.expr(a, lmin);
                self.write(&format!(" {op} "));
                self.expr(b, rmin);
                if paren {
                    self.write(")");
                }
            }
            Expr::Unary(op, a) => {
                let paren = UNARY_PREC < min;
                if paren {
                    self.write("(");
                }
                self.write(op);
                if op.chars().all(char::is_alphabetic) {
                    self.write(" ");
                }
                self.expr(a, UNARY_PREC);
                if paren {
                    self.write(")");
                }
            }
            Expr::Table(items) => {
                self.write("{");
                self.exprs(items);
                self.write("}");
            }
            Expr::Record(entries) => {
                self.write("{");
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    match key {
                        TableKey::Name(n) if is_lua_name(n) => self.write(&format!("{n} = ")),
                        TableKey::Name(n) => self.write(&format!("[{}] = ", lua_string(n.as_bytes()))),
                        TableKey::Index(i) => self.write(&format!("[{i}] = ")),
                    }
                    self.expr(value, 0);
                }
                self.write("}");
            }
            Expr::Function(params, body) => {
                self.write(&format!("function({})", params.join(", ")));
                self.end_line();
                self.block(body);
                self.line_start(None);
                self.write("end");
            }
        }
    }

    /// A callee or indexed value: names, calls and indexing print as they
    /// are; anything else needs parentheses.
    fn prefix_expr(&mut self, e: &Expr) {
        match e {
            Expr::Name(_) | Expr::Call(..) | Expr::Index(..) | Expr::Field(..) => self.expr(e, 0),
            _ => {
                self.write("(");
                self.expr(e, 0);
                self.write(")");
            }
        }
    }
}

const UNARY_PREC: u8 = 11;

/// Lua 5.4's binary operator precedence (higher binds tighter), and whether
/// the operator is right-associative.
fn binary_prec(op: &str) -> (u8, bool) {
    match op {
        "or" => (1, false),
        "and" => (2, false),
        "<" | ">" | "<=" | ">=" | "~=" | "==" => (3, false),
        "|" => (4, false),
        "~" => (5, false),
        "&" => (6, false),
        "<<" | ">>" => (7, false),
        ".." => (8, true),
        "+" | "-" => (9, false),
        "*" | "/" | "//" | "%" => (10, false),
        "^" => (12, true),
        _ => (0, false),
    }
}

/// A float as a Lua literal that reads back as the same float, and as a
/// float (never an integer): `3.0`, `1e+300`, `0x1p-1074`-free forms.
fn float_literal(f: f64) -> String {
    if f.is_nan() {
        return "(0.0/0.0)".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "math.huge".into() } else { "(-math.huge)".into() };
    }
    let text = format!("{f:?}");
    if text.contains(['.', 'e', 'E']) { text } else { format!("{text}.0") }
}

/// Whether `name` can follow a `.` in Lua: an ASCII identifier that isn't a
/// keyword (`global` counts, for Lua 5.5).
pub fn is_lua_name(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "global", "goto", "if", "in",
        "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&name)
}

/// A Lua string literal. Bytes outside printable ASCII are escaped, so the
/// bundle stays plain text.
pub fn lua_string(bytes: &[u8]) -> String {
    let mut s = String::from("\"");
    for &b in bytes {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            b'\r' => s.push_str("\\r"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\{b:03}")),
        }
    }
    s.push('"');
    s
}
