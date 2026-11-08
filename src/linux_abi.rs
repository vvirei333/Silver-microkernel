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

use crate::{linux_fd, scheduler, timer, user_memory};

// Linux AArch64 syscall numbers (asm-generic/unistd.h).
const SYS_DUP: u64 = 23;
const SYS_DUP3: u64 = 24;
const SYS_IOCTL: u64 = 29;
const SYS_FCNTL: u64 = 25;
const SYS_GETCWD: u64 = 17;
const SYS_CHDIR: u64 = 49;
const SYS_OPENAT: u64 = 56;
const SYS_CLOSE: u64 = 57;
const SYS_LSEEK: u64 = 62;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_WRITEV: u64 = 66;
const SYS_READLINKAT: u64 = 78;
const SYS_FSTAT: u64 = 80;
const SYS_NEWFSTATAT: u64 = 79;
const SYS_GETDENTS64: u64 = 61;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_SET_TID_ADDRESS: u64 = 96;
const SYS_SET_ROBUST_LIST: u64 = 99;
const SYS_CLOCK_GETTIME: u64 = 113;
const SYS_CLOCK_GETRES: u64 = 114;
const SYS_CLOCK_NANOSLEEP: u64 = 115;
const SYS_NANOSLEEP: u64 = 101;
const SYS_GETTIMEOFDAY: u64 = 169;
const SYS_RT_SIGPROCMASK: u64 = 135;
const SYS_UNAME: u64 = 160;
const SYS_GETPID: u64 = 172;
const SYS_GETPPID: u64 = 173;
const SYS_GETUID: u64 = 174;
const SYS_GETEUID: u64 = 175;
const SYS_GETGID: u64 = 176;
const SYS_GETEGID: u64 = 177;
const SYS_GETRLIMIT: u64 = 163;
const SYS_PRCTL: u64 = 167;
const SYS_SCHED_GETAFFINITY: u64 = 123;
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
const EACCES: u64 = 13;
const EFAULT: u64 = 14;
const ENOTDIR: u64 = 20;
const EISDIR: u64 = 21;
const EINVAL: u64 = 22;
const ENOTTY: u64 = 25;
const ESPIPE: u64 = 29;
const EROFS: u64 = 30;
const ERANGE: u64 = 34;
const ENOSYS: u64 = 38;

/// Linux returns `-errno`; sign-extend so `(long)x0 < 0` works in libc.
const fn errno(code: u64) -> u64 {
    (code as i64).wrapping_neg() as u64
}

/// `PROT_*` are identical in Linux and the Silver ABI (R=1, W=2, X=4).
const MAP_FIXED: u64 = 0x10;

