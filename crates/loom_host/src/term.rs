//! The terminal, for `std.term`: raw mode, and which keys are held down.
//!
//! Keys come from crossterm's key events. Where the terminal reports key
//! releases (the Windows console does), a key is held from its press to its
//! release; elsewhere a key counts as held for the one poll after it was
//! pressed. In raw mode Ctrl-C is a key like any other, so a poll that sees
//! it fails, and the program unravels: its `defer`s put the terminal back.

use std::collections::HashSet;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;

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
        let switched = if on { terminal::enable_raw_mode() } else { terminal::disable_raw_mode() };
        switched.map_err(|e| format!("can't switch the terminal's raw mode: {e}"))?;
        self.raw = on;
        Ok(())
    }

    /// A bit set of the keys of `keymap` that are held: bit `k` for its `k`th
    /// character (letters in either case).
    pub fn poll_keys(&mut self, keymap: &str) -> Result<u16, String> {
        let failed = |e: std::io::Error| format!("can't read the keyboard: {e}");
        let mut pressed = HashSet::new();
        while event::poll(Duration::ZERO).map_err(failed)? {
            let Event::Key(key) = event::read().map_err(failed)? else { continue };
            let KeyCode::Char(c) = key.code else { continue };
            let c = c.to_ascii_lowercase();
            if key.kind == KeyEventKind::Release {
                self.held.remove(&c);
                self.reports_releases = true;
                continue;
            }
            if c == 'c' && key.modifiers.contains(KeyModifiers::CONTROL) {
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
