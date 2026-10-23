//! Condition variable: the missing companion of the blocking mutex.
//!
//! `wait(guard)` atomically releases the mutex and blocks the current task;
//! `notify_one()` / `notify_all()` wake waiters. Because the mutex is
//! released *before* blocking, a task can sleep on a condition without
//! holding the lock a notifier needs — the deadlock the raw blocking mutex
//! could not avoid on its own.
//!
//! Usage discipline (same as std::sync::Condvar):
//! - check the condition and call `wait` while holding the mutex;
//! - call `notify_*` while holding the mutex;
//! - always re-check the condition after `wait` returns (spurious wakeups
//!   are possible in general; our wake_task is exact, but the pattern costs
//!   nothing and stays correct).
//!
//! The waiter list is plain data protected by the caller's mutex plus the
//! IRQ mask (single-CPU kernel) — no blocking lock of its own.

use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::{
    interrupts,
    mutex::{Mutex, MutexGuard},
    scheduler,
};

pub struct Condvar {
    /// Task IDs blocked in `wait`, in FIFO order. Only touched while the
    /// associated mutex is held and IRQs are masked.
    waiters: UnsafeCell<Vec<u64>>,
}

unsafe impl Sync for Condvar {}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            waiters: UnsafeCell::new(Vec::new()),
        }
    }

    /// Releases `guard` and blocks until another task calls `notify_one` or
    /// `notify_all` on this condvar, then re-acquires the mutex and returns
    /// the new guard. Call only while holding the mutex.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let mutex = guard.mutex();

        let saved_daif = interrupts::save_and_disable_irq();
        // Enqueue + release + block are atomic here (IRQs masked, single
        // CPU), so a notify that fires after our condition check cannot be
        // lost: it either sees us in the queue or hasn't run yet.
        unsafe { (*self.waiters.get()).push(scheduler::current_task_id()) };
        drop(guard);
        scheduler::block_current();
        interrupts::restore_irq(saved_daif);

        mutex.lock()
    }

    /// Wakes the longest-waiting task, if any. Call while holding the mutex.
    pub fn notify_one(&self) {
        let saved_daif = interrupts::save_and_disable_irq();
        let waiters = unsafe { &mut *self.waiters.get() };
        if !waiters.is_empty() {
            let id = waiters.remove(0);
            scheduler::wake_task(id);
        }
        interrupts::restore_irq(saved_daif);
    }

    /// Wakes every waiting task. Call while holding the mutex.
    pub fn notify_all(&self) {
        let saved_daif = interrupts::save_and_disable_irq();
        let waiters = unsafe { &mut *self.waiters.get() };
        for id in waiters.drain(..) {
            scheduler::wake_task(id);
        }
        interrupts::restore_irq(saved_daif);
    }
}
