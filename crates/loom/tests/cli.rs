//! End-to-end tests of the `loom` binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn loom(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--color", "never"])
        .args(args)
        .output()
        .expect("the loom binary runs")
}

fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("examples directory")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "loom"))
        .collect();
    paths.sort();
    paths
}

/// Writes `source` to a uniquely named temporary `.loom` file.
fn temp_source(name: &str, source: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("loom-cli-{}-{name}.loom", std::process::id()));
    std::fs::write(&path, source).expect("temp file is writable");
    path
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn check_rejects_examples_only_for_unimplemented_features() {
    // The type checker doesn't cover everything the examples use yet. Every
    // error it reports for them must say so (U0102), never anything else.
    let examples = examples();
    assert_eq!(examples.len(), 8);
    for path in examples {
        let out = loom(&["check", path.to_str().expect("utf-8 path")]);
        let stderr = text(&out.stderr);
        let wrong: Vec<&str> =
            stderr.lines().filter(|l| l.starts_with("error[") && !l.starts_with("error[U0102]")).collect();
        assert!(
            wrong.is_empty(),
            "{}:
{stderr}",
            path.display()
        );
    }
}

#[test]
fn emit_tokens_dumps_the_token_stream() {
    let path = temp_source("dump", "fn main() {\n    print(\"hi {name}\")\n}\n");
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "tokens"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let dump = text(&out.stdout);
    for expected in ["keyword fn", "ident main", "template-start", "text \"hi \"", "interp-open", "ident name", "eof"] {
        assert!(dump.contains(expected), "missing `{expected}` in:\n{dump}");
    }
    // `print` starts a new line indented by four columns.
    assert!(dump.lines().any(|l| l.contains("nl+4") && l.contains("ident print")), "{dump}");
}

#[test]
fn emit_ast_prints_s_expressions() {
    let path = temp_source(
        "ast",
        "fn main() {
    let x = 1 + 2 * 3
}
",
    );
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "ast"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim_end(), "(fn main() (block (let x (+ 1 (* 2 3)))))");
}

#[test]
fn parse_errors_exit_with_code_2() {
    let path = temp_source(
        "parse-error",
        "fn main() {
    let x = )
}
",
    );
    let out = loom(&["check", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("error[S0200]: expected an expression, found `)`"), "{stderr}");
    assert!(stderr.contains("failed to parse (1 error)"), "{stderr}");
}

#[test]
fn lexer_errors_exit_with_code_2() {
    let path = temp_source("broken", "let s = \"never closed\nlet t = 0b102\n");
    let out = loom(&["check", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("error[S0102]: unterminated string literal"), "{stderr}");
    assert!(stderr.contains("error[S0105]: invalid digit `2` in a binary literal"), "{stderr}");
    assert!(stderr.contains("--> "), "{stderr}");
    assert!(stderr.contains("failed to parse"), "{stderr}");
}

#[test]
fn missing_files_are_reported() {
    let out = loom(&["check", "definitely/not/here.loom"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("can't read"));
}

#[test]
fn unfinished_commands_say_so() {
    let path = temp_source("hello", "fn main() {}\n");
    let file = path.to_str().expect("utf-8 path");
    for args in [vec!["run", file], vec!["test", file], vec!["build", file]] {
        let out = loom(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("isn't implemented yet"), "{args:?}");
    }
}

#[test]
fn emit_hir_prints_resolved_names() {
    let path =
        temp_source("hir", "use std.io.print\nfn main() {\n    let x = 1\n    let x = x + 1\n    print(\"{x}\")\n}\n");
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "hir"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "(fn main() (block (let x#1 1) (let x#2 (+ x#1 1)) (call std.io.print (template {x#2}))))\n"
    );
}

#[test]
fn resolution_errors_exit_with_code_2_and_suggest_imports() {
    let path = temp_source("unresolved", "fn main() {\n    print(\"hi\")\n}\n");
    let out = loom(&["check", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("error[N0101]: there's no `print` in scope"), "{stderr}");
    assert!(stderr.contains("add `use std.io.print`"), "{stderr}");
}
