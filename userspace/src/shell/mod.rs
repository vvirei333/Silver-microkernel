//! The shell: command dispatch, one module per command family, and the few
//! items they share.
//!
//! silvershell.rs keeps the entry point, the input loop and the UART driver
//! link; everything the loop calls lives here.

pub mod bench;
pub mod commands;
pub mod files;
pub mod net;
pub mod proc;

pub use commands::{execute, fastfetch, names};
pub use proc::{find_task_id, find_task_id_preferring};

use crate::syscall;

pub(crate) const TICKS_PER_SECOND: u64 = 100;

pub(crate) const MIB: u64 = 1024 * 1024;

/// Longest filename and combined name+data the single-shot SFS IPC carries.
/// Full-path bound, not a component bound: `/sfs/apps/scopeprobe.manifest`
/// is longer than one name yet perfectly valid.
pub(crate) const SFS_PATH_MAX: usize = 191;

/// A driver request is answered immediately; waiting two seconds keeps the
/// shell responsive when the driver died between send and reply.
pub(crate) const RESPONSE_TIMEOUT_TICKS: u64 = 2 * TICKS_PER_SECOND;

pub(crate) const RESPONSE_POLL_TICKS: u64 = 10;

pub(crate) const LOGO: &str = include_str!("../../../logo.txt");

pub fn clear_screen() {
    syscall::print("\x1b[2J\x1b[H");
}

pub fn prompt() {
    syscall::print("\x1b[1;36mhowl>\x1b[0m ");
}

pub fn echo(byte: u8) {
    let bytes = [byte];
    if let Ok(text) = core::str::from_utf8(&bytes) {
        syscall::print(text);
    }
}
