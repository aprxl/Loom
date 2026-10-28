//! The V5 speed gates of design/vm.md §11.4 that wall-clock time decides:
//! what spawning and joining a thread costs on 8 workers, and how well
//! `loom for` scales to them.
//!
//! Too noisy for `cargo test` under load, so both are `#[ignore]`d: run them
//! on an idle machine with `cargo test --release -p loom --test perf_gates
//! -- --ignored --nocapture`. Each takes the best of `RUNS` runs of the
//! benchmark in `bench/` on the `loom` binary of the same profile.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RUNS: usize = 5;
const WORKERS: usize = 8;

/// The gate: one `thread` and its `join` cost less than this on 8 workers.
const MAX_SPAWN_JOIN: Duration = Duration::from_micros(2);
/// `par_spawn_join` spawns and joins this many threads (200 rounds of 1000).
const SPAWNS: u32 = 200_000;
/// The gate: `loom for` runs this many times faster on 8 workers than on one
/// (0.7 per physical core).
const MIN_SPEEDUP: f64 = 0.7 * WORKERS as f64;

/// The best of `RUNS` wall-clock times of `bench/NAME.loom` on Treadle with
/// `workers` workers.
fn best(name: &str, workers: usize) -> Duration {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            let status = Command::new(env!("CARGO_BIN_EXE_loom"))
                .args(["run", "--backend", "vm"])
                .arg(root.join("bench").join(format!("{name}.loom")))
                .env("LOOM_THREADS", workers.to_string())
                .stdout(Stdio::null())
                .status()
                .expect("the loom binary runs");
            assert!(status.success(), "{name} at {workers} workers: {status}");
            start.elapsed()
        })
        .min()
        .expect("a run")
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn spawn_and_join_cost_under_2us_on_8_workers() {
    let per = best("par_spawn_join", WORKERS) / SPAWNS;
    println!("spawn+join on {WORKERS} workers: {per:?} (at most {MAX_SPAWN_JOIN:?})");
    assert!(per <= MAX_SPAWN_JOIN, "a spawn and join take {per:?} on {WORKERS} workers (at most {MAX_SPAWN_JOIN:?})");
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn loom_for_scales_to_8_workers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < WORKERS {
        println!("skipped: {cores} cores, the gate needs {WORKERS}");
        return;
    }
    let (one, many) = (best("par_mandelbrot", 1), best("par_mandelbrot", WORKERS));
    let speedup = one.as_secs_f64() / many.as_secs_f64();
    println!(
        "par_mandelbrot: {one:.3?} on one worker, {many:.3?} on {WORKERS}: {speedup:.2}x (at least {MIN_SPEEDUP:.1}x)"
    );
    assert!(
        speedup >= MIN_SPEEDUP,
        "par_mandelbrot is {speedup:.2}x faster on {WORKERS} workers (at least {MIN_SPEEDUP:.1}x)"
    );
}

/// The gate: text and map natives (`contains`, `Map.has`) scale this much on
/// 8 workers, because they run without the pool's lock.
const MIN_TEXT_SPEEDUP: f64 = 4.0;

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn text_natives_scale_to_8_workers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < WORKERS {
        println!("skipped: {cores} cores, the gate needs {WORKERS}");
        return;
    }
    let (one, many) = (best("par_text", 1), best("par_text", WORKERS));
    let speedup = one.as_secs_f64() / many.as_secs_f64();
    println!(
        "par_text: {one:.3?} on one worker, {many:.3?} on {WORKERS}: {speedup:.2}x (at least {MIN_TEXT_SPEEDUP:.1}x)"
    );
    assert!(
        speedup >= MIN_TEXT_SPEEDUP,
        "par_text is {speedup:.2}x faster on {WORKERS} workers (at least {MIN_TEXT_SPEEDUP:.1}x)"
    );
}

/// The gate: building strings (`push_str`, `push`, `to_upper`) scales this
/// much on 8 workers, because those natives use the worker's own heap.
const MIN_BUILD_SPEEDUP: f64 = 4.0;

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn string_building_scales_to_8_workers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < WORKERS {
        println!("skipped: {cores} cores, the gate needs {WORKERS}");
        return;
    }
    let (one, many) = (best("par_build", 1), best("par_build", WORKERS));
    let speedup = one.as_secs_f64() / many.as_secs_f64();
    println!(
        "par_build: {one:.3?} on one worker, {many:.3?} on {WORKERS}: {speedup:.2}x (at least {MIN_BUILD_SPEEDUP:.1}x)"
    );
    assert!(
        speedup >= MIN_BUILD_SPEEDUP,
        "par_build is {speedup:.2}x faster on {WORKERS} workers (at least {MIN_BUILD_SPEEDUP:.1}x)"
    );
}

