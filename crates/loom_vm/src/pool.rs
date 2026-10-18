//! Several workers (design/vm.md §11).
//!
//! The program's threads are tasks, which are data, so any worker can run
//! any of them. The workers share one [`Sched`](crate::sched::Sched) and one
//! [`Ctx`](crate::natives::Ctx), both behind the pool's one lock: a task
//! operation or a native needing shared services runs with it held. The
//! dispatch loops, scalar leaves, atomic operations and worker-local heap
//! operations run without it. Sleeping releases it before blocking the
//! worker. A native that prints is serialized with the others.
//!
//! A worker that has nothing to run first spins on the count of ready tasks,
//! which it reads without the lock (an idle worker asking for the lock would
//! make the one that has it pay on every release), and takes the lock only
//! when a task is queued and the lock is free; after a while it waits on the
//! condition, and a worker that was woken spins again before it sleeps. The last one
//! to go idle, with no task ready and the program's own task waiting,
//! declares a deadlock. The first fault, or the program's own task ending,
//! stops them all.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, TryLockError};

use crate::Fault;

/// What the lock guards, besides the scheduler and the context.
#[derive(Debug, Default)]
pub(crate) struct PoolState {
    /// Workers with no task: the spinning and the sleeping.
    pub idle: usize,
    /// Those of them that wait on the condition.
    pub sleepers: usize,
    /// Those of the sleepers that have been notified and have not yet
    /// run (waking takes a while, and notifying again would only cost the
    /// notifier a system call).
    pub notified: usize,
    /// How the program ended: the first to say wins.
    pub outcome: Option<Result<Vec<u64>, Fault>>,
}

/// A value on a cache line of its own.
#[derive(Debug)]
#[repr(align(64))]
struct Alone<T>(T);

#[derive(Debug)]
pub(crate) struct Pool {
    /// On a line of its own: every lock and unlock writes it, and the flags
    /// below are read by workers that are running.
    state: Alone<Mutex<PoolState>>,
    cv: Condvar,
    pub workers: usize,
    /// The program is over (it ended, or something failed): every worker
    /// stops at its next check, which is at the next tick or wait.
    over: AtomicBool,
    /// How many tasks are ready (the scheduler's count), which an idle
    /// worker watches while it spins, without the lock.
    queued: Arc<AtomicUsize>,
    /// Whether any task has been cancelled yet (the scheduler's flag).
    cancel_seen: Arc<AtomicBool>,
}

impl Pool {
    pub fn new(workers: usize, cancel_seen: Arc<AtomicBool>, queued: Arc<AtomicUsize>) -> Pool {
        Pool {
            state: Alone(Mutex::new(PoolState::default())),
            cv: Condvar::new(),
            workers,
            over: AtomicBool::new(false),
            queued,
            cancel_seen,
        }
    }

    /// Takes the lock, and with it the scheduler and the context.
    pub fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits for something to change.
    pub fn wait<'a>(&'a self, guard: MutexGuard<'a, PoolState>) -> MutexGuard<'a, PoolState> {
        self.cv.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `ready` tasks are ready: the workers that spin will see them, and
    /// a sleeper is woken if the spinners and the sleepers already woken are
    /// fewer than the tasks (a notify with nobody asleep still costs a
    /// system call).
    pub fn wake_one(&self, state: &mut PoolState, ready: usize) {
        let coming = state.idle - state.sleepers + state.notified;
        if state.sleepers > state.notified && ready > coming {
            state.notified += 1;
            self.cv.notify_one();
        }
    }

    /// An idle worker's wait for a task, for at most `spins` turns. It
    /// watches the count of ready tasks and leaves the lock alone (a worker
    /// asking for it makes the one that holds it pay when it lets go),
    /// taking it only when there is something to take and it is free.
    /// Gives the lock, held, and the turns left (none if it ran out, or the
    /// program is over).
    pub fn spin_for_task(&self, mut spins: u32) -> (MutexGuard<'_, PoolState>, u32) {
        while spins > 0 && !self.is_over() {
            if self.queued.load(Ordering::Relaxed) > 0 {
                match self.state.0.try_lock() {
                    Ok(held) => return (held, spins),
                    Err(TryLockError::Poisoned(e)) => return (e.into_inner(), spins),
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            std::hint::spin_loop();
            spins -= 1;
        }
        (self.lock(), spins)
    }

    /// Whether a cancellation point has to look at the task: false while
    /// nothing has been cancelled, and then it needs no lock.
    pub fn cancel_seen(&self) -> bool {
        self.cancel_seen.load(Ordering::Acquire)
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
