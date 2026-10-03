//! Ctrl-C, for both backends: a flag the handler sets, which the program
//! checks at its loops and calls (design/vm.md §9.5). The program then
//! unwinds, running its `defer`s, and `loom` exits with 130. A second
//! Ctrl-C while it unwinds exits at once.
//!
//! `LOOM_INTERRUPT_AFTER_MS` sets the flag that many milliseconds after the
//! handler is installed, as a Ctrl-C would: how the CLI tests interrupt a
//! program.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Installs the handler (once) and gives its flag.
pub fn install() -> Arc<AtomicBool> {
    FLAG.get_or_init(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let set = flag.clone();
        let _ = ctrlc::set_handler(move || {
            if set.swap(true, Ordering::Relaxed) {
                std::process::exit(130);
            }
        });
        if let Some(ms) = std::env::var("LOOM_INTERRUPT_AFTER_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            let set = flag.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                set.store(true, Ordering::Relaxed);
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
