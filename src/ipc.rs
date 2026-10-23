//! Mailbox-based IPC: fixed-size messages between tasks.
//!
//! `receive()` blocks the caller until a message arrives. IPC state lives
//! behind the blocking kernel mutex, and every mailbox owns a Condvar — a
//! receiver sleeps on *its own* mailbox's wait queue, and `send()` wakes
//! exactly the waiters of the addressed mailbox. The old global receiver
//! list (where any send could spuriously wake an unrelated task) is gone.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::{condvar::Condvar, mutex::Mutex, scheduler};

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
    /// Wait queue of tasks blocked in `receive()` on this mailbox.
    condvar: Condvar,
}

impl Mailbox {
    fn new(owner: u64) -> Self {
        Self {
            owner,
            queue: VecDeque::new(),
            condvar: Condvar::new(),
        }
    }
}

struct IpcState {
    mailboxes: Vec<Mailbox>,
}

static IPC: Mutex<IpcState> = Mutex::new(IpcState {
    mailboxes: Vec::new(),
});

/// Delivers a message to the target's mailbox and wakes at most one task
/// waiting on that mailbox.
pub fn send(target: u64, message: Message) {
    let mut state = IPC.lock();
    let index = mailbox_index(&mut state, target);
    let mailbox = &mut state.mailboxes[index];
    mailbox.queue.push_back(message);
    mailbox.condvar.notify_one();
}

/// Blocks until a message for the current task arrives and returns it.
pub fn receive() -> Message {
    let id = scheduler::current_task_id();
    let mut guard = IPC.lock();

    loop {
        let index = mailbox_index(&mut guard, id);
        if let Some(message) = guard.mailboxes[index].queue.pop_front() {
            return message;
        }

        // Sleep on this mailbox's condvar: releases the lock, blocks, and
        // re-acquires it. The condvar pointer is re-taken on every iteration
        // because the mailboxes Vec may relocate while we sleep.
        let condvar = &guard.mailboxes[index].condvar as *const Condvar;
        guard = unsafe { (*condvar).wait(guard) };
    }
}

fn mailbox_index(state: &mut IpcState, owner: u64) -> usize {
    match state
        .mailboxes
        .iter()
        .position(|mailbox| mailbox.owner == owner)
    {
        Some(index) => index,
        None => {
            state.mailboxes.push(Mailbox::new(owner));
            state.mailboxes.len() - 1
        }
    }
}
