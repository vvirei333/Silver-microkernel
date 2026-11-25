use super::*;
use super::cap::{cap_denies_cwd, silver_cap};
use super::fs::*;
use super::ipc::*;
use super::memory::*;
use super::process::*;

/// Private Silver extension: op 0 enter capability mode, 1 limit fd rights,
/// 2 read fd rights. Not part of the Linux ABI.
const SYS_SILVER_CAP: u64 = 0x534c_5601;
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
const SYS_EXECVEAT: u64 = 281;
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
        SYS_SILVER_CAP => silver_cap(args[0], args[1], args[2], args[3]),
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
        SYS_CHROOT => {
            if cap_denies_cwd() { errno(EPERM) } else { 0 }
        }
        SYS_PIVOT_ROOT => errno(ENOSYS),
        SYS_SETSID => scheduler::current_tgid(),
        SYS_SETPGID | SYS_SETHOSTNAME => 0,
        SYS_GETPGID | SYS_GETSID => scheduler::current_tgid(),
        SYS_UMASK => 0o22,
        SYS_GETRUSAGE => linux_zero_struct(args[1], 144),
        SYS_SYSINFO => linux_zero_struct(args[0], 112),
        SYS_STATFS => {
            if cap_denies_cwd() {
                errno(EPERM)
            } else {
                linux_statfs(args[0], args[1])
            }
        }
        SYS_FSTATFS => linux_fstatfs(args[0], args[1]),
        SYS_UTIMENSAT => {
            if cap_denies_cwd() { errno(EPERM) } else { 0 }
        }
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
        SYS_EXECVEAT => linux_execveat(frame, args[0], args[1], args[2], args[3], args[4]),
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
            if cap_denies_cwd() {
                return errno(EPERM);
            }
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
