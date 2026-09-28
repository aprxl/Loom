//! Where module sources come from.
//!
//! `std` is embedded in the binary. Setting `LOOM_STD_DIR` reads it from a
//! directory instead, which is handy while editing the standard library.
//! User modules live next to the root file: `use app.parser` looks for
//! `app/parser.loom` (or `app.loom`, whose item `parser` is then imported).

use std::path::{Path, PathBuf};

/// A source file to load as a module.
#[derive(Clone, Debug)]
pub struct Source {
    /// The name diagnostics show for the file.
    pub name: String,
    pub text: String,
}

/// What a user module path points at.
#[derive(Clone, Debug)]
pub enum UserModule {
    File(Source),
    /// A directory: a module whose children are its files and subdirectories.
    Dir,
}

/// Supplies module sources to name resolution.
pub trait Loader {
    /// Every `std` module, as its path below `std` (`["io"]`, `["sys", "libc"]`) and source.
    fn std_modules(&self) -> Vec<(Vec<String>, Source)>;

    /// The user module at `path`, relative to the root file's directory.
    fn user_module(&self, path: &[String]) -> Option<UserModule>;
}

/// `std` as embedded at build time: `(path below std, source)`.
const EMBEDDED_STD: &[(&str, &str)] = &[
    ("prelude", include_str!("../../../runtime/std/prelude.loom")),
    ("env", include_str!("../../../runtime/std/env.loom")),
    ("error", include_str!("../../../runtime/std/error.loom")),
    ("fmt", include_str!("../../../runtime/std/fmt.loom")),
    ("fs", include_str!("../../../runtime/std/fs.loom")),
    ("io", include_str!("../../../runtime/std/io.loom")),
    ("iter", include_str!("../../../runtime/std/iter.loom")),
    ("list", include_str!("../../../runtime/std/list.loom")),
    ("map", include_str!("../../../runtime/std/map.loom")),
    ("math", include_str!("../../../runtime/std/math.loom")),
    ("mem", include_str!("../../../runtime/std/mem.loom")),
    ("num", include_str!("../../../runtime/std/num.loom")),
    ("ops", include_str!("../../../runtime/std/ops.loom")),
    ("option", include_str!("../../../runtime/std/option.loom")),
    ("rand", include_str!("../../../runtime/std/rand.loom")),
    ("set", include_str!("../../../runtime/std/set.loom")),
    ("slice", include_str!("../../../runtime/std/slice.loom")),
    ("string", include_str!("../../../runtime/std/string.loom")),
    ("sync", include_str!("../../../runtime/std/sync.loom")),
    ("target", include_str!("../../../runtime/std/target.loom")),
    ("term", include_str!("../../../runtime/std/term.loom")),
    ("time", include_str!("../../../runtime/std/time.loom")),
];

/// Loads user modules from the file system, and `std` from the embedded
/// copy or `LOOM_STD_DIR`.
#[derive(Debug)]
pub struct FsLoader {
    root_dir: PathBuf,
    std_dir: Option<PathBuf>,
}

impl FsLoader {
    /// A loader for a program whose root file is `root_file`.
    pub fn new(root_file: &Path) -> Self {
        let root_dir = root_file.parent().map(Path::to_path_buf).unwrap_or_default();
        let std_dir = std::env::var_os("LOOM_STD_DIR").map(PathBuf::from);
        Self { root_dir, std_dir }
    }
}

impl Loader for FsLoader {
    fn std_modules(&self) -> Vec<(Vec<String>, Source)> {
        match &self.std_dir {
            Some(dir) => {
                let mut out = Vec::new();
                read_dir_modules(dir, &mut Vec::new(), &mut out);
                out.sort_by(|a, b| a.0.cmp(&b.0));
                out
            }
            None => embedded_std(),
        }
    }

    fn user_module(&self, path: &[String]) -> Option<UserModule> {
        let mut base = self.root_dir.clone();
        base.extend(path);
        if base.is_dir() {
            return Some(UserModule::Dir);
        }
        let file = base.with_extension("loom");
        let text = std::fs::read_to_string(&file).ok()?;
        Some(UserModule::File(Source { name: file.display().to_string(), text }))
    }
}

/// The embedded `std`, in a fixed order.
pub fn embedded_std() -> Vec<(Vec<String>, Source)> {
    EMBEDDED_STD
        .iter()
        .map(|(path, text)| {
            let segments = path.split('/').map(str::to_string).collect();
            (segments, Source { name: format!("<std>/{path}.loom"), text: (*text).to_string() })
        })
        .collect()
}

fn read_dir_modules(dir: &Path, prefix: &mut Vec<String>, out: &mut Vec<(Vec<String>, Source)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
        if path.is_dir() {
            prefix.push(stem);
            read_dir_modules(&path, prefix, out);
            prefix.pop();
        } else if path.extension().is_some_and(|e| e == "loom")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            let mut segments = prefix.clone();
            segments.push(stem);
            out.push((segments, Source { name: path.display().to_string(), text }));
        }
    }
}

/// A loader for tests: `std` is embedded, and user modules come from a list.
#[derive(Debug, Default)]
pub struct MemoryLoader {
    pub user: Vec<(Vec<String>, Source)>,
}

impl Loader for MemoryLoader {
    fn std_modules(&self) -> Vec<(Vec<String>, Source)> {
        embedded_std()
    }

    fn user_module(&self, path: &[String]) -> Option<UserModule> {
        if let Some((_, source)) = self.user.iter().find(|(p, _)| p == path) {
            return Some(UserModule::File(source.clone()));
        }
        self.user.iter().any(|(p, _)| p.len() > path.len() && p.starts_with(path)).then_some(UserModule::Dir)
    }
}
