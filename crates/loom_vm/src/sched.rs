//! Tasks, looms and the reference schedule (design/vm.md §11).
//!
//! A thread is a task: its own value stack, and the registers the dispatch
//! loop keeps (`pc`, `fp`, the end of its segment, the unwind in flight).
//! Switching tasks swaps those, inside `execute`: no Rust stack is involved,
//! because natives never call Loom code. The running task's stack is in the
//! interpreter's hands; every other task's is here.
//!
//! This is the one-worker schedule, which the Lua backend implements the
//! same way:
//!
//! - `thread e` starts the task at once; the spawner waits at the front of
//!   the ready queue until it finishes or parks;
//! - a task parks at `join`, `sched_yield` and the end of a loom; waking
//!   appends it to the back of the queue;
//! - nothing is preempted.
//!
//! A loom's [`Group`] holds its tasks, and the first failure among them. The
//! parent observes the group at `join` and where the loom ends.

#![allow(unsafe_code)]

use std::collections::VecDeque;

use crate::Trap;
use crate::heap::Heap;
use crate::stack::Stack;

/// What a task operation asks the dispatch loop to do next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Carry on with the next instruction.
    Next,
    /// The running task waits (it's in a wait list or the ready queue
    /// already): another one runs. It goes on at the instruction itself
    /// when `again` (which looks at what it waits for once more), or after
    /// it.
    Park { again: bool },
    /// A new task runs now, and the spawner waits at the front of the queue.
    Start(usize),
}

/// The task operations, as the natives `task.*` and `loom.*` name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskOp {
    Spawn,
    Join,
    Yield,
    LoomNew,
    LoomLeave,
    LoomLeftover,
    LoomFree,
    Cancelled,
}

impl TaskOp {
    pub fn named(name: &str) -> Option<TaskOp> {
        Some(match name {
            "task.spawn" => TaskOp::Spawn,
            "task.join" => TaskOp::Join,
            "task.yield" => TaskOp::Yield,
            "loom.new" => TaskOp::LoomNew,
            "loom.leave" => TaskOp::LoomLeave,
            "loom.leftover" => TaskOp::LoomLeftover,
            "loom.free" => TaskOp::LoomFree,
            "task.cancelled" => TaskOp::Cancelled,
            _ => return None,
        })
    }
}

/// How a loom block's end was reached: `loom.leave`'s and `loom.free`'s
/// second argument.
pub(crate) mod mode {
    /// Falling off the end: the threads are waited for.
    pub const NORMAL: u64 = 0;
    /// `return`, `break`, `?`: the threads are cancelled, then waited for.
    #[allow(dead_code)]
    pub const EARLY: u64 = 1;
    /// An unwind passing through: as early, but nothing is raised (what is
    /// in flight goes on).
    pub const PAD: u64 = 2;
}

/// The slots of a thread's first stack segment, unless its entry frame
/// needs more: more segments follow as it goes deeper.
const FIRST_SEGMENT: usize = 2048;

#[derive(Debug)]
pub(crate) struct Task {
    /// While the task isn't running: its stack. (The running task's is the
    /// interpreter's.)
    pub stack: Stack,
    pub pc: *const u64,
    pub fp: *mut u64,
    pub seg_end: *mut u64,
    pub in_flight: Option<crate::Fault>,
    pub done: bool,
    /// Its result was taken by `join` or by the loom's leftover loop.
    pub taken: bool,
    pub cancelled: bool,
    /// It failed: its fault is the group's, and it has no result.
    pub failed: bool,
    /// Parked where a cancel is noticed (`join`, `sched_yield`).
    pub cancellable: bool,
    pub waiting: bool,
    pub waiting_on: Option<usize>,
    /// Tasks waiting for this one to finish.
    pub waiters: Vec<usize>,
    pub group: usize,
    /// Where the result is written (the entry function does it), and how
    /// many bytes.
    pub result: u64,
    pub result_bytes: u64,
    /// What the task runs in: freed when it ends.
    pub env: u64,
    pub env_bytes: u64,
    /// The function that drops a result nobody took, or 0.
    pub drop_fn: u32,
    pub epoch: u32,
    live: bool,
}

