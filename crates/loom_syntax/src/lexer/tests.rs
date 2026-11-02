use crate::diag::{Code, codes, render_plain};
use crate::source::SourceDb;
use crate::token::{Keyword, PrimType, Punct, TokenKind, describe};
use crate::{Lexed, lex};

fn lex_str(src: &str) -> (SourceDb, Lexed) {
    let mut db = SourceDb::new();
    let file = db.add("test.loom", src);
    let lexed = lex(&db, file);
    (db, lexed)
}

/// Token descriptions (without the final `eof`), asserting no diagnostics.
fn describe_all(src: &str) -> Vec<String> {
    let (db, lexed) = lex_str(src);
    let rendered: Vec<String> = lexed.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
    assert!(rendered.is_empty(), "unexpected diagnostics for {src:?}:\n{}", rendered.join("\n"));
    let mut out: Vec<String> = lexed.tokens.iter().map(|t| describe(&db, t)).collect();
    assert_eq!(out.pop().as_deref(), Some("eof"));
    out
}

/// The diagnostic codes reported for `src`.
fn codes_for(src: &str) -> Vec<Code> {
    lex_str(src).1.diagnostics.iter().map(|d| d.code).collect()
}

fn kinds(src: &str) -> Vec<TokenKind> {
    let (_, lexed) = lex_str(src);
    lexed.tokens.into_iter().map(|t| t.kind).collect()
}

// ---------------------------------------------------------------- words

#[test]
fn identifier_case_classes() {
    assert_eq!(
        describe_all("foo Foo FOO T X1 _ _x θ snake_case Point2D"),
        [
            "ident foo",
            "type Foo",
            "const FOO",
            "type T",
            "const X1",
            "underscore",
            "ident _x",
            "ident θ",
            "ident snake_case",
            "type Point2D",
        ]
    );
}

#[test]
fn keywords_and_contextual_words() {
    assert_eq!(
        describe_all("fun once take from get defer yield pull unsafe const Self self"),
        [
            "keyword fun",
            "ident once",
            "ident take",
            "ident from",
            "ident get",
            "keyword defer",
            "keyword yield",
            "keyword pull",
            "keyword unsafe",
            "keyword const",
            "keyword Self",
            "keyword self",
        ]
    );
}

#[test]
fn glued_as_forms() {
    assert_eq!(
        describe_all("x as u8 as% u4 as| Percent as? Cents as ?u8"),
        [
            "ident x",
            "keyword as",
            "prim u8",
            "keyword as%",
            "prim u4",
            "keyword as|",
            "type Percent",
            "keyword as?",
            "type Cents",
            "keyword as",
            "punct ?",
            "prim u8",
        ]
    );
}

#[test]
fn primitive_types() {
    assert_eq!(
        describe_all("u8 i128 u12 usize f32 bool never str u0 u129 i01"),
        [
            "prim u8",
            "prim i128",
            "prim u12",
            "prim usize",
            "prim f32",
            "prim bool",
            "prim never",
            "prim str",
            "ident u0",
            "ident u129",
            "ident i01",
        ]
    );
}

#[test]
fn builtin_case_keywords() {
    assert_eq!(
        kinds("some none ok err true false"),
        [
            TokenKind::Keyword(Keyword::Some),
            TokenKind::Keyword(Keyword::None),
            TokenKind::Keyword(Keyword::Ok),
            TokenKind::Keyword(Keyword::Err),
            TokenKind::Keyword(Keyword::True),
            TokenKind::Keyword(Keyword::False),
            TokenKind::Eof,
        ]
    );
}

// ---------------------------------------------------------------- numbers

#[test]
fn integers() {
    assert_eq!(
        describe_all("42 1_000_000 0xFF 0xff 0o755 0b1010 255u8 255_u8 4096usize 0xFFFF_FFFFu32 0b1010i8"),
        [
            "int 42",
            "int 1000000",
            "int 255",
            "int 255",
            "int 493",
            "int 10",
            "int 255 (u8)",
            "int 255 (u8)",
            "int 4096 (usize)",
            "int 4294967295 (u32)",
            "int 10 (i8)",
        ]
    );
}

#[test]
fn floats() {
    assert_eq!(
        describe_all("3.14 1e-9 2.5f32 1f32 1_000.5 6.02E23"),
        ["float 3.14", "float 1e-9", "float 2.5 (f32)", "float 1.0 (f32)", "float 1000.5", "float 6.02e23"]
    );
}

