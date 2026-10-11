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

fn main() {}
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
