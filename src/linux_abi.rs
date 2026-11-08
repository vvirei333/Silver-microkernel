//! AArch64 Linux syscall personality.
//!
//! A task spawned from a foreign ELF runs with [`scheduler::Personality::Linux`]
//! and enters the kernel with `x8` holding a **Linux** syscall number. This
//! module translates the subset we implement onto the kernel's own services,
//! so the same `svc #0` trampoline serves both ABIs.
//!
//! Return values follow Linux: a successful call returns its result in `x0`,
//! a failure returns a negative errno reinterpreted as `u64`. The Silver ABI's
//! sentinel values (`SYSCALL_ERROR` and friends) must never leak out of here.
//!
//! Scope of this slice: enough for a freestanding static binary and the early
//! libc startup (`write`, `exit`, `brk`, `mmap`, `clock_gettime`, ...). File
//! descriptors, signals and threads are deliberate no-ops or `-ENOSYS`; each
//! one is called out at its match arm.

use crate::{scheduler, syscall, timer, user_memory};

// Linux AArch64 syscall numbers (asm-generic/unistd.h).
const SYS_IOCTL: u64 = 29;
const SYS_CLOSE: u64 = 57;
const SYS_LSEEK: u64 = 62;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_WRITEV: u64 = 66;
const SYS_READLINKAT: u64 = 78;
const SYS_FSTAT: u64 = 80;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_SET_TID_ADDRESS: u64 = 96;
const SYS_SET_ROBUST_LIST: u64 = 99;
const SYS_CLOCK_GETTIME: u64 = 113;
const SYS_RT_SIGPROCMASK: u64 = 135;
const SYS_UNAME: u64 = 160;
const SYS_GETPID: u64 = 172;
const SYS_GETPPID: u64 = 173;
const SYS_GETTID: u64 = 178;
const SYS_SCHED_YIELD: u64 = 124;
const SYS_BRK: u64 = 214;
const SYS_MUNMAP: u64 = 215;
const SYS_MMAP: u64 = 222;
const SYS_MPROTECT: u64 = 226;
const SYS_PRLIMIT64: u64 = 261;
const SYS_GETRANDOM: u64 = 278;
const SYS_RSEQ: u64 = 293;

const EPERM: u64 = 1;
const ENOENT: u64 = 2;
const EBADF: u64 = 9;
const ENOMEM: u64 = 12;
const EFAULT: u64 = 14;
const EINVAL: u64 = 22;
const ENOTTY: u64 = 25;
const ESPIPE: u64 = 29;
const ENOSYS: u64 = 38;

/// Linux returns `-errno`; sign-extend so `(long)x0 < 0` works in libc.
const fn errno(code: u64) -> u64 {
    (code as i64).wrapping_neg() as u64
}

/// `PROT_*` are identical in Linux and the Silver ABI (R=1, W=2, X=4).
const MAP_FIXED: u64 = 0x10;

