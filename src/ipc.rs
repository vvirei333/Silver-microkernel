//! Mailbox-based IPC: fixed-size messages between tasks.
//!
//! `receive()` blocks the caller until a message arrives. IPC state is
//! protected by IRQ masking (the same discipline as TaskManager), NOT by the
//! blocking `mutex::Mutex`: a receiver registers itself and blocks as one
//! atomic step, and the sender must be able to push + wake without acquiring
//! any lock the blocked receiver holds.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::{interrupts, scheduler};

pub const PAYLOAD_SIZE: usize = 32;

#[derive(Copy, Clone)]
pub struct Message {
    pub sender: u64,
    pub payload: [u8; PAYLOAD_SIZE],
}

impl Message {
    pub fn new(sender: u64, bytes: &[u8]) -> Self {
        let mut payload = [0_u8; PAYLOAD_SIZE];
        let len = bytes.len().min(PAYLOAD_SIZE);
        payload[..len].copy_from_slice(&bytes[..len]);
        Self { sender, payload }
    }

    /// Payload viewed as text, truncated at the first NUL byte.
    pub fn text(&self) -> &[u8] {
        let end = self
            .payload
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(PAYLOAD_SIZE);
        &self.payload[..end]
    }
}

struct Mailbox {
    owner: u64,
    queue: VecDeque<Message>,
}

struct IpcState {
    mailboxes: Vec<Mailbox>,
    /// IDs of tasks currently blocked inside `receive()`, waiting for mail.
    receivers: Vec<u64>,
}

struct SharedIpc(UnsafeCell<IpcState>);

// Single-CPU kernel: every mutation happens with IRQs masked.
unsafe impl Sync for SharedIpc {}

static IPC: SharedIpc = SharedIpc(UnsafeCell::new(IpcState {
    mailboxes: Vec::new(),
    receivers: Vec::new(),
}));

fn ipc_state() -> &'static mut IpcState {
    unsafe { &mut *IPC.0.get() }
}

fn mailbox_for(state: &mut IpcState, owner: u64) -> &mut Mailbox {
    if let Some(index) = state.mailboxes.iter().position(|m| m.owner == owner) {
        return &mut state.mailboxes[index];
    }
    state.mailboxes.push(Mailbox {
        owner,
        queue: VecDeque::new(),
    });
    state.mailboxes.last_mut().unwrap()
}

/// Delivers a message to the target's mailbox, waking it if it is
/// blocked inside `receive()`.
pub fn send(target: u64, message: Message) {
    let saved_daif = interrupts::save_and_disable_irq();
    let state = ipc_state();

    mailbox_for(state, target).queue.push_back(message);

    if let Some(index) = state.receivers.iter().position(|&id| id == target) {
        state.receivers.remove(index);
        scheduler::wake_task(target);
    }

    interrupts::restore_irq(saved_daif);
}

/// Blocks until a message for the current task arrives and returns it.
pub fn receive() -> Message {
    let id = scheduler::current_task_id();

    loop {
        let saved_daif = interrupts::save_and_disable_irq();
        let state = ipc_state();

        if let Some(message) = mailbox_for(state, id).queue.pop_front() {
            interrupts::restore_irq(saved_daif);
            return message;
        }

        if !state.receivers.contains(&id) {
            state.receivers.push(id);
        }
        // Registration and block_current() are atomic here (IRQs masked), and
        // the sender consults the same receiver list — no wakeup can be lost.
        scheduler::block_current();
        interrupts::restore_irq(saved_daif);
        // Woken by a send: loop back and pick the message up.
    }
}
