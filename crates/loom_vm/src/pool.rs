//! Several workers (design/vm.md §11).
//!
//! The program's threads are tasks, which are data, so any worker can run
//! any of them. The workers share one [`Sched`](crate::sched::Sched) and one
//! [`Ctx`](crate::natives::Ctx), both behind the pool's one lock: a task
//! operation or a native runs with it held, and the dispatch loops run
//! without it, so compute-bound threads run side by side. (A native that
//! allocates, or prints, is serialized with the others: finer locks and
//! per-worker heaps are for when that's what's slow.)
//!
//! A worker that has nothing to run waits on the condition. The last one
//! to go idle, with no task ready and the program's own task waiting,
//! declares a deadlock. The first fault, or the program's own task ending,
//! stops them all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::Fault;

/// What the lock guards, besides the scheduler and the context.
#[derive(Debug, Default)]
pub(crate) struct PoolState {
    /// Workers waiting for a task to be ready.
    pub idle: usize,
    /// How the program ended: the first to say wins.
    pub outcome: Option<Result<Vec<u64>, Fault>>,
}

#[derive(Debug)]
pub(crate) struct Pool {
    state: Mutex<PoolState>,
    cv: Condvar,
    pub workers: usize,
    /// The program is over (it ended, or something failed): every worker
    /// stops at its next check, which is at the next tick or wait.
    over: AtomicBool,
}

impl Pool {
    pub fn new(workers: usize) -> Pool {
        Pool { state: Mutex::new(PoolState::default()), cv: Condvar::new(), workers, over: AtomicBool::new(false) }
    }

    /// Takes the lock, and with it the scheduler and the context.
    pub fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits for something to change.
    pub fn wait<'a>(&'a self, guard: MutexGuard<'a, PoolState>) -> MutexGuard<'a, PoolState> {
        self.cv.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn wake_one(&self) {
        self.cv.notify_one();
    }

    pub fn is_over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }

    /// Records how the program ended, if nothing has said yet, and stops
    /// the workers.
    pub fn finish(&self, state: &mut PoolState, outcome: Result<Vec<u64>, Fault>) {
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
        }
        self.over.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }
}
