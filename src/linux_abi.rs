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

use crate::mutex::Mutex;
use crate::{linux_fd, scheduler, timer, user_memory};
use alloc::string::String;
use alloc::vec::Vec;

struct FutexWaiter {
    address: u64,
    tgid: u64,
    task: u64,
    woken: bool,
}

static FUTEX_WAITERS: Mutex<Vec<FutexWaiter>> = Mutex::new(Vec::new());

// Linux AArch64 syscall numbers (asm-generic/unistd.h).
const SYS_DUP: u64 = 23;
const SYS_DUP3: u64 = 24;
const SYS_EVENTFD2: u64 = 19;
const SYS_POLL: u64 = 7;
const SYS_FUTEX: u64 = 98;
const SYS_MEMFD_CREATE: u64 = 279;
const SYS_IOCTL: u64 = 29;
const SYS_FCNTL: u64 = 25;
const SYS_STATX: u64 = 291;
const SYS_FCHMOD: u64 = 52;
const SYS_FCHMODAT: u64 = 53;
const SYS_FCHOWNAT: u64 = 54;
const SYS_FCHOWN: u64 = 55;
const SYS_CLOSE_RANGE: u64 = 436;
const SYS_GETCWD: u64 = 17;
const SYS_CHDIR: u64 = 49;
const SYS_OPENAT: u64 = 56;
const SYS_PIPE2: u64 = 59;
const SYS_PPOLL: u64 = 73;
const SYS_SIGNALFD4: u64 = 74;
const SYS_FSYNC: u64 = 82;
const SYS_FDATASYNC: u64 = 83;
const SYS_RT_SIGTIMEDWAIT: u64 = 137;
const SYS_SCHED_SETPARAM: u64 = 142;
const SYS_SCHED_SETSCHEDULER: u64 = 119;
const SYS_SCHED_GETSCHEDULER: u64 = 120;
const SYS_INOTIFY_INIT1: u64 = 26;
const SYS_INOTIFY_ADD_WATCH: u64 = 27;
const SYS_INOTIFY_RM_WATCH: u64 = 28;
const SYS_SET_PRIORITY: u64 = 140;
const SYS_GET_PRIORITY: u64 = 141;
const SYS_SCHED_GETATTR: u64 = 275;
const SYS_SCHED_SETATTR: u64 = 274;
const SYS_CLOCK_ADJTIME: u64 = 264;
const SYS_CLOSE: u64 = 57;
const SYS_LSEEK: u64 = 62;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_WRITEV: u64 = 66;
const SYS_READV: u64 = 65;
const SYS_SENDFILE: u64 = 71;
const SYS_COPY_FILE_RANGE: u64 = 285;
const SYS_READLINKAT: u64 = 78;
const SYS_FSTAT: u64 = 80;
const SYS_NEWFSTATAT: u64 = 79;
const SYS_FACCESSAT: u64 = 48;
const SYS_GETDENTS64: u64 = 61;
const SYS_EXIT: u64 = 93;
const SYS_CLONE: u64 = 220;
const SYS_WAIT4: u64 = 260;
const SYS_EXECVE: u64 = 221;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_SET_TID_ADDRESS: u64 = 96;
const SYS_SET_ROBUST_LIST: u64 = 99;
const SYS_CLOCK_GETTIME: u64 = 113;
const SYS_KILL: u64 = 129;
const SYS_TGKILL: u64 = 131;
const SYS_SIGALTSTACK: u64 = 132;
const SYS_RT_SIGACTION: u64 = 134;
pub const SYS_RT_SIGRETURN: u64 = 139;
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
const SYS_MADVISE: u64 = 233;
const SYS_PRLIMIT64: u64 = 261;
const SYS_GETRANDOM: u64 = 278;
const SYS_RSEQ: u64 = 293;
const SYS_PREAD64: u64 = 67;
const SYS_PWRITE64: u64 = 68;
const SYS_EPOLL_CREATE1: u64 = 20;
const SYS_EPOLL_CTL: u64 = 21;
const SYS_EPOLL_PWAIT: u64 = 22;
const SYS_EPOLL_PWAIT2: u64 = 441;
const SYS_TIMERFD_CREATE: u64 = 85;
const SYS_TIMERFD_SETTIME: u64 = 86;
const SYS_TIMERFD_GETTIME: u64 = 87;
const SYS_FLOCK: u64 = 32;
const SYS_MKDIRAT: u64 = 34;
const SYS_UNLINKAT: u64 = 35;
const SYS_RENAMEAT: u64 = 38;
const SYS_RENAMEAT2: u64 = 276;
const SYS_UMOUNT2: u64 = 39;
const SYS_MOUNT: u64 = 40;
const SYS_PIVOT_ROOT: u64 = 41;
const SYS_STATFS: u64 = 43;
const SYS_FSTATFS: u64 = 44;
const SYS_CHROOT: u64 = 51;
const SYS_SETPGID: u64 = 154;
const SYS_GETPGID: u64 = 155;
const SYS_GETSID: u64 = 156;
const SYS_SETSID: u64 = 157;
const SYS_SETHOSTNAME: u64 = 161;
const SYS_GETRUSAGE: u64 = 165;
const SYS_UMASK: u64 = 166;
const SYS_SYSINFO: u64 = 179;
const SYS_UTIMENSAT: u64 = 88;
const SYS_FACCESSAT2: u64 = 439;
const SYS_FTRUNCATE: u64 = 46;
const SYS_FALLOCATE: u64 = 47;
const SYS_SOCKET: u64 = 198;
const SYS_SOCKETPAIR: u64 = 199;
const SYS_BIND: u64 = 200;
const SYS_LISTEN: u64 = 201;
const SYS_ACCEPT: u64 = 202;
const SYS_CONNECT: u64 = 203;
const SYS_GETSOCKNAME: u64 = 204;
const SYS_GETPEERNAME: u64 = 205;
const SYS_SENDTO: u64 = 206;
const SYS_RECVFROM: u64 = 207;
const SYS_SETSOCKOPT: u64 = 208;
const SYS_GETSOCKOPT: u64 = 209;
const SYS_SHUTDOWN: u64 = 210;
const SYS_SENDMSG: u64 = 211;
const SYS_RECVMSG: u64 = 212;
const SYS_ACCEPT4: u64 = 242;

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

/// Kernel namespaces that stay global in the Linux view. Everything else an
/// absolute path names lives in the SFS mount: Linux's `/` **is** the SFS
/// root, the layout a postmarketOS-style rootfs expects.
const GLOBAL_PREFIXES: [&str; 5] = ["/sfs", "/tmp", "/dev", "/proc", "/sys"];

/// Maps a Linux absolute path into the kernel's VFS tree.
///
/// `--no-linux` tasks never call this. `/sfs` stays an alias so the existing
/// Arch-staged Phosh image (which was patched to `/sfs/...`) keeps working.
pub fn map_absolute(path: &str) -> String {
    for prefix in GLOBAL_PREFIXES {
        if path == prefix
            || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
        {
            return String::from(path);
        }
    }
    let mut mapped = String::with_capacity(path.len() + 4);
    mapped.push_str("/sfs");
    mapped.push_str(path);
    mapped
}

/// Resolves a Linux path (relative to `cwd`) into the VFS tree.
pub fn resolve(cwd: &str, path: &str) -> Result<String, ()> {
    if path.starts_with('/') {
        crate::vfs::resolve("/", &map_absolute(path))
    } else {
        crate::vfs::resolve(cwd, path)
    }
}

/// Like [`resolve`], but the result must be a directory.
pub fn resolve_directory(cwd: &str, path: &str) -> Result<String, ()> {
    let resolved = resolve(cwd, path)?;
    if crate::vfs::resolve_directory("/", &resolved).is_ok() {
        Ok(resolved)
    } else {
        Err(())
    }
}

