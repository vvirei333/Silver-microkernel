//! AArch64 Linux syscall personality.
//!
//! A task spawned from a foreign ELF runs with [scheduler::Personality::Linux]
//! and enters the kernel with x8 holding a **Linux** syscall number. This
//! module translates the subset we implement onto the kernel's own services,
//! so the same svc #0 trampoline serves both ABIs.
//!
//! Return values follow Linux: a successful call returns its result in x0,
//! a failure returns a negative errno reinterpreted as u64. The Silver ABI's
//! sentinel values (SYSCALL_ERROR and friends) must never leak out of here.
//!
//! Scope of this slice: enough for a freestanding static binary and the early
//! libc startup (write, exit, brk, mmap, clock_gettime, ...). File
//! descriptors, signals and threads are deliberate no-ops or -ENOSYS; each
//! one is called out at its match arm.
//!
//! The pieces live in one module per syscall family: dispatch owns the
//! number table and the match, fs the path/descriptor calls, process
//! fork/exec/signals/time, memory brk/mmap/futex and ipc pipes, sockets,
//! epoll and timerfd.

pub(super) use crate::mutex::Mutex;
pub(super) use crate::{linux_fd, scheduler, timer, user_memory};
pub(super) use alloc::string::String;
pub(super) use alloc::vec::Vec;

mod cap;
mod dispatch;
pub(crate) mod fs;
mod ipc;
mod memory;
mod process;

pub use dispatch::{dispatch, SYS_RT_SIGRETURN};
pub use fs::{map_absolute, resolve};
pub use memory::{futex_cleanup, futex_wake_address};

const EPERM: u64 = 1;
const ENOENT: u64 = 2;
const EIO: u64 = 5;
const ENOEXEC: u64 = 8;
const EBADF: u64 = 9;
const ECHILD: u64 = 10;
const EAGAIN: u64 = 11;
const ENOMEM: u64 = 12;
const EACCES: u64 = 13;
const EFAULT: u64 = 14;
const ENOTDIR: u64 = 20;
const EISDIR: u64 = 21;
const EINVAL: u64 = 22;
const EMFILE: u64 = 24;
const EEXIST: u64 = 17;
const ENOTTY: u64 = 25;
const ESPIPE: u64 = 29;
const EROFS: u64 = 30;
const ERANGE: u64 = 34;
const ENOSYS: u64 = 38;
const EPROTONOSUPPORT: u64 = 93;
const EAFNOSUPPORT: u64 = 97;
const ENOPROTOOPT: u64 = 92;
const ENOTSOCK: u64 = 88;

/// Linux returns `-errno`; sign-extend so `(long)x0 < 0` works in libc.
const fn errno(code: u64) -> u64 {
    (code as i64).wrapping_neg() as u64
}

/// `PROT_*` are identical in Linux and the Silver ABI (R=1, W=2, X=4).
const MAP_FIXED: u64 = 0x10;

/// Unix-domain socket ABI constants.
const AF_UNIX: u64 = 1;
const SOL_SOCKET: u64 = 1;
const SCM_RIGHTS: u64 = 1;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_CTRUNC: i32 = 0x8;

/// `openat` / `newfstatat` conventions.
const AT_FDCWD: i64 = -100;
const AT_EMPTY_PATH: u64 = 0x1000;
/// `newfstatat`/`statx`: report the link itself instead of its target.
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
/// `struct stat` mode classes.
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;
const S_IFSOCK: u32 = 0o140000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Last-N syscall trace of Linux guest tasks (nr, arg0, arg1, result, task).
pub const TRACE_RING_LEN: usize = 8192;
pub(crate) static TRACE_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut TRACE_RING: [[u64; 5]; TRACE_RING_LEN] = [[0; 5]; TRACE_RING_LEN];

/// Dispatches one Linux syscall. `frame` is the saved register bank, so
/// `frame[0..6]` are `x0..x5` and `frame[8]` is the number in `x8`.
/// EACCES failures (number, flags, path[48 bytes]) for pmemsave parsing -
/// the console drops prints under load, the ring does not. Only EACCES is
/// kept so the idle boot loop does not overwrite the interesting failures.
pub const FAIL_RING_LEN: usize = 32;
static FAIL_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut FAIL_RING: [[u64; 8]; FAIL_RING_LEN] = [[0; 8]; FAIL_RING_LEN];

/// Maps the fd layer's positive errno code onto a Linux `-errno` result.
fn finish(result: Result<u64, u64>) -> u64 {
    match result {
        Ok(value) => value,
        Err(code) => errno(code),
    }
}

/// Reads one `struct iovec` (base, length) out of user memory.
fn iovec_at(iovec: u64, index: u64) -> Option<(u64, u64)> {
    const IOVEC_BYTES: u64 = 16;
    let mut entry = [0_u8; 16];
    let address = iovec.wrapping_add(index * IOVEC_BYTES);
    if user_memory::copy_from_user(address, &mut entry).is_err() {
        return None;
    }
    Some((
        u64::from_le_bytes(entry[..8].try_into().unwrap()),
        u64::from_le_bytes(entry[8..].try_into().unwrap()),
    ))
}

/// Reads a `u64` from user memory (`off_t *` on AArch64).
fn user_u64(address: u64) -> Option<u64> {
    let mut raw = [0_u8; 8];
    user_memory::copy_from_user(address, &mut raw).ok()?;
    Some(u64::from_le_bytes(raw))
}

const SEEK_CUR: u64 = 1;
const SEEK_SET: u64 = 0;

const AT_REMOVEDIR: u64 = 0x200;
const RENAME_NOREPLACE: u64 = 1;
