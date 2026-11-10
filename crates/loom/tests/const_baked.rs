//! What compile-time code computed, read back from the VM that ran it by
//! type (design/vm.md §17, V4): the values of constants of every type that
//! is baked, and that the rest aren't read.

use std::collections::HashMap;

use loom_compiler::baked::{Baked, BakedVal};
use loom_compiler::heddle::ConstKey;
use loom_compiler::hir::resolve;
use loom_compiler::modules::FsLoader;
use loom_compiler::target::Target;
use loom_compiler::{consteval, typeck};
use loom_syntax::SourceDb;

/// The constants of `source` that were read back, by name.
fn read_back(source: &str, target: Target) -> HashMap<String, BakedVal> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("loom-baked-{}-{n}.loom", std::process::id()));
    std::fs::write(&path, source).expect("a temporary file");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), source.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(&path));
    assert!(!resolved.has_errors(), "{:?}", resolved.diagnostics);
    let typed = typeck::check_for(&resolved.program, target);
    assert!(!typed.has_errors(), "{:?}", typed.diagnostics);
    let baked: Baked = consteval::evaluate(&db, &resolved.program, &typed, false).expect("evaluates");
    let _ = std::fs::remove_file(&path);
    baked
        .consts
        .iter()
        .filter_map(|(key, c)| match key {
            ConstKey::Def(def) => Some((resolved.program.def(*def).name.clone(), c.val.clone())),
            ConstKey::Expr(..) => None,
        })
        .collect()
}

const SOURCE: &str = r#"
warp Point { x: i32, y: i32 }
warp Named { name: String, tags: List[u8], at: Point }
warp Pair(u8, str)
warp Colour {
    case Red
    case Green
}
warp Shape {
    case Dot
    case Disc(radius: u32)
}

const SMALL: u8 = 200
const NEG: i16 = -300
const MIN: i64 = -9223372036854775808
const MAX: u64 = 18446744073709551615
const ODD: u24 = 16777215
const FLAG: bool = true
const LETTER: char = 'é'
const ZERO: f64 = -0.0
const THIRD: f32 = 0.33333334
const INF: f64 = 1.0 / 0.0
const TEXT: str = "héllo"
const OWNED: String = "owned"
const NOTHING: () = ()
const PAIR: (u8, str) = (7, "seven")
const GRID: [u16; 4] = [1, 2, 3, 40000]
const NAMES: List[String] = ["ada", "grace"]
const NONE: ?i32 = none
const SOME: ?i32 = some(5)
const BOXED: ?String = some("in a niche")
const NESTED: ??i32 = some(none)
const UNIT_SOME: ?() = some(())
const ORIGIN: Point = Point(x: -1, y: 2)
const WHO: Named = Named(name: "lin", tags: [1, 2], at: Point(x: 3, y: 4))
const TUP: Pair = Pair(9, "nine")
const EMPTY: List[u32] = []
const BLANK: String = ""
const FLAGS: [bool; 3] = [true, false, true]
const LATER: Colour = Colour.Green
const DISC: Shape = .Disc(radius: 3)

fun main() {}
"#;