impl Task {
    fn empty() -> Task {
        Task {
            stack: Stack::empty(),
            pc: std::ptr::null(),
            fp: std::ptr::null_mut(),
            seg_end: std::ptr::null_mut(),
            in_flight: None,
            done: false,
            taken: false,
            cancelled: false,
            failed: false,
            cancellable: false,
            waiting: false,
            waiting_on: None,
            waiters: Vec::new(),
            group: 0,
            result: 0,
            result_bytes: 0,
            env: 0,
            env_bytes: 0,
            drop_fn: 0,
            epoch: 0,
            live: false,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Group {
    /// Its tasks, in the order they were started.
    pub tasks: Vec<usize>,
    /// What the first task to fail ended with.
    pub failure: Option<Trap>,
    pub epoch: u32,
    live: bool,
}

#[derive(Debug)]
pub(crate) struct Sched {
    tasks: Vec<Task>,
    groups: Vec<Group>,
    free_tasks: Vec<usize>,
    free_groups: Vec<usize>,
    /// The running task: 0 is the program's own.
    pub current: usize,
    ready: VecDeque<usize>,
    /// Raised in the main task when it next runs (a deadlock).
    pending: Option<Trap>,
}

/// A handle: the slot and how many times it has been used.
fn handle(index: usize, epoch: u32) -> u64 {
    (u64::from(epoch) << 32) | index as u64
}

fn stale(what: &str) -> Trap {
    Trap::Crash(format!("loom internal: a {what} handle that's no longer valid"))
}

impl Sched {
    pub fn new() -> Sched {
        let mut main = Task::empty();
        main.live = true;
        Sched {
            tasks: vec![main],
            groups: Vec::new(),
            free_tasks: Vec::new(),
            free_groups: Vec::new(),
            current: 0,
            ready: VecDeque::new(),
            pending: None,
        }
    }

    /// Back to just the program's own task, for a new call from outside.
    pub fn reset(&mut self) {
        self.tasks.truncate(1);
        self.tasks[0] = Task { live: true, ..Task::empty() };
        self.groups.clear();
        self.free_tasks.clear();
        self.free_groups.clear();
        self.current = 0;
        self.ready.clear();
        self.pending = None;
    }

    /// The memory of the stacks of the tasks that aren't running (the
    /// running one's is the interpreter's): what a thread may touch of
    /// its parent's frames.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.tasks.iter().filter(|t| t.live).flat_map(|t| t.stack.ranges()).collect()
    }

    // ------------------------------------------------------------ switching

    /// Saves the running task's registers and stack, and makes `next` the
    /// running one, with the registers it left: the new `pc`, `fp` and
    /// segment end.
    pub fn switch(
        &mut self,
        next: usize,
        pc: *const u64,
        fp: *mut u64,
        seg_end: *mut u64,
        in_flight: &mut Option<crate::Fault>,
        stack: &mut Stack,
    ) -> (*const u64, *mut u64, *mut u64) {
        let cur = self.current;
        if next == cur {
            return (pc, fp, seg_end);
        }
        {
            let t = &mut self.tasks[cur];
            t.pc = pc;
            t.fp = fp;
            t.seg_end = seg_end;
            std::mem::swap(&mut t.in_flight, in_flight);
            std::mem::swap(&mut t.stack, stack);
        }
        let t = &mut self.tasks[next];
        std::mem::swap(&mut t.stack, stack);
        std::mem::swap(&mut t.in_flight, in_flight);
        self.current = next;
        (t.pc, t.fp, t.seg_end)
    }

    /// What the task that just got the processor has to do before it goes
    /// on: a cancel it was parked for, or a deadlock.
    pub fn arrival(&mut self) -> Option<Trap> {
        let cur = self.current;
        let t = &mut self.tasks[cur];
        let was_parked = std::mem::take(&mut t.cancellable);
        if cur == 0
            && let Some(trap) = self.pending.take()
        {
            return Some(trap);
        }
        (was_parked && t.cancelled).then_some(Trap::Cancel)
    }

    /// The next task to run, when the running one parks or ends. `None`
    /// is a deadlock: nothing is ready, and the program's task waits.
    pub fn next_ready(&mut self) -> usize {
        if let Some(next) = self.ready.pop_front() {
            return next;
        }
        // Nothing can run: every task waits for another. The program's own
        // task is told, where it waits.
        let main = &mut self.tasks[0];
        main.waiting = false;
        if let Some(on) = main.waiting_on.take() {
            self.tasks[on].waiters.retain(|&w| w != 0);
        }
        self.pending = Some(Trap::Unravel("deadlock: every thread is waiting".into()));
        0
    }

    // ------------------------------------------------------------ waiting

    fn wake(&mut self, t: usize) {
        let task = &mut self.tasks[t];
        if task.waiting {
            task.waiting = false;
            task.waiting_on = None;
            self.ready.push_back(t);
        }
    }

    /// The running task waits for `on`.
    fn park_on(&mut self, on: usize, cancellable: bool) -> Step {
        let cur = self.current;
        let t = &mut self.tasks[cur];
        t.waiting = true;
        t.waiting_on = Some(on);
        t.cancellable = cancellable;
        self.tasks[on].waiters.push(cur);
        Step::Park { again: true }
    }

    fn cancel(&mut self, t: usize) {
        {
            let task = &mut self.tasks[t];
            if task.done || task.cancelled {
                return;
            }
            task.cancelled = true;
        }
        if let Some(on) = self.tasks[t].waiting_on {
            self.tasks[on].waiters.retain(|&w| w != t);
        }
        self.wake(t);
    }

    fn cancel_all(&mut self, g: usize, except: usize) {
        for i in 0..self.groups[g].tasks.len() {
            let t = self.groups[g].tasks[i];
            if t != except {
                self.cancel(t);
            }
        }
    }

    /// The first task of group `g`, in the order they started, that hasn't
    /// ended (and isn't the running one).
    fn unfinished(&self, g: usize) -> Option<usize> {
        self.groups[g].tasks.iter().copied().find(|&t| t != self.current && !self.tasks[t].done)
    }

    fn group_index(&self, h: u64) -> Result<usize, Trap> {
        let (i, epoch) = ((h & 0xFFFF_FFFF) as usize, (h >> 32) as u32);
        match self.groups.get(i) {
            Some(g) if g.live && g.epoch == epoch => Ok(i),
            _ => Err(stale("loom")),
        }
    }

    fn task_index(&self, h: u64) -> Result<usize, Trap> {
        let (i, epoch) = ((h & 0xFFFF_FFFF) as usize, (h >> 32) as u32);
        match self.tasks.get(i) {
            Some(t) if t.live && t.epoch == epoch && i != 0 => Ok(i),
            _ => Err(stale("thread")),
        }
    }

    // ------------------------------------------------------------ operations

    pub fn loom_new(&mut self) -> u64 {
        let g = Group { live: true, ..Group::default() };
        match self.free_groups.pop() {
            Some(i) => {
                let epoch = self.groups[i].epoch.wrapping_add(1);
                self.groups[i] = Group { epoch, ..g };
                handle(i, epoch)
            }
            None => {
                self.groups.push(g);
                handle(self.groups.len() - 1, 0)
            }
        }
    }

    /// The end of a loom block reached in `mode`: what's left of its threads
    /// is cancelled (but a normal end only when one failed), and waited for.
    pub fn leave(&mut self, g: u64, mode_: u64) -> Result<Step, Trap> {
        let g = self.group_index(g)?;
        if mode_ != mode::NORMAL || self.groups[g].failure.is_some() {
            self.cancel_all(g, self.current);
        }
        match self.unfinished(g) {
            // Not a cancellation point: a task leaving a loom waits for its
            // threads even when it's been cancelled itself.
            Some(t) => Ok(self.park_on(t, false)),
            None => Ok(Step::Next),
        }
    }

    /// The next result nobody took that needs dropping: its drop function
    /// and where it is, or `(0, 0)`.
    pub fn leftover(&mut self, g: u64) -> Result<(u64, u64), Trap> {
        let g = self.group_index(g)?;
        for i in 0..self.groups[g].tasks.len() {
            let t = self.groups[g].tasks[i];
            let task = &mut self.tasks[t];
            if task.done && !task.taken && !task.cancelled && !task.failed && task.drop_fn != 0 {
                task.taken = true;
                return Ok((u64::from(task.drop_fn), task.result));
            }
        }
        Ok((0, 0))
    }

    /// Ends a loom's group: its tasks' storage is given back, and the
    /// failure that's left is raised (unless the end was an unwind).
    pub fn free(&mut self, heap: &mut Heap, g: u64, mode_: u64) -> Result<(), Trap> {
        let g = self.group_index(g)?;
        let group = std::mem::take(&mut self.groups[g]);
        self.groups[g].epoch = group.epoch;
        self.free_groups.push(g);
        for &t in &group.tasks {
            let task = &mut self.tasks[t];
            if task.result_bytes > 0 {
                heap.free(task.result, task.result_bytes, 8)?;
            }
            *task = Task { epoch: task.epoch.wrapping_add(1), ..Task::empty() };
            self.free_tasks.push(t);
        }
        match group.failure {
            Some(trap) if mode_ != mode::PAD => Err(trap),
            _ => Ok(()),
        }
    }

    pub fn join(&mut self, h: u64, dest: u64) -> Result<Step, Trap> {
        let cur = self.current;
        if self.tasks[cur].cancelled {
            return Err(Trap::Cancel);
        }
        let t = self.task_index(h)?;
        let g = self.tasks[t].group;
        if self.tasks[t].taken {
            return Err(Trap::Unravel("this thread was joined already".into()));
        }
        // A failed group: what's left is cancelled and waited for, and the
        // parent unravels with the failure, wherever it was joining.
        if let Some(failure) = self.groups[g].failure.clone() {
            self.cancel_all(g, cur);
            return match self.unfinished(g) {
                Some(t) => Ok(self.park_on(t, false)),
                None => Err(failure),
            };
        }
        if !self.tasks[t].done {
            return Ok(self.park_on(t, true));
        }
        let task = &mut self.tasks[t];
        task.taken = true;
        if task.result_bytes > 0 {
            // SAFETY: the result area is `result_bytes` of the task's own
            // allocation, which the group keeps until the loom ends; `dest`
            // is a slot of the joining frame sized for the thread's type
            // (the compiler passes the same size to `task.spawn`).
            unsafe {
                std::ptr::copy_nonoverlapping(task.result as *const u8, dest as *mut u8, task.result_bytes as usize);
            }
        }
        Ok(Step::Next)
    }

    pub fn sched_yield(&mut self) -> Result<Step, Trap> {
        let cur = self.current;
        if self.tasks[cur].cancelled {
            return Err(Trap::Cancel);
        }
        self.tasks[cur].cancellable = true;
        self.ready.push_back(cur);
        Ok(Step::Park { again: false })
    }

    /// Whether the running task has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.tasks[self.current].cancelled
    }

    /// Makes a task of group `g` that runs the function at `entry`, which
    /// takes `(result address, env)`. Gives its handle and what to do.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &mut self,
        heap: &mut Heap,
        limit: usize,
        entry: (*const u64, usize, usize),
        g: u64,
        env: u64,
        env_bytes: u64,
        drop_fn: u64,
        result_bytes: u64,
    ) -> Result<(u64, Step), Trap> {
        let g = self.group_index(g)?;
        let (code, frame, params) = entry;
        if params != 2 {
            return Err(Trap::Crash("loom internal: a thread's function doesn't take (result, env)".into()));
        }
        let failed = self.groups[g].failure.is_some();
        let result = if result_bytes > 0 { heap.alloc(result_bytes, 8)? } else { 0 };
        if result_bytes > 0 {
            // SAFETY: just allocated, `result_bytes` long.
            unsafe { std::ptr::write_bytes(result as *mut u8, 0, result_bytes as usize) };
        }
        let mut task = Task {
            live: true,
            group: g,
            result,
            result_bytes,
            env,
            env_bytes,
            drop_fn: drop_fn as u32,
            ..Task::empty()
        };
        if failed {
            // Spawned into a failed group: it starts cancelled and never
            // runs.
            task.done = true;
            task.cancelled = true;
            Self::release_env(heap, &mut task)?;
        } else {
            let mut stack = Stack::task(limit, loom_isa::LINK_SLOTS + frame, FIRST_SEGMENT)?;
            let (base, seg_end) = stack.bounds();
            // SAFETY: the first segment holds the linkage and the entry's
            // frame, whose first two registers are its parameters.
            let fp = unsafe {
                let fp = base.add(loom_isa::LINK_SLOTS);
                *base = 0; // no return address: the task ends here
                *base.add(1) = 0;
                *base.add(2) = fp as u64;
                *fp = result;
                *fp.add(1) = env;
                fp
            };
            task.stack = stack;
            task.pc = code;
            task.fp = fp;
            task.seg_end = seg_end;
        }
        let index = match self.free_tasks.pop() {
            Some(i) => {
                task.epoch = self.tasks[i].epoch;
                self.tasks[i] = task;
                i
            }
            None => {
                self.tasks.push(task);
                self.tasks.len() - 1
            }
        };
        self.groups[g].tasks.push(index);
        let h = handle(index, self.tasks[index].epoch);
        if failed {
            return Ok((h, Step::Next));
        }
        // The spawner waits at the front, and carries on when the new task
        // parks or ends.
        self.ready.push_front(self.current);
        Ok((h, Step::Start(index)))
    }