#[test]
fn dots_after_numbers() {
    assert_eq!(describe_all("1..10"), ["int 1", "punct ..", "int 10"]);
    assert_eq!(describe_all("0..=x"), ["int 0", "punct ..=", "ident x"]);
    assert_eq!(describe_all("2.."), ["int 2", "punct .."]);
    assert_eq!(describe_all("64.kib()"), ["int 64", "punct .", "ident kib", "punct (", "punct )"]);
    assert_eq!(describe_all("0.0..=1.0"), ["float 0.0", "punct ..=", "float 1.0"]);
    // Tuple indices are integers, never floats.
    assert_eq!(describe_all("t.0.1"), ["ident t", "punct .", "int 0", "punct .", "int 1"]);
    assert_eq!(
        describe_all("_.size().0"),
        ["underscore", "punct .", "ident size", "punct (", "punct )", "punct .", "int 0"]
    );
}

#[test]
fn number_errors() {
    assert_eq!(codes_for("0b102"), [codes::S0105]);
    assert_eq!(codes_for("0xG1"), [codes::S0105]);
    assert_eq!(codes_for("0x"), [codes::S0105]);
    assert_eq!(codes_for("12abc"), [codes::S0104]);
    assert_eq!(codes_for("1.5u8"), [codes::S0104]);
    assert_eq!(codes_for("340282366920938463463374607431768211456"), [codes::S0103]);
    assert!(codes_for("340282366920938463463374607431768211455").is_empty());
}

// ---------------------------------------------------------------- twill

#[test]
fn twill_tokens() {
    assert_eq!(
        describe_all("0x8xy4 0x1nnn 0bc_ssss_ssss 0b10??_????"),
        [
            "twill 0x8xy4 w16 mask=0xF00F value=0x8004 x:u4 y:u4",
            "twill 0x1nnn w16 mask=0xF000 value=0x1000 n:u12",
            "twill 0bc_ssss_ssss w9 mask=0x0 value=0x0 c:u1 s:u8",
            "twill 0b10??_???? w8 mask=0xC0 value=0x80",
        ]
    );
}

#[test]
fn hex_with_only_digit_letters_is_an_int() {
    // In expressions `0x00e0` is just 224. In patterns the parser re-reads it
    // as twill (capturing `e`) and warns.
    assert_eq!(describe_all("0x00E0 0x00e0"), ["int 224", "int 224"]);
}

#[test]
fn chip8_opcodes_lex_as_twill() {
    for op in ["0x3xkk", "0xDxyn", "0xEx9E", "0xExA1", "0xFx0A", "0x8xyE", "0xAnnn", "0x0nnn"] {
        let k = kinds(op);
        assert!(matches!(k[0], TokenKind::Twill(_)), "{op} lexed as {:?}", k[0]);
    }
}

#[test]
fn twill_errors() {
    assert_eq!(codes_for("0b1G?"), [codes::S0106]);
    assert_eq!(codes_for(&format!("0x{}", "n".repeat(33))), [codes::S0106]);
}

// ---------------------------------------------------------------- strings

