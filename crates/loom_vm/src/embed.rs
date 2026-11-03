//! Host functions: Rust code an embedding application registers by name and
//! a program calls through `hostcall`.

use std::collections::BTreeMap;
use std::fmt;

use crate::natives::HostOps;

/// What a host function reaches besides its registers: the program's text and
/// the strings it builds. It has no output and no way back into the machine,
/// so a host function can't reenter the program.
pub struct HostCx<'a> {
    ops: &'a mut dyn HostOps,
}

impl<'a> HostCx<'a> {
    pub(crate) fn new(ops: &'a mut dyn HostOps) -> HostCx<'a> {
        HostCx { ops }
    }

    /// The `len` bytes of text at `addr`: a `str` argument's two registers.
    pub fn text(&self, addr: u64, len: u64) -> Result<&[u8], String> {
        self.ops.text(addr, len)
    }

    /// Writes a `String` of `text` to `dst`, a `String` result's first register.
    pub fn new_string(&mut self, text: &[u8], dst: u64) -> Result<(), String> {
        self.ops.new_string(text, dst)
    }

    /// The `len` bytes at `addr`: a value laid out by the compiler, read in place.
    pub fn bytes(&self, addr: u64, len: u64) -> Result<&[u8], String> {
        self.ops.bytes(addr, len)
    }

    /// The `len` bytes at `addr`, to write in place.
    pub fn bytes_mut(&mut self, addr: u64, len: u64) -> Result<&mut [u8], String> {
        self.ops.bytes_mut(addr, len)
    }
}

impl fmt::Debug for HostCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostCx").finish_non_exhaustive()
    }
}

/// A host function: it reads and writes its argument registers, and reaches
/// the program's text and strings through its [`HostCx`].
pub type HostFn = Box<dyn for<'c, 'a> FnMut(&'c mut HostCx<'a>, &mut [u64]) -> Result<(), String> + Send>;

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
