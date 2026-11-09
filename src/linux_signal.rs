//! Linux signal bookkeeping.
//!
//! This module records per-task dispositions, blocked masks and pending
//! signals, and applies the **default actions** that matter for a console OS:
//! `SIGKILL` always terminates, and a default-terminating signal delivered by
//! `kill` terminates the target. `SIGCHLD` is raised in the parent when a
//! child exits and is ignored by default, exactly like Linux.
//!
//! User-installed handlers are stored but **not invoked yet**: building an
//! AArch64 `rt_sigframe` and returning through `rt_sigreturn` is a separate
//! step. Programs that only wait for children (the common shell flow) behave
//! correctly; a handler that expects to run stays pending.

use alloc::vec::Vec;

use crate::mutex::Mutex;

pub const SIGCHLD: u32 = 17;
pub const SIGKILL: u32 = 9;

/// One `struct sigaction` (generic AArch64 layout: handler, flags, restorer,
/// mask).
#[derive(Copy, Clone, Default)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
}

struct Table {
    task: u64,
    actions: Vec<Option<Action>>,
    blocked: u64,
    pending: u64,
}

static TABLES: Mutex<Vec<Table>> = Mutex::new(Vec::new());

/// `SIG_DFL` and `SIG_IGN` sentinels, as Linux defines them.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

fn with_table<R>(task: u64, action: impl FnOnce(&mut Table) -> R) -> R {
    let mut tables = TABLES.lock();
    let index = match tables.iter().position(|table| table.task == task) {
        Some(index) => index,
        None => {
            let mut actions = Vec::with_capacity(65);
            actions.resize(65, None);
            tables.push(Table {
                task,
                actions,
                blocked: 0,
                pending: 0,
            });
            tables.len() - 1
        }
    };
    action(&mut tables[index])
}

fn bit(signal: u32) -> Option<u64> {
    if (1..=64).contains(&signal) {
        Some(1 << (signal - 1))
    } else {
        None
    }
}

pub fn action(task: u64, signal: u32) -> Option<Action> {
    bit(signal)?;
    with_table(task, |table| table.actions[signal as usize])
}

pub fn set_action(task: u64, signal: u32, action: Action) {
    if bit(signal).is_none() {
        return;
    }
    with_table(task, |table| {
        table.actions[signal as usize] = Some(action);
    });
}

pub fn blocked(task: u64) -> u64 {
    with_table(task, |table| table.blocked)
}

pub fn set_blocked(task: u64, mask: u64) {
    with_table(task, |table| table.blocked = mask);
}

/// Records a pending signal. Returns `false` for an invalid number or a
/// disposition that ignores the signal (Linux does not queue those).
pub fn raise(task: u64, signal: u32) -> bool {
    let Some(bit) = bit(signal) else {
        return false;
    };
    with_table(task, |table| {
        if table.actions[signal as usize]
            .map(|action| action.handler == SIG_IGN)
            .unwrap_or(false)
        {
            return false;
        }
        table.pending |= bit;
        true
    })
}

pub fn clear_pending(task: u64, signal: u32) {
    if let Some(bit) = bit(signal) {
        with_table(task, |table| table.pending &= !bit);
    }
}

pub fn pending(task: u64) -> u64 {
    with_table(task, |table| table.pending)
}

pub fn close_task(task: u64) {
    let mut tables = TABLES.lock();
    if let Some(index) = tables.iter().position(|table| table.task == task) {
        tables.remove(index);
    }
}

/// True when the task ever set a disposition or mask. Used to avoid creating a
/// Linux signal table for a native parent that simply forked a Linux child.
pub fn has_table(task: u64) -> bool {
    TABLES.lock().iter().any(|table| table.task == task)
}

/// Default action for a signal delivered with `SIG_DFL`. Signals whose default
/// is "ignore" or "stop" are not termination.
pub fn default_terminates(signal: u32) -> bool {
    !matches!(signal, SIGCHLD | 18 | 19 | 20 | 21 | 22 | 23 | 28)
}
