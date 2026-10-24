//! Mailbox-based IPC: fixed-size messages between tasks.
//!
//! `receive()` blocks the caller until a message arrives. IPC state lives
//! behind the blocking kernel mutex, and every mailbox owns a Condvar — a
//! receiver sleeps on *its own* mailbox's wait queue, and `send()` wakes
//! exactly the waiters of the addressed mailbox. The old global receiver
//! list (where any send could spuriously wake an unrelated task) is gone.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::{condvar::Condvar, mutex::Mutex, scheduler};

pub const PAYLOAD_SIZE: usize = 32;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct Message {
    pub sender: u64,
    pub payload: [u8; PAYLOAD_SIZE],
}

impl Message {
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
    /// Boxed so the condvar's address survives `mailboxes.push()`:
    /// a Vec growth relocates every Mailbox, and a raw pointer taken
    /// before blocking in `receive()` would dangle straight into the
    /// allocator's free list.
    condvar: Box<Condvar>,
}

impl Mailbox {
    fn new(owner: u64) -> Self {
        Self {
            owner,
            queue: VecDeque::new(),
            condvar: Box::new(Condvar::new()),
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

/// Blocks until any message for the current task arrives.
pub fn receive() -> Message {
    receive_matching(None)
}

/// Receives only a message from `sender`, preserving unrelated queued
/// messages. Nameserver RPC uses this so a concurrent PING cannot be mistaken
/// for a lookup response and discarded.
pub fn receive_from(sender: u64) -> Message {
    receive_matching(Some(sender))
}

fn receive_matching(sender: Option<u64>) -> Message {
    let id = scheduler::current_task_id();
    let mut guard = IPC.lock();

    loop {
        let index = mailbox_index(&mut guard, id);
        let mailbox = &mut guard.mailboxes[index];
        let position = mailbox.queue.iter().position(|message| {
            sender
                .map(|expected| message.sender == expected)
                .unwrap_or(true)
        });
        if let Some(message) = position.and_then(|index| mailbox.queue.remove(index)) {
            return message;
        }

        // Sleep on this mailbox's condvar: unrelated messages may wake us,
        // but remain queued until the matching reply or normal receive arrives.
        let condvar = &*guard.mailboxes[index].condvar as *const Condvar;
        guard = unsafe { (*condvar).wait(guard) };
    }
}

/// Drops the exited task's queued messages and mailbox. Only the owner can
/// block on a mailbox, so a running task cannot be waiting in its own queue.
pub fn remove_task(owner: u64) {
    let mut state = IPC.lock();
    state.mailboxes.retain(|mailbox| mailbox.owner != owner);
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