#[test]
fn constants_are_read_back_by_type() {
    use BakedVal::{Bool, Char, Float, Int, Seq, Str, Tuple, Unit};
    let none = BakedVal::None;
    let some = |v: BakedVal| BakedVal::Some(Box::new(v));
    let got = read_back(SOURCE, Target::VM);
    let s = |t: &str| Str(t.as_bytes().to_vec());
    let want: Vec<(&str, BakedVal)> = vec![
        ("SMALL", Int(200)),
        ("NEG", Int(-300)),
        ("MIN", Int(i64::MIN.into())),
        ("MAX", Int(u64::MAX.into())),
        ("ODD", Int(16_777_215)),
        ("FLAG", Bool(true)),
        ("LETTER", Char('é' as u32)),
        ("ZERO", Float((-0.0f64).to_bits())),
        ("THIRD", Float(f64::from(0.33333334f32).to_bits())),
        ("INF", Float(f64::INFINITY.to_bits())),
        ("TEXT", s("héllo")),
        ("OWNED", s("owned")),
        ("NOTHING", Unit),
        ("PAIR", Tuple(vec![Int(7), s("seven")])),
        ("GRID", Seq(vec![Int(1), Int(2), Int(3), Int(40000)])),
        ("NAMES", Seq(vec![s("ada"), s("grace")])),
        ("NONE", none.clone()),
        ("SOME", some(Int(5))),
        ("BOXED", some(s("in a niche"))),
        ("NESTED", some(none)),
        ("UNIT_SOME", some(Unit)),
        ("ORIGIN", Tuple(vec![Int(-1), Int(2)])),
        ("WHO", Tuple(vec![s("lin"), Seq(vec![Int(1), Int(2)]), Tuple(vec![Int(3), Int(4)])])),
        ("TUP", Tuple(vec![Int(9), s("nine")])),
        ("EMPTY", Seq(vec![])),
        ("BLANK", s("")),
        ("FLAGS", Seq(vec![Bool(true), Bool(false), Bool(true)])),
    ];
    for (name, val) in want {
        assert_eq!(got.get(name), Option::Some(&val), "{name}");
    }
    // Sums aren't read: they're computed when the program reads them.
    assert!(!got.contains_key("LATER"));
    assert!(!got.contains_key("DISC"));
}

/// A program with a table and a list of text, which are data (V4's passing
/// item 3): neither is computed by the program that runs.
const TABLES: &str = r#"
use std.io.print

const CRC_TABLE: [u32; 256] = {
    var t = [0u32; 256]
    var i = 0
    while i < 256 {
        var c = i as u32
        var k = 0
        while k < 8 {
            c = if c & 1 == 1 { 0xEDB8_8320 ~ (c >> 1) } else { c >> 1 }
            k += 1
        }
        t[i] = c
        i += 1
    }
    t
}
const WORDS: List[String] = ["alpha", "beta", "gamma"]
const LIMIT: u32 = 12 * 1000

fun main() {
    print("{CRC_TABLE[255]} {WORDS} {LIMIT}")
}
"#;

fn compile_tables() -> (SourceDb, loom_compiler::hir::Program, typeck::Typed) {
    // The same path each time, so what's compared isn't where it was.
    let path = std::env::temp_dir().join(format!("loom-tables-{}.loom", std::process::id()));
    std::fs::write(&path, TABLES).expect("a temporary file");
    let mut db = SourceDb::new();
    let file = db.add(path.display().to_string(), TABLES.to_string());
    let resolved = resolve(&mut db, file, &FsLoader::new(&path));
    assert!(!resolved.has_errors());
    let typed = typeck::check_for(&resolved.program, Target::VM);
    assert!(!typed.has_errors());
    (db, resolved.program, typed)
}

#[test]
fn a_table_and_a_list_of_strings_are_data_not_computed() {
    let (db, program, typed) = compile_tables();
    // Treadle: no function computes a constant, and the image's data has
    // the table's words.
    let compiled = loom_compiler::treadle::compile_at(&db, &program, &typed, loom_compiler::heddle::opt::Level::O0)
        .expect("compiles");
    let inits: Vec<&str> =
        compiled.heddle.funcs.iter().map(|f| f.name.as_str()).filter(|n| n.ends_with(".init")).collect();
    assert!(inits.is_empty(), "constants computed by the program: {inits:?}");
    let bytes: Vec<u8> = compiled.image.data.iter().flat_map(|w| w.to_le_bytes()).collect();
    let entry = 0x7707_3096u32.to_le_bytes();
    assert!(bytes.windows(4).any(|w| w == entry), "the table's second entry is in the data");
    assert!(!compiled.image.relocs.is_empty(), "the list points into the data");
    // Lua: the bundle assigns the literals, and has no initializer.
    let lua_typed = typeck::check_for(&program, Target::LUA);
    let bundle = loom_compiler::lower::lower(&db, &program, &lua_typed).expect("lowers");
    assert!(!bundle.lua.contains("CI."), "an initializer in the bundle");
    assert!(bundle.lua.contains("1996959894"), "the table's second entry is a literal");
    assert!(bundle.lua.contains("C.WORDS_"), "the list is assigned");
}

