//! `loom build` and `--exe` (V9): the image a program compiles to
//! survives a file, and the file, run by itself, does what `loom run` does.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Once;

use loom_compiler::heddle::opt::Level;
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{treadle, typeck};
use loom_syntax::SourceDb;

const LOOM: &str = env!("CARGO_BIN_EXE_loom");

const PROGRAM: &str = "use std.env
use std.io.print
use std.process

fun main() {
    let args = env.args()
    print(args.len())
    for a in args.skip(1) {
        print(a)
    }
    process.exit(3)
}
";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A fresh directory for one test's files.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-exe-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// `loom_runner` is a binary of another package, so cargo doesn't build it
/// for these tests: build it next to `loom`, once.
fn runner_is_built() {
    static BUILT: Once = Once::new();
    BUILT.call_once(|| {
        let profile_dir = Path::new(LOOM).parent().expect("loom's directory");
        let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cargo.args(["build", "-q", "-p", "loom_runner"]);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            cargo.arg("--release");
        }
        let status = cargo.status().expect("cargo runs");
        assert!(status.success(), "building loom_runner");
    });
}

fn loom(args: &[&str]) -> Output {
    Command::new(LOOM).args(["--color", "never"]).args(args).output().expect("loom runs")
}

/// The image of the program at `path`, compiled at `-O2`, or why not.
fn compile(path: &Path) -> Result<loom_isa::Image, String> {
    let mut db = SourceDb::new();
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file = db.add(path.display().to_string(), text);
    let resolved = resolve(&mut db, file, &FsLoader::new(path));
    if resolved.has_errors() {
        return Err(format!("{:?}", resolved.diagnostics));
    }
    let typed = typeck::check_for(&resolved.program, Target::VM);
    if typed.has_errors() {
        return Err(format!("{:?}", typed.diagnostics));
    }
    treadle::compile_at(&db, &resolved.program, &typed, Level::O2).map(|c| c.image).map_err(|d| format!("{d:?}"))
}

fn loom_files(dir: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root().join(dir))
        .expect("a directory of programs")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "loom"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_program_image_survives_a_file() {
    let mut round_tripped = 0;
    for path in loom_files("examples").into_iter().chain(loom_files("tests/run")) {
        // A program the compiler refuses has no image to check.
        let Ok(image) = compile(&path) else { continue };
        let decoded = loom_isa::decode(&loom_isa::encode(&image));
        assert!(decoded.as_ref() == Ok(&image), "{}: {:?}", path.display(), decoded.err());
        round_tripped += 1;
    }
    assert!(round_tripped >= 50, "only {round_tripped} programs compiled");
}

#[test]
fn the_examples_that_run_compile() {
    for name in ["calc", "chip8", "ledger", "lexer", "parallel", "tour", "widgets"] {
        let path = root().join("examples").join(format!("{name}.loom"));
        if let Err(e) = compile(&path) {
            panic!("{name}: {e}");
        }
    }
}

/// What `loom run` makes of `PROGRAM`, to compare a build's runs with.
fn reference(dir: &Path, args: &[&str]) -> Output {
    let source = dir.join("prog.loom");
    std::fs::write(&source, PROGRAM).expect("a source file");
    let mut run = vec!["run", source.to_str().expect("utf-8 path"), "--"];
    run.extend_from_slice(args);
    loom(&run)
}

fn same_run(got: &Output, want: &Output) {
    assert_eq!(text(&got.stdout), text(&want.stdout));
    assert_eq!(text(&got.stderr), text(&want.stderr));
    assert_eq!(got.status.code(), want.status.code());
}

