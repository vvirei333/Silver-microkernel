//! Counting semaphore built on the blocking mutex + condition variable.
//!
//! `wait()` acquires a permit, blocking when none are free; `signal()`
//! returns a permit and wakes the longest-waiting task. The semaphore
//! holds no lock while blocking, so it composes safely: this is what makes
//! the bounded channel in channel.rs possible — senders sleep in
//! `free_slots.wait()` without holding the queue lock.

use crate::{condvar::Condvar, mutex::Mutex};

pub struct Semaphore {
    permits: Mutex<u64>,
    condvar: Condvar,
}

unsafe impl Sync for Semaphore {}

impl Semaphore {
    pub const fn new(permits: u64) -> Self {
        Self {
            permits: Mutex::new(permits),
            condvar: Condvar::new(),
        }
    }

    /// Acquires a permit, blocking until one is available.
    pub fn wait(&self) {
        let mut guard = self.permits.lock();
        while *guard == 0 {
            guard = self.condvar.wait(guard);
        }
        *guard -= 1;
    }

    /// Returns a permit and wakes one waiter, if any.
    pub fn signal(&self) {
        let mut guard = self.permits.lock();
        *guard += 1;
        self.condvar.notify_one();
    }
}
