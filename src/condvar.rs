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

use crate::{interrupts, mutex::MutexGuard, scheduler, timer, wait_queue::WaitQueue};

pub struct Condvar {
    waiters: WaitQueue,
}

unsafe impl Sync for Condvar {}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            waiters: WaitQueue::new(),
        }
    }

    /// Releases `guard` and blocks until notified, then re-acquires the mutex.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let mutex = guard.mutex();
        let saved_daif = interrupts::save_and_disable_irq();
        // Queue registration, unlock and sleep are atomic with IRQs masked.
        // The scheduler also remembers the queue for exit cleanup.
        scheduler::register_waiter(&self.waiters);
        drop(guard);
        scheduler::block_current_on(&self.waiters);
        interrupts::restore_irq(saved_daif);

        mutex.lock()
    }

    /// Timed variant using an absolute timer deadline. The caller must re-check
    /// its condition: notification and timeout can race, and the mutex is only
    /// re-acquired after the scheduler resumes this task.
    pub fn wait_until<'a, T>(&self, guard: MutexGuard<'a, T>, deadline: u64) -> MutexGuard<'a, T> {
        if timer::ticks() >= deadline {
            return guard;
        }
        let mutex = guard.mutex();
        let saved_daif = interrupts::save_and_disable_irq();
        scheduler::register_waiter(&self.waiters);
        drop(guard);
        scheduler::block_current_on_until(&self.waiters, deadline);
        interrupts::restore_irq(saved_daif);

        mutex.lock()
    }

    pub fn notify_one(&self) {
        let saved_daif = interrupts::save_and_disable_irq();
        if let Some(id) = self.waiters.pop() {
            scheduler::wake_task(id);
        }
        interrupts::restore_irq(saved_daif);
    }

    pub fn notify_all(&self) {
        let saved_daif = interrupts::save_and_disable_irq();
        while let Some(id) = self.waiters.pop() {
            scheduler::wake_task(id);
        }
        interrupts::restore_irq(saved_daif);
    }
}
