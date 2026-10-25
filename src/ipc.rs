//! Bounded mailbox IPC with backpressure and timed receive.
//!
//! Every mailbox has independent `not_empty` and `not_full` condition queues.
//! Senders block while 16 messages are queued; removing any message wakes one.
//! Receivers can wait forever, poll, or sleep until an absolute timer deadline.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::{condvar::Condvar, mutex::Mutex, scheduler, timer};

pub const PAYLOAD_SIZE: usize = 32;
pub const MAILBOX_CAPACITY: usize = 16;
pub const INFINITE_TIMEOUT: u64 = u64::MAX;

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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SendError {
    TargetDead,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Timeout,
}

struct Mailbox {
    owner: u64,
    queue: VecDeque<Message>,
    /// Boxed so their addresses survive `mailboxes.push()` and Vec relocation.
    not_empty: Box<Condvar>,
    not_full: Box<Condvar>,
}

impl Mailbox {
    fn new(owner: u64) -> Self {
        Self {
            owner,
            queue: VecDeque::new(),
            not_empty: Box::new(Condvar::new()),
            not_full: Box::new(Condvar::new()),
        }
    }
}

struct IpcState {
    mailboxes: Vec<Mailbox>,
}

static IPC: Mutex<IpcState> = Mutex::new(IpcState {
    mailboxes: Vec::new(),
});

/// Blocks while the target queue is full. Mailbox destruction wakes all such
/// senders; the liveness check then prevents recreating a dead task's mailbox.
pub fn send(target: u64, message: Message) -> Result<(), SendError> {
    let mut guard = IPC.lock();
    loop {
        if !scheduler::task_alive(target) {
            return Err(SendError::TargetDead);
        }
        let index = mailbox_index(&mut guard, target);
        let mailbox = &mut guard.mailboxes[index];
        if mailbox.queue.len() < MAILBOX_CAPACITY {
            mailbox.queue.push_back(message);
            mailbox.not_empty.notify_one();
            return Ok(());
        }

        let not_full = &*mailbox.not_full as *const Condvar;
        guard = unsafe { (*not_full).wait(guard) };
    }
}

/// Receives with a tick timeout. Zero polls; `INFINITE_TIMEOUT` waits forever.
pub fn receive_timeout(timeout_ticks: u64) -> Result<Message, ReceiveError> {
    receive_matching(None, timeout_ticks)
}

/// Receives only a message from `sender`, preserving unrelated queued messages.
pub fn receive_from(sender: u64) -> Message {
    match receive_matching(Some(sender), INFINITE_TIMEOUT) {
        Ok(message) => message,
        Err(_) => unreachable!(),
    }
}

fn receive_matching(sender: Option<u64>, timeout_ticks: u64) -> Result<Message, ReceiveError> {
    let id = scheduler::current_task_id();
    let deadline = if timeout_ticks == INFINITE_TIMEOUT {
        None
    } else {
        Some(timer::ticks().saturating_add(timeout_ticks))
    };
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
            mailbox.not_full.notify_one();
            return Ok(message);
        }

        if deadline.map(|deadline| timer::ticks() >= deadline) == Some(true) {
            return Err(ReceiveError::Timeout);
        }

        let not_empty = &*mailbox.not_empty as *const Condvar;
        guard = if let Some(deadline) = deadline {
            unsafe { (*not_empty).wait_until(guard, deadline) }
        } else {
            unsafe { (*not_empty).wait(guard) }
        };
    }
}

/// Clears queued messages and wakes every waiter before freeing their queues.
/// A killed receiver may still be blocked (or stopped) on `not_empty`, so both
/// sides must detach from the mailbox before it is removed.
pub fn remove_task(owner: u64) {
    let mut state = IPC.lock();
    for mailbox in state
        .mailboxes
        .iter_mut()
        .filter(|mailbox| mailbox.owner == owner)
    {
        mailbox.queue.clear();
        mailbox.not_empty.notify_all();
        mailbox.not_full.notify_all();
    }
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