/// `openat` / `newfstatat` conventions.
const AT_FDCWD: i64 = -100;
const AT_EMPTY_PATH: u64 = 0x1000;
/// `struct stat` mode classes.
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Dispatches one Linux syscall. `frame` is the saved register bank, so
/// `frame[0..6]` are `x0..x5` and `frame[8]` is the number in `x8`.
pub fn dispatch(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) -> u64 {
    let number = frame[8];
    let args = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
    match number {
        SYS_WRITE => linux_write(args[0], args[1], args[2]),
        SYS_WRITEV => linux_writev(args[0], args[1], args[2]),
        SYS_READ => finish(linux_fd::read(scheduler::current_task_id(), args[0], args[1], args[2])),
        SYS_OPENAT => linux_openat(args[0], args[1], args[2], args[3]),
        SYS_CLOSE => finish(linux_fd::close(scheduler::current_task_id(), args[0]).map(|_| 0)),
        SYS_LSEEK => finish(linux_fd::lseek(
            scheduler::current_task_id(),
            args[0],
            args[1] as i64,
            args[2],
        )),
        SYS_DUP => finish(linux_fd::dup(scheduler::current_task_id(), args[0])),
        SYS_DUP3 => finish(linux_fd::dup2(
            scheduler::current_task_id(),
            args[0],
            args[1],
        )),
        SYS_FCNTL => linux_fcntl(args[0], args[1], args[2]),
        SYS_GETCWD => linux_getcwd(args[0], args[1]),
        SYS_CHDIR => linux_chdir(args[0]),
        SYS_IOCTL => linux_ioctl(args[0], args[1], args[2]),
        SYS_FSTAT => finish(linux_fd::fstat(scheduler::current_task_id(), args[0], args[1]).map(|_| 0)),
        SYS_NEWFSTATAT => linux_newfstatat(args[0], args[1], args[2], args[3]),
        SYS_GETDENTS64 => finish(linux_fd::getdents64(
            scheduler::current_task_id(),
            args[0],
            args[1],
            args[2],
        )),
        SYS_READLINKAT => errno(ENOENT),
        SYS_NANOSLEEP => linux_nanosleep(args[0], args[1]),
        SYS_CLOCK_NANOSLEEP => linux_nanosleep(args[2], args[3]),
        SYS_CLOCK_GETRES => linux_clock_getres(args[1]),
        SYS_GETTIMEOFDAY => linux_gettimeofday(args[0]),
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
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_GETRLIMIT => linux_getrlimit(args[0], args[1]),
        SYS_SCHED_GETAFFINITY => linux_sched_getaffinity(args[1], args[2]),
        SYS_PRCTL => 0,
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
    finish(linux_fd::write(
        scheduler::current_task_id(),
        fd,
        buffer,
        count,
    ))
}

/// Maps the fd layer's positive errno code onto a Linux `-errno` result.
fn finish(result: Result<u64, u64>) -> u64 {
    match result {
        Ok(value) => value,
        Err(code) => errno(code),
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

/// `openat(2)`: delegates to the process fd table, which also recognizes the
/// synthetic `/dev` nodes before touching the VFS.
fn linux_openat(dirfd: u64, path_ptr: u64, flags: u64, _mode: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        return errno(ENOENT);
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    finish(linux_fd::open(
        scheduler::current_task_id(),
        &cwd,
        &path,
        flags,
    ))
}

/// `newfstatat(2)`: stat by path, or by descriptor with `AT_EMPTY_PATH`.
fn linux_newfstatat(dirfd: u64, path_ptr: u64, buffer: u64, flags: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let task = scheduler::current_task_id();
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return errno(ENOENT);
        }
        return finish(linux_fd::fstat(task, dirfd, buffer).map(|_| 0));
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    if let Ok(size) = crate::vfs::file_len(&cwd, &path) {
        return match linux_fd::write_stat(buffer, S_IFREG | 0o644, size as u64, 0) {
            Ok(()) => 0,
            Err(code) => errno(code),
        };
    }
    if crate::vfs::read_dir(&cwd, &path).is_ok() {
        return match linux_fd::write_stat(buffer, S_IFDIR | 0o755, 0, 0) {
            Ok(()) => 0,
            Err(code) => errno(code),
        };
    }
    errno(ENOENT)
}

/// `fcntl(2)`: descriptor duplication plus the flags queries every libc probes.
fn linux_fcntl(fd: u64, command: u64, argument: u64) -> u64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    let task = scheduler::current_task_id();
    match command {
        F_DUPFD | F_DUPFD_CLOEXEC => finish(linux_fd::dup_from(task, fd, argument)),
        // We do not model descriptor or status flags yet; report "none" and
        // accept changes as no-ops.
        F_GETFD | F_GETFL => 0,
        F_SETFD | F_SETFL => 0,
        _ => errno(EINVAL),
    }
}

fn linux_getcwd(buffer: u64, capacity: u64) -> u64 {
    let cwd = scheduler::current_task_cwd();
    let bytes = cwd.as_bytes();
    if bytes.len() + 1 > capacity as usize {
        return errno(ERANGE);
    }
    if user_memory::copy_to_user(bytes, buffer).is_err() {
        return errno(EFAULT);
    }
    let nul = [0_u8; 1];
    if user_memory::copy_to_user(&nul, buffer + bytes.len() as u64).is_err() {
        return errno(EFAULT);
    }
    (bytes.len() + 1) as u64
}

fn linux_chdir(path_ptr: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let cwd = scheduler::current_task_cwd();
    let Ok(resolved) = crate::vfs::resolve_directory(&cwd, &path) else {
        return errno(ENOENT);
    };
    if scheduler::set_current_task_cwd(resolved) {
        0
    } else {
        errno(EACCES)
    }
}

/// Resolves `dirfd` to a working directory: `AT_FDCWD` uses the process cwd,
/// a directory descriptor uses its own path, anything else is `EBADF`.
fn dirfd_cwd(dirfd: u64) -> Result<alloc::string::String, u64> {
    if dirfd as i64 == AT_FDCWD {
        return Ok(scheduler::current_task_cwd());
    }
    linux_fd::directory_path(scheduler::current_task_id(), dirfd).ok_or_else(|| errno(EBADF))
}

/// Copies a NUL-terminated user string (bounded to 255 bytes).
fn user_string(pointer: u64) -> Option<alloc::string::String> {
    let mut bytes = alloc::vec::Vec::new();
    for index in 0..4096u64 {
        let mut byte = [0_u8; 1];
        user_memory::copy_from_user(pointer.wrapping_add(index), &mut byte).ok()?;
        if byte[0] == 0 {
            break;
        }
        if bytes.len() >= 255 {
            return None;
        }
        bytes.push(byte[0]);
    }
    core::str::from_utf8(&bytes)
        .ok()
        .map(alloc::string::String::from)
}

fn linux_clock_gettime(_clock: u64, timespec: u64) -> u64 {
    // `timer::ticks()` is 100 Hz; report it as whole seconds plus the
    // remaining fraction.
    let ticks = timer::ticks();
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&(ticks / 100).to_le_bytes());
    bytes[8..].copy_from_slice(&((ticks % 100) * 10_000_000).to_le_bytes());
    if user_memory::copy_to_user(&bytes, timespec).is_err() {
        return errno(EFAULT);
    }
    0
}