#[test]
fn plain_strings_and_escapes() {
    assert_eq!(describe_all(r#""hello""#), [r#"str "hello""#]);
    assert_eq!(describe_all(r#""a\n\t\\\"b\x1b\u{1F9F6}""#), [r#"str "a\n\t\\\"b\u{1b}🧶""#]);
    assert_eq!(describe_all(r#""{{literal}}""#), [r#"str "{literal}""#]);
    assert_eq!(describe_all(r#"r"C:\raw\{x}""#), [r#"str "C:\\raw\\{x}""#]);
    assert_eq!(describe_all(r#"b"ab\x00\xFF""#), [r#"bytes b"ab\x00\xff""#]);
}

#[test]
fn templates() {
    assert_eq!(
        describe_all(r#""x = {x + 1}!""#),
        [
            "template-start",
            r#"text "x = ""#,
            "interp-open",
            "ident x",
            "punct +",
            "int 1",
            "interp-close",
            r#"text "!""#,
            "template-end",
        ]
    );
}

#[test]
fn template_format_specs() {
    assert_eq!(
        describe_all(r#""{tok.span.start:>3}  {m:.2}""#),
        [
            "template-start",
            "interp-open",
            "ident tok",
            "punct .",
            "ident span",
            "punct .",
            "ident start",
            r#"spec ">3""#,
            "interp-close",
            r#"text "  ""#,
            "interp-open",
            "ident m",
            r#"spec ".2""#,
            "interp-close",
            "template-end",
        ]
    );
    // A `:` inside brackets belongs to the expression, not the spec.
    let d = describe_all(r#""{f(x: 1):?}""#);
    assert!(d.contains(&"punct :".to_string()) && d.contains(&r#"spec "?""#.to_string()), "{d:?}");
}

#[test]
fn nested_templates_and_braces() {
    assert_eq!(
        describe_all(r#""a{f("b{c}")}d""#),
        [
            "template-start",
            r#"text "a""#,
            "interp-open",
            "ident f",
            "punct (",
            "template-start",
            r#"text "b""#,
            "interp-open",
            "ident c",
            "interp-close",
            "template-end",
            "punct )",
            "interp-close",
            r#"text "d""#,
            "template-end",
        ]
    );
    // Braces inside the expression (a dot-block) don't close the interpolation.
    let d = describe_all(r#""{p.{ .x = 1 }}""#);
    assert_eq!(d.iter().filter(|s| *s == "interp-close").count(), 1, "{d:?}");
    assert_eq!(d.iter().filter(|s| *s == "punct }").count(), 1, "{d:?}");
}

#[test]
fn escape_then_interpolation() {
    // From examples/compile_time.loom: a literal backslash-u, then an interpolation with a spec.
    assert_eq!(
        describe_all(r#""\\u{c as u32:04x}""#),
        [
            "template-start",
            r#"text "\\u""#,
            "interp-open",
            "ident c",
            "keyword as",
            "prim u32",
            r#"spec "04x""#,
            "interp-close",
            "template-end",
        ]
    );
}

#[test]
fn multi_line_strings_strip_shared_indentation() {
    let src = "let s = \"\"\"\n        hello\n          world\n        \"\"\"\n";
    assert_eq!(describe_all(src), ["keyword let", "ident s", "punct =", r#"str "hello\n  world""#]);

    let inline = "\"\"\"one line\"\"\"";
    assert_eq!(describe_all(inline), [r#"str "one line""#]);

    let with_interp = "\"\"\"\n    a {x}\n    b\n    \"\"\"";
    assert_eq!(
        describe_all(with_interp),
        ["template-start", r#"text "a ""#, "interp-open", "ident x", "interp-close", r#"text "\nb""#, "template-end"]
    );
}

#[test]
fn string_errors() {
    assert_eq!(codes_for(r#""abc"#), [codes::S0102]);
    assert_eq!(codes_for("\"abc\nlet x = 1"), [codes::S0102]);
    assert_eq!(codes_for(r#""a } b""#), [codes::S0109]);
    assert_eq!(codes_for(r#""\q""#), [codes::S0108]);
    assert_eq!(codes_for(r#""\xFF""#), [codes::S0108]);
    assert_eq!(codes_for(r#""\u{110000}""#), [codes::S0108]);
    assert_eq!(codes_for(r#"b"é""#), [codes::S0110]);
    assert_eq!(codes_for("\"{x"), [codes::S0111]);
    // Inside `{…}`, a `"` opens a nested string. When that string runs off
    // the line too, the unterminated interpolation is the one error.
    assert_eq!(codes_for("\"{x\""), [codes::S0111]);
    assert_eq!(codes_for("let s = \"k {\"\nlet t = 1"), [codes::S0111]);
    // A nested string in a `"""` interpolation still reports its own line.
    assert_eq!(codes_for("\"\"\"\n{f(\"a\n)}\n\"\"\""), [codes::S0102]);
    assert_eq!(codes_for("\"{x\nlet y = 2"), [codes::S0111]);
    assert_eq!(codes_for("\"{x:>3\n"), [codes::S0111]);
    assert_eq!(codes_for("\"\"\"never closed"), [codes::S0102]);
}

#[test]
fn unterminated_interpolation_keeps_the_stream_well_formed() {
    let k = kinds("\"{x\nlet y = 2");
    let opens = k.iter().filter(|t| **t == TokenKind::TemplateStart).count();
    let ends = k.iter().filter(|t| **t == TokenKind::TemplateEnd).count();
    assert_eq!((opens, ends), (1, 1));
    // Lexing recovers and continues with the next line.
    assert!(k.contains(&TokenKind::Keyword(Keyword::Let)));
}

// ---------------------------------------------------------------- chars

#[test]
fn chars() {
    assert_eq!(
        describe_all(r"'a' '\n' '\u{1F9F6}' '█' '\\' '\''"),
        ["char 'a'", r"char '\n'", "char '🧶'", "char '█'", r"char '\\'", r"char '\''"]
    );
}

#[test]
fn char_errors() {
    assert_eq!(codes_for("''"), [codes::S0107]);
    assert_eq!(codes_for("'ab'"), [codes::S0107]);
    assert_eq!(codes_for("'a"), [codes::S0107]);
}

// ---------------------------------------------------------------- comments & trivia

#[test]
fn comments() {
    assert_eq!(describe_all("a // line\nb"), ["ident a", "ident b"]);
    assert_eq!(describe_all("a /* x /* nested */ y */ b"), ["ident a", "ident b"]);
    assert_eq!(describe_all("/// Adds.\n////not doc\nfun"), [r#"doc "Adds.""#, "keyword fun"]);
    assert_eq!(codes_for("/* /* */"), [codes::S0101]);
}

#[test]
fn unexpected_characters() {
    assert_eq!(codes_for("a $ b"), [codes::S0100]);
    assert_eq!(codes_for("“quoted”"), [codes::S0100, codes::S0100]);
    let k = kinds("a € b");
    assert_eq!(k[1], TokenKind::Error);
    assert_eq!(k[2], TokenKind::Ident);
}

// ---------------------------------------------------------------- punctuation

#[test]
fn maximal_munch() {
    let all = "..= .. . +%= +% += + -|= -| -? -> - *|= *? *% ~> <~ ~= ~ => == = != ! <<= << <= < >>= >> >= > &= & |= | ? ^ @ # , ; :";
    let got: Vec<String> = describe_all(all).into_iter().map(|d| d.trim_start_matches("punct ").to_string()).collect();
    let want: Vec<&str> = all.split(' ').collect();
    assert_eq!(got, want);
}

#[test]
fn pull_arrow_is_always_one_token() {
    assert_eq!(kinds("a <~b")[1], TokenKind::Punct(Punct::PullArrow));
    assert_eq!(kinds("a < ~b")[1], TokenKind::Punct(Punct::Lt));
}

// ---------------------------------------------------------------- line metadata

#[test]
fn newline_and_indent_metadata() {
    let src = "let names = people\n    .filter(f)\n.map(g)\nlet x = /* a\n b */ 1";
    let (_, lexed) = lex_str(src);
    let meta: Vec<(bool, u32)> = lexed
        .tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Punct(Punct::Dot) || t.kind == TokenKind::Keyword(Keyword::Let))
        .map(|t| (t.nl_before, t.line_indent))
        .collect();
    // `let`, deeper `.filter`, flush `.map`, second `let`.
    assert_eq!(meta, [(false, 0), (true, 4), (true, 0), (true, 0)]);

    // A newline inside a block comment still separates tokens.
    let one = lexed.tokens.iter().find(|t| matches!(t.kind, TokenKind::Int(_))).expect("int");
    assert!(one.nl_before);
    assert_eq!(one.line_indent, 1);
}

#[test]
fn tokens_inside_templates_never_start_lines() {
    let (_, lexed) = lex_str("\"\"\"\n  {a +\n   b}\n  \"\"\"");
    assert!(lexed.tokens.iter().filter(|t| t.kind != TokenKind::Eof).all(|t| !t.nl_before));
}

#[test]
fn crlf_sources_lex_like_lf() {
    assert_eq!(describe_all("let a = 1\r\nlet b = 2\r\n"), describe_all("let a = 1\nlet b = 2\n"));
}

#[test]
fn prim_suffix_types_are_ints() {
    let k = kinds("7u12");
    assert!(
        matches!(k[0], TokenKind::Int(ref i) if i.suffix == Some(PrimType::Int { signed: false, bits: 12 })),
        "{k:?}"
    );
}

// ---------------------------------------------------------------- diagnostics & examples

#[test]
fn diagnostics_render_rustc_style() {
    let (db, lexed) = lex_str("let s = \"never closed\n");
    let text = render_plain(&db, &lexed.diagnostics[0]);
    assert!(text.starts_with("error[S0102]: unterminated string literal"), "{text}");
    assert!(text.contains("--> test.loom:1:9"), "{text}");
    assert!(text.contains("= help: strings can't span lines"), "{text}");
}

#[test]
fn every_example_lexes_cleanly() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("loom") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable example");
        let mut db = SourceDb::new();
        let file = db.add(path.display().to_string(), text);
        let lexed = lex(&db, file);
        let rendered: Vec<String> = lexed.diagnostics.iter().map(|d| render_plain(&db, d)).collect();
        assert!(rendered.is_empty(), "{}:\n{}", path.display(), rendered.join("\n"));
        assert!(!lexed.tokens.iter().any(|t| t.kind == TokenKind::Error), "{}", path.display());
        seen += 1;
    }
    assert_eq!(seen, 8, "expected all eight examples");
}