/// Dispatches one Linux syscall. `frame` is the saved register bank, so
/// `frame[0..6]` are `x0..x5` and `frame[8]` is the number in `x8`.
pub fn dispatch(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) -> u64 {
    let number = frame[8];
    let args = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
    match number {
        SYS_WRITE => linux_write(args[0], args[1], args[2]),
        SYS_WRITEV => linux_writev(args[0], args[1], args[2]),
        SYS_READ => linux_read(args[0], args[1], args[2]),
        SYS_CLOSE => linux_close(args[0]),
        SYS_LSEEK => errno(ESPIPE),
        SYS_IOCTL => linux_ioctl(args[0], args[1], args[2]),
        SYS_FSTAT => linux_fstat(args[0], args[1]),
        SYS_READLINKAT => errno(ENOENT),
        SYS_SCHED_YIELD => {
            scheduler::yield_current();
            0
        }
        SYS_EXIT | SYS_EXIT_GROUP => scheduler::exit_current_with_status(args[0]),
        SYS_SET_TID_ADDRESS => scheduler::current_task_id(),
        SYS_SET_ROBUST_LIST => 0, // single-threaded: nothing to register
        SYS_CLOCK_GETTIME => linux_clock_gettime(args[0], args[1]),
        SYS_RT_SIGPROCMASK => 0, // no signal delivery yet
        SYS_UNAME => linux_uname(args[0]),
        SYS_GETPID | SYS_GETTID => scheduler::current_task_id(),
        SYS_GETPPID => scheduler::current_task_ppid().unwrap_or(0),
        SYS_BRK => linux_brk(args[0]),
        SYS_MUNMAP => linux_munmap(args[0], args[1]),
        SYS_MMAP => linux_mmap(args[0], args[1], args[2], args[3]),
        // Our mappings are created with their final permissions; there is no
        // page-level protection state to change yet.
        SYS_MPROTECT => 0,
        SYS_PRLIMIT64 => 0, // no rlimits; report "unchanged"
        SYS_GETRANDOM => linux_getrandom(args[0], args[1], args[2]),
        // glibc probes rseq and falls back when it is unsupported.
        SYS_RSEQ => errno(ENOSYS),
        _ => {
            #[cfg(feature = "linux-abi-trace")]
            trace_unsupported(number);
            errno(ENOSYS)
        }
    }
}

/// One console line per unsupported Linux syscall, so the next implementation
/// slice is driven by what a real libc actually asks for. Capped so a retry
/// loop cannot flood the console. Opt-in: `--features linux-abi-trace`.
#[cfg(feature = "linux-abi-trace")]
fn trace_unsupported(number: u64) {
    static mut COUNT: u32 = 0;
    unsafe {
        if COUNT < 48 {
            COUNT += 1;
            crate::console::write_str("LINUX ENOSYS ");
            crate::console::write_dec_u64(number);
            crate::console::write_str("\r\n");
        }
    }
}

fn linux_write(fd: u64, buffer: u64, count: u64) -> u64 {
    match fd {
        1 | 2 => {
            let result = syscall::write_user_output(buffer, count);
            if result == syscall::SYSCALL_ERROR {
                errno(EFAULT)
            } else {
                count
            }
        }
        _ => errno(EBADF),
    }
}

/// No file-descriptor table yet: stdin reports end-of-file so a libc reading
/// it does not spin, and any other descriptor is invalid.
fn linux_read(fd: u64, _buffer: u64, _count: u64) -> u64 {
    match fd {
        0 => 0,
        _ => errno(EBADF),
    }
}

fn linux_close(fd: u64) -> u64 {
    // The console descriptors are process-lifetime objects; "closing" them is
    // accepted but changes nothing yet. Any other descriptor does not exist.
    if fd <= 2 {
        0
    } else {
        errno(EBADF)
    }
}

/// `writev(2)`: scatter output over up to 16 `struct iovec`s.
fn linux_writev(fd: u64, iovec: u64, count: u64) -> u64 {
    const MAX_IOVECS: u64 = 16;
    const IOVEC_BYTES: u64 = 16;
    if count > MAX_IOVECS {
        return errno(EINVAL);
    }
    if fd != 1 && fd != 2 {
        return errno(EBADF);
    }
    let mut total = 0u64;
    for index in 0..count {
        let mut entry = [0_u8; 16];
        let address = iovec.wrapping_add(index * IOVEC_BYTES);
        if user_memory::copy_from_user(address, &mut entry).is_err() {
            return errno(EFAULT);
        }
        let base = u64::from_le_bytes(entry[..8].try_into().unwrap());
        let length = u64::from_le_bytes(entry[8..].try_into().unwrap());
        let result = linux_write(fd, base, length);
        if result == errno(EFAULT) {
            return result;
        }
        total += length;
    }
    total
}

/// `ioctl(2)`: only the terminal size query is meaningful while stdout is the
/// console. Everything else reports "not a tty".
fn linux_ioctl(fd: u64, request: u64, argument: u64) -> u64 {
    const TIOCGWINSZ: u64 = 0x5413;
    if fd > 2 {
        return errno(EBADF);
    }
    if request != TIOCGWINSZ {
        return errno(ENOTTY);
    }
    // struct winsize { unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel }
    let mut size = [0_u8; 8];
    size[0..2].copy_from_slice(&24_u16.to_le_bytes());
    size[2..4].copy_from_slice(&80_u16.to_le_bytes());
    if user_memory::copy_to_user(&size, argument).is_err() {
        return errno(EFAULT);
    }
    0
}

