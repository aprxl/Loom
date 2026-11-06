//! `loom docs`: the documentation site, built into the binary (see
//! `build.rs`), unpacked into a folder of its own and opened in the browser.
//! Like `zig std`, it works offline and always matches this `loom`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Every file of `docs/`, by its path there (with `/`).
pub static FILES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/docs.rs"));

/// Written last, so a folder cut short by a crash is unpacked again.
const DONE: &str = ".unpacked";

/// The page `loom docs` opens.
const LANDING: &str = "index.html";

pub fn docs(open: bool) -> ExitCode {
    let dir = std::env::temp_dir().join(format!("loom-docs-{}-{:016x}", env!("CARGO_PKG_VERSION"), hash()));
    if let Err(e) = unpack(&dir) {
        eprintln!("error: can't write the docs to `{}`: {e}", dir.display());
        return ExitCode::from(super::EXIT_INTERNAL);
    }
    let page = on_disk(&dir, LANDING);
    println!("{}", page.display());
    if open && let Err(e) = open_in_browser(&page) {
        eprintln!("error: can't open a browser: {e}");
        eprintln!("open the page above yourself");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// FNV-1a over every path and file, so each build's docs get their own folder.
fn hash() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (path, bytes) in FILES {
        for &b in path.as_bytes().iter().chain(&[0]).chain(bytes.iter()).chain(&[0]) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn unpack(dir: &Path) -> std::io::Result<()> {
    if dir.join(DONE).is_file() {
        return Ok(());
    }
    for (path, bytes) in FILES {
        let dest = on_disk(dir, path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dest, bytes)?;
    }
    std::fs::write(dir.join(DONE), "")
}

/// Where `path` (with `/`) is unpacked under `dir`.
fn on_disk(dir: &Path, path: &str) -> PathBuf {
    path.split('/').fold(dir.to_path_buf(), |d, part| d.join(part))
}

fn open_in_browser(page: &Path) -> std::io::Result<()> {
    let mut command = if cfg!(windows) {
        // Opens a file with its default program; no shell parses the path.
        std::process::Command::new("explorer.exe")
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    // explorer.exe exits with 1 even when it worked, so only starting counts.
    command.arg(page).spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_landing_page_is_built_in() {
        assert!(FILES.iter().any(|(f, _)| *f == LANDING), "no {LANDING} under docs/");
    }

    /// The samples run in the browser: the playground's script and the
    /// compiler it loads (built by `docs/play/build.py`) are built in.
    #[test]
    fn the_playground_is_built_in() {
        for file in ["play/play.js", "play/loom-wasm.js", "weave.js"] {
            assert!(FILES.iter().any(|(f, _)| *f == file), "no {file} under docs/");
        }
    }

    /// Every relative link and stylesheet points at a page that's built in,
    /// so the site works offline.
    #[test]
    fn every_link_works() {
        let mut broken = Vec::new();
        for (path, bytes) in FILES.iter().filter(|(p, _)| p.ends_with(".html")) {
            let text = String::from_utf8_lossy(bytes);
            for target in links(&text) {
                if target.contains("://") || target.starts_with("mailto:") || target.starts_with('#') {
                    continue;
                }
                let target = target.split('#').next().unwrap_or_default();
                let resolved = resolve(path, target);
                if !FILES.iter().any(|(f, _)| *f == resolved) {
                    broken.push(format!("{path}: {target}"));
                }
            }
        }
        assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
    }

    /// The values of `href="…"` and `src="…"`.
    fn links(html: &str) -> Vec<&str> {
        let mut found = Vec::new();
        for attr in ["href=\"", "src=\""] {
            let mut rest = html;
            while let Some(at) = rest.find(attr) {
                rest = &rest[at + attr.len()..];
                if let Some(end) = rest.find('"') {
                    found.push(&rest[..end]);
                }
            }
        }
        found
    }

    /// `target`, relative to the page at `from`, as a path under `docs/`.
    fn resolve(from: &str, target: &str) -> String {
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for part in target.split('/') {
            match part {
                "." | "" => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        parts.join("/")
    }
}
