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
const SYS_FUTEX: u64 = 98;
const SYS_MEMFD_CREATE: u64 = 279;
const SYS_IOCTL: u64 = 29;
const SYS_FCNTL: u64 = 25;
const SYS_GETCWD: u64 = 17;
const SYS_CHDIR: u64 = 49;
const SYS_OPENAT: u64 = 56;
const SYS_PIPE2: u64 = 59;
const SYS_PPOLL: u64 = 73;
const SYS_CLOSE: u64 = 57;
const SYS_LSEEK: u64 = 62;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_WRITEV: u64 = 66;
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
const SYS_RT_SIGRETURN: u64 = 139;
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
const SYS_MKDIRAT: u64 = 34;
const SYS_UNLINKAT: u64 = 35;
const SYS_RENAMEAT: u64 = 38;
const SYS_RENAMEAT2: u64 = 276;
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
const MSG_DONTWAIT: u64 = 0x40;
const MSG_CTRUNC: i32 = 0x8;

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
            linux_fd::memfd_truncate(
                scheduler::current_task_id(),
                args[0],
                args[1] as usize,
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
        SYS_FACCESSAT => linux_faccessat(args[0], args[1], args[2]),
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
        SYS_EXIT => scheduler::exit_current_with_status(args[0]),
        SYS_EXIT_GROUP => scheduler::exit_group(args[0]),
        SYS_EXECVE => linux_execve(frame, args[0], args[1], args[2]),
        SYS_CLONE => linux_clone(frame, args[0], args[1], args[2], args[3], args[4]),
        SYS_WAIT4 => finish(linux_wait4(args[0] as i64, args[1], args[2])),
        SYS_SET_TID_ADDRESS => scheduler::current_task_id(),
        SYS_SET_ROBUST_LIST => 0, // single-threaded: nothing to register
        SYS_CLOCK_GETTIME => linux_clock_gettime(args[0], args[1]),
        SYS_RT_SIGPROCMASK => linux_rt_sigprocmask(args[0], args[1], args[2]),
        SYS_RT_SIGACTION => linux_rt_sigaction(args[0], args[1], args[2]),
        SYS_RT_SIGRETURN => {
            crate::linux_signal::restore_frame(frame);
            0
        }
        SYS_SIGALTSTACK => linux_sigaltstack(args[0], args[1]),
        SYS_KILL => linux_kill(args[0] as i64, args[1]),
        SYS_TGKILL => linux_kill(args[1] as i64, args[2]),
        SYS_UNAME => linux_uname(args[0]),
        SYS_GETPID => scheduler::current_tgid(),
        SYS_GETTID => scheduler::current_task_id(),
        SYS_GETPPID => scheduler::current_task_ppid().unwrap_or(0),
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
    let Some(argv) = user_string_array(argv_pointer) else {
        return errno(EFAULT);
    };
    let Some(envp) = user_string_array(envp_pointer) else {
        return errno(EFAULT);
    };
    let cwd = scheduler::current_task_cwd();
    let Ok(resolved) = crate::vfs::resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
    match crate::vfs::file_len(&cwd, &resolved) {
        Ok(length) if length <= EXEC_LIMIT => {}
        _ => return errno(ENOENT),
    }
    let Ok(image) = crate::linux_elf::load_linux_program_path(&cwd, &resolved, EXEC_LIMIT) else {
        return errno(ENOEXEC);
    };
    let Some(result) = scheduler::exec_linux_current(image, argv, envp) else {
        return errno(ENOMEM);
    };
    // ELR is frame word 31 (x0-x30, ELR, SPSR). The user stack pointer lives
    // in SP_EL0, which the trampoline does not touch.
    frame[31] = result.entry as u64;
    unsafe {
        core::arch::asm!("msr sp_el0, {}", in(reg) result.stack_pointer);
    }
    0
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
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_THREAD: u64 = 0x0001_0000;
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

    if flags & CLONE_VM != 0 {
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

    if stack != 0 || tls != 0 || flags & CLONE_THREAD != 0 || flags & CLONE_SIGHAND != 0 {
        return errno(EINVAL);
    }
    match scheduler::fork_current(frame) {
        Some(pid) => pid,
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
    if max_events == 0 || max_events > 64 {
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

fn linux_pipe2(fds_pointer: u64, _flags: u64) -> u64 {
    let task = scheduler::current_task_id();
    let Ok((read_fd, write_fd)) = linux_fd::pipe(task) else {
        return errno(EMFILE);
    };
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

/// `ppoll(2)`: readiness for up to 32 descriptors, with a timeout. A null
/// timeout blocks, but is capped at 60 s so a stuck poll cannot wedge the
/// kernel forever (there is no signal delivery to break it yet).
fn linux_ppoll(
    fds_pointer: u64,
    nfds: u64,
    timeout_pointer: u64,
) -> Result<u64, u64> {
    const POLLERR: i16 = 0x008;
    const POLLHUP: i16 = 0x010;
    const POLLNVAL: i16 = 0x020;
    if nfds > 32 {
        return Err(EINVAL);
    }
    let count = nfds as usize;
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
    if (base != 1 && base != 2) || protocol != 0 {
        return errno(EPROTONOSUPPORT);
    }
    let nonblock = kind & SOCK_NONBLOCK != 0;
    let task = scheduler::current_task_id();
    match linux_fd::socketpair(task, nonblock) {
        Ok((first, second)) => {
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
    if domain != AF_UNIX {
        return errno(EAFNOSUPPORT);
    }
    let base = kind & 0xf;
    if (base != 1 && base != 2) || protocol != 0 {
        return errno(EPROTONOSUPPORT);
    }
    finish(linux_fd::socket(
        scheduler::current_task_id(),
        kind & SOCK_NONBLOCK != 0,
    ))
}

fn linux_bind(fd: u64, address: u64, length: u64) -> u64 {
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
                scheduler::exit_current_with_status(128 + signal as u64);
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
    let Ok(resolved) = crate::vfs::resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
    if mode & W_OK != 0
        && !silver_microkernel::path_is_under(&resolved, crate::scheduler::LINUX_SCRATCH_PREFIX)
    {
        // SFS is read-only; the Linux scratch tree (`/tmp`) is the one place
        // a toolkit may create caches. A blanket EROFS made fontconfig
        // declare every cache directory unwritable.
        return errno(EROFS);
    }
    if resolved.starts_with("/dev/")
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
    let Ok(resolved) = crate::vfs::resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
    // Directories first: the SFS backend reports a directory inode's size as
    // 0, so `file_len` succeeds for one and used to classify it as a regular
    // file. Toolkits stat XDG/data directories and reject a "file" where a
    // directory is required (xkbcommon refused its own config root).
    if crate::vfs::read_dir(&cwd, &path).is_ok() {
        return match linux_fd::write_stat(
            buffer,
            S_IFDIR | 0o755,
            0,
            linux_fd::path_ino(&resolved),
        ) {
            Ok(()) => 0,
            Err(code) => errno(code),
        };
    }
    if let Ok(size) = crate::vfs::file_len(&cwd, &path) {
        return match linux_fd::write_stat(
            buffer,
            S_IFREG | 0o644,
            size as u64,
            linux_fd::path_ino(&resolved),
        ) {
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
        F_GETFD => 0,
        F_GETFL => match linux_fd::status_flags(task, fd) {
            Some(flags) => flags,
            None => errno(EBADF),
        },
        F_SETFD => 0,
        F_SETFL => {
            let _ = linux_fd::set_nonblock(task, fd, argument & 0x800 != 0);
            0
        }
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
    let resolved = crate::vfs::resolve(&cwd, &path).map_err(|_| ENOENT)?;
    if !silver_microkernel::path_is_under(&resolved, crate::scheduler::LINUX_SCRATCH_PREFIX) {
        return Err(EROFS);
    }
    Ok((cwd, path))
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
                errno(EACCES)
            }
        }
    }
}

fn linux_unlinkat(dirfd: u64, path_pointer: u64, flags: u64) -> u64 {
    if flags & !AT_REMOVEDIR != 0 {
        return errno(EINVAL);
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
        errno(EACCES)
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
    let Ok(resolved) = crate::vfs::resolve_directory(&cwd, &path) else {
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
        if bytes.len() >= 255 {
            return None;
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
    let mut result = Vec::new();
    if pointer == 0 {
        return Some(result);
    }
    for index in 0..64u64 {
        let mut raw = [0_u8; 8];
        user_memory::copy_from_user(pointer.wrapping_add(index * 8), &mut raw).ok()?;
        let entry = u64::from_le_bytes(raw);
        if entry == 0 {
            return Some(result);
        }
        result.push(user_string(entry)?.into_bytes());
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
        b"0.1.1",
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
        let placed = match fixed {
            Some(address) => scheduler::mmap_anonymous_at(length as usize, prot, address),
            None => scheduler::mmap_anonymous(length as usize, prot),
        };
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
    const MAX_FILE_MMAP_BYTES: usize = 64 * 1024 * 1024;
    if length as usize > MAX_FILE_MMAP_BYTES {
        return errno(ENOMEM);
    }
    let task = scheduler::current_task_id();
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
    if flags & !0x3 != 0 {
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
