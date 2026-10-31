//! FIFO queue of blocked task IDs.
//!
//! Every operation runs with IRQs masked on the current single-CPU kernel.
//! A blocked task remembers its queue so lifecycle cleanup can remove it.
//!
//! Lifecycle invariant (see `scheduler::detach_from_wait_queue`): a task that
//! leaves the blocked state without being popped by a notify — timeout expiry
//! or death — is removed from its queue before it can be observed as Ready or
//! Dead, so `pop` never yields a stale id and a queue never outlives a task
//! that is still registered in it.

use alloc::vec::Vec;
use core::cell::UnsafeCell;

pub struct WaitQueue {
    waiters: UnsafeCell<Vec<u64>>,
}

unsafe impl Sync for WaitQueue {}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            waiters: UnsafeCell::new(Vec::new()),
        }
    }

    pub fn enqueue(&self, id: u64) {
        let waiters = unsafe { &mut *self.waiters.get() };
        if !waiters.contains(&id) {
            waiters.push(id);
        }
    }

    pub fn pop(&self) -> Option<u64> {
        let waiters = unsafe { &mut *self.waiters.get() };
        if waiters.is_empty() {
            None
        } else {
            Some(waiters.remove(0))
        }
    }

    pub fn remove(&self, id: u64) {
        unsafe { (*self.waiters.get()).retain(|waiter| *waiter != id) };
    }
}

#[cfg(test)]
mod tests {
    use super::WaitQueue;

    // The queue methods take &self and mutate through UnsafeCell: one shared
    // queue per test, exercised sequentially (the same discipline the kernel
    // guarantees with its IRQ mask).
    fn queue() -> WaitQueue {
        WaitQueue::new()
    }

    #[test]
    fn pop_is_fifo_and_empty_yields_none() {
        let q = queue();
        assert_eq!(q.pop(), None);
        q.enqueue(3);
        q.enqueue(1);
        q.enqueue(2);
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn enqueue_deduplicates() {
        let q = queue();
        q.enqueue(7);
        q.enqueue(7);
        assert_eq!(q.pop(), Some(7));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn remove_deletes_only_the_target() {
        let q = queue();
        q.enqueue(1);
        q.enqueue(2);
        q.enqueue(3);
        q.remove(2);
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn remove_of_absent_id_is_noop() {
        let q = queue();
        q.enqueue(5);
        q.remove(9); // detached twice or never registered: must not panic
        assert_eq!(q.pop(), Some(5));
        q.remove(5);
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn timeout_detach_leaves_no_stale_entry() {
        // Regression model for the expiry path: the task registers, the
        // deadline expires and `detach_from_wait_queue` removes it — a later
        // notify must NOT pop the already-Ready task (lost wakeup).
        let q = queue();
        q.enqueue(42);
        q.remove(42); // expiry detach
        assert_eq!(q.pop(), None); // notify_one would find nothing to burn
        // And a fresh block by the same task works again (queue reuse).
        q.enqueue(42);
        assert_eq!(q.pop(), Some(42));
    }

    #[test]
    fn queue_is_reusable_after_full_drain() {
        // notify_all drains the queue; the next wait cycle must work.
        let q = queue();
        for round in 0..3 {
            let base = 100 + round;
            q.enqueue(base);
            q.enqueue(base + 1);
            assert_eq!(q.pop(), Some(base));
            assert_eq!(q.pop(), Some(base + 1));
            assert_eq!(q.pop(), None);
        }
    }
}
