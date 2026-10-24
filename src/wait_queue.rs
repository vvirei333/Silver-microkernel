//! FIFO queue of blocked task IDs.
//!
//! Every operation runs with IRQs masked on the current single-CPU kernel.
//! A blocked task remembers its queue so lifecycle cleanup can remove it.

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
