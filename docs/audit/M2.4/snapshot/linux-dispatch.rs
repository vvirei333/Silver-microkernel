// src/linux_abi.rs: dispatcher head (184-310)
pub fn dispatch(frame: &mut [u64; crate::syscall::FRAME_QUADWORDS]) -> u64 {
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
        SYS_STATX => linux_statx(args[0], args[1], args[2], args[3], args[4]),
        SYS_FCHMOD | SYS_FCHOWN => linux_fchmod(args[0]),
        SYS_FCHMODAT | SYS_FCHOWNAT => linux_fchmodat(args[0], args[1]),
        SYS_CLOSE_RANGE => linux_close_range(args[0], args[1], args[2]),
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

// sigaltstack (1407-1455)
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

// kill/tgkill + may_signal (1457-1530)
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