    fn release_env(heap: &mut Heap, task: &mut Task) -> Result<(), Trap> {
        if task.env_bytes > 0 {
            heap.free(task.env, task.env_bytes, 8)?;
            task.env_bytes = 0;
        }
        Ok(())
    }

    /// The running task ended: `trap` is what it unwound with, if it did.
    /// Records it in its group (a failure cancels the others), wakes
    /// whoever waits for it, and gives back what it held. Its stack is the
    /// caller's to drop once it has switched away.
    pub fn finish(&mut self, heap: &mut Heap, trap: Option<Trap>) -> Result<(), Trap> {
        let cur = self.current;
        let g = self.tasks[cur].group;
        self.tasks[cur].done = true;
        match trap {
            None => {}
            Some(Trap::Cancel) => self.tasks[cur].cancelled = true,
            Some(trap) => {
                self.tasks[cur].failed = true;
                if self.groups[g].failure.is_none() {
                    self.groups[g].failure = Some(trap);
                    self.cancel_all(g, cur);
                }
            }
        }
        let mut task = std::mem::replace(&mut self.tasks[cur], Task::empty());
        let result = Self::release_env(heap, &mut task);
        let waiters = std::mem::take(&mut task.waiters);
        task.live = true;
        // Whatever is in `task.stack` here is the placeholder: the real
        // stack is the interpreter's, and is dropped by its owner.
        self.tasks[cur] = task;
        for w in waiters {
            self.wake(w);
        }
        result
    }

    /// Takes a finished task's stack out of the interpreter's hands, after
    /// the switch away from it: dropped with the task's other storage.
    pub fn drop_stack(&mut self, t: usize) {
        self.tasks[t].stack = Stack::empty();
    }
}
