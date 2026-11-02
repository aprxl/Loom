//! Host functions: Rust code an embedding application registers by name and
//! a program calls through `host.call`.

use std::collections::BTreeMap;
use std::fmt;

/// A host function: it reads its argument registers and writes its results over them.
pub type HostFn = Box<dyn FnMut(&mut [u64]) -> Result<(), String> + Send>;

/// The host functions of one run, by the name each import is registered under.
#[derive(Default)]
pub struct HostTable(BTreeMap<String, HostFn>);

/// The functions bound to a run's imports, in import order: each with the number of registers it takes.
#[derive(Default)]
pub(crate) struct HostFns(pub Vec<(usize, HostFn)>);

impl HostTable {
    /// Registers `f` under `name`. A name registered twice is an error.
    pub fn register(&mut self, name: &str, f: HostFn) -> Result<(), String> {
        if self.0.contains_key(name) {
            return Err(format!("host function `{name}` is registered twice"));
        }
        self.0.insert(name.to_owned(), f);
        Ok(())
    }

    /// The function registered under `name`, which it no longer holds.
    pub(crate) fn take(&mut self, name: &str) -> Option<HostFn> {
        self.0.remove(name)
    }
}

impl fmt::Debug for HostTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

impl fmt::Debug for HostFns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HostFns({})", self.0.len())
    }
}
