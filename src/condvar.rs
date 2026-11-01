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

use alloc::sync::Arc;
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
    ///
    /// Shared-queue flavor: the caller holds this condvar in an `Arc` (mailbox
    /// condvars), so the scheduler's wait link keeps the queue alive for as
    /// long as the blocked task may still need to unlink from it.
    pub fn wait<'a, T>(self: &Arc<Self>, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        crate::console::write_str("C1\r\n");
        crate::smp::probe(1);
        let mutex = guard.mutex();
        let saved_daif = interrupts::save_and_disable_irq();
        crate::smp::probe(2);
        // Queue registration, unlock and sleep are atomic with IRQs masked.
        drop(guard);
        crate::console::write_str("C2\r\n");
        crate::smp::probe(3);
        scheduler::block_current_on(&self.waiters, scheduler::WaitLink::Shared(Arc::clone(self)));
        crate::smp::probe(7);
        crate::console::write_str("C3\r\n");
        interrupts::restore_irq(saved_daif);

        mutex.lock()
    }

    /// Timed variant using an absolute timer deadline. The caller must re-check
    /// its condition: notification and timeout can race, and the mutex is only
    /// re-acquired after the scheduler resumes this task.
    pub fn wait_until<'a, T>(
        self: &Arc<Self>,
        guard: MutexGuard<'a, T>,
        deadline: u64,
    ) -> MutexGuard<'a, T> {
        if timer::ticks() >= deadline {
            return guard;
        }
        let mutex = guard.mutex();
        let saved_daif = interrupts::save_and_disable_irq();
        drop(guard);
        scheduler::block_current_on_until(
            &self.waiters,
            scheduler::WaitLink::Shared(Arc::clone(self)),
            deadline,
        );
        interrupts::restore_irq(saved_daif);

        mutex.lock()
    }

    /// `'static`-queue flavor for condvars embedded in kernel objects that are
    /// never dropped (semaphores inside statics): the scheduler stores an
    /// [`scheduler::WaitLink::Immortal`] link.
    pub fn wait_immortal<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let mutex = guard.mutex();
        let saved_daif = interrupts::save_and_disable_irq();
        drop(guard);
        scheduler::block_current_on(
            &self.waiters,
            scheduler::WaitLink::Immortal(&self.waiters),
        );
        interrupts::restore_irq(saved_daif);

        mutex.lock()
    }

    /// Removes one task id from the wait queue without waking it. Used by the
    /// scheduler's lifecycle cleanup; safe on an orphaned queue because the
    /// waiter holds the `Arc`.
    pub fn remove_waiter(&self, id: u64) {
        self.waiters.remove(id);
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
