use super::*;
use super::fs::{dirfd_cwd, resolve, user_string, user_string_array};

/// `sched_getattr(2)`: SCHED_OTHER, no runtime constraints. The caller's
/// size field is untrusted - a kernel Vec with a user capacity would panic
/// the allocator, so anything past the struct cap reports EINVAL.
pub(super) fn linux_sched_getattr(attr_pointer: u64, size: u64) -> u64 {
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

/// `execve(2)`: replace the process image. On success the syscall trampoline
/// must return to a different EL0 entry and stack, so this mutates the saved
/// frame (ELR) and SP_EL0 instead of returning normally.
pub(super) fn linux_execve(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    path_pointer: u64,
    argv_pointer: u64,
    envp_pointer: u64,
) -> u64 {
    // Path-based exec would reopen exactly the namespace capability mode is
    // meant to remove; `execveat(fd, "", ..., AT_EMPTY_PATH)` is the escape.
    if scheduler::current_task_capability_mode() {
        return errno(EPERM);
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
    exec_path(frame, &cwd, path, argv, envp)
}

/// `execveat(2)`: the fd-relative form, and the only way to exec after
/// entering capability mode. A non-empty path still needs `dirfd` lookup
/// rights; an empty path with `AT_EMPTY_PATH` uses the descriptor itself.
pub(super) fn linux_execveat(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    dirfd: u64,
    path_pointer: u64,
    argv_pointer: u64,
    envp_pointer: u64,
    flags: u64,
) -> u64 {
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    const AT_EMPTY_PATH: u64 = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return errno(EINVAL);
    }
    let Some(path) = user_string(path_pointer) else {
        return errno(EFAULT);
    };
    let Some(argv) = user_string_array(argv_pointer) else {
        return errno(EFAULT);
    };
    let Some(envp) = user_string_array(envp_pointer) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return errno(ENOENT);
        }
        let task = scheduler::current_task_id();
        if let Err(code) =
            linux_fd::require_right(task, dirfd, linux_fd::FD_READ | linux_fd::FD_FSTAT)
        {
            return errno(code);
        }
        if let Some(resolved) = linux_fd::vfs_path(task, dirfd) {
            return exec_path(frame, "/", resolved, argv, envp);
        }
        // Anonymous files have no path, so read the image out of the memfd.
        let bytes = match linux_fd::memfd_bytes(task, dirfd, 32 * 1024 * 1024) {
            Ok(bytes) => bytes,
            Err(code) => return errno(code),
        };
        return exec_bytes(frame, bytes, argv, envp);
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    exec_path(frame, &cwd, path, argv, envp)
}

fn exec_path(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    cwd: &str,
    path: alloc::string::String,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> u64 {
    const EXEC_LIMIT: usize = 32 * 1024 * 1024;
    // `execve` from a `CLONE_VM` sibling would build a new address space and
    // then release the leader's, pulling the tables out from under every
    // running thread. Until a thread takes over the whole group, refuse
    // instead of corrupting the process.
    if scheduler::current_is_thread() {
        return errno(EINVAL);
    }
    crate::console::write_str(&path);
    crate::console::write_str("\r\n");
    let mut target = path;
    let mut arguments = argv;
    let mut depth = 0u32;
    let resolved = loop {
        let Ok(resolved) = resolve(cwd, &target) else {
            crate::console::write_str("EXECFAIL resolve ");
            crate::console::write_str(&target);
            crate::console::write_str("\r\n");
            return errno(ENOENT);
        };
        if !scheduler::current_read_path_allowed(&resolved) {
            return errno(EACCES);
        }
        match crate::vfs::file_len(cwd, &resolved) {
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
        let header = crate::vfs::read_at(cwd, &resolved, 0, 256).unwrap_or_default();
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
    let Ok(image) = crate::linux_elf::load_linux_program_path(cwd, &resolved, EXEC_LIMIT) else {
        return errno(ENOEXEC);
    };
    replace_image(frame, image, arguments, envp)
}

/// Commits a loaded image to the current task. Kept separate so both the
/// path loader and the descriptor loader end at exactly one place.
fn replace_image(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    image: crate::linux_elf::LinuxImage,
    arguments: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> u64 {
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

/// Executes an ELF (or a `#!` script) already read from a descriptor.
fn exec_bytes(
    frame: &mut [u64; crate::syscall::FRAME_QUADWORDS],
    bytes: Vec<u8>,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> u64 {
    const EXEC_LIMIT: usize = 32 * 1024 * 1024;
    if bytes.starts_with(b"#!") {
        let header = &bytes[..bytes.len().min(256)];
        let Some((interpreter, argument)) = parse_shebang(&header[2..]) else {
            return errno(ENOEXEC);
        };
        let mut arguments = Vec::with_capacity(argv.len() + 2);
        arguments.push(interpreter.clone().into_bytes());
        if let Some(argument) = argument {
            arguments.push(argument.into_bytes());
        }
        arguments.extend(argv);
        return exec_path(frame, "/", interpreter, arguments, envp);
    }
    let Ok(image) = crate::linux_elf::load_linux_program("/", &bytes, EXEC_LIMIT) else {
        return errno(ENOEXEC);
    };
    replace_image(frame, image, argv, envp)
}

/// Parses the rest of a `#!` line: the interpreter path and at most one
/// optional argument (Linux passes the remainder as one argument).
pub(super) fn parse_shebang(bytes: &[u8]) -> Option<(String, Option<String>)> {
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
pub(super) fn linux_clone(
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
pub(super) fn linux_wait4(pid: i64, status_pointer: u64, options: u64) -> Result<u64, u64> {
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

/// `rt_sigprocmask(2)`: block/unblock/set the calling task's signal mask.
pub(super) fn linux_rt_sigprocmask(how: u64, set_pointer: u64, old_pointer: u64) -> u64 {
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
pub(super) fn linux_signalfd4(fd: u64, set_pointer: u64, size: u64, flags: u64) -> u64 {
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
pub(super) fn linux_rt_sigtimedwait(set_pointer: u64, info_pointer: u64, _size: u64) -> u64 {
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
pub(super) fn linux_rt_sigaction(signal: u64, action_pointer: u64, old_pointer: u64) -> u64 {
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
pub(super) fn linux_sigaltstack(stack_pointer: u64, old_pointer: u64) -> u64 {
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
pub(super) fn linux_kill(pid: i64, signal: u64) -> u64 {
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
pub(super) fn may_signal(sender: u64, target: u64) -> bool {
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

pub(super) fn linux_clock_gettime(_clock: u64, timespec: u64) -> u64 {
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
pub(super) fn linux_clock_getres(resolution: u64) -> u64 {
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

pub(super) fn linux_gettimeofday(time: u64) -> u64 {
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
pub(super) fn linux_nanosleep(request: u64, remaining: u64) -> u64 {
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

pub(super) fn linux_getrlimit(resource: u64, buffer: u64) -> u64 {
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

pub(super) fn linux_sched_getaffinity(size: u64, mask: u64) -> u64 {
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

pub(super) fn linux_uname(buffer: u64) -> u64 {
    // `struct utsname` is six 65-byte fields.
    let mut bytes = [0_u8; 6 * 65];
    let fields: [&[u8]; 6] = [
        b"Linux",
        b"werewolf",
        silver_microkernel::VERSION.as_bytes(),
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

pub(super) fn linux_getrandom(buffer: u64, length: u64, flags: u64) -> u64 {
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
