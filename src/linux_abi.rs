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
const ENOTDIR: u64 = 20;
const EISDIR: u64 = 21;
const EINVAL: u64 = 22;
const ENOTTY: u64 = 25;
const ESPIPE: u64 = 29;
const EROFS: u64 = 30;
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
const O_WRONLY: u64 = 0o1;
const O_RDWR: u64 = 0o2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_DIRECTORY: u64 = 0o200000;
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
        SYS_READ => linux_read(args[0], args[1], args[2]),
        SYS_OPENAT => linux_openat(args[0], args[1], args[2], args[3]),
        SYS_CLOSE => linux_close(args[0]),
        SYS_LSEEK => linux_lseek(args[0], args[1], args[2]),
        SYS_IOCTL => linux_ioctl(args[0], args[1], args[2]),
        SYS_FSTAT => linux_fstat(args[0], args[1]),
        SYS_NEWFSTATAT => linux_newfstatat(args[0], args[1], args[2], args[3]),
        SYS_GETDENTS64 => linux_getdents64(args[0], args[1], args[2]),
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

/// `read(2)`: fd 0 reports end-of-file (no tty input yet), fds 1/2 are
/// write-only, and fd 3+ reads from the VFS at the descriptor's offset.
fn linux_read(fd: u64, buffer: u64, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    if fd == 0 {
        return 0;
    }
    if fd < 3 {
        return errno(EBADF);
    }
    let owner = scheduler::current_task_id();
    if crate::vfs::fd_kind(owner, fd) == Some(crate::vfs::FD_KIND_DIR) {
        return errno(EISDIR);
    }
    let limit = (count as usize).min(1 << 20);
    match crate::vfs::read_with(owner, fd, limit, |bytes| {
        if bytes.is_empty() {
            Ok(())
        } else {
            user_memory::copy_to_user(bytes, buffer).map_err(|_| ())
        }
    }) {
        Ok(read) => read as u64,
        Err(()) => errno(EBADF),
    }
}

fn linux_close(fd: u64) -> u64 {
    // The console descriptors are process-lifetime objects; "closing" them is
    // accepted but changes nothing. A VFS descriptor is really released.
    if fd <= 2 {
        0
    } else if crate::vfs::close(scheduler::current_task_id(), fd).is_ok() {
        0
    } else {
        errno(EBADF)
    }
}

