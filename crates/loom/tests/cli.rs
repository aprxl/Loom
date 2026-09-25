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
fn run_passes_arguments_to_the_program() {
    let source = "use std.env\nuse std.io.print\n\nfn main() {\n    let args = env.args()\n    print(args.len())\n    for a in args.skip(1) {\n        print(a)\n    }\n}\n";
    let path = temp_source("args", source);
    let path = path.to_str().expect("utf-8 path");
    let out = loom(&["run", path, "--", "one", "two"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "3\none\ntwo\n");
}

#[test]
fn chip8_tests_pass() {
    let chip8 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/chip8.loom");
    let out = loom(&["test", chip8.to_str().expect("utf-8 path")]);
    assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
    assert!(text(&out.stdout).ends_with("test result: ok. 3 passed; 0 failed\n"), "{}", text(&out.stdout));
}

#[test]
fn test_runs_the_tests_and_fails_when_one_does() {
    let source = "test good() = assert(true)\ntest bad() = assert(false, \"nope\")\n";
    let path = temp_source("tests", source);
    let out = loom(&["test", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let report = text(&out.stdout);
    assert!(report.contains("test good ... ok\ntest bad ... FAILED\n"), "{report}");
    assert!(report.contains("unravel: nope"), "{report}");
    assert!(report.ends_with("test result: FAILED. 1 passed; 1 failed\n"), "{report}");
}

#[test]
fn run_prints_and_exits_0() {
    let path = temp_source(
        "run-ok",
        "use std.io.print

fn main() {
    print(\"hello, loom\")
}
",
    );
    let out = loom(&["run", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "hello, loom
"
    );
}

#[test]
fn unravels_exit_101_with_a_loom_location() {
    let path = temp_source(
        "run-unravel",
        "use std.io.print

fn main() {
    let x: u8 = 250
    print(x + 10)
}
",
    );
    let out = loom(&["run", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(101));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("unravel: arithmetic overflow"), "{stderr}");
    assert!(stderr.contains("run-unravel.loom:5:5"), "{stderr}");
    assert!(stderr.contains("print(x + 10)"), "{stderr}");
}

#[test]
fn build_writes_a_bundle_and_a_source_map() {
    let path = temp_source(
        "build",
        "use std.io.print

fn main() {
    print(\"built\")
}
",
    );
    let out_path = std::env::temp_dir().join(format!("loom-cli-{}-built.lua", std::process::id()));
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "-o", out_path.to_str().expect("utf-8 path")]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let lua = std::fs::read_to_string(&out_path).expect("bundle written");
    assert!(lua.starts_with("-- Generated by loom"));
    let map = std::fs::read_to_string(format!("{}.map", out_path.display())).expect("map written");
    assert!(map.starts_with("loom-map 1"));
    // The bundle runs on its own in the host.
    let host = loom(&["lua-host", out_path.to_str().expect("utf-8 path")]);
    assert_eq!(
        text(&host.stdout),
        "built
"
    );
}

#[test]
fn a_program_needs_main() {
    let path = temp_source(
        "no-main",
        "fn helper() {}
",
    );
    let out = loom(&["run", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("error[N0116]"));
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
