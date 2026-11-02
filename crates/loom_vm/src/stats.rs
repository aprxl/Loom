//! Optional operation counts. Workers collect their own counters and merge
//! them only when they stop; no shared atomic is added to dispatch.

/// The native categories in [`RuntimeStats::native_calls`] and
/// [`RuntimeStats::native_locks`].
pub const NATIVE_CATEGORIES: [&str; 7] = ["task", "heap", "atomic", "scalar", "text/map", "host", "other"];

/// Additional counters collected by the `stats` build. Lock counts cover
/// native dispatch and task operations, not idle-worker queue acquisition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeStats {
    pub native_calls: [u64; 7],
    pub native_locks: [u64; 7],
    pub task_locks: u64,
    pub leftover_probes: u64,
    pub unfinished_probes: u64,
    pub cancellation_probes: u64,
    pub task_stacks: u64,
    /// Times a task found a constant being computed by another and waited.
    pub const_waits: u64,
}

impl RuntimeStats {
    pub(crate) fn add(&mut self, other: &Self) {
        for i in 0..NATIVE_CATEGORIES.len() {
            self.native_calls[i] += other.native_calls[i];
            self.native_locks[i] += other.native_locks[i];
        }
        self.task_locks += other.task_locks;
        self.leftover_probes += other.leftover_probes;
        self.unfinished_probes += other.unfinished_probes;
        self.cancellation_probes += other.cancellation_probes;
        self.task_stacks += other.task_stacks;
        self.const_waits += other.const_waits;
    }
}

pub(crate) fn category(name: &str) -> usize {
    if name.starts_with("task.")
        || name.starts_with("loom.")
        || name.starts_with("mutex.")
        || name.starts_with("pipe.")
        || name == "const.begin"
    {
        0
    } else if name.starts_with("mem.") {
        1
    } else if name.starts_with("atomic.") {
        2
    } else if name.starts_with("int.") || name.starts_with("float.") || name.starts_with("char.") {
        3
    } else if name.starts_with("str.") || name.starts_with("string.") || name.starts_with("map.") {
        4
    } else if name.starts_with("fs.")
        || name.starts_with("env.")
        || name.starts_with("time.")
        || name.starts_with("term.")
        || name.starts_with("process.")
    {
        5
    } else {
        6
    }
}
