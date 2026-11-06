//! The terminal, for `std.term`: raw mode, and which keys are held down.
//!
//! Keys come from crossterm's key events. Where the terminal reports key
//! releases (the Windows console does), a key is held from its press to its
//! release; elsewhere a key counts as held for the one poll after it was
//! pressed. In raw mode Ctrl-C is a key like any other, so a poll that sees
//! it fails, and the program unravels: its `defer`s put the terminal back.
//!
//! A WebAssembly build has no terminal: raw mode and keys are refused, and
//! the size is 80 by 24.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(target_family = "wasm"))]
use native::{next_key, set_raw_mode, terminal_size};
#[cfg(target_family = "wasm")]
use none::{next_key, set_raw_mode, terminal_size};

/// Whether raw mode is on, for the ways out that have no [`Term`] to ask: a
/// second Ctrl-C, a panic.
static RAW: AtomicBool = AtomicBool::new(false);

/// Leaves raw mode if it's on, from anywhere.
pub fn restore_terminal() {
    if RAW.swap(false, Ordering::Relaxed) {
        let _ = set_raw_mode(false);
    }
}

/// The terminal as one program run sees it.
#[derive(Debug, Default)]
pub struct Term {
    /// Whether the program turned raw mode on (and hasn't turned it off).
    raw: bool,
    held: HashSet<char>,
    /// Set once a release has been seen: from then on releases are trusted.
    reports_releases: bool,
}

impl Term {
    pub fn raw_mode(&mut self, on: bool) -> Result<(), String> {
        set_raw_mode(on).map_err(|e| format!("can't switch the terminal's raw mode: {e}"))?;
        self.raw = on;
        RAW.store(on, Ordering::Relaxed);
        Ok(())
    }

    /// A bit set of the keys of `keymap` that are held: bit `k` for its `k`th
    /// character (letters in either case).
    pub fn poll_keys(&mut self, keymap: &str) -> Result<u16, String> {
        let failed = |e: std::io::Error| format!("can't read the keyboard: {e}");
        let mut pressed = HashSet::new();
        while let Some(key) = next_key().map_err(failed)? {
            let Some(c) = key.char else { continue };
            let c = c.to_ascii_lowercase();
            if key.released {
                self.held.remove(&c);
                self.reports_releases = true;
                continue;
            }
            if c == 'c' && key.ctrl {
                return Err("interrupted (Ctrl-C)".into());
            }
            self.held.insert(c);
            pressed.insert(c);
        }
        let mut bits = 0u16;
        for (k, c) in keymap.chars().take(16).enumerate() {
            let c = c.to_ascii_lowercase();
            if self.held.contains(&c) || pressed.contains(&c) {
                bits |= 1 << k;
            }
        }
        if !self.reports_releases {
            self.held.clear();
        }
        Ok(bits)
    }

    /// Leaves raw mode if the program left it on.
    pub fn restore(&mut self) {
        if self.raw {
            let _ = self.raw_mode(false);
        }
    }
}

impl Drop for Term {
    /// However the run ends, the terminal is left as it was found.
    fn drop(&mut self) {
        self.restore();
    }
}

/// The terminal's columns and rows, or 80 and 24 when there is none.
pub fn size() -> (u16, u16) {
    terminal_size().unwrap_or((80, 24))
}

/// A key event: its character (none for keys that aren't one), whether it
/// was a release, and whether Ctrl was held.
struct Key {
    char: Option<char>,
    released: bool,
    ctrl: bool,
}

#[cfg(not(target_family = "wasm"))]
mod native {
    use std::time::Duration;

    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal;

    use super::Key;

    pub fn set_raw_mode(on: bool) -> std::io::Result<()> {
        if on { terminal::enable_raw_mode() } else { terminal::disable_raw_mode() }
    }

    /// The next key event waiting, if any; never blocks.
    pub fn next_key() -> std::io::Result<Option<Key>> {
        while event::poll(Duration::ZERO)? {
            let Event::Key(key) = event::read()? else { continue };
            return Ok(Some(Key {
                char: if let KeyCode::Char(c) = key.code { Some(c) } else { None },
                released: key.kind == KeyEventKind::Release,
                ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            }));
        }
        Ok(None)
    }

    pub fn terminal_size() -> std::io::Result<(u16, u16)> {
        terminal::size()
    }
}

#[cfg(target_family = "wasm")]
mod none {
    use super::Key;

    fn no_terminal() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Unsupported, "there is no terminal here")
    }

    pub fn set_raw_mode(_on: bool) -> std::io::Result<()> {
        Err(no_terminal())
    }

    pub fn next_key() -> std::io::Result<Option<Key>> {
        Err(no_terminal())
    }

    pub fn terminal_size() -> std::io::Result<(u16, u16)> {
        Err(no_terminal())
    }
}
