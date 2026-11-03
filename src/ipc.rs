//! Bounded mailbox IPC with backpressure and timed receive.
//!
//! Every mailbox has independent `not_empty` and `not_full` condition queues.
//! Senders block while 16 messages are queued; removing any message wakes one.
//! Receivers can wait forever, poll, or sleep until an absolute timer deadline.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
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
    QueueFull,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Timeout,
    /// The task whose reply was awaited exited before sending it. Waiting
    /// forever would hang the caller after the mailbox was destroyed.
    TargetDead,
}

struct Mailbox {
    owner: u64,
    queue: VecDeque<Message>,
    /// Arc'd so their addresses survive `mailboxes.push()` and Vec relocation;
    /// blocked tasks hold a clone, which keeps the queue alive for their
    /// lifecycle unlink no matter when the mailbox is dropped.
    not_empty: Arc<Condvar>,
    not_full: Arc<Condvar>,
}

impl Mailbox {
    fn new(owner: u64) -> Self {
        Self {
            owner,
            queue: VecDeque::new(),
            not_empty: Arc::new(Condvar::new()),
            not_full: Arc::new(Condvar::new()),
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

        let not_full = Arc::clone(&mailbox.not_full);
        guard = not_full.wait(guard);
    }
}

/// Sends without waiting for queue space. Full and dead mailboxes reject the
/// message, allowing drivers to drop input rather than block their IRQ loop.
pub fn try_send(target: u64, message: Message) -> Result<(), SendError> {
    let mut guard = IPC.lock();
    if !scheduler::task_alive(target) {
        return Err(SendError::TargetDead);
    }
    let index = mailbox_index(&mut guard, target);
    let mailbox = &mut guard.mailboxes[index];
    if mailbox.queue.len() >= MAILBOX_CAPACITY {
        return Err(SendError::QueueFull);
    }
    mailbox.queue.push_back(message);
    mailbox.not_empty.notify_one();
    Ok(())
}

/// Receives with a tick timeout. Zero polls; `INFINITE_TIMEOUT` waits forever.
pub fn receive_timeout(timeout_ticks: u64) -> Result<Message, ReceiveError> {
    receive_matching(None, timeout_ticks)
}

/// Receives only a message from `sender`, preserving unrelated queued messages.
///
/// Returns [`ReceiveError::TargetDead`] when the awaited task is gone. The
/// check runs before every wait and after every wakeup; `remove_task`
/// broadcasts on all mailbox queues, so a death cannot be missed between the
/// liveness check and the actual sleep.
pub fn receive_from(sender: u64) -> Result<Message, ReceiveError> {
    receive_matching(Some(sender), INFINITE_TIMEOUT)
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

        // A reply that was already queued above wins over the sender's death.
        // With no reply pending, a dead awaited sender must fail the operation
        // instead of sleeping on a mailbox notification it will never get.
        if let Some(expected) = sender {
            if !scheduler::task_alive(expected) {
                return Err(ReceiveError::TargetDead);
            }
        }

        if deadline.map(|deadline| timer::ticks() >= deadline) == Some(true) {
            return Err(ReceiveError::Timeout);
        }

        let not_empty = Arc::clone(&mailbox.not_empty);
        guard = if let Some(deadline) = deadline {
            not_empty.wait_until(guard, deadline)
        } else {
            not_empty.wait(guard)
        };
    }
}

/// Clears queued messages and wakes every waiter before freeing their queues.
/// A killed receiver may still be blocked (or stopped) on `not_empty`, so both
/// sides must detach from the mailbox before it is removed.
///
/// Every mailbox is notified, not only the dead owner's: a task blocked in
/// `receive_from(dead_id)` waits on *its own* mailbox, and this broadcast is
/// what lets it re-check `task_alive(dead_id)` and return instead of sleeping
/// forever. Spurious wakeups simply re-check the queue and block again.
pub fn remove_task(owner: u64) {
    let mut state = IPC.lock();
    for mailbox in &mut state.mailboxes {
        if mailbox.owner == owner {
            mailbox.queue.clear();
        }
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