/// Building twice gives the same image and bundle (V4's passing item 4),
/// with other allocations in between so no address could be what's the same.
#[test]
fn building_twice_in_a_process_gives_the_same_bytes() {
    let build = || {
        let (db, program, typed) = compile_tables();
        let image = loom_compiler::treadle::compile(&db, &program, &typed).expect("compiles").image;
        let lua_typed = typeck::check_for(&program, Target::LUA);
        let bundle = loom_compiler::lower::lower(&db, &program, &lua_typed).expect("lowers");
        (image, bundle.lua, bundle.map.to_text())
    };
    let first = build();
    // Move the heap about.
    let junk: Vec<Vec<u8>> = (0..300).map(|i| vec![i as u8; 1 + (i * 37) % 4099]).collect();
    std::mem::forget(junk);
    let second = build();
    assert_eq!(first.1, second.1, "the bundle changed");
    assert_eq!(first.2, second.2, "the source map changed");
    assert_eq!(first.0, second.0, "the image changed");
}

/// The same, across processes: the bundle and the listing of the image.
#[test]
fn building_in_two_processes_gives_the_same_bytes() {
    let dir = std::env::temp_dir().join(format!("loom-twice-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    let source = dir.join("tables.loom");
    std::fs::write(&source, TABLES).expect("a file");
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_loom")).args(args).output().expect("runs loom");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    };
    let src = source.to_str().expect("utf-8");
    let listing_a = run(&["build", "--emit", "bytecode", src]);
    let listing_b = run(&["build", "--emit", "bytecode", src]);
    assert_eq!(listing_a, listing_b, "the image's listing changed");
    let (a, b) = (dir.join("a.lua"), dir.join("b.lua"));
    run(&["build", "--backend", "lua", src, "-o", a.to_str().expect("utf-8")]);
    run(&["build", "--backend", "lua", src, "-o", b.to_str().expect("utf-8")]);
    assert_eq!(std::fs::read(&a).expect("a bundle"), std::fs::read(&b).expect("a bundle"));
    assert_eq!(
        std::fs::read(dir.join("a.lua.map")).expect("a map"),
        std::fs::read(dir.join("b.lua.map")).expect("a map")
    );
    let text = String::from_utf8_lossy(&listing_a);
    assert!(text.contains("-> data+"), "the listing shows the relocations");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `usize` is 64 bits on Treadle and 63 on the Lua target, and what
/// compile-time code computed with it is read back as that.
#[test]
fn usize_is_the_targets() {
    // `target` is the target being built: what the constants see.
    let source = "const BIG: usize = usize.MAX\nconst SBIG: isize = isize.MIN\nconst BITS: u64 = target.pointer_bits as u64\nfun main() {}\n";
    let vm = read_back(source, Target::VM);
    assert_eq!(vm["BIG"], BakedVal::Int(u64::MAX.into()));
    assert_eq!(vm["SBIG"], BakedVal::Int(i64::MIN.into()));
    assert_eq!(vm["BITS"], BakedVal::Int(64));
    let lua = read_back(source, Target::LUA);
    assert_eq!(lua["BIG"], BakedVal::Int(((1u128 << 63) - 1) as i128));
    assert_eq!(lua["SBIG"], BakedVal::Int(-(1i128 << 62)));
    assert_eq!(lua["BITS"], BakedVal::Int(63));
}