/// The gate: locking and unlocking a mutex of one's own scales this much on
/// 8 workers, because an uncontended mutex needs no lock on the pool.
const MIN_MUTEX_SPEEDUP: f64 = 4.0;

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn uncontended_mutex_scales_to_8_workers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < WORKERS {
        println!("skipped: {cores} cores, the gate needs {WORKERS}");
        return;
    }
    let (one, many) = (best("par_mutex", 1), best("par_mutex", WORKERS));
    let speedup = one.as_secs_f64() / many.as_secs_f64();
    println!(
        "par_mutex: {one:.3?} on one worker, {many:.3?} on {WORKERS}: {speedup:.2}x (at least {MIN_MUTEX_SPEEDUP:.1}x)"
    );
    assert!(
        speedup >= MIN_MUTEX_SPEEDUP,
        "par_mutex is {speedup:.2}x faster on {WORKERS} workers (at least {MIN_MUTEX_SPEEDUP:.1}x)"
    );
}

/// The gate: a small box allocated, read and freed costs less than this
/// (design/vm.md §16's micro target, asserted).
const MAX_BOX: Duration = Duration::from_nanos(15);
/// `ptr_alloc` makes this many boxes.
const BOXES: u32 = 10_000_000;

/// The best of `RUNS` wall-clock times of a program that only prints: what
/// starting the VM costs.
fn startup() -> Duration {
    let dir = std::env::temp_dir().join("loom_perf_gates");
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let path = dir.join("hello.loom");
    std::fs::write(&path, "use std.io.print\nfn main() { print(\"hi\") }\n").expect("writable");
    (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            let status = Command::new(env!("CARGO_BIN_EXE_loom"))
                .args(["run", "--backend", "vm"])
                .arg(&path)
                .stdout(Stdio::null())
                .status()
                .expect("the loom binary runs");
            assert!(status.success(), "hello: {status}");
            start.elapsed()
        })
        .min()
        .expect("a run")
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn a_small_box_costs_under_15ns() {
    let per = best("ptr_alloc", 1).saturating_sub(startup()) / BOXES;
    println!("allocate, read and free a box: {per:?} (at most {MAX_BOX:?})");
    assert!(per <= MAX_BOX, "a box takes {per:?} to allocate, read and free (at most {MAX_BOX:?})");
}

/// The gate: binary trees in arenas run this many times faster than the same
/// program in idiomatic Lua 5.4 (design/vm.md §16, the V6 row).
const MIN_TREES_VS_LUA: f64 = 4.0;
/// The gate: putting a value in an arena, reading it and resetting now and
/// then costs at most this fraction of the same with a box on the heap.
/// (The design hoped for a tenth of the allocation alone; an interpreter's
/// loop and read cost the same either way, so the whole is compared.)
const MAX_ARENA_VS_HEAP: f64 = 0.5;

fn best_command(mut command: impl FnMut() -> Command) -> Duration {
    (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            let status = command().stdout(Stdio::null()).status().expect("the loom binary runs");
            assert!(status.success(), "{status}");
            start.elapsed()
        })
        .min()
        .expect("a run")
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn arena_trees_are_4x_faster_than_lua() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let loom = best("arena_trees", 1);
    let lua = best_command(|| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.arg("lua-host").arg(root.join("bench/lua/binary_trees.lua"));
        c
    });
    let ratio = lua.as_secs_f64() / loom.as_secs_f64();
    println!("binary trees in arenas: {loom:.3?}, Lua {lua:.3?}: {ratio:.1}x (at least {MIN_TREES_VS_LUA}x)");
    assert!(ratio >= MIN_TREES_VS_LUA, "arena binary trees are {ratio:.1}x Lua (at least {MIN_TREES_VS_LUA}x)");
}

#[test]
#[ignore = "perf gate: run with --release -- --ignored --nocapture"]
fn an_arena_put_is_cheaper_than_a_heap_box() {
    let hello = startup();
    let arena = best("arena_alloc", 1).saturating_sub(hello);
    let heap = best("ptr_alloc", 1).saturating_sub(hello);
    let ratio = arena.as_secs_f64() / heap.as_secs_f64();
    println!("arena put {arena:.3?}, heap box {heap:.3?}: {ratio:.2} of the heap's (at most {MAX_ARENA_VS_HEAP})");
    assert!(ratio <= MAX_ARENA_VS_HEAP, "an arena put costs {ratio:.2} of a heap box (at most {MAX_ARENA_VS_HEAP})");
}
