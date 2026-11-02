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
    let path = temp_source("dump", "fun main() {\n    print(\"hi {name}\")\n}\n");
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "tokens"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let dump = text(&out.stdout);
    for expected in ["keyword fun", "ident main", "template-start", "text \"hi \"", "interp-open", "ident name", "eof"]
    {
        assert!(dump.contains(expected), "missing `{expected}` in:\n{dump}");
    }
    // `print` starts a new line indented by four columns.
    assert!(dump.lines().any(|l| l.contains("nl+4") && l.contains("ident print")), "{dump}");
}

#[test]
fn emit_ast_prints_s_expressions() {
    let path = temp_source(
        "ast",
        "fun main() {
    let x = 1 + 2 * 3
}
",
    );
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "ast"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim_end(), "(fun main() (block (let x (+ 1 (* 2 3)))))");
}

#[test]
fn parse_errors_exit_with_code_2() {
    let path = temp_source(
        "parse-error",
        "fun main() {
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
fn docs_unpacks_the_site() {
    let out = loom(&["docs", "--no-open"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let page = PathBuf::from(text(&out.stdout).trim_end());
    let html = std::fs::read_to_string(&page).expect("the landing page is unpacked");
    assert!(html.contains("<html"), "{html}");
    // Unpacking again is a no-op, and says the same.
    let again = loom(&["docs", "--no-open"]);
    assert_eq!(text(&again.stdout), text(&out.stdout));
}

#[test]
fn run_passes_arguments_to_the_program() {
    let source = "use std.env\nuse std.io.print\n\nfun main() {\n    let args = env.args()\n    print(args.len())\n    for a in args.skip(1) {\n        print(a)\n    }\n}\n";
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

fun main() {
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

fun main() {
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

fun main() {
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

/// A host callback never raises, whatever it is given: a bad argument comes
/// back as the `fault` value (an error returned from a callback is raised with
/// a `longjmp` over mlua's Rust frames, which a release build aborts on).
#[test]
fn a_host_callback_with_a_bad_argument_returns_a_fault() {
    let path = temp_source(
        "host_fault",
        "local host = __LOOM_HOST
local tag, memory, message = host.make_dir({})
assert(tag == host.fault and memory == false and type(message) == 'string', 'no fault')
assert(host.cwd() ~= host.fault, 'a good call still answers')
assert(host.write({}) == host.fault, 'write answers a fault for what is not text')
return function() end
",
    );
    let out = loom(&["lua-host", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_program_needs_main() {
    let path = temp_source(
        "no-main",
        "fun helper() {}
",
    );
    let out = loom(&["run", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("error[N0116]"));
}

#[test]
fn emit_hir_prints_resolved_names() {
    let path =
        temp_source("hir", "use std.io.print\nfun main() {\n    let x = 1\n    let x = x + 1\n    print(\"{x}\")\n}\n");
    let out = loom(&["build", path.to_str().expect("utf-8 path"), "--emit", "hir"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "(fun main() (block (let x#1 1) (let x#2 (+ x#1 1)) (call std.io.print (template {x#2}))))\n"
    );
}

#[test]
fn resolution_errors_exit_with_code_2_and_suggest_imports() {
    let path = temp_source("unresolved", "fun main() {\n    print(\"hi\")\n}\n");
    let out = loom(&["check", path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("error[N0101]: there's no `print` in scope"), "{stderr}");
    assert!(stderr.contains("add `use std.io.print`"), "{stderr}");
}

// ------------------------------------------------------------ Treadle

#[test]
fn treadle_unravels_like_lua() {
    // The same report, word for word, and the same exit code.
    let path = temp_source(
        "vm_unravel",
        "use std.io.print\n\nfun bump(x: u8) -> u8 {\n    let y = x + 10\n    y\n}\n\nfun main() {\n    print(bump(200))\n    print(bump(250))\n}\n",
    );
    let file = path.to_str().expect("utf-8 path");
    let lua = loom(&["run", "--backend", "lua", file]);
    let vm = loom(&["run", "--backend", "vm", file]);
    assert_eq!(vm.status.code(), Some(101));
    assert_eq!(lua.status.code(), Some(101));
    assert_eq!(text(&vm.stdout), "210\n");
    assert_eq!(text(&vm.stdout), text(&lua.stdout));
    assert_eq!(text(&vm.stderr), text(&lua.stderr));
    assert!(text(&vm.stderr).contains(":4:5"), "{}", text(&vm.stderr));
}

#[test]
fn treadle_recurses_a_million_deep_and_overflows_cleanly() {
    let deep = temp_source(
        "vm_deep",
        "use std.io.print\n\nfun down(n: u64) -> u64 = if n == 0 { 0 } else { down(n - 1) + 1 }\n\nfun main() {\n    print(down(1000000))\n}\n",
    );
    let out = loom(&["run", "--backend", "vm", deep.to_str().expect("utf-8 path")]);
    assert_eq!((out.status.code(), text(&out.stdout)), (Some(0), "1000000\n".into()), "{}", text(&out.stderr));
    let endless = temp_source(
        "vm_endless",
        "use std.io.print\n\nfun forever(n: u64) -> u64 = forever(n + 1) + 1\n\nfun main() {\n    print(forever(0))\n}\n",
    );
    let out = loom(&["run", "--backend", "vm", endless.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(101));
    assert!(
        text(&out.stderr).starts_with("unravel: stack overflow: too many calls inside each other"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn emit_heddle_and_bytecode_show_treadle_s_code() {
    let path = temp_source("vm_emit", "fun main() {\n    var x: i32 = 1\n    x += 2\n}\n");
    let file = path.to_str().expect("utf-8 path");
    let heddle = loom(&["build", file, "--emit", "heddle"]);
    assert!(heddle.status.success(), "{}", text(&heddle.stderr));
    assert!(text(&heddle.stdout).contains("fn main() -> ()"), "{}", text(&heddle.stdout));
    let bytecode = loom(&["build", file, "--emit", "bytecode"]);
    assert!(bytecode.status.success(), "{}", text(&bytecode.stderr));
    assert!(text(&bytecode.stdout).contains("(entry)"), "{}", text(&bytecode.stdout));
}

#[test]
fn backtraces_list_every_frame_on_treadle() {
    // A lambda and a builtin method in between: frames with no locations
    // (std's methods, glue) are left out, the program's are all there. At
    // `-O2`, `inner` and `middle` are inlined, and still frames of their
    // own.
    let path = temp_source(
        "backtrace",
        "use std.io.print\n\
         fun inner(xs: [i32], i: usize) -> i32 = xs[i]\n\
         fun middle(xs: List[i32]) -> i32 = inner(xs, 5) + 1\n\
         fun main() {\n\
         \x20   let xs: List[i32] = [1, 2, 3]\n\
         \x20   xs.each(x => print(middle(xs)))\n\
         }\n",
    );
    let file = path.to_str().expect("utf-8 path");
    let listing = text(&loom(&["-O2", "build", file, "--emit", "bytecode"]).stdout);
    assert!(!listing.contains("call        inner"), "inlined at -O2:\n{listing}");
    let mut reports = Vec::new();
    for level in ["-O0", "-O2"] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", level, "run", "--backend", "vm"])
            .arg(&path)
            .env("LOOM_BACKTRACE", "1")
            .output()
            .expect("the loom binary runs");
        let err = text(&out.stderr);
        assert_eq!(out.status.code(), Some(101), "{err}");
        let frames: Vec<&str> = err.lines().skip_while(|l| *l != "backtrace:").skip(1).collect();
        let names: Vec<&str> = frames.iter().filter_map(|l| l.split_whitespace().nth(1)).collect();
        assert_eq!(names, ["inner", "middle", "main.{lambda}", "main"], "{level}: {err}");
        assert!(frames[0].ends_with(":2:5"), "{level}: {err}");
        assert!(frames[1].ends_with(":3:5"), "{level}: {err}");
        reports.push(err);
    }
    let _ = std::fs::remove_file(&path);
    assert_eq!(reports[0], reports[1], "the same report at -O0 and -O2");
}

#[test]
fn process_exit_unwinds_then_exits_with_its_code_on_both_backends() {
    let path = temp_source(
        "exit",
        "use std.io.print
use std.process

fun work(n: u32) -> u32 {
    defer print(\"leaving work\")
    if n > 2 { process.exit(3) }
    n + 1
}

fun main() {
    defer print(\"main's defer\")
    let names: List[String] = [\"a\", \"b\"]
    print(work(1))
    print(work(5))
    print(\"not reached {names}\")
}
",
    );
    let file = path.to_str().expect("utf-8 path");
    for backend in ["lua", "vm"] {
        let out = loom(&["run", "--backend", backend, file]);
        assert_eq!(out.status.code(), Some(3), "{backend}: {}", text(&out.stderr));
        assert_eq!(
            text(&out.stdout),
            "leaving work
2
leaving work
main's defer
",
            "{backend}"
        );
        assert_eq!(text(&out.stderr), "", "{backend}");
    }
}

#[test]
fn ctrl_c_unwinds_then_exits_130_on_both_backends() {
    // Through the flag the handler sets: LOOM_INTERRUPT_AFTER_MS sets it.
    // The second loop's back edge repeats its test and branches back on
    // Treadle (rotated, at -O2): that branch notices the interrupt too.
    for (name, spin) in [("interrupt", "while true { n +%= 1 }"), ("interrupt_test", "while n != 7 { n +%= 2 }")] {
        let path = temp_source(
            name,
            &format!(
                "use std.io.print

fun main() {{
    defer print(\"cleaned up\")
    var n = 0u64
    print(\"spinning\")
    {spin}
}}
"
            ),
        );
        let file = path.to_str().expect("utf-8 path");
        for (backend, level) in [("lua", "-O2"), ("vm", "-O2"), ("vm", "-O0")] {
            let out = Command::new(env!("CARGO_BIN_EXE_loom"))
                .args(["--color", "never", level, "run", "--backend", backend, file])
                .env("LOOM_INTERRUPT_AFTER_MS", "200")
                .output()
                .expect("the loom binary runs");
            assert_eq!(out.status.code(), Some(130), "{name} {backend} {level}: {}", text(&out.stderr));
            assert_eq!(
                text(&out.stdout),
                "spinning
cleaned up
",
                "{name} {backend} {level}"
            );
            assert_eq!(
                text(&out.stderr),
                "interrupted (Ctrl-C)
",
                "{name} {backend} {level}"
            );
        }
    }
}

#[test]
fn an_interrupt_before_the_run_starts_is_not_lost() {
    // At 0 ms the flag may be set before Lua's watch is registered.
    let path = temp_source(
        "interrupt_early",
        "use std.io.print

fun main() {
    defer print(\"cleaned up\")
    var n = 0u64
    while true { n +%= 1 }
}
",
    );
    let file = path.to_str().expect("utf-8 path");
    for backend in ["lua", "vm"] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, file])
            .env("LOOM_INTERRUPT_AFTER_MS", "0")
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(130), "{backend}: {}", text(&out.stderr));
        assert_eq!(
            text(&out.stderr),
            "interrupted (Ctrl-C)
",
            "{backend}"
        );
    }
}

#[test]
fn ctrl_c_at_a_back_edge_after_an_if_runs_the_defers() {
    // The loop's back edge is a branch of the `if`'s condition once the
    // empty block between them is gone: it still unwinds through `defer`s.
    let path = temp_source(
        "interrupt_branch",
        "use std.io.print

fun main() {
    defer print(\"cleaned up\")
    var i = 0u64
    loop {
        i = i +% 1
        if i == 0 { print(i) }
    }
}
",
    );
    let file = path.to_str().expect("utf-8 path");
    for backend in ["lua", "vm"] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, file])
            .env("LOOM_INTERRUPT_AFTER_MS", "200")
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(130), "{backend}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "cleaned up\n", "{backend}");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_runaway_recursion_unravels_quickly_with_a_short_backtrace() {
    let path = temp_source(
        "runaway",
        "use std.io.print\nfun down(n: u64) -> u64 { 1 + down(n + 1) }\nfun main() { print(down(0)) }\n",
    );
    let started = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--color", "never", "run", "--backend", "vm"])
        .arg(&path)
        .env("LOOM_BACKTRACE", "1")
        .output()
        .expect("the loom binary runs");
    let _ = std::fs::remove_file(&path);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(101), "{err}");
    assert!(err.contains("stack overflow"), "{err}");
    assert!(err.contains(" more frames ..."), "{err}");
    assert!(err.lines().count() < 200, "{} lines", err.lines().count());
    // Debug builds are slower than release (about 0.3 s): a generous bound.
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
}

#[test]
fn ctrl_c_reaches_a_shuttle_that_never_yields() {
    // The Lua backend runs shuttles as coroutines, each with a hook of its
    // own. Treadle builds a generator a `for` loop calls in place, and
    // steps one held in a variable: both stop, and its `defer` runs.
    for (name, call) in [("interrupt_shuttle", "for x in rare()"), ("interrupt_held", "let g = rare()\n    for x in g")]
    {
        let path = temp_source(
            name,
            &format!(
                "use std.io.print

fun rare() ~> u64 {{
    defer print(\"generator closed\")
    var i = 0u64
    loop {{
        i = i +% 1
        if i == 0 {{ yield i }}
    }}
}}

fun main() {{
    defer print(\"main cleaned up\")
    {call} {{ print(x) }}
}}
"
            ),
        );
        let file = path.to_str().expect("utf-8 path");
        for backend in ["lua", "vm"] {
            let started = std::time::Instant::now();
            let out = Command::new(env!("CARGO_BIN_EXE_loom"))
                .args(["--color", "never", "run", "--backend", backend, file])
                .env("LOOM_INTERRUPT_AFTER_MS", "300")
                .output()
                .expect("the loom binary runs");
            assert_eq!(out.status.code(), Some(130), "{name} on {backend}: {}", text(&out.stderr));
            assert_eq!(text(&out.stdout), "generator closed\nmain cleaned up\n", "{name} on {backend}");
            assert_eq!(text(&out.stderr), "interrupted (Ctrl-C)\n", "{name} on {backend}");
            assert!(started.elapsed().as_secs() < 30, "{name} on {backend}: {:?}", started.elapsed());
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn written_lines_are_in_the_file_when_it_is_read_back_on_both_backends() {
    // A `File` stays open between writes; reading the path sees them all.
    let out_path = std::env::temp_dir().join(format!("loom_cli_lines_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out_path);
    let shown = out_path.to_str().expect("utf-8 path").replace('\\', "/");
    let path = temp_source(
        "file_lines",
        &format!(
            "use std.io.print
use std.fs.{{File, IoError, read_text}}

fun run() -> () ! IoError {{
    var f = File.append(\"{shown}\")?
    f.write_line(\"one\")?
    f.write_line(\"two\")?
    var g = File.append(\"{shown}\")?
    g.write_line(\"three\")?
    let text = read_text(\"{shown}\")?
    print(text.lines().count())
}}

fun main() {{
    run() mend e => print(\"failed\")
}}
"
        ),
    );
    let file = path.to_str().expect("utf-8 path");
    for backend in ["lua", "vm"] {
        let _ = std::fs::remove_file(&out_path);
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, file])
            .output()
            .expect("the loom binary runs");
        assert_eq!(text(&out.stdout), "3\n", "{backend}: {}", text(&out.stderr));
        let on_disk = std::fs::read_to_string(&out_path).expect("the file is written");
        assert_eq!(on_disk.replace("\r\n", "\n"), "one\ntwo\nthree\n", "{backend}");
    }
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn ctrl_c_ends_a_long_sleep_on_both_backends() {
    let path = temp_source(
        "interrupt_sleep",
        "use std.io.print
use std.time.{secs, sleep}

fun main() {
    defer print(\"cleaned up\")
    print(\"sleeping\")
    sleep(secs(120))
    print(\"slept\")
}
",
    );
    let file = path.to_str().expect("utf-8 path");
    for backend in ["lua", "vm"] {
        let started = std::time::Instant::now();
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, file])
            .env("LOOM_INTERRUPT_AFTER_MS", "300")
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(130), "{backend}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "sleeping\ncleaned up\n", "{backend}");
        assert!(started.elapsed().as_secs() < 30, "{backend}: {:?}", started.elapsed());
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn process_exit_in_a_thread_cancels_the_rest_and_exits_on_both_backends() {
    let path = temp_source(
        "thread_exit",
        "use std.io.print
use std.process
use std.sync.sched_yield

fun parked() {
    defer print(\"parked cleanup\")
    sched_yield()
    print(\"parked finished\")
}

fun quitter() {
    defer print(\"quitter cleanup\")
    process.exit(3)
}

fun main() {
    defer print(\"main's defer\")
    loom {
        thread parked()
        thread quitter()
        ()
    }
    print(\"not reached\")
}
",
    );
    let file = path.to_str().expect("utf-8 path");
    for (backend, level) in [("lua", "-O2"), ("vm", "-O2"), ("vm", "-O0")] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", level, "run", "--backend", backend, file])
            .env("LOOM_THREADS", "1")
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(3), "{backend} {level}: {}", text(&out.stderr));
        assert_eq!(
            text(&out.stdout),
            "quitter cleanup
parked cleanup
main's defer
",
            "{backend} {level}"
        );
        assert_eq!(text(&out.stderr), "", "{backend} {level}");
    }
    // With several workers the cleanups come in any order (and the parked
    // thread may have finished before it was cancelled), but the exit code
    // is the same, and the ones that must run do.
    for workers in ["2", "4"] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", "vm", file])
            .env("LOOM_THREADS", workers)
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(3), "{workers} workers: {}", text(&out.stderr));
        let stdout = text(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(lines.contains(&"quitter cleanup"), "{workers} workers: {lines:?}");
        assert!(lines.contains(&"main's defer"), "{workers} workers: {lines:?}");
        assert!(!lines.contains(&"not reached"), "{workers} workers: {lines:?}");
    }
}

#[test]
fn ctrl_c_in_a_thread_unwinds_every_task_and_exits_130_on_both_backends() {
    let path = temp_source(
        "thread_interrupt",
        "use std.io.print
use std.sync.sched_yield

fun parked() {
    defer print(\"parked cleanup\")
    sched_yield()
    print(\"parked finished\")
}

fun spin() {
    defer print(\"spin cleanup\")
    var n = 0u64
    while true { n +%= 1 }
}

fun main() {
    defer print(\"main's defer\")
    print(\"spinning\")
    loom {
        thread parked()
        thread spin()
        ()
    }
}
",
    );
    let file = path.to_str().expect("utf-8 path");
    for (backend, level) in [("lua", "-O2"), ("vm", "-O2"), ("vm", "-O0")] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", level, "run", "--backend", backend, file])
            .env("LOOM_INTERRUPT_AFTER_MS", "200")
            .env("LOOM_THREADS", "1")
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(130), "{backend} {level}: {}", text(&out.stderr));
        assert_eq!(
            text(&out.stdout),
            "spinning
spin cleanup
parked cleanup
main's defer
",
            "{backend} {level}"
        );
        assert_eq!(text(&out.stderr), "interrupted (Ctrl-C)\n", "{backend} {level}");
    }
    // Several workers: the spinning thread runs beside the others, and the
    // interrupt unwinds every task all the same.
    for workers in ["2", "4"] {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", "vm", file])
            .env("LOOM_INTERRUPT_AFTER_MS", "200")
            .env("LOOM_THREADS", workers)
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(130), "{workers} workers: {}", text(&out.stderr));
        let stdout = text(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        for must in ["spinning", "spin cleanup", "parked cleanup", "main's defer"] {
            assert!(lines.contains(&must), "{workers} workers: {lines:?}");
        }
        assert_eq!(text(&out.stderr), "interrupted (Ctrl-C)\n", "{workers} workers");
    }
}

/// `examples/parallel.loom` reads this repository's docs and runs unsandboxed
/// from the repository root: it ends well on both backends, the picture and
/// the prime count are right, and what's printed is the same from Lua, from
/// Treadle with one worker and with many (the word counts change with every
/// edit of the docs, so they aren't pinned).
#[test]
fn parallel_example_runs_the_same_on_both_backends() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let run = |backend: &str, workers: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, "examples/parallel.loom"])
            .env("LOOM_THREADS", workers)
            .current_dir(&root)
            .output()
            .expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(0), "{backend} x{workers}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    let lua = run("lua", "1");
    // 28 rows of the picture, then the primes and the top five words.
    assert_eq!(lua.lines().count(), 28 + 1 + 5, "{lua}");
    assert!(
        lua.contains(
            "primes below 200k: 17984
"
        ),
        "{lua}"
    );
    assert!(lua.lines().take(28).all(|l| l.chars().count() == 72), "{lua}");
    for workers in ["1", "4"] {
        assert_eq!(run("vm", workers), lua, "Treadle with {workers} workers");
    }
}

#[test]
fn stdin_and_stderr_work_on_both_backends() {
    let source = "use std.io.{print, write, eprint, read_line, lines}\n\nfun main() {\n    write(\"a\")\n    eprint(\"err\")\n    print(read_line())\n    var n = 0\n    for line in lines() {\n        n += line.len()\n    }\n    print(n)\n    print(read_line())\n}\n";
    let path = temp_source("stdin", source);
    let path = path.to_str().expect("utf-8 path");
    for backend in ["lua", "vm"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, path])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the loom binary runs");
        std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), b"hello\t\nab\r\ncd\nlast").expect("input");
        let out = child.wait_with_output().expect("finishes");
        assert!(out.status.success(), "{backend}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "ahello\t\n8\nnone\n", "{backend}");
        assert_eq!(text(&out.stderr), "err\n", "{backend}");
    }
}

/// A sleep is a cancellation point (design/vm.md §11.4): a thread asleep for
/// twenty seconds is woken when its loom is leaving, so the run is quick.
#[test]
fn a_sleeping_thread_is_cancelled() {
    for (backend, threads) in [("vm", None), ("vm", Some("1")), ("lua", None)] {
        let label = format!("{backend} {}", threads.unwrap_or("default workers"));
        let path = temp_source(
            "sleep-cancel",
            "use std.time.{sleep, ms}\nfun main() {\n    loom {\n        let a = thread { sleep(ms(20000)); 1 }\n        let b = thread { sleep(ms(50)); unravel(\"stop\") }\n        a.join()\n    }\n}\n",
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_loom"));
        command.args(["--color", "never", "run", "--backend", backend, path.to_str().expect("utf-8 path")]);
        if let Some(n) = threads {
            command.env("LOOM_THREADS", n);
        }
        let start = std::time::Instant::now();
        let out = command.output().expect("the loom binary runs");
        let _ = std::fs::remove_file(&path);
        assert!(!out.status.success(), "{label}: the thread's unravel ends the run");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{label}: the sleeper was not cancelled: {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn sleepers_park_instead_of_holding_workers() {
    let path = temp_source(
        "sleep-park",
        "use std.time.{sleep, ms}\nfun main() {\n    loom {\n        let a = thread { sleep(ms(300)); 1 }\n        let b = thread { sleep(ms(300)); 1 }\n        let c = thread { sleep(ms(300)); 1 }\n        a.join()\n        b.join()\n        c.join()\n    }\n}\n",
    );
    for backend in ["vm", "lua"] {
        let started = std::time::Instant::now();
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--color", "never", "run", "--backend", backend, path.to_str().expect("utf-8 path")])
            .env("LOOM_THREADS", "2")
            .output()
            .expect("the loom binary runs");
        assert!(out.status.success(), "{backend}: {}", text(&out.stderr));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{backend}: the sleepers held their workers: {:?}",
            started.elapsed()
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn ctrl_c_ends_a_sleep() {
    let path = temp_source(
        "interrupt_sleeper",
        "use std.time.{sleep, ms}\nfun main() {\n    loom {\n        let a = thread { sleep(ms(20000)); 1 }\n        sleep(ms(20000))\n    }\n}\n",
    );
    for (backend, threads) in [("vm", None), ("vm", Some("1")), ("lua", None)] {
        let label = format!("{backend} {}", threads.unwrap_or("default workers"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_loom"));
        command
            .args(["--color", "never", "run", "--backend", backend, path.to_str().expect("utf-8 path")])
            .env("LOOM_INTERRUPT_AFTER_MS", "200");
        if let Some(n) = threads {
            command.env("LOOM_THREADS", n);
        }
        let started = std::time::Instant::now();
        let out = command.output().expect("the loom binary runs");
        assert_eq!(out.status.code(), Some(130), "{label}: {}", text(&out.stderr));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{label}: the sleep did not end on Ctrl-C: {:?}",
            started.elapsed()
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_blocking_read_frees_its_worker() {
    let path = temp_source(
        "read-frees-worker",
        "use std.io.{print, read_line}\nuse std.time.{sleep, ms}\nfun main() {\n    loom {\n        let a = thread { print(read_line()) }\n        let b = thread { sleep(ms(50)); print(\"tick\") }\n        a.join()\n        b.join()\n    }\n}\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--color", "never", "run", "--backend", "vm", path.to_str().expect("utf-8 path")])
        .env("LOOM_THREADS", "2")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the loom binary runs");
    let mut stdin = child.stdin.take().expect("stdin");
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::io::Write::write_all(&mut stdin, b"line\n").expect("input");
    drop(stdin);
    let out = child.wait_with_output().expect("finishes");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "tick\nline\n");
    let _ = std::fs::remove_file(&path);
}

/// A prompt printed before a read reaches the host before the read blocks,
/// though other tasks are printing on other workers (which queue their lines
/// and hand them over in batches): the lines stay whole and in order.
#[test]
fn a_prompt_is_not_left_in_a_worker_queue() {
    let path = temp_source(
        "prompt-queue",
        r#"use std.io.{print, read_line}
use std.sync.Atomic

fun main() {
    let done = Atomic[u64](0)
    loom {
        let a = thread {
            var j = 0u64
            while done.load() == 0 and j < 300000 {
                print("a {j}")
                j += 1
            }
        }
        let b = thread {
            var j = 0u64
            while done.load() == 0 and j < 300000 {
                print("b {j}")
                j += 1
            }
        }
        print("prompt")
        let name = match read_line() {
            some(l) => l
            none => ""
        }
        done.store(1)
        print("hello {name}")
        a.join()
        b.join()
    }
}
"#,
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--color", "never", "run", "--backend", "vm", path.to_str().expect("utf-8 path")])
        .env("LOOM_THREADS", "4")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the loom binary runs");
    let stdout = child.stdout.take().expect("stdout");
    let (lines, seen) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    // The prompt arrives while the read is still waiting for its input.
    let mut out = Vec::new();
    let started = std::time::Instant::now();
    loop {
        match seen.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(line) => {
                let prompt = line == "prompt";
                out.push(line);
                if prompt {
                    break;
                }
            }
            Err(_) => assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the prompt did not arrive before the read ({} lines came, the last {:?})",
                out.len(),
                out.iter().rev().take(3).collect::<Vec<_>>()
            ),
        }
    }
    let mut stdin = child.stdin.take().expect("stdin");
    std::io::Write::write_all(
        &mut stdin, b"ann
",
    )
    .expect("input");
    drop(stdin);
    let status = child.wait().expect("finishes");
    reader.join().expect("the reader ends");
    out.extend(seen.try_iter());
    assert!(status.success(), "{status}");
    assert!(out.iter().any(|l| l == "hello ann"), "no greeting after {} lines", out.len());
    // Every other line is whole, and each task's lines are in order.
    let mut next = [0u64; 2];
    for line in out.iter().filter(|l| *l != "prompt" && *l != "hello ann") {
        let (who, n) = line.split_once(' ').expect("a task's line");
        let k = usize::from(who == "b");
        assert!(who == "a" || who == "b", "a torn line: {line:?}");
        assert_eq!(n.parse::<u64>().expect("a number"), next[k], "{line:?} out of order");
        next[k] += 1;
    }
    let _ = std::fs::remove_file(&path);
}

/// A read is a cancellation point: a thread that unravels while another reads
/// cancels it, and the read's text is never printed. On Lua a read blocks the
/// whole program, so no other thread runs during it: only Treadle is tested here.
#[test]
fn a_read_is_a_cancellation_point() {
    let path = temp_source(
        "read-cancel",
        "use std.io.{print, read_line}\nfun main() {\n    loom {\n        let a = thread { let l = read_line(); print(\"read\") }\n        let b = thread { unravel(\"stop\") }\n    }\n}\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--color", "never", "run", "--backend", "vm", path.to_str().expect("utf-8 path")])
        .env("LOOM_THREADS", "2")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the loom binary runs");
    let mut stdin = child.stdin.take().expect("stdin");
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::io::Write::write_all(&mut stdin, b"x\n").expect("input");
    drop(stdin);
    let out = child.wait_with_output().expect("finishes");
    assert!(!out.status.success(), "the thread's unravel ends the run");
    assert!(!text(&out.stdout).contains("read"), "the read went on after the cancel: {}", text(&out.stdout));
    let _ = std::fs::remove_file(&path);
}