/// The reverse mapping for `getcwd`: hide the `/sfs` mount point.
pub fn display_path(path: &str) -> String {
    if path == "/sfs" {
        return String::from("/");
    }
    match path.strip_prefix("/sfs/") {
        Some(rest) => {
            let mut shown = String::with_capacity(rest.len() + 1);
            shown.push('/');
            shown.push_str(rest);
            shown
        }
        None => String::from(path),
    }
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

/// Watch ids for the inotify stub.
static INOTIFY_WD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// Last-N syscall trace of Linux guest tasks (nr, arg0, arg1, result, task).
pub const TRACE_RING_LEN: usize = 8192;
pub(crate) static TRACE_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut TRACE_RING: [[u64; 5]; TRACE_RING_LEN] = [[0; 5]; TRACE_RING_LEN];

/// `sched_getattr(2)`: SCHED_OTHER, no runtime constraints. The caller's
/// size field is untrusted - a kernel Vec with a user capacity would panic
/// the allocator, so anything past the struct cap reports EINVAL.
fn linux_sched_getattr(attr_pointer: u64, size: u64) -> u64 {
    if attr_pointer == 0 || size < 48 || size > 256 {
        return errno(EINVAL);
    }
    let mut attr = alloc::vec![0_u8; size as usize];
    attr[0..4].copy_from_slice(&((size as u32).to_le_bytes())); // size
    attr[4..8].copy_from_slice(&0_u32.to_le_bytes()); // SCHED_OTHER
    if user_memory::copy_to_user(&attr, attr_pointer).is_err() {
        return errno(EFAULT);
    }
    0
}

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

pub fn dispatch(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) -> u64 {
    let number = frame[8];
    let result = dispatch_body(frame);
    // Last-N syscall trace of the Linux guest stack: (nr, arg0, arg1,
    // result, task). With the fd numbers visible, a "which fd should have
    // been open" question answers itself from the open/close history around
    // the failing call. Native tasks (init, shell, net_server) have small
    // ids and are skipped; the boot-session's OpenRC helpers stay below 120
    // as well, so their sysinit loops cannot flush the window.
    // mprotect/prlimit/futex floods from Mesa/GLib would wash the ring out;
    // they are never the interesting failures.
    if crate::scheduler::current_task_id() >= 120
        && !matches!(number, 226 | 172 | 141 | 119 | 120 | 274 | 275 | 113)
    {
        // futex (98) is traced for the engine-idle investigation; clock_gettime
        // (113) is filtered — a busy compositor loop emits thousands per
        // minute and drowns the failure window. Dumps must stay close to the
        // moment either way.
        let index = TRACE_RING_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let slot = index % TRACE_RING_LEN;
        unsafe {
            TRACE_RING[slot][0] = number;
            TRACE_RING[slot][1] = frame[0];
            TRACE_RING[slot][2] = frame[1];
            TRACE_RING[slot][3] = result;
            TRACE_RING[slot][4] = crate::scheduler::current_task_id();
        }
    }
    if (result as i64) < 0 {
        let code = (result as i64).unsigned_abs();
        #[cfg(feature = "linux-abi-trace")]
        {
            crate::console::write_str("LINUX ERR ");
            crate::console::write_dec_u64(number);
            crate::console::write_str(" -> ");
            crate::console::write_dec_u64(code);
            crate::console::write_str("\r\n");
        }
        // EACCES/ENOSYS/EPROTONOSUPPORT/EAFNOSUPPORT carry the boot-blocking
        // failures; the noisy idle loop (ENOENT/ECHILD/ENOMEM) must not flush
        // it.
        if code == EACCES || code == ENOSYS || code == EPROTONOSUPPORT || code == EAFNOSUPPORT {
            let index = FAIL_RING_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let mut path = [0_u8; 48];
            // The path argument is arg1 for every path syscall (openat,
            // faccessat, mkdirat, unlinkat, newfstatat, statx...).
            let _ = user_memory::copy_from_user(frame[1], &mut path);
            let slot = index % FAIL_RING_LEN;
            let mut record = [0_u64; 8];
            record[0] = number;
            record[1] = code;
            record[2] = frame[0];
            record[3] = frame[1];
            record[4] = frame[2];
            for (word, bytes) in record[5..].iter_mut().zip(path.chunks_exact(8)) {
                *word = u64::from_le_bytes(bytes.try_into().unwrap());
            }
            unsafe {
                FAIL_RING[slot] = record;
            }
        }
    }
    result
}

fn dispatch_body(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) -> u64 {
    let number = frame[8];
    let args = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
    dispatch_inner(frame, number, args)
}

fn dispatch_inner(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    number: u64,
    args: [u64; 6],
) -> u64 {
    match number {
        SYS_WRITE => linux_write(args[0], args[1], args[2]),
        SYS_WRITEV => linux_writev(args[0], args[1], args[2]),
        SYS_READV => linux_readv(args[0], args[1], args[2]),
        SYS_SENDFILE => linux_sendfile(args[0], args[1], args[2], args[3]),
        SYS_COPY_FILE_RANGE => linux_copy_file_range(
            args[0], args[1], args[2], args[3], args[4], args[5],
        ),
        SYS_READ => finish(linux_fd::read(scheduler::current_task_id(), args[0], args[1], args[2])),
        SYS_OPENAT => linux_openat(args[0], args[1], args[2], args[3]),
        SYS_EPOLL_CREATE1 => linux_epoll_create(args[0]),
        SYS_EPOLL_CTL => linux_epoll_ctl(args[0], args[1], args[2], args[3]),
        SYS_EPOLL_PWAIT => linux_epoll_wait(args[0], args[1], args[2], args[3] as i64),
        SYS_EPOLL_PWAIT2 => linux_epoll_wait2(args[0], args[1], args[2], args[3]),
        SYS_TIMERFD_CREATE => linux_timerfd_create(args[0], args[1]),
        SYS_TIMERFD_SETTIME => linux_timerfd_settime(args[0], args[1], args[2], args[3]),
        SYS_TIMERFD_GETTIME => linux_timerfd_gettime(args[0], args[1]),
        SYS_EVENTFD2 => finish(linux_fd::eventfd(
            scheduler::current_task_id(),
            args[0],
            args[1] & 0x800 != 0,
        )),
        SYS_MEMFD_CREATE => finish(linux_fd::memfd(scheduler::current_task_id())),
        SYS_FTRUNCATE => finish(
            linux_fd::truncate(
                scheduler::current_task_id(),
                args[0],
                args[1],
            )
            .map(|_| 0),
        ),
        SYS_FALLOCATE => {
            if args[1] != 0 {
                errno(ENOSYS)
            } else {
                finish(
                    linux_fd::memfd_fallocate(
                        scheduler::current_task_id(),
                        args[0],
                        args[2] as usize,
                        args[3] as usize,
                    )
                    .map(|_| 0),
                )
            }
        }
        SYS_FUTEX => linux_futex(args[0], args[1], args[2], args[3]),
        SYS_PIPE2 => linux_pipe2(args[0], args[1]),
        SYS_POLL => finish(linux_poll(
            args[0],
            args[1],
            args[2] as i32 as i64,
        )),
        SYS_PPOLL => finish(linux_ppoll(args[0], args[1], args[2])),
        SYS_CLOSE => finish(linux_fd::close(scheduler::current_task_id(), args[0]).map(|_| 0)),
        SYS_PREAD64 => linux_pread(args[0], args[1], args[2], args[3]),
        SYS_PWRITE64 => linux_pwrite(args[0], args[1], args[2], args[3]),
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
        SYS_MKDIRAT => linux_mkdirat(args[0], args[1], args[2]),
        SYS_UNLINKAT => linux_unlinkat(args[0], args[1], args[2]),
        SYS_RENAMEAT => linux_renameat(args[0], args[1], args[2], args[3], 0),
        SYS_RENAMEAT2 => linux_renameat(args[0], args[1], args[2], args[3], args[4]),
        SYS_IOCTL => linux_ioctl(args[0], args[1], args[2]),
        SYS_FSTAT => finish(linux_fd::fstat(scheduler::current_task_id(), args[0], args[1]).map(|_| 0)),
        SYS_NEWFSTATAT => linux_newfstatat(args[0], args[1], args[2], args[3]),
        SYS_STATX => linux_statx(args[0], args[1], args[2], args[3], args[4]),
        SYS_FCHMOD | SYS_FCHOWN => linux_fchmod(args[0]),
        SYS_FCHMODAT | SYS_FCHOWNAT => linux_fchmodat(args[0], args[1]),
        SYS_CLOSE_RANGE => linux_close_range(args[0], args[1], args[2]),
        SYS_MOUNT => linux_mount(args[0], args[1], args[2], args[3]),
        SYS_UMOUNT2 => linux_umount(args[0], args[1]),
        // The Linux root already is the SFS mount; a chroot cannot narrow it
        // further because every path is mapped onto `/sfs`.
        SYS_CHROOT => 0,
        SYS_PIVOT_ROOT => errno(ENOSYS),
        SYS_SETSID => scheduler::current_tgid(),
        SYS_SETPGID | SYS_SETHOSTNAME => 0,
        SYS_GETPGID | SYS_GETSID => scheduler::current_tgid(),
        SYS_UMASK => 0o22,
        SYS_GETRUSAGE => linux_zero_struct(args[1], 144),
        SYS_SYSINFO => linux_zero_struct(args[0], 112),
        SYS_STATFS => linux_zero_struct(args[1], 120),
        SYS_FSTATFS => linux_zero_struct(args[2], 120),
        SYS_UTIMENSAT => 0,
        SYS_FACCESSAT2 => linux_faccessat(args[0], args[1], args[2]),
        SYS_FACCESSAT => linux_faccessat(args[0], args[1], args[2]),
        SYS_GETDENTS64 => finish(linux_fd::getdents64(
            scheduler::current_task_id(),
            args[0],
            args[1],
            args[2],
        )),
        SYS_READLINKAT => linux_readlinkat(args[0], args[1], args[2], args[3]),
        SYS_NANOSLEEP => linux_nanosleep(args[0], args[1]),
        SYS_CLOCK_NANOSLEEP => linux_nanosleep(args[2], args[3]),
        SYS_CLOCK_GETRES => linux_clock_getres(args[1]),
        SYS_GETTIMEOFDAY => linux_gettimeofday(args[0]),
        SYS_SCHED_YIELD => {
            scheduler::yield_current();
            0
        }
        SYS_EXIT => scheduler::exit_current_with_status(args[0]),
        SYS_EXIT_GROUP => scheduler::exit_group(args[0]),
        SYS_EXECVE => linux_execve(frame, args[0], args[1], args[2]),
        SYS_CLONE => linux_clone(frame, args[0], args[1], args[2], args[3], args[4]),
        SYS_WAIT4 => finish(linux_wait4(args[0] as i64, args[1], args[2])),
        SYS_SET_TID_ADDRESS => scheduler::current_task_id(),
        SYS_SET_ROBUST_LIST => 0, // single-threaded: nothing to register
        SYS_CLOCK_GETTIME => linux_clock_gettime(args[0], args[1]),
        SYS_RT_SIGPROCMASK => linux_rt_sigprocmask(args[0], args[1], args[2]),
        SYS_SIGNALFD4 => linux_signalfd4(args[0], args[1], args[2], args[3]),
        SYS_FSYNC | SYS_FDATASYNC => {
            // The VFS is RAM-backed (scratch tree) or writes straight through
            // to the SFS image; there is no dirty page cache to flush.
            finish(Ok(0))
        }
        SYS_RT_SIGTIMEDWAIT => linux_rt_sigtimedwait(args[0], args[1], args[3]),
        SYS_SCHED_SETPARAM => 0,
        // SCHED_OTHER for everything: the worker-priority warnings from
        // deniald's cpu_scheduling want a clean "accepted" answer.
        SYS_SCHED_SETSCHEDULER => {
            if args[1] > 2 {
                errno(EINVAL)
            } else {
                0
            }
        }
        // The policy is the return value itself (0 = SCHED_OTHER).
        SYS_SCHED_GETSCHEDULER => 0,
        SYS_INOTIFY_INIT1 => finish(linux_fd::inotify_init(
            scheduler::current_task_id(),
            args[0] & 0x800 != 0,
        )),
        SYS_INOTIFY_ADD_WATCH => {
            if !linux_fd::is_inotify(scheduler::current_task_id(), args[0]) {
                errno(EBADF)
            } else {
                // Static watch ids: the instance never reports events, so the
                // id only needs to be valid for rm_watch.
                INOTIFY_WD.fetch_add(1, core::sync::atomic::Ordering::Relaxed) as u64
            }
        }
        SYS_INOTIFY_RM_WATCH => {
            if !linux_fd::is_inotify(scheduler::current_task_id(), args[0]) {
                errno(EBADF)
            } else {
                0
            }
        }
        SYS_SET_PRIORITY => 0,
        // The nice value is the return value itself.
        SYS_GET_PRIORITY => 0,
        SYS_SCHED_SETATTR => 0,
        SYS_SCHED_GETATTR => linux_sched_getattr(args[1], args[2]),
        SYS_CLOCK_ADJTIME => 0,
        SYS_RT_SIGACTION => linux_rt_sigaction(args[0], args[1], args[2]),
        SYS_RT_SIGRETURN => {
            crate::linux_signal::restore_frame(frame);
            0
        }
        SYS_SIGALTSTACK => linux_sigaltstack(args[0], args[1]),
        SYS_KILL => linux_kill(args[0] as i64, args[1]),
        SYS_TGKILL => linux_kill(args[1] as i64, args[2]),
        SYS_UNAME => linux_uname(args[0]),
        SYS_GETPID => scheduler::current_linux_pid(),
        SYS_GETTID => scheduler::current_task_id(),
        SYS_GETPPID => scheduler::current_linux_ppid(),
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_GETRLIMIT => linux_getrlimit(args[0], args[1]),
        SYS_SCHED_GETAFFINITY => linux_sched_getaffinity(args[1], args[2]),
        SYS_PRCTL => 0,
        SYS_BRK => linux_brk(args[0]),
        SYS_MUNMAP => linux_munmap(args[0], args[1]),
        SYS_MMAP => linux_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYS_MPROTECT => linux_mprotect(args[0], args[1], args[2]),
        SYS_MADVISE => 0,
        SYS_PRLIMIT64 => {
            if args[3] != 0 {
                linux_getrlimit(args[1], args[3])
            } else {
                0
            }
        }
        SYS_GETRANDOM => linux_getrandom(args[0], args[1], args[2]),
        SYS_SOCKETPAIR => linux_socketpair(args[0], args[1], args[2], args[3]),
        SYS_SENDMSG => linux_sendmsg(args[0], args[1], args[2]),
        SYS_RECVMSG => linux_recvmsg(args[0], args[1], args[2]),
        SYS_SENDTO => linux_sendto(args[0], args[1], args[2], args[3]),
        SYS_RECVFROM => linux_recvfrom(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYS_SHUTDOWN => finish(
            linux_fd::socket_shutdown(scheduler::current_task_id(), args[0], args[1]).map(|_| 0),
        ),
        SYS_GETSOCKNAME | SYS_GETPEERNAME => {
            linux_sockname(scheduler::current_task_id(), args[0], args[1], args[2])
        }
        SYS_SETSOCKOPT => linux_setsockopt(args[0], args[1]),
        SYS_GETSOCKOPT => linux_getsockopt(args[0], args[1], args[2], args[3], args[4]),
        SYS_FLOCK => linux_flock(args[0], args[1]),
        SYS_SOCKET => linux_socket(args[0], args[1], args[2]),
        SYS_BIND => linux_bind(args[0], args[1], args[2]),
        SYS_LISTEN => finish(
            linux_fd::listen(scheduler::current_task_id(), args[0], args[1]).map(|_| 0),
        ),
        SYS_CONNECT => linux_connect(args[0], args[1], args[2]),
        SYS_ACCEPT => linux_accept(args[0], args[1], args[2], 0),
        SYS_ACCEPT4 => linux_accept(args[0], args[1], args[2], args[3]),
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

/// `execve(2)`: replace the process image. On success the syscall trampoline
/// must return to a different EL0 entry and stack, so this mutates the saved
/// frame (ELR) and SP_EL0 instead of returning normally.
fn linux_execve(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    path_pointer: u64,
    argv_pointer: u64,
    envp_pointer: u64,
) -> u64 {
    const EXEC_LIMIT: usize = 32 * 1024 * 1024;
    // `execve` from a `CLONE_VM` sibling would build a new address space and
    // then release the leader's, pulling the tables out from under every
    // running thread. Until a thread takes over the whole group, refuse
    // instead of corrupting the process.
    if scheduler::current_is_thread() {
        return errno(EINVAL);
    }
    let Some(path) = user_string(path_pointer) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        return errno(ENOENT);
    }
    crate::console::write_str(&path);
    crate::console::write_str("\r\n");
    let Some(argv) = user_string_array(argv_pointer) else {
        return errno(EFAULT);
    };
    let Some(envp) = user_string_array(envp_pointer) else {
        return errno(EFAULT);
    };
    let cwd = scheduler::current_task_cwd();
    let mut target = path;
    let mut arguments = argv;
    let mut depth = 0u32;
    let resolved = loop {
        let Ok(resolved) = resolve(&cwd, &target) else {
            crate::console::write_str("EXECFAIL resolve ");
            crate::console::write_str(&target);
            crate::console::write_str("\r\n");
            return errno(ENOENT);
        };
        if !scheduler::current_read_path_allowed(&resolved) {
            return errno(EACCES);
        }
        match crate::vfs::file_len(&cwd, &resolved) {
            Ok(length) if length <= EXEC_LIMIT => {}
            _ => {
                crate::console::write_str("EXECFAIL len ");
                crate::console::write_str(&resolved);
                crate::console::write_str("\r\n");
                return errno(ENOENT);
            }
        }
        // A `#!` script is executed through its interpreter, like Linux: the
        // kernel never tries to load shell text as an ELF.
        let header = crate::vfs::read_at(&cwd, &resolved, 0, 256).unwrap_or_default();
        if header.starts_with(b"#!") {
            depth += 1;
            if depth > 4 {
                return errno(ENOEXEC);
            }
            let Some((interpreter, argument)) = parse_shebang(&header[2..]) else {
                return errno(ENOEXEC);
            };
            let mut next = Vec::with_capacity(arguments.len() + 2);
            next.push(interpreter.clone().into_bytes());
            if let Some(argument) = argument {
                next.push(argument.into_bytes());
            }
            next.push(target.clone().into_bytes());
            for item in arguments.iter().skip(1) {
                next.push(item.clone());
            }
            arguments = next;
            target = interpreter;
            continue;
        }
        break resolved;
    };
    let Ok(image) = crate::linux_elf::load_linux_program_path(&cwd, &resolved, EXEC_LIMIT) else {
        return errno(ENOEXEC);
    };
    let Some(result) = scheduler::exec_linux_current(image, arguments, envp) else {
        return errno(ENOMEM);
    };
    // A successful exec closes every `FD_CLOEXEC` descriptor. Rust's
    // `Command::spawn` relies on this to close the child end of its
    // exec-error socketpair and unblock the parent.
    linux_fd::close_cloexec(scheduler::current_task_id());
    // ELR is frame word 31 (x0-x30, ELR, SPSR). The user stack pointer lives
    // in SP_EL0, which the trampoline does not touch.
    frame[31] = result.entry as u64;
    unsafe {
        core::arch::asm!("msr sp_el0, {}", in(reg) result.stack_pointer);
    }
    0
}

/// Parses the rest of a `#!` line: the interpreter path and at most one
/// optional argument (Linux passes the remainder as one argument).
fn parse_shebang(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    let text = core::str::from_utf8(line).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let (interpreter, rest) = match text.split_once(|character: char| character.is_whitespace()) {
        Some((interpreter, rest)) => (interpreter, rest.trim()),
        None => (text, ""),
    };
    if interpreter.is_empty() {
        return None;
    }
    Some((
        String::from(interpreter),
        if rest.is_empty() {
            None
        } else {
            Some(String::from(rest))
        },
    ))
}

/// `clone(2)` with fork semantics (`SIGCHLD`, no shared VM). Thread creation
/// is not supported yet and is reported as `-ENOSYS`.
fn linux_clone(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    flags: u64,
    stack: u64,
    parent_tid: u64,
    tls: u64,
    child_tid: u64,
) -> u64 {
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_VFORK: u64 = 0x0000_4000;
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_THREAD: u64 = 0x0001_0000;
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

    if flags & CLONE_VM != 0 {
        if flags & CLONE_VFORK != 0 && flags & CLONE_THREAD == 0 {
            // `vfork`: BusyBox, dash and musl's `posix_spawn` compile spawn
            // paths against it. Real vfork shares the address space and
            // suspends the parent until the child execs; a full copy is
            // semantically safe for the `vfork`/`execve` pattern and keeps
            // the parent runnable. A non-zero `stack` is musl's `__clone`
            // trampoline: the child must start on *that* stack, because the
            // function pointer it calls is stored at its top.
            return match scheduler::fork_current_with_sp(
                frame,
                if stack != 0 { Some(stack) } else { None },
            ) {
                Some(pid) => {
                    if flags & CLONE_PARENT_SETTID != 0 && parent_tid != 0 {
                        let _ = user_memory::copy_to_user(
                            &(pid as u32).to_le_bytes(),
                            parent_tid,
                        );
                    }
                    pid
                }
                None => errno(EAGAIN),
            };
        }
        if flags & CLONE_THREAD == 0 {
            // Shared VM without a thread group would need a process object.
            return errno(ENOSYS);
        }
        if stack == 0 {
            return errno(EINVAL);
        }
        let clear = if flags & CLONE_CHILD_CLEARTID != 0 {
            Some(child_tid)
        } else {
            None
        };
        return match scheduler::spawn_thread(
            frame,
            stack,
            if flags & CLONE_SETTLS != 0 { tls } else { 0 },
            parent_tid,
            child_tid,
            flags & CLONE_PARENT_SETTID != 0,
            flags & CLONE_CHILD_SETTID != 0,
            clear,
        ) {
            Some(tid) => tid,
            None => errno(EAGAIN),
        };
    }

    // A plain `fork` may still pass a stack/TLS pointer (musl's `fork`
    // forwards TPIDR_EL0); `fork_current` copies the parent's TLS, so both
    // are ignored rather than rejected.
    if flags & CLONE_THREAD != 0 || flags & CLONE_SIGHAND != 0 {
        return errno(EINVAL);
    }
    match scheduler::fork_current(frame) {
        Some(pid) => {
            crate::console::write_dec_u64(pid);
            crate::console::write_str("\r\n");
            if flags & CLONE_PARENT_SETTID != 0 && parent_tid != 0 {
                let _ = user_memory::copy_to_user(&(pid as u32).to_le_bytes(), parent_tid);
            }
            if flags & (CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID) != 0 && child_tid != 0 {
                let _ = scheduler::set_child_tid(
                    pid,
                    child_tid,
                    pid as u32,
                    flags & CLONE_CHILD_CLEARTID != 0,
                );
            }
            pid
        }
        None => errno(EAGAIN),
    }
}

/// `wait4(2)`. The exit status is encoded the Linux way: normal exit is
/// `(code & 0xff) << 8`, which makes `WIFEXITED` true and `WEXITSTATUS` return
/// the low byte.
fn linux_wait4(pid: i64, status_pointer: u64, options: u64) -> Result<u64, u64> {
    const WNOHANG: u64 = 1;
    match scheduler::wait_child(pid, options & WNOHANG != 0) {
        scheduler::WaitChild::Exited(child, status) => {
            // A user SIGCHLD handler must still run after the reap; only
            // default/ignored dispositions consume the pending bit here.
            let task = scheduler::current_task_id();
            let handler = crate::linux_signal::action(task, crate::linux_signal::SIGCHLD)
                .map(|action| action.handler)
                .unwrap_or(crate::linux_signal::SIG_DFL);
            if handler == crate::linux_signal::SIG_DFL
                || handler == crate::linux_signal::SIG_IGN
            {
                crate::linux_signal::clear_pending(task, crate::linux_signal::SIGCHLD);
            }
            if status_pointer != 0 {
                let word = ((status & 0xff) << 8) as u32;
                user_memory::copy_to_user(&word.to_le_bytes(), status_pointer)
                    .map_err(|_| EFAULT)?;
            }
            Ok(child)
        }
        scheduler::WaitChild::StillRunning => Ok(0),
        scheduler::WaitChild::NoChild => Err(ECHILD),
    }
}

/// `pipe2(2)`: installs a read and a write descriptor.
fn linux_epoll_create(flags: u64) -> u64 {
    // EPOLL_CLOEXEC is the only flag; this ABI has no exec-time descriptor
    // closing anyway.
    if flags & !0x80000 != 0 {
        return errno(EINVAL);
    }
    finish(linux_fd::epoll_create(scheduler::current_task_id()))
}

fn linux_epoll_ctl(epfd: u64, operation: u64, fd: u64, event_pointer: u64) -> u64 {
    const EPOLL_CTL_DEL: u64 = 2;
    let (events, data) = if operation == EPOLL_CTL_DEL {
        (0u32, 0u64)
    } else {
        if event_pointer == 0 {
            return errno(EFAULT);
        }
        let mut raw = [0_u8; 16];
        if user_memory::copy_from_user(event_pointer, &mut raw).is_err() {
            return errno(EFAULT);
        }
        (
            u32::from_le_bytes(raw[0..4].try_into().unwrap()),
            u64::from_le_bytes(raw[8..16].try_into().unwrap()),
        )
    };
    finish(
        linux_fd::epoll_ctl(
            scheduler::current_task_id(),
            epfd,
            operation,
            fd,
            events,
            data,
        )
        .map(|_| 0),
    )
}

fn linux_epoll_wait(
    epfd: u64,
    events_pointer: u64,
    max_events: u64,
    timeout_ms: i64,
) -> u64 {
    // glibc's `epoll_pwait` asks for 1024 slots; only the entries actually
    // ready are written, so any user-chosen size is safe here. Reject only a
    // zero or absurd request.
    if max_events == 0 || max_events > 65_536 {
        return errno(EINVAL);
    }
    let ready = match linux_fd::epoll_wait(
        scheduler::current_task_id(),
        epfd,
        max_events as usize,
        timeout_ms,
    ) {
        Ok(ready) => ready,
        Err(code) => return errno(code),
    };
    for (index, (events, data)) in ready.iter().enumerate() {
        let mut raw = [0_u8; 16];
        raw[0..4].copy_from_slice(&events.to_le_bytes());
        raw[8..16].copy_from_slice(&data.to_le_bytes());
        if user_memory::copy_to_user(&raw, events_pointer + (index * 16) as u64).is_err() {
            return errno(EFAULT);
        }
    }
    ready.len() as u64
}

/// `epoll_pwait2`: the timeout is a `timespec` pointer, not milliseconds.
fn linux_epoll_wait2(
    epfd: u64,
    events_pointer: u64,
    max_events: u64,
    timeout_pointer: u64,
) -> u64 {
    let timeout_ms = if timeout_pointer == 0 {
        -1
    } else {
        let mut raw = [0_u8; 16];
        if user_memory::copy_from_user(timeout_pointer, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let seconds = i64::from_le_bytes(raw[..8].try_into().unwrap());
        let nanoseconds = i64::from_le_bytes(raw[8..].try_into().unwrap());
        if seconds < 0 {
            -1
        } else {
            seconds
                .saturating_mul(1000)
                .saturating_add(nanoseconds / 1_000_000)
        }
    };
    linux_epoll_wait(epfd, events_pointer, max_events, timeout_ms)
}

fn ticks_from_timespec(seconds: u64, nanoseconds: u64) -> u64 {
    seconds
        .saturating_mul(100)
        .saturating_add(nanoseconds / 10_000_000)
}

fn write_itimerspec(pointer: u64, interval: u64, remaining: u64) -> Result<(), u64> {
    let mut raw = [0_u8; 32];
    raw[0..8].copy_from_slice(&(interval / 100).to_le_bytes());
    raw[8..16].copy_from_slice(&((interval % 100) * 10_000_000).to_le_bytes());
    raw[16..24].copy_from_slice(&(remaining / 100).to_le_bytes());
    raw[24..32].copy_from_slice(&((remaining % 100) * 10_000_000).to_le_bytes());
    user_memory::copy_to_user(&raw, pointer).map_err(|_| EFAULT)
}

fn linux_timerfd_create(_clock: u64, flags: u64) -> u64 {
    if flags & !(0x800 | 0x80000) != 0 {
        return errno(EINVAL);
    }
    finish(linux_fd::timerfd_create(
        scheduler::current_task_id(),
        flags & 0x800 != 0,
    ))
}

fn linux_timerfd_settime(fd: u64, flags: u64, new_pointer: u64, old_pointer: u64) -> u64 {
    const TFD_TIMER_ABSTIME: u64 = 1;
    if flags & !3 != 0 || new_pointer == 0 {
        return errno(EINVAL);
    }
    let task = scheduler::current_task_id();
    if old_pointer != 0 {
        let (interval, remaining) = match linux_fd::timerfd_gettime(task, fd) {
            Ok(state) => state,
            Err(code) => return errno(code),
        };
        if write_itimerspec(old_pointer, interval, remaining).is_err() {
            return errno(EFAULT);
        }
    }
    let mut raw = [0_u8; 32];
    if user_memory::copy_from_user(new_pointer, &mut raw).is_err() {
        return errno(EFAULT);
    }
    let interval = ticks_from_timespec(
        u64::from_le_bytes(raw[0..8].try_into().unwrap()),
        u64::from_le_bytes(raw[8..16].try_into().unwrap()),
    );
    let value = ticks_from_timespec(
        u64::from_le_bytes(raw[16..24].try_into().unwrap()),
        u64::from_le_bytes(raw[24..32].try_into().unwrap()),
    );
    finish(
        linux_fd::timerfd_settime(task, fd, interval, value, flags & TFD_TIMER_ABSTIME != 0)
            .map(|_| 0),
    )
}

fn linux_timerfd_gettime(fd: u64, current_pointer: u64) -> u64 {
    let (interval, remaining) =
        match linux_fd::timerfd_gettime(scheduler::current_task_id(), fd) {
            Ok(state) => state,
            Err(code) => return errno(code),
        };
    if write_itimerspec(current_pointer, interval, remaining).is_err() {
        return errno(EFAULT);
    }
    0
}

fn linux_pipe2(fds_pointer: u64, flags: u64) -> u64 {
    const O_NONBLOCK: u64 = 0o4000;
    const O_CLOEXEC: u64 = 0o2000000;
    let task = scheduler::current_task_id();
    let Ok((read_fd, write_fd)) = linux_fd::pipe(task) else {
        return errno(EMFILE);
    };
    if flags & O_NONBLOCK != 0 {
        let _ = linux_fd::set_nonblock(task, read_fd, true);
        let _ = linux_fd::set_nonblock(task, write_fd, true);
    }
    if flags & O_CLOEXEC != 0 {
        let _ = linux_fd::set_cloexec(task, read_fd, true);
        let _ = linux_fd::set_cloexec(task, write_fd, true);
    }
    let mut pair = [0_u8; 8];
    pair[..4].copy_from_slice(&(read_fd as u32).to_le_bytes());
    pair[4..].copy_from_slice(&(write_fd as u32).to_le_bytes());
    if user_memory::copy_to_user(&pair, fds_pointer).is_err() {
        let _ = linux_fd::close(task, read_fd);
        let _ = linux_fd::close(task, write_fd);
        return errno(EFAULT);
    }
    0
}

/// `poll(2)`: same readiness loop as `ppoll`, timeout in milliseconds
/// (-1 = block, capped like `ppoll` so a stuck poll cannot wedge the kernel).
/// seatd's main loop lives on this call; without it glibc `poll()` returns
/// ENOSYS and the daemon exits immediately.
fn linux_poll(
    fds_pointer: u64,
    nfds: u64,
    timeout_ms: i64,
) -> Result<u64, u64> {
    let timeout = if timeout_ms < 0 {
        None
    } else {
        Some(timeout_ms as u64)
    };
    poll_loop(fds_pointer, nfds, timeout)
}

/// `ppoll(2)`: readiness for up to 32 descriptors, with a timeout. A null
/// timeout blocks, but is capped at 60 s so a stuck poll cannot wedge the
/// kernel forever (there is no signal delivery to break it yet).
fn linux_ppoll(
    fds_pointer: u64,
    nfds: u64,
    timeout_pointer: u64,
) -> Result<u64, u64> {
    let timeout_ms = if timeout_pointer == 0 {
        None
    } else {
        let mut bytes = [0_u8; 16];
        user_memory::copy_from_user(timeout_pointer, &mut bytes).map_err(|_| EFAULT)?;
        let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
        let nanoseconds = i64::from_le_bytes(bytes[8..].try_into().unwrap());
        if seconds < 0 || nanoseconds < 0 {
            None
        } else {
            Some(
                (seconds as u64)
                    .saturating_mul(1000)
                    .saturating_add(nanoseconds as u64 / 1_000_000),
            )
        }
    };
    poll_loop(fds_pointer, nfds, timeout_ms)
}

fn poll_loop(
    fds_pointer: u64,
    nfds: u64,
    timeout_ms: Option<u64>,
) -> Result<u64, u64> {
    const POLLERR: i16 = 0x008;
    const POLLHUP: i16 = 0x010;
    const POLLNVAL: i16 = 0x020;
    if nfds > 32 {
        return Err(EINVAL);
    }
    let count = nfds as usize;
    let start = timer::ticks();
    let deadline = timeout_ms.map(|ms| start + timer::milliseconds_to_ticks(ms));
    loop {
        let mut ready = 0u64;
        let mut entries: Vec<(i32, i16, i16)> = Vec::with_capacity(count);
        for index in 0..count {
            let address = fds_pointer + (index * 8) as u64;
            let mut raw = [0_u8; 8];
            user_memory::copy_from_user(address, &mut raw).map_err(|_| EFAULT)?;
            let fd = i32::from_le_bytes(raw[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(raw[4..6].try_into().unwrap());
            let revents = if fd < 0 {
                0
            } else {
                match linux_fd::poll_ready(scheduler::current_task_id(), fd as u64) {
                    Some(bits) => bits & (events | POLLERR | POLLHUP),
                    None => POLLNVAL,
                }
            };
            if revents != 0 {
                ready += 1;
            }
            entries.push((fd, events, revents));
        }
        let expired = match deadline {
            Some(deadline) => timer::ticks() >= deadline,
            None => timer::ticks().saturating_sub(start) >= 6000,
        };
        if ready != 0 || expired || timeout_ms == Some(0) {
            for (index, &(fd, events, revents)) in entries.iter().enumerate() {
                let mut raw = [0_u8; 8];
                raw[0..4].copy_from_slice(&fd.to_le_bytes());
                raw[4..6].copy_from_slice(&events.to_le_bytes());
                raw[6..8].copy_from_slice(&revents.to_le_bytes());
                user_memory::copy_to_user(&raw, fds_pointer + (index * 8) as u64)
                    .map_err(|_| EFAULT)?;
            }
            return Ok(ready);
        }
        scheduler::sleep(1);
    }
}

/// One `struct msghdr` from userspace (LP64 layout).
struct MsgHdr {
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
    flags: i32,
}

fn read_msghdr(pointer: u64) -> Result<MsgHdr, u64> {
    if pointer == 0 {
        return Err(EFAULT);
    }
    let mut raw = [0_u8; 56];
    user_memory::copy_from_user(pointer, &mut raw).map_err(|_| EFAULT)?;
    Ok(MsgHdr {
        iov: u64::from_le_bytes(raw[16..24].try_into().unwrap()),
        iovlen: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
        control: u64::from_le_bytes(raw[32..40].try_into().unwrap()),
        controllen: u64::from_le_bytes(raw[40..48].try_into().unwrap()),
        flags: i32::from_le_bytes(raw[48..52].try_into().unwrap()),
    })
}

/// Gathers the `iovec` array into one contiguous payload.
fn read_iovecs(header: &MsgHdr, limit: usize) -> Result<Vec<u8>, u64> {
    if header.iovlen > 64 {
        return Err(EINVAL);
    }
    let mut bytes = Vec::new();
    for index in 0..header.iovlen {
        let mut raw = [0_u8; 16];
        user_memory::copy_from_user(header.iov + index * 16, &mut raw).map_err(|_| EFAULT)?;
        let base = u64::from_le_bytes(raw[..8].try_into().unwrap());
        let length = u64::from_le_bytes(raw[8..].try_into().unwrap());
        let length = usize::try_from(length).map_err(|_| EINVAL)?;
        if bytes.len().saturating_add(length) > limit {
            return Err(EINVAL);
        }
        let start = bytes.len();
        bytes.resize(start + length, 0);
        if length != 0 {
            user_memory::copy_from_user(base, &mut bytes[start..]).map_err(|_| EFAULT)?;
        }
    }
    Ok(bytes)
}

/// Scatters up to `bytes.len()` bytes back into the caller's `iovec` array.
fn write_iovecs(header: &MsgHdr, bytes: &[u8]) -> Result<(), u64> {
    let mut offset = 0usize;
    for index in 0..header.iovlen {
        if offset == bytes.len() {
            break;
        }
        let mut raw = [0_u8; 16];
        user_memory::copy_from_user(header.iov + index * 16, &mut raw).map_err(|_| EFAULT)?;
        let base = u64::from_le_bytes(raw[..8].try_into().unwrap());
        let length = u64::from_le_bytes(raw[8..].try_into().unwrap());
        let length = (length as usize).min(bytes.len() - offset);
        if length != 0 {
            user_memory::copy_to_user(&bytes[offset..offset + length], base)
                .map_err(|_| EFAULT)?;
            offset += length;
        }
    }
    Ok(())
}

/// Extracts `SCM_RIGHTS` descriptors from a control buffer.
fn parse_rights(control: u64, controllen: u64) -> Result<Vec<u64>, u64> {
    let mut fds = Vec::new();
    if control == 0 || controllen < 16 || controllen > 4096 {
        return Ok(fds);
    }
    let mut raw = alloc::vec![0_u8; controllen as usize];
    user_memory::copy_from_user(control, &mut raw).map_err(|_| EFAULT)?;
    let mut offset = 0usize;
    while offset + 16 <= raw.len() {
        let length = u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap());
        let level = i32::from_le_bytes(raw[offset + 8..offset + 12].try_into().unwrap());
        let kind = i32::from_le_bytes(raw[offset + 12..offset + 16].try_into().unwrap());
        if length < 16 {
            break;
        }
        // `length` is attacker-controlled: a plain `offset + length` wraps in
        // release builds. A wrapped sum used to pass the bound check and then
        // made `offset` wrap backwards, so the loop never advanced and the
        // kernel spun forever holding the kernel lock. Every step is checked
        // and must make strict forward progress.
        let end = match (offset as u64).checked_add(length) {
            Some(end) if end <= controllen => end as usize,
            _ => break,
        };
        if level == SOL_SOCKET as i32 && kind == SCM_RIGHTS as i32 {
            let count = (end - offset - 16) / 4;
            if count > 64 {
                return Err(EINVAL);
            }
            for index in 0..count {
                let start = offset + 16 + index * 4;
                let fd = i32::from_le_bytes(raw[start..start + 4].try_into().unwrap());
                if fd >= 0 {
                    fds.push(fd as u64);
                }
            }
        }
        let advance = match (length as usize).checked_add(7) {
            Some(value) => value & !7,
            None => break,
        };
        offset = match offset.checked_add(advance) {
            Some(next) if next > offset => next,
            _ => break,
        };
    }
    Ok(fds)
}

/// Writes one `SCM_RIGHTS` control message. Returns flags to merge into
/// `msg_flags` (`MSG_CTRUNC` when the buffer was too small; those descriptors
/// are closed, as Linux does).
fn write_rights(task: u64, control: u64, controllen: u64, fds: &[u64]) -> i32 {
    if fds.is_empty() {
        return 0;
    }
    let needed = 16 + fds.len() * 4;
    let spaced = (needed + 7) & !7;
    if control == 0 || (controllen as usize) < spaced {
        for fd in fds {
            let _ = linux_fd::close(task, *fd);
        }
        return MSG_CTRUNC;
    }
    let mut block = alloc::vec![0_u8; spaced];
    block[..8].copy_from_slice(&(needed as u64).to_le_bytes());
    block[8..12].copy_from_slice(&(SOL_SOCKET as i32).to_le_bytes());
    block[12..16].copy_from_slice(&(SCM_RIGHTS as i32).to_le_bytes());
    for (index, fd) in fds.iter().enumerate() {
        let start = 16 + index * 4;
        block[start..start + 4].copy_from_slice(&(*fd as i32).to_le_bytes());
    }
    if user_memory::copy_to_user(&block, control).is_err() {
        return MSG_CTRUNC;
    }
    0
}

fn linux_socketpair(domain: u64, kind: u64, protocol: u64, pair: u64) -> u64 {
    if domain != AF_UNIX {
        return errno(EAFNOSUPPORT);
    }
    let base = kind & 0xf;
    // Rust's `Command` creates its exec-error channel with
    // `socketpair(AF_UNIX, SOCK_SEQPACKET|SOCK_CLOEXEC)`. The kernel keeps
    // byte-stream semantics internally, which is sufficient for that channel
    // and for every other `socketpair` user we have.
    if (base != 1 && base != 2 && base != 5) || protocol != 0 {
        return errno(EPROTONOSUPPORT);
    }
    let nonblock = kind & SOCK_NONBLOCK != 0;
    let task = scheduler::current_task_id();
    match linux_fd::socketpair(task, nonblock) {
        Ok((first, second)) => {
            if kind & SOCK_CLOEXEC != 0 {
                let _ = linux_fd::set_cloexec(task, first, true);
                let _ = linux_fd::set_cloexec(task, second, true);
            }
            let mut raw = [0_u8; 8];
            raw[..4].copy_from_slice(&(first as i32).to_le_bytes());
            raw[4..].copy_from_slice(&(second as i32).to_le_bytes());
            if user_memory::copy_to_user(&raw, pair).is_err() {
                let _ = linux_fd::close(task, first);
                let _ = linux_fd::close(task, second);
                return errno(EFAULT);
            }
            0
        }
        Err(code) => errno(code),
    }
}

fn linux_sendmsg(fd: u64, message: u64, flags: u64) -> u64 {
    let header = match read_msghdr(message) {
        Ok(header) => header,
        Err(code) => return errno(code),
    };
    let bytes = match read_iovecs(&header, 1 << 20) {
        Ok(bytes) => bytes,
        Err(code) => return errno(code),
    };
    let pass = match parse_rights(header.control, header.controllen) {
        Ok(pass) => pass,
        Err(code) => return errno(code),
    };
    let task = scheduler::current_task_id();
    finish(linux_fd::socket_send(
        task,
        fd,
        &bytes,
        &pass,
        flags & MSG_DONTWAIT != 0,
    ))
}

fn linux_recvmsg(fd: u64, message: u64, flags: u64) -> u64 {
    let header = match read_msghdr(message) {
        Ok(header) => header,
        Err(code) => return errno(code),
    };
    let mut total = 0usize;
    for index in 0..header.iovlen.min(64) {
        let mut raw = [0_u8; 16];
        if user_memory::copy_from_user(header.iov + index * 16, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let length = u64::from_le_bytes(raw[8..].try_into().unwrap());
        total = match total.checked_add(length as usize) {
            Some(total) if total <= 1 << 20 => total,
            _ => return errno(EINVAL),
        };
    }
    let mut bytes = alloc::vec![0_u8; total];
    let task = scheduler::current_task_id();
    let (read, fds) = match linux_fd::socket_recv(
        task,
        fd,
        &mut bytes,
        flags & MSG_DONTWAIT != 0,
    ) {
        Ok(result) => result,
        Err(code) => return errno(code),
    };
    if write_iovecs(&header, &bytes[..read]).is_err() {
        return errno(EFAULT);
    }
    let control_flags = write_rights(task, header.control, header.controllen, &fds);
    let mut output = [0_u8; 16];
    let size = if control_flags & MSG_CTRUNC != 0 || fds.is_empty() {
        0u64
    } else {
        ((16 + fds.len() * 4 + 7) & !7) as u64
    };
    output[..8].copy_from_slice(&size.to_le_bytes());
    let msg_flags = (header.flags & !MSG_CTRUNC) | control_flags;
    output[8..12].copy_from_slice(&msg_flags.to_le_bytes());
    if user_memory::copy_to_user(&output, message + 40).is_err() {
        return errno(EFAULT);
    }
    read as u64
}

fn linux_sendto(fd: u64, buffer: u64, length: u64, flags: u64) -> u64 {
    let length = (length as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; length];
    if user_memory::copy_from_user(buffer, &mut bytes).is_err() {
        return errno(EFAULT);
    }
    finish(linux_fd::socket_send(
        scheduler::current_task_id(),
        fd,
        &bytes,
        &[],
        flags & MSG_DONTWAIT != 0,
    ))
}

fn linux_recvfrom(
    fd: u64,
    buffer: u64,
    length: u64,
    flags: u64,
    address: u64,
    address_length: u64,
) -> u64 {
    let length = (length as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; length];
    let task = scheduler::current_task_id();
    let read = match linux_fd::socket_recv(task, fd, &mut bytes, flags & MSG_DONTWAIT != 0) {
        Ok((read, _)) => read,
        Err(code) => return errno(code),
    };
    if user_memory::copy_to_user(&bytes[..read], buffer).is_err() {
        return errno(EFAULT);
    }
    let _ = write_sockaddr(address, address_length, &[]);
    read as u64
}

/// Reads an `AF_UNIX` `sockaddr_un`. Filesystem names are NUL-terminated;
/// abstract names keep the leading NUL and their full byte length.
fn read_sockaddr(address: u64, length: u64) -> Result<Vec<u8>, u64> {
    if address == 0 || length < 3 {
        return Err(EINVAL);
    }
    let length = (length as usize).min(110);
    let mut raw = alloc::vec![0_u8; length];
    user_memory::copy_from_user(address, &mut raw).map_err(|_| EFAULT)?;
    let family = u16::from_le_bytes(raw[0..2].try_into().unwrap());
    if family != AF_UNIX as u16 {
        return Err(EAFNOSUPPORT);
    }
    let body = &raw[2..];
    let path = if body[0] == 0 {
        if body.len() < 2 {
            return Err(EINVAL);
        }
        body.to_vec()
    } else {
        let end = body.iter().position(|byte| *byte == 0).unwrap_or(body.len());
        if end == 0 {
            return Err(EINVAL);
        }
        body[..end].to_vec()
    };
    Ok(path)
}

/// Writes `family + path` into a caller `sockaddr_un` and stores the used
/// length. `path` is empty for an unnamed socket.
fn write_sockaddr(address: u64, length_pointer: u64, path: &[u8]) -> Result<(), u64> {
    let mut capacity = 0u32;
    if length_pointer != 0 {
        let mut raw = [0_u8; 4];
        user_memory::copy_from_user(length_pointer, &mut raw).map_err(|_| EFAULT)?;
        capacity = u32::from_le_bytes(raw);
    }
    // Filesystem names are returned NUL-terminated and the length includes
    // the terminator, matching Linux. Abstract names carry their own leading
    // NUL and no extra terminator.
    let filesystem = path.first().map(|byte| *byte != 0).unwrap_or(false);
    let terminator = usize::from(filesystem);
    let total = 2 + path.len() + terminator;
    if address != 0 && capacity >= 2 {
        let family = (AF_UNIX as u16).to_le_bytes();
        user_memory::copy_to_user(&family, address).map_err(|_| EFAULT)?;
        if capacity as usize >= total && !path.is_empty() {
            user_memory::copy_to_user(path, address + 2).map_err(|_| EFAULT)?;
            if terminator != 0 {
                let nul = [0_u8; 1];
                user_memory::copy_to_user(&nul, address + 2 + path.len() as u64)
                    .map_err(|_| EFAULT)?;
            }
        }
    }
    if length_pointer != 0 {
        let used = if address == 0 {
            0
        } else {
            total.min(capacity as usize) as u32
        };
        user_memory::copy_to_user(&used.to_le_bytes(), length_pointer).map_err(|_| EFAULT)?;
    }
    Ok(())
}

fn linux_sockname(task: u64, fd: u64, address: u64, length_pointer: u64) -> u64 {
    let path = linux_fd::socket_local_name(task, fd).unwrap_or_default();
    match write_sockaddr(address, length_pointer, &path) {
        Ok(()) => 0,
        Err(code) => errno(code),
    }
}

fn linux_socket(domain: u64, kind: u64, protocol: u64) -> u64 {
    if domain == 16 {
        // AF_NETLINK: only libudev's uevent monitor uses it. The stub accepts
        // binds and sends but never reports data (no uevent publisher).
        let base = kind & 0xf;
        if base == 2 || base == 3 || protocol == 15 {
            return finish(linux_fd::netlink_socket(
                scheduler::current_task_id(),
                kind & SOCK_NONBLOCK != 0,
            ));
        }
        return errno(EPROTONOSUPPORT);
    }
    if domain != AF_UNIX {
        return errno(EAFNOSUPPORT);
    }
    let base = kind & 0xf;
    // SOCK_SEQPACKET callers (stream semantics here) are accepted: the
    // deniald startup path creates one before its first session log.
    if (base != 1 && base != 2 && base != 5) || protocol != 0 {
        return errno(EPROTONOSUPPORT);
    }
    finish(linux_fd::socket(
        scheduler::current_task_id(),
        kind & SOCK_NONBLOCK != 0,
    ))
}

/// `fchmod`/`fchown`: the VFS has no per-inode permission bits, so the call
/// only has to prove the descriptor is alive.
fn linux_fchmod(fd: u64) -> u64 {
    if linux_fd::status_flags(scheduler::current_task_id(), fd).is_none() {
        return errno(EBADF);
    }
    0
}

/// `fchmodat`/`fchownat`: same, by path. The path has to resolve.
fn linux_fchmodat(dirfd: u64, path_pointer: u64) -> u64 {
    let Some(path) = user_string(path_pointer) else {
        return errno(EFAULT);
    };
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    match resolve(&cwd, &path) {
        Ok(_) => 0,
        Err(_) => errno(ENOENT),
    }
}

/// `close_range(2)`: glib's spawn path uses it to drop inherited descriptors.
/// It calls `close_range(3, ~0U, 0)`; walking to `u32::MAX` one descriptor at
/// a time burned a core forever before the child could exec (the terminal
/// "froze" on its first command). Only descriptor numbers the fd table can
/// hold exist, so clamp to its limit.
fn linux_close_range(first: u64, last: u64, flags: u64) -> u64 {
    if flags & !1 != 0 {
        return errno(EINVAL);
    }
    const MAX_DESCRIPTORS: u64 = 256;
    let last = last.min(MAX_DESCRIPTORS - 1);
    if first > last {
        return 0;
    }
    let task = scheduler::current_task_id();
    let mut fd = first;
    while fd <= last {
        let _ = linux_fd::close(task, fd);
        fd += 1;
    }
    0
}

/// `flock(2)`. The guest compositors use an advisory lock on their Wayland
/// lockfile to detect a live socket owner. This kernel has no per-file lock
/// table yet, so the lock is granted: a second compositor then fails at
/// `bind` with EADDRINUSE and tries the next `wayland-N` name, which is
/// exactly what `wl_display_add_socket_auto` expects.
fn linux_flock(fd: u64, operation: u64) -> u64 {
    const LOCK_SH: u64 = 1;
    const LOCK_EX: u64 = 2;
    const LOCK_NB: u64 = 4;
    const LOCK_UN: u64 = 8;
    if linux_fd::status_flags(scheduler::current_task_id(), fd).is_none() {
        return errno(EBADF);
    }
    match operation & !LOCK_NB {
        LOCK_SH | LOCK_EX | LOCK_UN => 0,
        _ => errno(EINVAL),
    }
}

fn linux_bind(fd: u64, address: u64, length: u64) -> u64 {
    // Netlink binds carry sockaddr_nl, not a unix path; the stub accepts
    // them as-is (libudev binds NETLINK_KOBJECT_UEVENT group 2).
    if linux_fd::is_netlink(scheduler::current_task_id(), fd) {
        return 0;
    }
    let path = match read_sockaddr(address, length) {
        Ok(path) => path,
        Err(code) => return errno(code),
    };
    finish(linux_fd::bind(scheduler::current_task_id(), fd, path).map(|_| 0))
}

fn linux_connect(fd: u64, address: u64, length: u64) -> u64 {
    let path = match read_sockaddr(address, length) {
        Ok(path) => path,
        Err(code) => return errno(code),
    };
    finish(linux_fd::connect(scheduler::current_task_id(), fd, path).map(|_| 0))
}

fn linux_accept(fd: u64, address: u64, length_pointer: u64, flags: u64) -> u64 {
    let task = scheduler::current_task_id();
    let new_fd = match linux_fd::accept(task, fd, flags & SOCK_NONBLOCK != 0) {
        Ok(new_fd) => new_fd,
        Err(code) => return errno(code),
    };
    // The accepted peer is unnamed in this model (clients do not bind).
    if let Err(code) = write_sockaddr(address, length_pointer, &[]) {
        let _ = linux_fd::close(task, new_fd);
        return errno(code);
    }
    new_fd
}

fn linux_setsockopt(_fd: u64, level: u64) -> u64 {
    if level == SOL_SOCKET {
        0
    } else {
        errno(ENOPROTOOPT)
    }
}

fn linux_getsockopt(fd: u64, level: u64, name: u64, value: u64, length: u64) -> u64 {
    const SO_PEERCRED: u64 = 17;
    if level != SOL_SOCKET {
        return errno(ENOPROTOOPT);
    }
    let task = scheduler::current_task_id();
    if linux_fd::status_flags(task, fd).is_none() {
        return errno(EBADF);
    }
    if name == SO_PEERCRED {
        // `struct ucred { pid_t pid; uid_t uid; gid_t gid; }`. The Linux
        // personality runs as root, so EXTERNAL authentication sees uid 0 on
        // both sides.
        if !linux_fd::is_socket(task, fd) {
            return errno(ENOTSOCK);
        }
        let mut capacity = 0u32;
        if length != 0 {
            let mut raw = [0_u8; 4];
            if user_memory::copy_from_user(length, &mut raw).is_err() {
                return errno(EFAULT);
            }
            capacity = u32::from_le_bytes(raw);
        }
        if capacity < 12 {
            return errno(EINVAL);
        }
        let mut credentials = [0_u8; 12];
        // zbus asks for peer credentials while negotiating a private
        // connection; a zero pid is "unknown" and some peers reject it.
        let pid = scheduler::current_task_id().min(u32::MAX as u64) as u32;
        credentials[0..4].copy_from_slice(&pid.to_le_bytes());
        credentials[4..8].copy_from_slice(&0u32.to_le_bytes()); // uid
        credentials[8..12].copy_from_slice(&0u32.to_le_bytes()); // gid
        if value != 0 && user_memory::copy_to_user(&credentials, value).is_err() {
            return errno(EFAULT);
        }
        if length != 0 {
            let used = 12u32.to_le_bytes();
            if user_memory::copy_to_user(&used, length).is_err() {
                return errno(EFAULT);
            }
        }
        return 0;
    }
    if linux_fd::status_flags(task, fd).is_none() {
        return errno(EBADF);
    }
    let answer: i32 = match name {
        3 => 1,          // SO_TYPE = SOCK_STREAM
        4 => 0,          // SO_ERROR
        7 | 8 => 212992, // SO_SNDBUF / SO_RCVBUF
        38 => 0,         // SO_PROTOCOL
        39 => AF_UNIX as i32,
        _ => return errno(ENOPROTOOPT),
    };
    if value != 0 {
        if user_memory::copy_to_user(&answer.to_le_bytes(), value).is_err() {
            return errno(EFAULT);
        }
    }
    if length != 0 {
        let size = 4u32.to_le_bytes();
        if user_memory::copy_to_user(&size, length).is_err() {
            return errno(EFAULT);
        }
    }
    0
}

/// `rt_sigprocmask(2)`: block/unblock/set the calling task's signal mask.
fn linux_rt_sigprocmask(how: u64, set_pointer: u64, old_pointer: u64) -> u64 {
    let task = scheduler::current_task_id();
    let old = crate::linux_signal::blocked(task);
    if set_pointer != 0 {
        let mut raw = [0_u8; 8];
        if user_memory::copy_from_user(set_pointer, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let set = u64::from_le_bytes(raw);
        let mask = match how {
            0 => old | set,          // SIG_BLOCK
            1 => old & !set,         // SIG_UNBLOCK
            2 => set,                // SIG_SETMASK
            _ => return errno(EINVAL),
        };
        crate::linux_signal::set_blocked(task, mask);
    }
    if old_pointer != 0 && user_memory::copy_to_user(&old.to_le_bytes(), old_pointer).is_err() {
        return errno(EFAULT);
    }
    0
}

/// `signalfd4(2)`: create or update a descriptor that receives its masked
/// signals as readable records. calloop's `Signals` source needs it; without
/// it the compositor's first `?` fails with a stale errno.
fn linux_signalfd4(fd: u64, set_pointer: u64, size: u64, flags: u64) -> u64 {
    const SFD_NONBLOCK: u64 = 0x4000;
    if size != 8 {
        return errno(EINVAL);
    }
    let mut raw = [0_u8; 8];
    if set_pointer != 0 && user_memory::copy_from_user(set_pointer, &mut raw).is_err() {
        return errno(EFAULT);
    }
    let mask = if set_pointer == 0 { 0 } else { u64::from_le_bytes(raw) };
    let fd = fd as i64 as i32 as i64;
    finish(crate::linux_fd::signalfd(
        scheduler::current_task_id(),
        fd,
        mask,
        flags & SFD_NONBLOCK != 0,
    ))
}

/// `rt_sigtimedwait(2)`: dequeue one pending signal from the set atomically.
/// With a null timeout this is the polling variant; a timeout polls and
/// sleeps until a matching signal arrives or the deadline passes.
fn linux_rt_sigtimedwait(set_pointer: u64, info_pointer: u64, _size: u64) -> u64 {
    const SIGINFO_SIZE: usize = 128;
    let task = scheduler::current_task_id();
    let mut set = [0_u8; 8];
    if set_pointer != 0 && user_memory::copy_from_user(set_pointer, &mut set).is_err() {
        return errno(EFAULT);
    }
    let mask = if set_pointer == 0 {
        0
    } else {
        u64::from_le_bytes(set)
    };
    if mask == 0 {
        return errno(EINVAL);
    }
    let deadline = timer::ticks().saturating_add(timer::milliseconds_to_ticks(100));
    loop {
        let pending = crate::linux_signal::pending(task) & mask;
        for signal in 1..=64_u32 {
            if pending & (1 << (signal - 1)) == 0 {
                continue;
            }
            crate::linux_signal::clear_pending(task, signal);
            if info_pointer != 0 {
                let mut record = [0_u8; SIGINFO_SIZE];
                record[0..4].copy_from_slice(&signal.to_le_bytes()); // ssi_signo
                record[12..16].copy_from_slice(&(-1_i32).to_le_bytes()); // SI_KERNEL
                if user_memory::copy_to_user(&record, info_pointer).is_err() {
                    return errno(EFAULT);
                }
            }
            return signal as u64;
        }
        if timer::ticks() >= deadline {
            return errno(EAGAIN);
        }
        scheduler::sleep(1);
    }
}

/// `rt_sigaction(2)`: store/retrieve one disposition. The AArch64
/// `struct sigaction` is `{ handler, flags, restorer, mask }` (32 bytes).
fn linux_rt_sigaction(signal: u64, action_pointer: u64, old_pointer: u64) -> u64 {
    if signal == 0 || signal > 64 {
        return errno(EINVAL);
    }
    let task = scheduler::current_task_id();
    if old_pointer != 0 {
        let old = crate::linux_signal::action(task, signal as u32).unwrap_or_default();
        let mut bytes = [0_u8; 32];
        bytes[..8].copy_from_slice(&old.handler.to_le_bytes());
        bytes[8..16].copy_from_slice(&old.flags.to_le_bytes());
        bytes[16..24].copy_from_slice(&old.restorer.to_le_bytes());
        bytes[24..32].copy_from_slice(&old.mask.to_le_bytes());
        if user_memory::copy_to_user(&bytes, old_pointer).is_err() {
            return errno(EFAULT);
        }
    }
    if action_pointer != 0 {
        let mut bytes = [0_u8; 32];
        if user_memory::copy_from_user(action_pointer, &mut bytes).is_err() {
            return errno(EFAULT);
        }
        let action = crate::linux_signal::Action {
            handler: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            flags: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            restorer: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            mask: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        };
        crate::linux_signal::set_action(task, signal as u32, action);
    }
    0
}

/// `sigaltstack(2)`: register/read the alternate signal stack. The kernel
/// `stack_t` is `{ sp, flags(int), pad, size }` (24 bytes).
fn linux_sigaltstack(stack_pointer: u64, old_pointer: u64) -> u64 {
    const SS_ONSTACK: u64 = 1;
    const SS_DISABLE: u64 = 2;
    const MINSIGSTKSZ: u64 = 2048;
    let task = scheduler::current_task_id();

    if old_pointer != 0 {
        let mut bytes = [0_u8; 24];
        if let Some((sp, size)) = crate::linux_signal::altstack(task) {
            bytes[..8].copy_from_slice(&sp.to_le_bytes());
            let flags = if crate::linux_signal::on_altstack(task) {
                SS_ONSTACK
            } else {
                0
            };
            bytes[8..12].copy_from_slice(&(flags as i32).to_le_bytes());
            bytes[16..24].copy_from_slice(&size.to_le_bytes());
        } else {
            bytes[8..12].copy_from_slice(&(SS_DISABLE as i32).to_le_bytes());
        }
        if user_memory::copy_to_user(&bytes, old_pointer).is_err() {
            return errno(EFAULT);
        }
    }

    if stack_pointer != 0 {
        let mut bytes = [0_u8; 24];
        if user_memory::copy_from_user(stack_pointer, &mut bytes).is_err() {
            return errno(EFAULT);
        }
        let sp = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let flags = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as u64;
        let size = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        if flags & SS_DISABLE != 0 {
            crate::linux_signal::set_altstack(task, None);
        } else if flags & SS_ONSTACK != 0 || sp == 0 || size < MINSIGSTKSZ {
            return errno(EINVAL);
        } else if sp.checked_add(size).is_none() {
            return errno(EINVAL);
        } else {
            crate::linux_signal::set_altstack(task, Some((sp, size)));
        }
    }
    0
}

/// `kill(2)`/`tgkill(2)`. `SIGKILL` and default-terminating signals terminate
/// the target; a default-ignored signal (including `SIGCHLD`) is a no-op; a
/// user handler is recorded as pending and entered when the target next
/// returns to EL0. `pid == 0` or a negative pid targets the caller (no process
/// groups).
fn linux_kill(pid: i64, signal: u64) -> u64 {
    const ESRCH: u64 = 3;
    if signal > 64 {
        return errno(EINVAL);
    }
    let task = scheduler::current_task_id();
    let target = if pid > 0 { pid as u64 } else { task };
    if target != task {
        if !scheduler::task_alive(target) {
            return errno(ESRCH);
        }
        // Linux checks the sender's credentials here. Every task runs as uid
        // 0 in this system, so a uid comparison would authorize everything;
        // the real boundary is the kernel object graph: self, own threads,
        // parent/child, an explicit CAP_KILL handle, or another Linux task.
        // Native daemons (net_server, wolfser, drivers) are unreachable from
        // a stray pid guess.
        if !may_signal(task, target) {
            return errno(EPERM);
        }
    }
    if signal == 0 {
        return 0;
    }
    let signal = signal as u32;
    let handler = if crate::linux_signal::has_table(target) {
        crate::linux_signal::action(target, signal)
            .map(|action| action.handler)
            .unwrap_or(crate::linux_signal::SIG_DFL)
    } else {
        crate::linux_signal::SIG_DFL
    };
    if handler == crate::linux_signal::SIG_IGN {
        return 0;
    }
    if handler == crate::linux_signal::SIG_DFL {
        if crate::linux_signal::default_terminates(signal) || signal == crate::linux_signal::SIGKILL {
            if target == task {
                // A default-terminating signal ends the whole thread group
                // (glibc abort() raises against itself): the other threads
                // share the leader's address space and would fault on the
                // torn-down pages if the leader died alone.
                scheduler::exit_group(128 + signal as u64);
            }
            let _ = scheduler::kill_task(target);
        }
        return 0;
    }
    // A real handler: remember the signal and, for another task, wake it from
    // sleep/block so its next return to EL0 (SVC or IRQ) enters the handler.
    crate::linux_signal::raise(target, signal);
    if target != task {
        scheduler::wake_for_signal(target);
    }
    0
}

/// Authorization for `kill(2)`: self, same thread group, parent or child, a
/// delegated `CAP_KILL` handle, or another Linux task. Native daemons stay out
/// of reach unless a handle explicitly grants `CAP_KILL`.
#[cfg(feature = "linux-abi")]
fn may_signal(sender: u64, target: u64) -> bool {
    if sender == target || scheduler::group_leader(sender) == scheduler::group_leader(target) {
        return true;
    }
    if scheduler::task_parent(target) == Some(sender)
        || scheduler::task_parent(sender) == Some(target)
    {
        return true;
    }
    if scheduler::task_has_target_rights(sender, target, crate::capability::CAP_KILL) {
        return true;
    }
    matches!(
        (
            scheduler::task_personality(sender),
            scheduler::task_personality(target),
        ),
        (Some(scheduler::Personality::Linux), Some(scheduler::Personality::Linux))
    )
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

/// `writev(2)`: scatter output over up to 64 `struct iovec`s.
///
/// musl's stdio writes every stream through `writev`, not `write`; the old
/// implementation only accepted the console (`fd 1`/`2`) and returned
/// `EBADF` for files, so `openrc` silently produced a zero-byte `deptree`.
fn linux_writev(fd: u64, iovec: u64, count: u64) -> u64 {
    const MAX_IOVECS: u64 = 64;
    if count > MAX_IOVECS {
        return errno(EINVAL);
    }
    let mut total = 0u64;
    for index in 0..count {
        let Some((base, mut length)) = iovec_at(iovec, index) else {
            return if total > 0 { total } else { errno(EFAULT) };
        };
        let mut address = base;
        while length > 0 {
            let written = linux_write(fd, address, length);
            if (written as i64) < 0 {
                return if total > 0 { total } else { written };
            }
            if written == 0 {
                return total;
            }
            total += written;
            address = address.wrapping_add(written);
            length -= written;
        }
    }
    total
}

/// Reads a `u64` from user memory (`off_t *` on AArch64).
fn user_u64(address: u64) -> Option<u64> {
    let mut raw = [0_u8; 8];
    user_memory::copy_from_user(address, &mut raw).ok()?;
    Some(u64::from_le_bytes(raw))
}

const SEEK_CUR: u64 = 1;
const SEEK_SET: u64 = 0;

/// `sendfile(2)`: copy between descriptors in 64 KiB kernel chunks.
///
/// busybox `cp` (and therefore the OpenRC/Alpine triggers) uses `sendfile`
/// when both ends are files; without it `cp` failed with `EFAULT` and the
/// glib schema copy in `boot-session` never ran.
fn linux_sendfile(output: u64, input: u64, offset_ptr: u64, count: u64) -> u64 {
    let task = scheduler::current_task_id();
    let mut offset = if offset_ptr != 0 {
        match user_u64(offset_ptr) {
            Some(offset) => offset,
            None => return errno(EFAULT),
        }
    } else {
        match linux_fd::lseek(task, input, 0, SEEK_CUR) {
            Ok(offset) => offset,
            Err(code) => return errno(code),
        }
    };
    let mut total = 0u64;
    let mut buffer = alloc::vec![0_u8; 64 * 1024];
    while total < count {
        let want = (count - total).min(buffer.len() as u64) as usize;
        let read = match linux_fd::pread(task, input, offset, &mut buffer[..want]) {
            Ok(read) => read,
            Err(code) => return if total > 0 { total } else { errno(code) },
        };
        if read == 0 {
            break;
        }
        let mut written = 0usize;
        while written < read {
            let progress = match linux_fd::write_kernel(task, output, &buffer[written..read]) {
                Ok(progress) => progress,
                Err(code) => return if total > 0 { total } else { errno(code) },
            };
            if progress == 0 {
                break;
            }
            written += progress;
        }
        total += written as u64;
        offset += written as u64;
        if written < read {
            break;
        }
    }
    if offset_ptr != 0 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&offset.to_le_bytes());
        let _ = user_memory::copy_to_user(&raw, offset_ptr);
    } else {
        // `pread` does not move the descriptor; the caller expects sendfile to.
        let _ = linux_fd::lseek(task, input, offset as i64, SEEK_SET);
    }
    total
}

/// `copy_file_range(2)`: the same chunked copy with independent offsets.
/// When an offset pointer is given the descriptor's own offset must not move,
/// so the output position is restored after the copy.
fn linux_copy_file_range(
    input: u64,
    input_offset_ptr: u64,
    output: u64,
    output_offset_ptr: u64,
    length: u64,
    flags: u64,
) -> u64 {
    if flags != 0 {
        return errno(EINVAL);
    }
    let task = scheduler::current_task_id();
    let mut input_offset = if input_offset_ptr != 0 {
        match user_u64(input_offset_ptr) {
            Some(offset) => offset,
            None => return errno(EFAULT),
        }
    } else {
        match linux_fd::lseek(task, input, 0, SEEK_CUR) {
            Ok(offset) => offset,
            Err(code) => return errno(code),
        }
    };
    let mut saved_output_offset = None;
    let mut output_offset = 0u64;
    if output_offset_ptr != 0 {
        let Some(offset) = user_u64(output_offset_ptr) else {
            return errno(EFAULT);
        };
        output_offset = offset;
        saved_output_offset = linux_fd::lseek(task, output, 0, SEEK_CUR).ok().map(|offset| offset as i64);
        if let Err(code) = linux_fd::lseek(task, output, offset as i64, SEEK_SET) {
            return errno(code);
        }
    }
    let mut total = 0u64;
    let mut buffer = alloc::vec![0_u8; 64 * 1024];
    while total < length {
        let want = (length - total).min(buffer.len() as u64) as usize;
        let read = match linux_fd::pread(task, input, input_offset, &mut buffer[..want]) {
            Ok(read) => read,
            Err(code) => return if total > 0 { total } else { errno(code) },
        };
        if read == 0 {
            break;
        }
        let mut written = 0usize;
        while written < read {
            let progress = match linux_fd::write_kernel(task, output, &buffer[written..read]) {
                Ok(progress) => progress,
                Err(code) => return if total > 0 { total } else { errno(code) },
            };
            if progress == 0 {
                break;
            }
            written += progress;
        }
        total += written as u64;
        input_offset += written as u64;
        if written < read {
            break;
        }
    }
    if input_offset_ptr != 0 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&input_offset.to_le_bytes());
        let _ = user_memory::copy_to_user(&raw, input_offset_ptr);
    } else {
        let _ = linux_fd::lseek(task, input, input_offset as i64, SEEK_SET);
    }
    if output_offset_ptr != 0 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&(output_offset + total).to_le_bytes());
        let _ = user_memory::copy_to_user(&raw, output_offset_ptr);
        if let Some(original) = saved_output_offset {
            let _ = linux_fd::lseek(task, output, original as i64, SEEK_SET);
        }
    }
    total
}

/// `readv(2)`: gather input into up to 64 `struct iovec`s. musl's stdio
/// reads streams through `readv`, so without this `deptree_load` and every
/// other `FILE` read saw `ENOSYS`.
fn linux_readv(fd: u64, iovec: u64, count: u64) -> u64 {
    const MAX_IOVECS: u64 = 64;
    if count > MAX_IOVECS {
        return errno(EINVAL);
    }
    let mut total = 0u64;
    for index in 0..count {
        let Some((base, length)) = iovec_at(iovec, index) else {
            return if total > 0 { total } else { errno(EFAULT) };
        };
        let mut done = 0u64;
        while done < length {
            let remaining = length - done;
            let read = finish(linux_fd::read(
                scheduler::current_task_id(),
                fd,
                base.wrapping_add(done),
                remaining,
            ));
            if (read as i64) < 0 {
                return if total > 0 { total } else { read };
            }
            if read == 0 {
                return total;
            }
            total += read;
            done += read;
            // A short read ends the call: the next descriptor must not block.
            if read < remaining {
                return total;
            }
        }
    }
    total
}

/// `ioctl(2)`: only the terminal size query is meaningful while stdout is the
/// console. Everything else reports "not a tty".
fn linux_ioctl(fd: u64, request: u64, argument: u64) -> u64 {
    const TIOCGWINSZ: u64 = 0x5413;
    const TCGETS: u64 = 0x5401;
    const FIONBIO: u64 = 0x5421;
    /// `_IO('W', 0x01)`: present the current `/dev/gpufb` contents.
    const GPU_IOCTL_FLUSH: u64 = 0x5701;
    /// udmabuf: turn a memfd into a dmabuf fd. The modern uapi renumbered
    /// UDMABUF_CREATE from 0x15 to 0x42 (24-byte struct udmabuf_create).
    const UDMABUF_CREATE: u64 = 0x4018_7542;
    /// dma-buf ioctls llvmpipe runs on its dummy dmabuf.
    const DMA_BUF_SYNC: u64 = 0x4008_6200;
    const DMA_BUF_EXPORT_SYNC_FILE: u64 = 0xC008_6202;
    let task = scheduler::current_task_id();
    if linux_fd::is_drm(task, fd) {
        if let Some(result) = crate::drm::ioctl(request, argument) {
            return result;
        }
        if let Some(result) = crate::drm::render_ioctl(request, argument) {
            return result;
        }
    }
    if request == GPU_IOCTL_FLUSH && linux_fd::is_gpufb(task, fd) {
        return if crate::virtio_gpu::flush() {
            0
        } else {
            errno(EIO)
        };
    }
    // Rust's runtime (and socket2) set non-blocking through `ioctl(FIONBIO)`
    // on sockets and pipes. The fd is not a terminal, so the tty branch below
    // must not answer EBADF for it.
    if request == FIONBIO {
        if linux_fd::status_flags(task, fd).is_none() {
            return errno(EBADF);
        }
        let mut raw = [0_u8; 4];
        if user_memory::copy_from_user(argument, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let enabled = u32::from_le_bytes(raw) != 0;
        return match linux_fd::set_nonblock(task, fd, enabled) {
            // A file-backed descriptor is always "ready"; treat the request
            // as a successful no-op like Linux does for regular files.
            Ok(()) => 0,
            Err(_) => 0,
        };
    }
    // Virtual-terminal ioctls for the synthetic VT console (`/dev/tty0`).
    // seatd drives every session activation through them; without answers the
    // seat never activates and clients cannot open devices.
    if linux_fd::is_console(task, fd) {
        const VT_OPENQRY: u64 = 0x5600;
        const VT_SETMODE: u64 = 0x5601;
        const VT_GETMODE: u64 = 0x5602;
        const VT_GETSTATE: u64 = 0x5603;
        const VT_SENDSIG: u64 = 0x5604;
        const VT_RELDISP: u64 = 0x5605;
        const VT_ACTIVATE: u64 = 0x5606;
        const VT_WAITACTIVE: u64 = 0x5607;
        const VT_DISALLOCATE: u64 = 0x5608;
        const KDSETMODE: u64 = 0x4B3A;
        const KDGETMODE: u64 = 0x4B3B;
        const TIOCSCTTY: u64 = 0x540E;
        match request {
            VT_OPENQRY => {
                let one = 1_u32.to_le_bytes();
                if user_memory::copy_to_user(&one, argument).is_err() {
                    return errno(EFAULT);
                }
                return 0;
            }
            VT_GETSTATE => {
                // struct vt_stat { unsigned short v_active, v_signal, v_state; }
                let mut stat = [0_u8; 6];
                stat[0..2].copy_from_slice(&1_u16.to_le_bytes());
                stat[4..6].copy_from_slice(&0x0002_u16.to_le_bytes());
                if user_memory::copy_to_user(&stat, argument).is_err() {
                    return errno(EFAULT);
                }
                return 0;
            }
            VT_GETMODE => {
                // struct vt_mode { char mode(VT_AUTO=0), waitv; short relsig, acksig, frsig; }
                let mode = [0_u8; 8];
                if user_memory::copy_to_user(&mode, argument).is_err() {
                    return errno(EFAULT);
                }
                return 0;
            }
            VT_SETMODE | VT_SENDSIG | VT_RELDISP | VT_ACTIVATE | VT_WAITACTIVE
            | VT_DISALLOCATE | KDSETMODE | KDGETMODE | TIOCSCTTY => return 0,
            _ => {}
        }
    }
    if linux_fd::is_udmabuf(task, fd) {
        crate::console::write_str("UDMABUF-IOCTL req=0x");
        crate::console::write_hex_u64(request);
        crate::console::write_str("\r\n");
    }
    if request == UDMABUF_CREATE && linux_fd::is_udmabuf(task, fd) {
        // struct udmabuf_create { u32 memfd, u32 flags, u64 offset, u64 size }
        let mut raw = [0_u8; 24];
        if user_memory::copy_from_user(argument, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let memfd = u64::from(raw[0])
            | u64::from(raw[1]) << 8
            | u64::from(raw[2]) << 16
            | u64::from(raw[3]) << 24;
        let size = u64::from(raw[16])
            | u64::from(raw[17]) << 8
            | u64::from(raw[18]) << 16
            | u64::from(raw[19]) << 24
            | u64::from(raw[20]) << 32
            | u64::from(raw[21]) << 40
            | u64::from(raw[22]) << 48
            | u64::from(raw[23]) << 56;
        let result = linux_fd::udmabuf_create(task, memfd, 0, size);
        crate::console::write_str("UDMABUF-CREATE memfd=");
        crate::console::write_dec_u64(memfd);
        crate::console::write_str(" size=");
        crate::console::write_dec_u64(size);
        crate::console::write_str(" -> ");
        match &result {
            Ok(fd) => crate::console::write_dec_u64(*fd),
            Err(code) => crate::console::write_dec_u64(*code),
        }
        crate::console::write_str("\r\n");
        return finish(result);
    }
    if linux_fd::is_memfd(task, fd) {
        if request == DMA_BUF_EXPORT_SYNC_FILE {
            // struct dma_buf_export_sync_file { u32 flags, s32 fd }: the fence
            // completes on the CPU, so the exported sync file is signalled.
            let Ok(sync_fd) = linux_fd::eventfd(task, 1, true) else {
                return errno(ENOMEM);
            };
            let mut out = [0_u8; 8];
            out[4..8].copy_from_slice(&(sync_fd as i32).to_le_bytes());
            if user_memory::copy_to_user(&out, argument).is_err() {
                return errno(EFAULT);
            }
            return 0;
        }
        if request == DMA_BUF_SYNC {
            // CPU-side memory: the read/write barriers are no-ops here.
            return 0;
        }
    }
    if request == TCGETS {
        // glibc's isatty() probes TCGETS; a console answers with a zeroed
        // termios so isatty returns true.
        if linux_fd::is_console(task, fd) {
            let termios = [0_u8; 60];
            if user_memory::copy_to_user(&termios, argument).is_err() {
                return errno(EFAULT);
            }
            return 0;
        }
    }
    // Everything that reached here is not a device with a specific handler.
    // The POSIX answer for an OPEN descriptor is ENOTTY ("inappropriate
    // ioctl for device"); EBADF only means the descriptor is not open.
    // libsystemd's isatty_safe asserts on EBADF for fds above stdio - the
    // deniald startup died on exactly that distinction.
    if linux_fd::status_flags(task, fd).is_none() {
        return errno(EBADF);
    }
    if request != TIOCGWINSZ {
        return errno(ENOTTY);
    }
    if !linux_fd::is_console(task, fd) {
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

/// `faccessat(2)`: existence/readability probe. The Linux view is read-only,
/// so a write-access check fails with `-EROFS`.
fn linux_faccessat(dirfd: u64, path_ptr: u64, mode: u64) -> u64 {
    const W_OK: u64 = 2;
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
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    if crate::sysfs::lookup(&resolved).is_some()
        || crate::procfs::lookup(&resolved).is_some()
        || linux_fd::device_exists(&resolved)
    {
        // Synthetic nodes are read-only; a write probe reports EROFS like the
        // rest of the image.
        return if mode & W_OK != 0 { errno(EROFS) } else { 0 };
    }
    // Unlisted /proc and /sys nodes do not exist: probing libraries expect
    // the clean ENOENT, not the VFS read-path EACCES.
    if resolved == "/proc" || resolved.starts_with("/proc/") || resolved.starts_with("/sys/") {
        return errno(ENOENT);
    }
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
    if mode & W_OK != 0 && !crate::scheduler::linux_write_allowed(&resolved) {
        // SFS is read-only; the Linux scratch tree (`/tmp`) is the one place
        // a toolkit may create caches. A blanket EROFS made fontconfig
        // declare every cache directory unwritable.
        return errno(EROFS);
    }
    // Only the nodes the kernel actually exposes: a blanket "/dev exists"
    // made libdrm's `drmGetMinorType` see card128 and classify the render
    // node as a primary one.
    if linux_fd::device_exists(&resolved)
        || crate::vfs::file_len(&cwd, &resolved).is_ok()
        || crate::vfs::resolve_directory(&cwd, &resolved).is_ok()
    {
        0
    } else {
        errno(ENOENT)
    }
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
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    match stat_resolved(&cwd, &path, &resolved, follow) {
        Ok((mode, size, ino, Some(rdev))) => {
            finish(linux_fd::write_stat_rdev(buffer, mode, size, ino, rdev).map(|_| 0))
        }
        Ok((mode, size, ino, None)) => {
            finish(linux_fd::write_stat(buffer, mode, size, ino).map(|_| 0))
        }
        Err(code) => errno(code),
    }
}

/// Shared path stat for `newfstatat` and `statx`.
fn stat_resolved(
    cwd: &str,
    path: &str,
    resolved: &str,
    follow: bool,
) -> Result<(u32, u64, u64, Option<u64>), u64> {
    if let Some(node) = crate::sysfs::lookup(resolved) {
        let (mode, size) = match node {
            crate::sysfs::Node::File(data) => (S_IFREG | 0o444, data.len() as u64),
            crate::sysfs::Node::Dir => (S_IFDIR | 0o755, 0),
            crate::sysfs::Node::Symlink(target) => (S_IFLNK | 0o777, target.len() as u64),
        };
        return Ok((mode, size, linux_fd::path_ino(resolved), None));
    }
    if let Some(node) = crate::procfs::lookup(resolved) {
        let (mode, size) = match node {
            crate::procfs::Node::File(data) => (S_IFREG | 0o444, data.len() as u64),
            crate::procfs::Node::Dir(names) => (S_IFDIR | 0o555, names.len() as u64),
            crate::procfs::Node::Symlink(target) => (S_IFLNK | 0o777, target.len() as u64),
        };
        return Ok((mode, size, linux_fd::path_ino(resolved), None));
    }
    if let Some((mode, rdev)) = synthetic_device_stat(resolved) {
        return Ok((mode, 0, linux_fd::path_ino(resolved), Some(rdev)));
    }
    if !scheduler::current_read_path_allowed(resolved) {
        return Err(EACCES);
    }
    // `lstat`/`AT_SYMLINK_NOFOLLOW` reports the link itself. SFS stores the
    // target in the inode; following it is the default `stat` path below.
    if !follow && crate::vfs::lstat_kind("/", resolved).ok() == Some(3) {
        let length = crate::vfs::link_len("/", resolved).unwrap_or(0) as u64;
        return Ok((S_IFLNK | 0o777, length, linux_fd::path_ino(resolved), None));
    }
    // Directories first: the SFS backend reports a directory inode's size as
    // 0, so `file_len` succeeds for one and used to classify it as a regular
    // file. Toolkits stat XDG/data directories and reject a "file" where a
    // directory is required (xkbcommon refused its own config root).
    if crate::vfs::read_dir("/", resolved).is_ok() {
        return Ok((S_IFDIR | 0o755, 0, linux_fd::path_ino(resolved), None));
    }
    if let Ok(size) = crate::vfs::file_len("/", resolved) {
        let file_mode = if crate::scheduler::linux_ram_path(resolved) {
            S_IFREG | 0o644
        } else {
            // SFS stores no permission bits and is read-only; report files
            // executable so gio's g_find_program_in_path() accepts /sfs.
            S_IFREG | 0o555
        };
        return Ok((file_mode, size as u64, linux_fd::path_ino(resolved), None));
    }
    // Bound AF_UNIX sockets live only in the kernel registry; deniald stats
    // its portal socket right after binding it.
    if linux_fd::listener_exists(resolved.as_bytes()) {
        return Ok((S_IFSOCK | 0o600, 0, linux_fd::path_ino(resolved), None));
    }
    Err(ENOENT)
}

/// `statx(2)`: glib and wlroots probe it first; without it they fall back to
/// repeated retry loops. Writes the fixed 256-byte `struct statx`.
fn linux_statx(dirfd: u64, path_ptr: u64, flags: u64, _mask: u64, buffer: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let task = scheduler::current_task_id();
    let fields = if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return errno(ENOENT);
        }
        match linux_fd::stat_fields(task, dirfd) {
            Ok(fields) => fields,
            Err(code) => return errno(code),
        }
    } else {
        let cwd = match dirfd_cwd(dirfd) {
            Ok(cwd) => cwd,
            Err(code) => return code,
        };
        let Ok(resolved) = resolve(&cwd, &path) else {
            return errno(ENOENT);
        };
        match stat_resolved(&cwd, &path, &resolved, flags & AT_SYMLINK_NOFOLLOW == 0) {
            Ok(fields) => fields,
            Err(code) => return errno(code),
        }
    };
    match write_statx(buffer, fields.0, fields.1, fields.2, fields.3) {
        Ok(()) => 0,
        Err(code) => errno(code),
    }
}

fn write_statx(
    buffer: u64,
    mode: u32,
    size: u64,
    ino: u64,
    rdev: Option<u64>,
) -> Result<(), u64> {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    // systemd's fds_are_same_mount() statx's fds with STATX_MNT_ID and
    // asserts the result: without the bit the mount id reads as an error and
    // the assert aborts the process.
    const STATX_MNT_ID: u32 = 0x8000;
    let mut raw = [0_u8; 256];
    raw[0..4].copy_from_slice(&(STATX_BASIC_STATS | STATX_MNT_ID).to_le_bytes());
    raw[4..8].copy_from_slice(&4096_u32.to_le_bytes());
    raw[16..20].copy_from_slice(&1_u32.to_le_bytes());
    raw[28..30].copy_from_slice(&(mode as u16).to_le_bytes());
    raw[32..40].copy_from_slice(&ino.to_le_bytes());
    raw[40..48].copy_from_slice(&size.to_le_bytes());
    raw[48..56].copy_from_slice(&(size / 512).to_le_bytes());
    if let Some(device) = rdev {
        raw[128..132].copy_from_slice(&linux_fd::major(device).to_le_bytes());
        raw[132..136].copy_from_slice(&linux_fd::minor(device).to_le_bytes());
    }
    // One mount id for the whole synthetic tree: same-mount comparisons see
    // every descriptor as shared, which matches the single-filesystem view.
    raw[144..152].copy_from_slice(&1_u64.to_le_bytes());
    user_memory::copy_to_user(&raw, buffer).map_err(|_| EFAULT)
}

/// Mode and `st_rdev` for a synthetic `/dev` node.
fn synthetic_device_stat(path: &str) -> Option<(u32, u64)> {
    const S_IFCHR: u32 = 0o020000;
    match path {
        // Synthetic directory roots (udev enumerate stats and scans them).
        "/dev" | "/dev/input" => Some((S_IFDIR | 0o755, 0)),
        "/dev/dri" if crate::virtio_gpu::size().is_some() => Some((S_IFDIR | 0o755, 0)),
        "/dev/dri/card0" if crate::virtio_gpu::size().is_some() => Some((
            S_IFCHR | 0o666,
            linux_fd::makedev(crate::sysfs::DRM_MAJOR, crate::sysfs::CARD_MINOR),
        )),
        "/dev/dri/renderD128" if crate::virtio_gpu::size().is_some() => Some((
            S_IFCHR | 0o666,
            linux_fd::makedev(crate::sysfs::DRM_MAJOR, crate::sysfs::RENDER_MINOR),
        )),
        _ if linux_fd::device_exists(path) => Some((S_IFCHR | 0o666, 0)),
        _ => None,
    }
}

/// `readlinkat(2)`: the synthetic sysfs symlinks (`device/subsystem`).
fn linux_readlinkat(dirfd: u64, path_ptr: u64, buffer: u64, size: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    let target = match crate::sysfs::symlink(&resolved) {
        Some(target) => String::from(target),
        None => match crate::procfs::lookup(&resolved) {
            Some(crate::procfs::Node::Symlink(target)) => String::from(target),
            _ => match crate::vfs::read_link("/", &resolved) {
                Ok(target) => target,
                Err(()) => return errno(EINVAL),
            },
        },
    };
    let bytes = target.as_bytes();
    let take = bytes.len().min(size as usize);
    if user_memory::copy_to_user(&bytes[..take], buffer).is_err() {
        return errno(EFAULT);
    }
    take as u64
}

/// `fcntl(2)`: descriptor duplication plus the flags queries every libc probes.
fn linux_fcntl(fd: u64, command: u64, argument: u64) -> u64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const F_ADD_SEALS: u64 = 1033;
    const F_GET_SEALS: u64 = 1034;
    const EPERM: u64 = 1;
    let task = scheduler::current_task_id();
    match command {
        F_DUPFD => finish(linux_fd::dup_from(task, fd, argument, false)),
        F_DUPFD_CLOEXEC => finish(linux_fd::dup_from(task, fd, argument, true)),
        F_GETFD => match linux_fd::get_cloexec(task, fd) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(code) => errno(code),
        },
        F_GETFL => match linux_fd::status_flags(task, fd) {
            Some(flags) => flags,
            None => errno(EBADF),
        },
        F_SETFD => finish(linux_fd::set_cloexec(task, fd, argument & 1 != 0).map(|_| 0)),
        F_SETFL => {
            let _ = linux_fd::set_nonblock(task, fd, argument & 0x800 != 0);
            0
        }
        F_ADD_SEALS => {
            let result = linux_fd::memfd_add_seals(task, fd, argument as u32).map(|_| 0);
            if result.is_err() {
                crate::console::write_str("F_ADD_SEALS fd=");
                crate::console::write_dec_u64(fd);
                crate::console::write_str(" failed\r\n");
            }
            finish(result)
        }
        F_GET_SEALS => match linux_fd::memfd_get_seals(task, fd) {
            Ok(seals) => u64::from(seals),
            Err(code) => errno(code),
        },
        _ => errno(EINVAL),
    }
}

fn linux_getcwd(buffer: u64, capacity: u64) -> u64 {
    let cwd = display_path(&scheduler::current_task_cwd());
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

const AT_REMOVEDIR: u64 = 0x200;
const RENAME_NOREPLACE: u64 = 1;

fn scratch_resolved(dirfd: u64, path_pointer: u64) -> Result<(String, String), u64> {
    let Some(path) = user_string(path_pointer) else {
        return Err(EFAULT);
    };
    if path.is_empty() {
        return Err(ENOENT);
    }
    let cwd = dirfd_cwd(dirfd)?;
    let resolved = resolve(&cwd, &path).map_err(|_| ENOENT)?;
    if !crate::scheduler::linux_write_allowed(&resolved) {
        return Err(EROFS);
    }
    // The mapped absolute path is what the VFS mutation must use: a relative
    // name under `/run` lives at `/sfs/run` in the kernel tree.
    Ok((String::from("/"), resolved))
}

fn linux_mkdirat(dirfd: u64, path_pointer: u64, _mode: u64) -> u64 {
    let (cwd, path) = match scratch_resolved(dirfd, path_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    match crate::vfs::mkdir(&cwd, &path) {
        Ok(()) => 0,
        Err(()) => {
            if crate::vfs::resolve_directory(&cwd, &path).is_ok() {
                errno(EEXIST)
            } else {
                // POSIX: a missing parent is ENOENT, not EACCES. Rust's
                // create_dir_all only recurses on ENOENT - reporting EACCES
                // here aborts xcursor_sentinel's theme-directory setup.
                let parent = match path.rsplit_once('/') {
                    Some(("", _)) => "/",
                    Some((base, _)) => base,
                    None => "/",
                };
                if crate::vfs::resolve_directory(&cwd, parent).is_ok() {
                    errno(EACCES)
                } else {
                    errno(ENOENT)
                }
            }
        }
    }
}

fn linux_unlinkat(dirfd: u64, path_pointer: u64, flags: u64) -> u64 {
    if flags & !AT_REMOVEDIR != 0 {
        return errno(EINVAL);
    }
    // POSIX shm lives in the synthetic /dev/shm namespace, not in the
    // scratch VFS; wlroots calls shm_unlink() on every buffer it allocates.
    if let Some(path) = user_string(path_pointer) {
        if let Ok(cwd) = dirfd_cwd(dirfd) {
            if let Ok(resolved) = resolve(&cwd, &path) {
                if let Some(name) = resolved.strip_prefix("/dev/shm/") {
                    return match linux_fd::shm_unlink(name) {
                        Ok(()) => 0,
                        Err(code) => errno(code),
                    };
                }
                // A stale bound socket node: the registry is the only state.
                if linux_fd::listener_unlink(resolved.as_bytes()) {
                    return 0;
                }
            }
        }
    }
    let (cwd, path) = match scratch_resolved(dirfd, path_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    let result = if flags & AT_REMOVEDIR != 0 {
        crate::vfs::rmdir(&cwd, &path, scheduler::cwd_in_use)
    } else {
        crate::vfs::unlink(&cwd, &path)
    };
    if result.is_ok() {
        0
    } else {
        // A failed removal here means the target is missing (the write
        // permission was already checked by scratch_resolved); callers such
        // as OpenRC's shutdown loop expect the POSIX ENOENT.
        errno(ENOENT)
    }
}

fn linux_renameat(
    old_dirfd: u64,
    old_pointer: u64,
    new_dirfd: u64,
    new_pointer: u64,
    flags: u64,
) -> u64 {
    if flags & !(RENAME_NOREPLACE | 2) != 0 || flags & 2 != 0 {
        return errno(ENOSYS);
    }
    let (_, old_path) = match scratch_resolved(old_dirfd, old_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    let (_, new_path) = match scratch_resolved(new_dirfd, new_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    let result = if flags & RENAME_NOREPLACE != 0 {
        crate::vfs::rename("/", &old_path, &new_path, scheduler::cwd_in_use)
    } else {
        crate::vfs::replace("/", &old_path, &new_path)
    };
    if result.is_ok() {
        0
    } else {
        errno(EACCES)
    }
}

fn linux_chdir(path_ptr: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let cwd = scheduler::current_task_cwd();
    let Ok(resolved) = resolve_directory(&cwd, &path) else {
        return errno(ENOENT);
    };
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
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
        bytes.push(byte[0]);
    }
    core::str::from_utf8(&bytes)
        .ok()
        .map(alloc::string::String::from)
}

/// Copies a `char *const argv[]`/`envp[]` array: up to 64 NUL-terminated
/// strings, bounded like `user_string`. A null pointer means an empty array.
fn user_string_array(pointer: u64) -> Option<Vec<Vec<u8>>> {
    /// Linux allows large environments; the old 64-entry cap made `execve`
    /// fail with `EFAULT` for any command whose environment grew past it
    /// (busybox `cp` after OpenRC exported its variables).
    const MAX_ENTRIES: u64 = 1024;
    /// Total bytes of all strings, so a hostile array cannot exhaust the heap.
    const MAX_BYTES: usize = 256 * 1024;
    let mut result = Vec::new();
    let mut total = 0usize;
    if pointer == 0 {
        return Some(result);
    }
    for index in 0..MAX_ENTRIES {
        let mut raw = [0_u8; 8];
        user_memory::copy_from_user(pointer.wrapping_add(index * 8), &mut raw).ok()?;
        let entry = u64::from_le_bytes(raw);
        if entry == 0 {
            return Some(result);
        }
        let text = user_string(entry)?.into_bytes();
        total = total.checked_add(text.len() + 1)?;
        if total > MAX_BYTES {
            return None;
        }
        result.push(text);
    }
    None
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
    crate::console::write_dec_u64(millis);
    crate::console::write_str("\r\n");
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
    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;
    let limit: u64 = match resource {
        RLIMIT_STACK => 8 * 1024 * 1024,
        RLIMIT_NOFILE => 256,
        _ => 1 << 30,
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
    let size = (size as usize).min(1024);
    // Report the CPUs the machine actually has. Rust's
    // `available_parallelism` and async-global-executor size their worker
    // pool from this; answering "one CPU" on an SMP guest collapses the
    // executor to a single thread, which starves zbus's reader behind the
    // executor's `tick()` loop.
    let cpus = crate::smp::cpu_count().clamp(1, size * 8);
    let mut bytes = alloc::vec![0_u8; size];
    for cpu in 0..cpus {
        bytes[cpu / 8] |= 1 << (cpu % 8);
    }
    if user_memory::copy_to_user(&bytes, mask).is_err() {
        return errno(EFAULT);
    }
    size as u64
}

fn linux_uname(buffer: u64) -> u64 {
    // `struct utsname` is six 65-byte fields.
    let mut bytes = [0_u8; 6 * 65];
    let fields: [&[u8]; 6] = [
        b"Linux",
        b"werewolf",
        b"0.1.2",
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

/// `futex(2)`: without thread support a contended wait cannot be woken by
/// another task in the same address space. `FUTEX_WAIT` therefore validates
/// the value, sleeps for the requested timeout (or 10 ms when none is given)
/// and reports `-ETIMEDOUT`; `FUTEX_WAKE` reports zero waiters. This keeps
/// single-threaded libc paths working without pretending threads exist.
fn linux_futex(address: u64, operation: u64, value: u64, timeout: u64) -> u64 {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;
    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_WAKE_BITSET: u64 = 10;
    const ETIMEDOUT: u64 = 110;
    let task = scheduler::current_task_id();
    let tgid = scheduler::current_tgid();
    match operation & 0x7f {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let mut raw = [0_u8; 4];
            if user_memory::copy_from_user(address, &mut raw).is_err() {
                return errno(EFAULT);
            }
            if u32::from_le_bytes(raw) != value as u32 {
                return errno(EAGAIN);
            }
            let deadline = if timeout == 0 {
                None
            } else {
                let mut bytes = [0_u8; 16];
                if user_memory::copy_from_user(timeout, &mut bytes).is_err() {
                    return errno(EFAULT);
                }
                let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
                let nanoseconds = i64::from_le_bytes(bytes[8..].try_into().unwrap());
                if seconds < 0 {
                    None
                } else {
                    let millis = (seconds as u64).saturating_mul(1000)
                        + nanoseconds.max(0) as u64 / 1_000_000;
                    Some(
                        timer::ticks().saturating_add(timer::milliseconds_to_ticks(millis)),
                    )
                }
            };
            FUTEX_WAITERS.lock().push(FutexWaiter {
                address,
                tgid,
                task,
                woken: false,
            });
            loop {
                {
                    let mut waiters = FUTEX_WAITERS.lock();
                    let Some(entry) = waiters
                        .iter()
                        .find(|entry| entry.task == task && entry.address == address)
                    else {
                        return 0;
                    };
                    if entry.woken {
                        waiters.retain(|entry| entry.task != task || entry.address != address);
                        return 0;
                    }
                }
                let mut current = [0_u8; 4];
                if user_memory::copy_from_user(address, &mut current).is_err() {
                    futex_cleanup(task);
                    return errno(EFAULT);
                }
                if u32::from_le_bytes(current) != value as u32 {
                    futex_cleanup(task);
                    return 0;
                }
                if let Some(deadline) = deadline {
                    if timer::ticks() >= deadline {
                        futex_cleanup(task);
                        return errno(ETIMEDOUT);
                    }
                }
                scheduler::sleep(1);
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            futex_wake_address(address, tgid, value as usize) as u64
        }
        _ => errno(ENOSYS),
    }
}

/// Wakes up to `count` waiters on `(address, tgid)`. Returns how many were
/// marked; `wake_for_signal` also interrupts their polling sleep.
pub fn futex_wake_address(address: u64, tgid: u64, count: usize) -> usize {
    let mut wake = Vec::new();
    {
        let mut waiters = FUTEX_WAITERS.lock();
        for entry in waiters.iter_mut() {
            if wake.len() >= count {
                break;
            }
            if entry.address == address && entry.tgid == tgid && !entry.woken {
                entry.woken = true;
                wake.push(entry.task);
            }
        }
    }
    for task in &wake {
        scheduler::wake_for_signal(*task);
    }
    wake.len()
}

pub fn futex_cleanup(task: u64) {
    FUTEX_WAITERS.lock().retain(|entry| entry.task != task);
}

fn linux_mmap(
    address: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> u64 {
    const MAP_SHARED: u64 = 0x01;
    const MAP_PRIVATE: u64 = 0x02;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANONYMOUS: u64 = 0x20;
    const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
    const PROT_WRITE: u64 = 0x2;
    const PAGE_SIZE: u64 = crate::memory::PAGE_SIZE as u64;

    if length == 0 {
        return errno(EINVAL);
    }
    // Read permission is implied by every mapping we create; reject unknown
    // protection bits rather than silently ignoring them.
    if prot & !0x7 != 0 {
        return errno(EINVAL);
    }
    if flags & (MAP_PRIVATE | MAP_SHARED) == 0 {
        return errno(EINVAL);
    }
    let fixed_bits = MAP_FIXED | MAP_FIXED_NOREPLACE;
    let fixed = if flags & fixed_bits != 0 {
        if address == 0 || address % PAGE_SIZE != 0 {
            return errno(EINVAL);
        }
        Some(address as usize)
    } else {
        // A hint without MAP_FIXED is advisory; first-fit ignores it.
        None
    };

    if flags & MAP_ANONYMOUS != 0 {
        // Anonymous memory is lazily backed, like Linux: the virtual range is
        // reserved now and each page is committed when it is first touched.
        // Eager backing charged the whole span up front, so a single 8 MiB
        // `pthread_create` stack cost 2048 frames of the per-task budget even
        // though only a handful of its pages are ever used; a GTK-sized
        // process (most of its budget already spent on library mappings) then
        // failed the charge and thread creation returned EAGAIN. A pure
        // `PROT_NONE` reservation was already handled this way (glibc's
        // 128 MiB `new_heap` probe, ld.so's library spans).
        let lazy = true;
        let placed = match fixed {
            Some(address) if lazy => {
                scheduler::mmap_anonymous_at_lazy(length as usize, prot, address)
            }
            Some(address) => scheduler::mmap_anonymous_at(length as usize, prot, address),
            None if lazy => scheduler::mmap_anonymous_lazy(length as usize, prot),
            None => scheduler::mmap_anonymous(length as usize, prot),
        };
        scheduler::mmap_ring_record(0, placed.unwrap_or(0), length as usize, prot | flags);
        return placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM));
    }

    // File-backed mapping. Linux requires a page-aligned file offset.
    if offset % PAGE_SIZE != 0 {
        return errno(EINVAL);
    }
    // A `MAP_SHARED` memfd is a true shared window: every process that maps
    // the same file (passed over SCM_RIGHTS) sees the same frames. This is
    // what Wayland's `wl_shm` buffers use. Other files still copy into
    // private frames, and a writable shared mapping of a read-only SFS file
    // is refused rather than silently losing its write-back.
    if flags & MAP_SHARED != 0 && linux_fd::is_prime(scheduler::current_task_id(), fd) {
        let task = scheduler::current_task_id();
        let Some((pa, size)) = linux_fd::prime_info(task, fd) else {
            return errno(EINVAL);
        };
        if length as usize > size {
            return errno(EINVAL);
        }
        return crate::drm::map_prime(pa + offset as usize, length as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(EINVAL));
    }
    if flags & MAP_SHARED != 0 && linux_fd::is_drm(scheduler::current_task_id(), fd) {
        // DRM dumb buffers: the offset is the buffer's guest-physical address,
        // handed out by DRM_IOCTL_MODE_MAP_DUMB.
        if offset == 0 {
            return errno(EINVAL);
        }
        return crate::drm::map_dumb(length as usize, offset as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(EINVAL));
    }
    if flags & MAP_SHARED != 0 && linux_fd::is_gpufb(scheduler::current_task_id(), fd) {
        // The virtio-gpu scanout. The kernel owns the contiguous frames; the
        // mapping is Non-Cacheable so the device sees the compositor's writes.
        if offset != 0 {
            return errno(EINVAL);
        }
        return scheduler::map_gpufb(length as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(ENOMEM));
    }
    if flags & MAP_SHARED != 0
        && linux_fd::is_ivshmem(scheduler::current_task_id(), fd)
    {
        // Host-shared display surface. The kernel owns the BAR placement and
        // maps it Non-Cacheable; only offset 0 of the window is meaningful.
        if offset != 0 || length as usize > crate::ivshmem::BAR_SIZE {
            return errno(EINVAL);
        }
        return scheduler::map_ivshmem(length as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(ENOMEM));
    }
    if flags & MAP_SHARED != 0 {
        let task = scheduler::current_task_id();
        if let Ok((physical, owner)) =
            linux_fd::memfd_shared(task, fd, offset as usize, length as usize)
        {
            let placed = scheduler::mmap_shared_file(
                length as usize,
                prot,
                fixed,
                physical,
                owner,
            );
            return placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM));
        }
        if prot & PROT_WRITE != 0 {
            return errno(EINVAL);
        }
    }
    // MAP_PRIVATE: copy the file range into fresh frames, one page at a time.
    // A library segment is routinely larger than the whole-file read cap
    // (`MAX_FILE_BYTES`, 8 MiB — libgtk-3 is ~8.5 MiB), and the contiguous
    // allocator cannot find a 2000-page run after a few hundred mappings.
    // Mesa's `libgallium` pulls in `libLLVM`, which is 171 MiB; the loader
    // maps each PT_LOAD segment with one mmap. The mapping is still paged
    // (one frame per file page), so the cap only bounds a single request.
    const MAX_FILE_MMAP_BYTES: usize = 256 * 1024 * 1024;
    if length as usize > MAX_FILE_MMAP_BYTES {
        return errno(ENOMEM);
    }
    let task = scheduler::current_task_id();
    // A file from the (immutable) filesystem is mapped lazily: the loader maps
    // whole PT_LOAD segments, and an eager copy of libLLVM alone cost 7.7 s of
    // block-device reads for pages the program never executes. memfd and other
    // synthetic descriptors keep the eager path below.
    if let Some(path) = linux_fd::vfs_path(task, fd) {
        let placed = scheduler::mmap_file_lazy(
            length as usize,
            prot,
            fixed,
            path,
            offset as usize,
        );
        scheduler::mmap_ring_record(1, placed.unwrap_or(0), length as usize, prot | flags);
        return placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM));
    }
    let placed = scheduler::mmap_file_paged(length as usize, prot, fixed, |page_offset, buffer| {
        // The buffer arrives zeroed; a short read leaves the rest as the
        // zero-fill Linux expects past EOF.
        linux_fd::pread(task, fd, offset + page_offset as u64, buffer).is_ok()
    });
    placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM))
}

fn linux_mprotect(address: u64, length: u64, prot: u64) -> u64 {
    const PAGE_SIZE: u64 = crate::memory::PAGE_SIZE as u64;
    if address % PAGE_SIZE != 0 || prot & !0x7 != 0 {
        return errno(EINVAL);
    }
    if length == 0 {
        return 0;
    }
    if scheduler::mprotect(address as usize, length as usize, prot) {
        0
    } else {
        crate::console::write_str("LINUX: mprotect failed addr=0x");
        crate::console::write_hex_u64(address);
        crate::console::write_str(" len=0x");
        crate::console::write_hex_u64(length);
        crate::console::write_str(" prot=0x");
        crate::console::write_hex_u64(prot);
        crate::console::write_str("\r\n");
        errno(ENOMEM)
    }
}

fn linux_munmap(address: u64, length: u64) -> u64 {
    if address == 0 || length == 0 {
        return errno(EINVAL);
    }
    scheduler::mmap_ring_record(3, address as usize, length as usize, 0);
    if scheduler::munmap(address as usize, length as usize) {
        0
    } else {
        errno(EINVAL)
    }
}

fn linux_pread(fd: u64, buffer: u64, count: u64, offset: u64) -> u64 {
    let count = (count as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; count];
    let task = scheduler::current_task_id();
    match linux_fd::pread(task, fd, offset, &mut bytes) {
        Ok(read) => {
            if user_memory::copy_to_user(&bytes[..read], buffer).is_err() {
                return errno(EFAULT);
            }
            read as u64
        }
        Err(code) => errno(code),
    }
}

fn linux_pwrite(fd: u64, buffer: u64, count: u64, offset: u64) -> u64 {
    let count = (count as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; count];
    if user_memory::copy_from_user(buffer, &mut bytes).is_err() {
        return errno(EFAULT);
    }
    match linux_fd::pwrite(scheduler::current_task_id(), fd, offset, &bytes) {
        Ok(written) => written as u64,
        Err(code) => errno(code),
    }
}

fn linux_getrandom(buffer: u64, length: u64, flags: u64) -> u64 {
    // GRND_NONBLOCK(1) | GRND_RANDOM(2) | GRND_INSECURE(4). The kernel CSPRNG
    // never blocks, so all three are answered with real random bytes.
    if flags & !0x7 != 0 {
        return errno(EINVAL);
    }
    let length = length.min(256) as usize;
    let mut bytes = alloc::vec![0_u8; length];
    crate::entropy::fill(&mut bytes);
    if user_memory::copy_to_user(&bytes, buffer).is_err() {
        return errno(EFAULT);
    }
    length as u64
}

/// `mount(2)`. The kernel has no mountable block filesystems in the Linux
/// view: `proc`/`sysfs`/`devtmpfs`/`cgroup*` are synthetic and answered from
/// the kernel tables, and `tmpfs` becomes a `ramfs` backend inside the
/// writable service subtrees. Flags are accepted and ignored.
fn linux_mount(source: u64, target: u64, filesystem: u64, _flags: u64) -> u64 {
    let _ = source;
    let Some(target) = user_string(target) else {
        return errno(EFAULT);
    };
    let filesystem = if filesystem == 0 {
        String::new()
    } else {
        match user_string(filesystem) {
            Some(name) => name,
            None => return errno(EFAULT),
        }
    };
    match filesystem.as_str() {
        "proc" | "sysfs" | "devtmpfs" | "devpts" | "cgroup" | "cgroup2" | "securityfs"
        | "debugfs" | "tracefs" | "mqueue" | "efivarfs" | "fusectl" | "configfs" => 0,
        "tmpfs" | "ramfs" | "" => {
            let Ok(resolved) = resolve("/", &target) else {
                return errno(ENOENT);
            };
            if !scheduler::linux_write_allowed(&resolved) {
                return errno(EACCES);
            }
            if crate::vfs::mount("/", &resolved, "ramfs", "", 0, |_| false).is_ok() {
                return 0;
            }
            // An existing mount is success; a missing directory is ENOENT.
            match crate::vfs::resolve_directory("/", &resolved) {
                Ok(_) => 0,
                Err(()) => errno(ENOENT),
            }
        }
        _ => errno(EINVAL),
    }
}

/// `umount2(2)`: synthetic mounts are permanent and the kernel never wants a
/// Linux task to detach the root, so this is an idempotent success for a path
/// that resolves and `ENOENT` otherwise.
fn linux_umount(target: u64, _flags: u64) -> u64 {
    let Some(target) = user_string(target) else {
        return errno(EFAULT);
    };
    match resolve("/", &target) {
        Ok(_) => 0,
        Err(()) => errno(ENOENT),
    }
}

/// Zero-fills a user structure for the calls whose values do not matter to a
/// booting init (`getrusage`, `sysinfo`, `statfs`). A null pointer is a
/// no-op, matching the kernel's tolerance for optional outputs.
fn linux_zero_struct(pointer: u64, length: usize) -> u64 {
    if pointer == 0 {
        return 0;
    }
    let bytes = alloc::vec![0_u8; length];
    if user_memory::copy_to_user(&bytes, pointer).is_err() {
        return errno(EFAULT);
    }
    0
}
