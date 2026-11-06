//! The playground says what the CLI says: every sample of the docs
//! (`docs/lang/samples`, checked against `loom` by `python docs/gen.py
//! --check`), played by [`loom_play::play`], ends with the exit code its
//! last line names and prints exactly its `.out`.
//!
//! `docs/play/skip.txt` lists the samples the playground can't run as the
//! CLI does (the sandbox changes what they see), each with why; the docs
//! show those with their checked output and no Run button. One it lists that
//! plays the same is a failure, so the list stays honest.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use loom_play::{Mode, play};
use loom_sys::Output;

fn docs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// `NAME why` lines, `#` comments.
fn skipped() -> Vec<String> {
    let text = std::fs::read_to_string(docs().join("play/skip.txt")).expect("docs/play/skip.txt");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().next().unwrap_or_default().to_string())
        .collect()
}

/// The mode a sample's last line (`//@ MODE ...`) names, as the mode to
/// play it in and the exit code it should end with.
fn directive(source: &str) -> (Mode, u8, bool) {
    let last = source.trim_end().lines().last().unwrap_or_default();
    let word = last.strip_prefix("//@ ").and_then(|r| r.split_whitespace().next()).unwrap_or_default();
    match word {
        "run" => (Mode::Run, 0, true),
        "unravel" => (Mode::Run, 101, true),
        "error" => (Mode::Run, 1, true),
        "check" => (Mode::Check, 0, false),
        "check-fail" => (Mode::Check, 2, true),
        "test" => (Mode::Test, 0, true),
        "test-fail" => (Mode::Test, 1, true),
        _ => panic!("no `//@ MODE` last line: {last:?}"),
    }
}

/// What `samples.py` keeps of a run: standard output, then standard error
/// after a marker when there was both.
fn combine(out: &str, err: &str) -> String {
    if !out.is_empty() && !err.is_empty() { format!("{out}--- stderr ---\n{err}") } else { format!("{out}{err}") }
}

#[test]
fn every_docs_sample_plays_as_the_cli_runs_it() {
    let dir = docs().join("lang/samples");
    let skip = skipped();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("docs/lang/samples")
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".loom").map(String::from))
        .collect();
    names.sort();
    assert!(names.len() > 20, "found only {} samples", names.len());
    let mut problems = Vec::new();
    for name in &names {
        let source = std::fs::read_to_string(dir.join(format!("{name}.loom"))).expect("a sample").replace("\r\n", "\n");
        let (mode, want_code, has_out) = directive(&source);
        // The CLI runs `loom run docs/lang/samples/NAME.loom` from the
        // repository's root, so that's the name in its diagnostics.
        let file = format!("docs/lang/samples/{name}.loom");
        let buffer = Rc::new(RefCell::new(Vec::new()));
        let played = play(&file, &source, mode, Output::Capture(buffer.clone()));
        let out = String::from_utf8_lossy(&buffer.borrow()).into_owned();
        let got = combine(&out, &played.stderr);
        let want = if has_out {
            std::fs::read_to_string(dir.join(format!("{name}.out"))).unwrap_or_default().replace("\r\n", "\n")
        } else {
            got.clone()
        };
        let same = played.code == want_code && got == want;
        match (same, skip.contains(name)) {
            (true, true) => problems.push(format!("{name}: plays as the CLI runs it, so take it out of skip.txt")),
            (false, false) => problems
                .push(format!("{name}: exit {} (want {want_code})\n--- want\n{want}--- got\n{got}", played.code)),
            _ => {}
        }
    }
    for name in &skip {
        if !names.contains(name) {
            problems.push(format!("skip.txt names {name}, which isn't a sample"));
        }
    }
    assert!(problems.is_empty(), "{} problem(s):\n\n{}", problems.len(), problems.join("\n\n"));
}