#[test]
fn an_exe_does_what_loom_run_does() {
    runner_is_built();
    let dir = scratch("exe");
    let args = ["one", "two words", "3"];
    let want = reference(&dir, &args);
    assert_eq!(want.status.code(), Some(3), "{}", text(&want.stderr));
    assert_eq!(text(&want.stdout), "4\none\ntwo words\n3\n");

    let exe = dir.join("prog");
    let built =
        loom(&["build", "--exe", dir.join("prog.loom").to_str().expect("utf-8"), "-o", exe.to_str().expect("utf-8")]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    let exe = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let got = Command::new(&exe).args(args).output().expect("the exe runs");
    same_run(&got, &want);
}

#[test]
fn an_image_runs_with_loom_run() {
    let dir = scratch("image");
    let args = ["one", "two words", "3"];
    let want = reference(&dir, &args);

    let image = dir.join("prog.lmi");
    let built = loom(&["build", dir.join("prog.loom").to_str().expect("utf-8"), "-o", image.to_str().expect("utf-8")]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    let mut run = vec!["run", image.to_str().expect("utf-8"), "--"];
    run.extend_from_slice(&args);
    let got = loom(&run);
    same_run(&got, &want);
}

#[test]
fn a_damaged_image_is_refused() {
    let dir = scratch("bad-image");
    let image = dir.join("prog.lmi");
    std::fs::write(dir.join("prog.loom"), PROGRAM).expect("a source file");
    let built = loom(&["build", dir.join("prog.loom").to_str().expect("utf-8"), "-o", image.to_str().expect("utf-8")]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    let mut bytes = std::fs::read(&image).expect("the image");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x40;
    std::fs::write(&image, bytes).expect("a damaged image");
    let out = loom(&["run", image.to_str().expect("utf-8")]);
    assert_eq!(out.status.code(), Some(1));
    let message = text(&out.stderr);
    assert!(message.starts_with("loom: ") && message.contains("prog.lmi: "), "{message}");
}

#[test]
fn a_tampered_exe_exits_70() {
    runner_is_built();
    let dir = scratch("tampered");
    std::fs::write(dir.join("prog.loom"), PROGRAM).expect("a source file");
    let exe = dir.join("prog");
    let built =
        loom(&["build", "--exe", dir.join("prog.loom").to_str().expect("utf-8"), "-o", exe.to_str().expect("utf-8")]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    let exe = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let mut bytes = std::fs::read(&exe).expect("the exe");
    // The trailer holds the image's length; flip a byte in the middle of the image.
    let len_at = bytes.len() - 16;
    let image_len = u64::from_le_bytes(bytes[len_at..len_at + 8].try_into().expect("8 bytes")) as usize;
    bytes[len_at - image_len / 2] ^= 0x01;
    std::fs::write(&exe, bytes).expect("a tampered exe");
    let out = Command::new(&exe).output().expect("the exe starts");
    assert_eq!(out.status.code(), Some(70), "{}", text(&out.stderr));
    assert!(text(&out.stderr).starts_with("loom: corrupt executable: "), "{}", text(&out.stderr));
}

#[test]
fn build_flags_conflict() {
    let out = loom(&["build", "--exe", "--backend", "lua", "x.loom"]);
    assert_eq!(out.status.code(), Some(2));
    let out = loom(&["build", "--exe", "--emit", "bytecode", "x.loom"]);
    assert_eq!(out.status.code(), Some(2));
}

// ------------------------------------------------------------ perf gates
//
// Wall-clock, so `#[ignore]`d; run on an idle machine:
// `cargo test --release -p loom --test build_exe -- --ignored --nocapture --test-threads=1`

fn best_of<T>(n: usize, mut f: impl FnMut() -> T) -> std::time::Duration {
    (0..n)
        .map(|_| {
            let start = std::time::Instant::now();
            std::hint::black_box(f());
            start.elapsed()
        })
        .min()
        .expect("at least one run")
}

/// Decoding the largest example's image takes at most 1 µs per KB of image
/// and 200 µs in all.
#[test]
#[ignore = "wall-clock gate: run in release on an idle machine"]
fn decoding_an_image_is_fast() {
    let (name, bytes) = loom_files("examples")
        .into_iter()
        .filter_map(|p| compile(&p).ok().map(|image| (p, loom_isa::encode(&image))))
        .max_by_key(|(_, bytes)| bytes.len())
        .expect("an example compiles");
    let time = best_of(5, || loom_isa::decode(&bytes).expect("decodes"));
    let kb = bytes.len() as f64 / 1024.0;
    let per_kb = time.as_secs_f64() * 1e6 / kb;
    println!("{}: {kb:.1} KB decoded in {time:?} ({per_kb:.3} µs/KB)", name.display());
    assert!(per_kb <= 1.0, "{per_kb:.3} µs per KB, gate 1.0");
    assert!(time.as_micros() <= 200, "{time:?}, gate 200 µs");
}

/// What a hello-world executable costs above starting a process at all is
/// at most half of what `loom run` on its source costs above that. The
/// floor is the runner with no image, which does nothing but fail to find
/// one. Best of 15, the three interleaved.
#[test]
#[ignore = "wall-clock gate: run in release on an idle machine"]
fn exe_startup_above_process_floor_is_half_of_loom_run() {
    runner_is_built();
    let dir = scratch("startup");
    let source = dir.join("hello.loom");
    std::fs::write(
        &source,
        "use std.io.print

fun main() {
    print(\"hello\")
}
",
    )
    .expect("a source file");
    let exe = dir.join("hello");
    let built = loom(&["build", "--exe", source.to_str().expect("utf-8"), "-o", exe.to_str().expect("utf-8")]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    let exe = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let bare = Path::new(LOOM).with_file_name(format!("loom_runner{}", std::env::consts::EXE_SUFFIX));
    let time = |command: &mut Command| {
        let start = std::time::Instant::now();
        std::hint::black_box(command.output().expect("runs"));
        start.elapsed().as_secs_f64()
    };
    let (mut floor, mut run, mut standalone) = (f64::MAX, f64::MAX, f64::MAX);
    for _ in 0..15 {
        floor = floor.min(time(&mut Command::new(&bare)));
        run = run.min(time(Command::new(LOOM).args(["run", source.to_str().expect("utf-8")])));
        standalone = standalone.min(time(&mut Command::new(&exe)));
    }
    let ms = |t: f64| t * 1e3;
    let (own_exe, own_run) = (standalone - floor, run - floor);
    println!(
        "floor {:.2} ms, exe {:.2} ms, loom run {:.2} ms: above the floor {:.2} ms against {:.2} ms ({:.2}x)",
        ms(floor),
        ms(standalone),
        ms(run),
        ms(own_exe),
        ms(own_run),
        own_exe / own_run
    );
    assert!(own_exe <= 0.5 * own_run, "{:.2} ms above the floor, loom run {:.2} ms above it", ms(own_exe), ms(own_run));
}