/// `fstat(2)` for the three console descriptors. The layout is the aarch64
/// `struct stat` (128 bytes); glibc uses `st_mode` to decide whether stdout is
/// a terminal and `st_blksize` to size its stdio buffer.
fn linux_fstat(fd: u64, buffer: u64) -> u64 {
    const S_IFCHR: u32 = 0o020000;
    if fd > 2 {
        return errno(EBADF);
    }
    let mut stat = [0_u8; 128];
    stat[16..20].copy_from_slice(&(S_IFCHR | 0o620).to_le_bytes()); // st_mode
    stat[20..24].copy_from_slice(&1_u32.to_le_bytes()); // st_nlink
    stat[56..60].copy_from_slice(&1024_u32.to_le_bytes()); // st_blksize
    if user_memory::copy_to_user(&stat, buffer).is_err() {
        return errno(EFAULT);
    }
    0
}

fn linux_clock_gettime(_clock: u64, timespec: u64) -> u64 {
    // `timer::ticks()` is 100 Hz; report it as whole seconds plus the
    // remaining tenth-of-a-second fraction.
    let ticks = timer::ticks();
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&(ticks / 100).to_le_bytes());
    bytes[8..].copy_from_slice(&((ticks % 100) * 10_000_000).to_le_bytes());
    if user_memory::copy_to_user(&bytes, timespec).is_err() {
        return errno(EFAULT);
    }
    0
}

fn linux_uname(buffer: u64) -> u64 {
    // `struct utsname` is six 65-byte fields.
    let mut bytes = [0_u8; 6 * 65];
    let fields: [&[u8]; 6] = [
        b"Linux",
        b"werewolf",
        b"0.1.0",
        b"#1 SMP",
        b"aarch64",
        b"(none)",
    ];
    for (index, text) in fields.iter().enumerate() {
        let start = index * 65;
        bytes[start..start + text.len()].copy_from_slice(text);
    }
    if user_memory::copy_to_user(&bytes, buffer).is_err() {
        return errno(EFAULT);
    }
    0
}

fn linux_brk(requested: u64) -> u64 {
    let current = scheduler::change_program_break(0).unwrap_or(0) as u64;
    if requested == 0 {
        return current;
    }
    scheduler::change_program_break(requested as usize)
        .map(|address| address as u64)
        .unwrap_or(current)
}

fn linux_mmap(address: u64, length: u64, prot: u64, flags: u64) -> u64 {
    if length == 0 {
        return errno(EINVAL);
    }
    // An address hint is ignored; MAP_FIXED is refused rather than silently
    // placing the mapping elsewhere (a caller that asked for a fixed address
    // must not get a different one).
    if flags & MAP_FIXED != 0 && address != 0 {
        return errno(EINVAL);
    }
    scheduler::mmap_anonymous(length as usize, prot)
        .map(|placed| placed as u64)
        .unwrap_or(errno(ENOMEM))
}

fn linux_munmap(address: u64, length: u64) -> u64 {
    if address == 0 || length == 0 {
        return errno(EINVAL);
    }
    if scheduler::munmap(address as usize, length as usize) {
        0
    } else {
        errno(EINVAL)
    }
}

fn linux_getrandom(buffer: u64, length: u64, flags: u64) -> u64 {
    if flags & !0x3 != 0 {
        return errno(EINVAL);
    }
    let length = length.min(256) as usize;
    let mut bytes = alloc::vec![0_u8; length];
    let mut state = timer::ticks() ^ 0x2545_F491_4F6C_DD1D;
    for byte in &mut bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    if user_memory::copy_to_user(&bytes, buffer).is_err() {
        return errno(EFAULT);
    }
    length as u64
}