fn linux_lseek(fd: u64, offset: u64, whence: u64) -> u64 {
    if fd <= 2 {
        return errno(ESPIPE);
    }
    match crate::vfs::lseek(
        scheduler::current_task_id(),
        fd,
        offset as i64,
        whence,
    ) {
        Ok(position) => position,
        Err(()) => errno(EINVAL),
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

/// `fstat(2)`. The layout is the aarch64 `struct stat` (128 bytes); glibc
/// uses `st_mode` to decide whether stdout is a terminal and `st_size` /
/// `st_blksize` for buffering and file operations.
fn linux_fstat(fd: u64, buffer: u64) -> u64 {
    if fd <= 2 {
        return write_stat(buffer, S_IFCHR | 0o620, 0, fd.max(1));
    }
    let owner = scheduler::current_task_id();
    let Ok(size) = crate::vfs::fd_len(owner, fd) else {
        return errno(EBADF);
    };
    let kind = crate::vfs::fd_kind(owner, fd).unwrap_or(crate::vfs::FD_KIND_FILE);
    let mode = if kind == crate::vfs::FD_KIND_DIR {
        S_IFDIR | 0o755
    } else {
        S_IFREG | 0o644
    };
    write_stat(buffer, mode, size as u64, fd)
}

fn write_stat(buffer: u64, mode: u32, size: u64, ino: u64) -> u64 {
    let mut stat = [0_u8; 128];
    stat[8..16].copy_from_slice(&ino.to_le_bytes()); // st_ino
    stat[16..20].copy_from_slice(&mode.to_le_bytes()); // st_mode
    stat[20..24].copy_from_slice(&1_u32.to_le_bytes()); // st_nlink
    stat[48..56].copy_from_slice(&size.to_le_bytes()); // st_size
    stat[56..60].copy_from_slice(&1024_u32.to_le_bytes()); // st_blksize
    stat[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // st_blocks
    if user_memory::copy_to_user(&stat, buffer).is_err() {
        return errno(EFAULT);
    }
    0
}

/// `openat(2)`. The Silver VFS is read-only from the Linux personality for
/// now, so any write intent fails with `-EROFS` before a descriptor exists.
fn linux_openat(dirfd: u64, path_ptr: u64, flags: u64, _mode: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        return errno(ENOENT);
    }
    if flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND) != 0 {
        return errno(EROFS);
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    let owner = scheduler::current_task_id();
    if flags & O_DIRECTORY != 0 {
        return crate::vfs::open_dir(owner, &cwd, &path)
            .map(|fd| fd)
            .unwrap_or_else(|_| errno(ENOENT));
    }
    // Linux allows opening a directory without O_DIRECTORY.
    if let Ok(fd) = crate::vfs::open(owner, &cwd, &path, 0) {
        return fd;
    }
    crate::vfs::open_dir(owner, &cwd, &path)
        .map(|fd| fd)
        .unwrap_or_else(|_| errno(ENOENT))
}

/// `newfstatat(2)`: stat by path, or by descriptor with `AT_EMPTY_PATH`.
fn linux_newfstatat(dirfd: u64, path_ptr: u64, buffer: u64, flags: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return errno(ENOENT);
        }
        return linux_fstat(dirfd, buffer);
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    if let Ok(size) = crate::vfs::file_len(&cwd, &path) {
        return write_stat(buffer, S_IFREG | 0o644, size as u64, 0);
    }
    if crate::vfs::read_dir(&cwd, &path).is_ok() {
        return write_stat(buffer, S_IFDIR | 0o755, 0, 0);
    }
    errno(ENOENT)
}

/// `getdents64(2)`: serializes up to `count` bytes of directory entries. The
/// descriptor offset advances only by the entries that fit, so a short buffer
/// neither loses nor repeats an entry.
fn linux_getdents64(fd: u64, buffer: u64, count: u64) -> u64 {
    const DIRENT_HEADER: usize = 19; // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1)
    if fd <= 2 {
        return errno(ENOTDIR);
    }
    if count == 0 {
        return 0;
    }
    let owner = scheduler::current_task_id();
    let Ok((entries, start)) = crate::vfs::dir_entries_from_fd(owner, fd, 128) else {
        return errno(ENOTDIR);
    };
    if entries.is_empty() {
        return 0;
    }
    let limit = (count as usize).min(64 * 1024);
    let mut output = alloc::vec![0_u8; limit];
    let mut used = 0usize;
    let mut serialized = 0usize;
    for entry in &entries {
        let name_len = (entry.name_len as usize).min(entry.name.len());
        let record = (DIRENT_HEADER + name_len + 1 + 7) & !7;
        if used + record > limit {
            break;
        }
        output[used..used + 8].copy_from_slice(&((start + serialized + 1) as u64).to_le_bytes());
        output[used + 8..used + 16]
            .copy_from_slice(&((start + serialized + 1) as i64).to_le_bytes());
        output[used + 16..used + 18].copy_from_slice(&(record as u16).to_le_bytes());
        output[used + 18] = if entry.kind == crate::vfs::FD_KIND_DIR {
            4 // DT_DIR
        } else {
            8 // DT_REG
        };
        output[used + DIRENT_HEADER..used + DIRENT_HEADER + name_len]
            .copy_from_slice(&entry.name[..name_len]);
        used += record;
        serialized += 1;
    }
    if serialized == 0 {
        // The first entry does not fit: Linux reports EINVAL rather than EOF.
        return errno(EINVAL);
    }
    if user_memory::copy_to_user(&output[..used], buffer).is_err() {
        return errno(EFAULT);
    }
    if crate::vfs::advance_dir_fd(owner, fd, serialized).is_err() {
        return errno(EBADF);
    }
    used as u64
}

/// Resolves `dirfd` to a working directory: `AT_FDCWD` uses the process cwd,
/// a directory descriptor uses its own path, anything else is `EBADF`.
fn dirfd_cwd(dirfd: u64) -> Result<alloc::string::String, u64> {
    if dirfd as i64 == AT_FDCWD {
        return Ok(scheduler::current_task_cwd());
    }
    let owner = scheduler::current_task_id();
    match crate::vfs::fd_kind(owner, dirfd) {
        Some(crate::vfs::FD_KIND_DIR) => {
            crate::vfs::fd_path(owner, dirfd).ok_or_else(|| errno(EBADF))
        }
        _ => Err(errno(EBADF)),
    }
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
