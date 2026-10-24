//! Bounded channel: the textbook composition of two semaphores and a mutex.
//!
//! - `free_slots` starts at `capacity`: `send` waits on it, which is exactly
//!   the backpressure — a sender blocks when the queue is full;
//! - `items` starts at 0: `receive` waits on it, blocking while empty;
//! - `queue` hands the actual values over, protected by a short mutex
//!   critical section (never held while blocking).
//!
//! Both primitives come from semaphore.rs; the channel holds no lock while
//! sleeping, so producers and consumers can never deadlock each other.

use alloc::collections::VecDeque;

use crate::{mutex::Mutex, semaphore::Semaphore};

pub struct Channel<T> {
    free_slots: Semaphore,
    items: Semaphore,
    queue: Mutex<VecDeque<T>>,
}

unsafe impl<T> Sync for Channel<T> {}

impl<T> Channel<T> {
    pub const fn new(capacity: usize) -> Self {
        Self {
            free_slots: Semaphore::new(capacity as u64),
            items: Semaphore::new(0),
            queue: Mutex::new(VecDeque::new()),
        }
    }

    /// Enqueues an item, blocking while the queue is full (backpressure).
    pub fn send(&self, item: T) {
        self.free_slots.wait();
        self.queue.lock().push_back(item);
        self.items.signal();
    }

    /// Dequeues the oldest item, blocking while the queue is empty.
    pub fn receive(&self) -> T {
        self.items.wait();
        let item = self
            .queue
            .lock()
            .pop_front()
            .expect("semaphore permit without a queued item");
        self.free_slots.signal();
        item
    }
}
