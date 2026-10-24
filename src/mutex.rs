//! Blocking mutex with FIFO waiters and explicit ownership tracking.
//! Single-CPU design — all state changes happen with IRQs disabled.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use crate::{interrupts, scheduler, wait_queue::WaitQueue};

pub struct Mutex<T> {
    value: UnsafeCell<T>,
    state: UnsafeCell<MutexState>,
}

pub(crate) struct MutexState {
    owner: Option<u64>,
    waiters: WaitQueue,
}

unsafe impl<T> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            state: UnsafeCell::new(MutexState {
                owner: None,
                waiters: WaitQueue::new(),
            }),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.raw_lock();
        MutexGuard { mutex: self }
    }

    /// Acquires without a guard; ownership is registered with the current
    /// task so an exit cannot leave this mutex locked forever.
    pub fn raw_lock(&self) {
        loop {
            let saved_daif = interrupts::save_and_disable_irq();
            let state = unsafe { &mut *self.state.get() };
            let id = scheduler::current_task_id();

            if state.owner.is_none() {
                state.owner = Some(id);
                scheduler::register_mutex(self.state.get());
                interrupts::restore_irq(saved_daif);
                return;
            }

            scheduler::block_current_on(&state.waiters);
            interrupts::restore_irq(saved_daif);
        }
    }

    pub fn raw_unlock(&self) {
        let saved_daif = interrupts::save_and_disable_irq();
        let state = unsafe { &mut *self.state.get() };
        let id = scheduler::current_task_id();
        debug_assert_eq!(state.owner, Some(id), "unlock by a non-owner");
        state.owner = None;
        scheduler::unregister_mutex(self.state.get());

        if let Some(waiter) = state.waiters.pop() {
            scheduler::wake_task(waiter);
        }
        interrupts::restore_irq(saved_daif);
    }
}

/// Releases a mutex during task reaping. The caller removes the pointer from
/// the dying task itself; only the next waiter needs to be returned here.
pub(crate) unsafe fn release_owner(state: *mut MutexState, owner: u64) -> Option<u64> {
    let state = unsafe { &mut *state };
    if state.owner != Some(owner) {
        return None;
    }
    state.owner = None;
    state.waiters.pop()
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<'a, T> MutexGuard<'a, T> {
    pub fn mutex(&self) -> &'a Mutex<T> {
        self.mutex
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.raw_unlock();
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