/// `clock_getres(2)`: the scheduler tick is the resolution.
fn linux_clock_getres(resolution: u64) -> u64 {
    if resolution == 0 {
        return 0;
    }
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&10_000_000_u64.to_le_bytes());
    if user_memory::copy_to_user(&bytes, resolution).is_err() {
        return errno(EFAULT);
    }
    0
}

fn linux_gettimeofday(time: u64) -> u64 {
    if time == 0 {
        return 0;
    }
    let ticks = timer::ticks();
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&(ticks / 100).to_le_bytes());
    bytes[8..16].copy_from_slice(&((ticks % 100) * 10_000).to_le_bytes());
    if user_memory::copy_to_user(&bytes, time).is_err() {
        return errno(EFAULT);
    }
    0
}

/// `nanosleep`/`clock_nanosleep`: blocking is a scheduler sleep, clamped so a
/// program cannot wedge the kernel with an absurd timeout.
fn linux_nanosleep(request: u64, remaining: u64) -> u64 {
    let mut bytes = [0_u8; 16];
    if user_memory::copy_from_user(request, &mut bytes).is_err() {
        return errno(EFAULT);
    }
    let seconds = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let nanoseconds = u64::from_le_bytes(bytes[8..].try_into().unwrap());
    let millis = seconds
        .saturating_mul(1000)
        .saturating_add(nanoseconds / 1_000_000)
        .min(60_000);
    if millis != 0 {
        scheduler::sleep(millis);
    } else {
        scheduler::yield_current();
    }
    if remaining != 0 {
        let zero = [0_u8; 16];
        let _ = user_memory::copy_to_user(&zero, remaining);
    }
    0
}

fn linux_getrlimit(resource: u64, buffer: u64) -> u64 {
    const RLIMIT_NOFILE: u64 = 7;
    let limit: u64 = if resource == RLIMIT_NOFILE {
        256
    } else {
        1 << 30
    };
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&limit.to_le_bytes());
    bytes[8..].copy_from_slice(&limit.to_le_bytes());
    if user_memory::copy_to_user(&bytes, buffer).is_err() {
        return errno(EFAULT);
    }
    0
}

fn linux_sched_getaffinity(size: u64, mask: u64) -> u64 {
    if size < 8 {
        return errno(EINVAL);
    }
    let mut bytes = [0_u8; 8];
    bytes[0] = 1; // one CPU
    if user_memory::copy_to_user(&bytes, mask).is_err() {
        return errno(EFAULT);
    }
    8
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
