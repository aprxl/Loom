//! Ctrl-C, for both backends: a flag the handler sets (design/vm.md §9.5),
//! and watchers it tells. Treadle checks the flag at its ticks; the Lua VM
//! is told through a watcher, which installs a hook then (a hook installed
//! all along would slow every Lua instruction down). The program unwinds,
//! running its `defer`s, and `loom` exits with 130. A second Ctrl-C while
//! it unwinds exits at once.
//!
//! `LOOM_INTERRUPT_AFTER_MS` interrupts the program that many milliseconds
//! after the handler is installed, as a Ctrl-C would: how the CLI tests
//! interrupt a program.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

type Watcher = Box<dyn Fn() + Send>;

/// What to tell of an interrupt, by key.
static WATCHERS: Mutex<Vec<(u64, Watcher)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Interrupts the program: sets the flag and tells the watchers. `false`
/// when it was interrupted already.
fn interrupt(flag: &AtomicBool) -> bool {
    if flag.swap(true, Ordering::Relaxed) {
        return false;
    }
    if let Ok(watchers) = WATCHERS.lock() {
        for (_, w) in watchers.iter() {
            w();
        }
    }
    true
}

/// Installs the handler (once) and gives its flag.
pub fn install() -> Arc<AtomicBool> {
    FLAG.get_or_init(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let set = flag.clone();
        let _ = ctrlc::set_handler(move || {
            if !interrupt(&set) {
                // The second one: out at once, but with the terminal as it
                // was found. (Output the program hadn't flushed is lost.)
                crate::term::restore_terminal();
                let _ = std::io::Write::flush(&mut std::io::stdout());
                std::process::exit(130);
            }
        });
        if let Some(ms) = std::env::var("LOOM_INTERRUPT_AFTER_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            let set = flag.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                interrupt(&set);
            });
        }
        flag
    })
    .clone()
}

/// The flag, if the handler is installed: a run checks it only then.
pub fn installed() -> Option<Arc<AtomicBool>> {
    FLAG.get().cloned()
}

/// Calls `f` on an interrupt, until the [`Watch`] goes. It runs on the
/// handler's thread, while the watch can't go.
pub fn watch(f: impl Fn() + Send + 'static) -> Watch {
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    // Registered before the flag is read, under the lock `interrupt` tells
    // the watchers by: an interrupt either sees this watcher or has set the
    // flag already, so it can't fall between the two. (Told twice at worst.)
    if let Ok(mut watchers) = WATCHERS.lock() {
        watchers.push((key, Box::new(f)));
        if FLAG.get().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            (watchers[watchers.len() - 1].1)();
        }
    }
    Watch(key)
}

/// A watcher of interrupts (see [`watch`]); dropping it stops it.
#[derive(Debug)]
pub struct Watch(u64);

impl Drop for Watch {
    fn drop(&mut self) {
        if let Ok(mut watchers) = WATCHERS.lock() {
            watchers.retain(|(k, _)| *k != self.0);
        }
    }
}
