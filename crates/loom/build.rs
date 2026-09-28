//! Embeds the documentation site (`docs/`) for `loom docs`: every page,
//! stylesheet and image under it, in path order, so builds are reproducible.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// What a browser needs; the generators and their data stay behind.
const EXTENSIONS: &[&str] = &["html", "css", "svg", "png"];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let docs = manifest.join("../../docs");
    println!("cargo:rerun-if-changed={}", docs.display());
    let mut files = Vec::new();
    collect(&docs, &docs, &mut files);
    files.sort();
    let mut out = String::from("&[\n");
    for (published, source) in &files {
        writeln!(out, "    ({published:?}, include_bytes!({:?}) as &[u8]),", source.display()).expect("a String");
    }
    out.push(']');
    let dest = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo")).join("docs.rs");
    std::fs::write(dest, out).expect("OUT_DIR is writable");
}

/// The files under `dir`, as (path relative to `root` with `/`, path on disk).
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e)) {
            let relative = path.strip_prefix(root).expect("under root");
            let published: Vec<String> =
                relative.components().map(|c| c.as_os_str().to_string_lossy().into()).collect();
            files.push((published.join("/"), path));
        }
    }
}
