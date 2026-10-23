//! Blocking mutex: a contending task sleeps in a FIFO wait queue instead of
//! spinning. Single-CPU design — all state changes happen with IRQs disabled,
//! which is our critical-section mechanism until SMP arrives.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use crate::{interrupts, scheduler};

pub struct Mutex<T> {
    value: UnsafeCell<T>,
    state: UnsafeCell<MutexState>,
}

struct MutexState {
    locked: bool,
    /// Task IDs currently blocked on this mutex, in FIFO order.
    waiters: Vec<u64>,
}

unsafe impl<T> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            state: UnsafeCell::new(MutexState {
                locked: false,
                waiters: Vec::new(),
            }),
        }
    }

    /// Acquires the mutex, blocking the current task until it is available.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        loop {
            let saved_daif = interrupts::save_and_disable_irq();
            let state = unsafe { &mut *self.state.get() };

            if !state.locked {
                state.locked = true;
                interrupts::restore_irq(saved_daif);
                return MutexGuard { mutex: self };
            }

            // Contended: enqueue ourselves once, then block until woken.
            let id = scheduler::current_task_id();
            if !state.waiters.contains(&id) {
                state.waiters.push(id);
            }
            scheduler::block_current();
            interrupts::restore_irq(saved_daif);
            // Woken by an unlock — loop back and try to acquire again.
        }
    }

    fn unlock(&self) {
        let saved_daif = interrupts::save_and_disable_irq();
        let state = unsafe { &mut *self.state.get() };
        debug_assert!(state.locked, "unlocking a mutex that is not locked");
        state.locked = false;

        if !state.waiters.is_empty() {
            let id = state.waiters.remove(0);
            scheduler::wake_task(id);
        }
        interrupts::restore_irq(saved_daif);
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}
