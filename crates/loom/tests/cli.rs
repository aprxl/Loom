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
fn check_accepts_every_example() {
    let examples = examples();
    assert_eq!(examples.len(), 8);
    for path in examples {
        let out = loom(&["check", path.to_str().expect("utf-8 path")]);
        assert!(out.status.success(), "{}:\n{}", path.display(), text(&out.stderr));
        assert!(text(&out.stdout).contains("no errors"));
    }
}

#[test]
fn emit_tokens_dumps_the_token_stream() {
    let path = temp_source("dump", "fn main() {\n    print(\"hi {name}\")\n}\n");
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "tokens"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let dump = text(&out.stdout);
    for expected in [
        "keyword fn",
        "ident main",
        "template-start",
        "text \"hi \"",
        "interp-open",
        "ident name",
        "eof",
    ] {
        assert!(dump.contains(expected), "missing `{expected}` in:\n{dump}");
    }
    // `print` starts a new line indented by four columns.
    assert!(
        dump.lines().any(|l| l.contains("nl+4") && l.contains("ident print")),
        "{dump}"
    );
}

#[test]
fn lexer_errors_exit_with_code_2() {
    let path = temp_source("broken", "let s = \"never closed\nlet t = 0b102\n");
    let out = loom(&["check", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("error[S0102]: unterminated string literal"), "{stderr}");
    assert!(
        stderr.contains("error[S0105]: invalid digit `2` in a binary literal"),
        "{stderr}"
    );
    assert!(stderr.contains("--> "), "{stderr}");
    assert!(stderr.contains("failed to lex (2 errors)"), "{stderr}");
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
