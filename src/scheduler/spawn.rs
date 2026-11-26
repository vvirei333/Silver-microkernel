//! Task creation: boot tasks, native programs, ELF images and Linux threads.
//!
//! A child module of `scheduler`, like `wait.rs`: spawning needs the parent's
//! private task table, ready queues and capability helpers, and nothing outside
//! the scheduler should reach into the spawn path. Entry points other
//! subsystems call (`spawn_user`, `spawn_elf*`, `spawn_thread`,
//! `spawn_user_with_args*`) are `pub` and re-exported by `scheduler`; the
//! boot-only helpers (`spawn_kernel`, `spawn_idle`) are `pub(super)`.

use super::*;
use super::switch::enqueue_ready;
pub(super) fn spawn_kernel(entry: extern "C" fn() -> !) -> Option<u64> {
    spawn_inner(Some(entry), None, Vec::new(), Vec::new(), Vec::new(), false)
}

/// Spawns one idle task and marks it explicitly. Boot idle is id 0; AP idle
/// tasks are spawned from `finish_init` so they cannot shift SCOUT_ID or any
/// other spawn-order identity.
pub(super) fn spawn_idle(explicit_id: Option<u64>) -> Option<u64> {
    let saved_next = {
        let manager = scheduler_manager()?;
        let saved = manager.next_id;
        if let Some(id) = explicit_id {
            manager.next_id = id;
        }
        saved
    };
    let result = spawn_kernel(idle_task);
    if let Some(manager) = scheduler_manager() {
        if explicit_id.is_some() {
            manager.next_id = saved_next;
        }
        if let Some(id) = result {
            if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
                task.is_idle = true;
            }
        }
    }
    result
}

pub fn spawn_user(program: u64, argument: u64) -> Option<u64> {
    spawn_inner(
        None,
        Some((program, argument)),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        false,
    )
}

pub fn spawn_user_with_cap(program: u64, argument: u64) -> Option<capability::CapId> {
    // The spawn handle needs one CapTable slot. Check before creating the task
    // so a full table cannot leak a running child without a handle.
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_user(program, argument)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// Spawns a task from an already-loaded dynamic ELF image (the VFS/`run`
/// path). The image is consumed; on success the caller receives a capability
/// handle to the new task so it can lend it the console, wait on it and kill
/// it, exactly like [`spawn_user_with_cap`]. Unlike embedded programs, a
/// dynamically loaded task receives no kernel-granted capabilities by default
/// — any device rights must be delegated by its parent.
pub fn spawn_elf(image: LoadedImage, argument: u64) -> Option<capability::CapId> {
    spawn_elf_with_scope(image, argument, None)
}

/// [`spawn_elf`] with an optional write scope from the bundle manifest. The
/// task still receives no global rights: without `CAP_FS_WRITE` it may only
/// write paths under `fs_write_prefix`.
pub fn spawn_elf_with_scope(
    image: LoadedImage,
    argument: u64,
    fs_write_prefix: Option<String>,
) -> Option<capability::CapId> {
    spawn_elf_with_policy(image, argument, fs_write_prefix, None)
}

/// [`spawn_elf_with_scope`] with an optional block/inode quota for the write
/// prefix. The quota is enforced by the syscall write paths; a trusted task
/// with its own `CAP_FS_WRITE` is never restricted by it.
pub fn spawn_elf_with_policy(
    image: LoadedImage,
    argument: u64,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
) -> Option<capability::CapId> {
    spawn_elf_with_policy_and_args(
        image,
        argument,
        fs_write_prefix,
        fs_quota,
        Personality::Native,
        Vec::new(),
        Vec::new(),
    )
}

/// [`spawn_elf_with_policy`] with an explicit syscall personality and initial
/// `argv`/`envp`. A Linux image must pass at least `argv[0]`, because Linux
/// `_start` reads `argc` from the initial stack.
pub fn spawn_elf_with_policy_and_args(
    image: LoadedImage,
    argument: u64,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
    personality: Personality,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<capability::CapId> {
    if scheduler_manager().map_or(true, |manager| !has_task_capacity(manager)) {
        // Dropping `image` releases its frames (RAII).
        return None;
    }
    // Reserve the parent's handle slot before the child exists; otherwise the
    // task would run without a capability and the insert would break the
    // CapTable bound.
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_elf_task(
        Some(image),
        None,
        argument,
        fs_write_prefix,
        fs_quota,
        personality,
        argv,
        envp,
        false,
    )?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// Spawns a foreign (Linux ABI) image. Same capability model as
/// [`spawn_elf_with_policy_and_args`]: the child starts with no rights and the
/// parent gets a handle it can delegate.
#[cfg(feature = "linux-abi")]
pub fn spawn_linux_elf_with_args(
    image: LinuxImage,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<capability::CapId> {
    if scheduler_manager().map_or(true, |manager| !has_task_capacity(manager)) {
        crate::console::write_str("LINUX: no task capacity\r\n");
        return None;
    }
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        crate::console::write_str("LINUX: no capability capacity\r\n");
        return None;
    }
    let task_id = match spawn_elf_task(
        None,
        Some(image),
        0,
        fs_write_prefix,
        fs_quota,
        Personality::Linux,
        argv,
        envp,
        true,
    ) {
        Some(task_id) => task_id,
        None => {
            crate::console::write_str("LINUX: spawn_elf_task failed\r\n");
            return None;
        }
    };
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// Creates a Linux thread sharing the current task's address space
/// (`CLONE_VM|CLONE_THREAD`). The new task gets a fresh kernel stack, the
/// caller-provided user stack/TLS and a copy of the SVC frame with `x0 = 0`.
/// It does not own the space or the process resources; the leader keeps them.
#[cfg(feature = "linux-abi")]
pub fn spawn_thread(
    frame: &[u64; crate::syscall::FRAME_QUADWORDS],
    stack: u64,
    tls: u64,
    parent_tid: u64,
    child_tid: u64,
    parent_settid: bool,
    child_settid: bool,
    clear_child_tid: Option<u64>,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    let (
        ttbr0,
        tgid,
        program_break,
        cwd,
        argv,
        envp,
        prefix,
        read_prefix,
        quota,
        priority,
        cap_mode,
        fp,
    ) = {
        let parent = manager.tasks.get(smp::current_index())?;
        if parent.personality != Personality::Linux {
            return None;
        }
        (
            parent.ttbr0,
            parent.tgid,
            parent.program_break,
            parent.cwd.clone(),
            parent.argv.clone(),
            parent.envp.clone(),
            parent.fs_write_prefix.clone(),
            parent.read_prefix.clone(),
            parent.fs_quota,
            parent.priority,
            parent.cap_mode,
            unsafe { *smp::this_cpu().fp.get() },
        )
    };
    // A thread stack is part of the process footprint, not a separate ledger:
    // charge it to the group leader so `RLIMIT_AS`-style accounting (and the
    // per-task frame limit) sees the real cost of N threads. The thread's own
    // `mem_frames` stays zero; its address-space operations are routed to the
    // leader by `with_group_owner` anyway.
    let leader_index = manager.tasks.iter().position(|task| task.id == tgid)?;
    if !mem_policy::task_charge_ok(
        manager.tasks[leader_index].mem_frames,
        TASK_STACK_PAGES,
        task_frame_limit(),
    ) {
        return None;
    }
    let kernel_stack_start = kernel_stack::alloc()?;
    let frame_address = phys_to_virt(kernel_stack_start + TASK_STACK_SIZE) - 272;
    unsafe {
        core::ptr::copy_nonoverlapping(
            frame.as_ptr() as *const u8,
            frame_address as *mut u8,
            264,
        );
        core::ptr::write_bytes((frame_address + 264) as *mut u8, 0, 8);
        (frame_address as *mut u64).write(0);
    }
    let id = manager.next_id;
    let mut context = TaskContext::zero();
    context.stack_pointer = frame_address as u64;
    context.user_stack_pointer = stack;
    context.elr = silver_fork_bootstrap as *const () as usize as u64;
    context.tls = tls;
    let task = Task {
        fs_write_prefix: prefix,
        fs_quota: quota,
        read_prefix,
        owns_space: false,
        tgid,
        pid_one: false,
        is_thread: true,
        clear_child_tid,
        id,
        state: TaskState::Ready,
        resume_state: None,
        program: user_program::PROGRAM_DYNAMIC,
        argument: 0,
        personality: Personality::Linux,
        cap_mode,
        cap_table: capability::CapTable::new(),
        bound_irq: None,
        irq_pending: false,
        arguments: Vec::new(),
        ppid: None,
        exit_status: None,
        resources_released: false,
        argv,
        envp,
        cwd,
        owns_console: false,
        priority,
        context,
        fp,
        stack: MemoryRegion::new(
            kernel_stack_start,
            kernel_stack_start + TASK_STACK_SIZE,
        ),
        user_stack: MemoryRegion::new(0, 0),
        user_stack_generation: 0,
        private_page: 0,
        private_page_generation: 0,
        space_token: TableToken::empty(),
        dma_pages: Vec::new(),
        mmap_regions: Vec::new(),
        mmap_bytes: 0,
        signal_trampoline: None,
        mem_frames: 0,
        program_break,
        waiting_on: None,
        owned_mutexes: Vec::new(),
        running_on: None,
        is_idle: false,
        ttbr0,
        user_image: None,
        linux_image: None,
        heap_pages: Vec::new(),
    };
    // Publish the TID before the child can run and clear it again.
    let word = (id as u32).to_le_bytes();
    if parent_settid
        && parent_tid != 0
        && crate::user_memory::copy_to_user(&word, parent_tid).is_err()
    {
        kernel_stack::free(kernel_stack_start);
        return None;
    }
    if child_settid
        && child_tid != 0
        && crate::user_memory::copy_to_user(&word, child_tid).is_err()
    {
        kernel_stack::free(kernel_stack_start);
        return None;
    }
    manager.tasks[leader_index].mem_frames += TASK_STACK_PAGES;
    manager.tasks.push(task);
    manager.next_id += 1;
    let index = manager.tasks.len() - 1;
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(id)
}

fn spawn_elf_task(
    user_image: Option<LoadedImage>,
    linux_image: Option<LinuxImage>,
    argument: u64,
    fs_write_prefix: Option<String>,
    fs_quota: Option<(u32, u32)>,
    personality: Personality,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
    foreground: bool,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    let id = manager.next_id;
    let cwd = manager
        .tasks
        .get(smp::current_index())
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"));
    let mut task = match Task::new(
        id,
        None,
        Some((user_program::PROGRAM_DYNAMIC, argument)),
        user_image,
        linux_image,
        Vec::new(),
        argv,
        envp,
        current_task_pid(),
        fs_write_prefix,
        fs_quota,
        personality,
    ) {
        Some(task) => task,
        None => {
            crate::console::write_str("TASK: new failed\r\n");
            return None;
        }
    };
    task.cwd = cwd;
    task.cap_mode = super::current_task_capability_mode();
    manager.tasks.push(task);
    manager.next_id += 1;
    // A freshly spawned task starts Ready: queue it on the spawning CPU.
    let index = manager.tasks.len() - 1;
    // A foreground child (the shell's `linuxrun`) gets the console before it
    // can be dispatched. Assigning it from the shell *after* spawn is racy: on
    // an SMP boot another CPU can run and even exit the child between the two
    // syscalls, and its output would be discarded.
    if foreground && current_task_owns_console() {
        set_console_owner(id);
    }
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(id)
}

pub fn spawn_user_with_args_and_cap(
    program: u64,
    argument: u64,
    arguments: Vec<u8>,
) -> Option<capability::CapId> {
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_user_with_args(program, argument, arguments)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

/// Spawns an embedded program and makes it the console owner in the same
/// scheduler pass (`SYS_SPAWN_ARGS_CONSOLE`).
///
/// This is what the shell uses for foreground commands: the child cannot run
/// before it owns the console, so a fast utility cannot exit outputless
/// between `SYS_SPAWN` and `SYS_CONSOLE_SET`. The caller must hold
/// `CAP_CONSOLE_GRANT`; the syscall layer checks that before calling in.
pub fn spawn_user_with_args_console_and_cap(
    program: u64,
    argument: u64,
    arguments: Vec<u8>,
) -> Option<capability::CapId> {
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_inner(
        None,
        Some((program, argument)),
        arguments,
        Vec::new(),
        Vec::new(),
        true,
    )?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

pub fn spawn_user_with_args(program: u64, argument: u64, arguments: Vec<u8>) -> Option<u64> {
    spawn_inner(
        None,
        Some((program, argument)),
        arguments,
        Vec::new(),
        Vec::new(),
        false,
    )
}

/// Spawns an embedded program with `argv`/`envp` delivered on its initial
/// user stack (the versioned `SYS_SPAWN_ARGSV` path). The legacy flat
/// `arguments` blob stays empty, so `SYS_ARGS` keeps its old behaviour.
pub fn spawn_user_with_argv_envp_and_cap(
    program: u64,
    argument: u64,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Option<capability::CapId> {
    if current_task_mut().map_or(true, |task| !task.cap_table.has_capacity()) {
        return None;
    }
    let task_id = spawn_inner(None, Some((program, argument)), Vec::new(), argv, envp, false)?;
    let parent = current_task_mut()?;
    Some(parent.cap_table.insert(task_id, capability::CAP_ALL))
}

fn spawn_inner(
    entry: Option<extern "C" fn() -> !>,
    user: Option<(u64, u64)>,
    arguments: Vec<u8>,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
    console_to_child: bool,
) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    let id = manager.next_id;

    // Resolve the embedded image before `Task::new`: a lookup or load failure
    // then needs no cleanup, because no frame has been allocated yet. The
    // dynamic path passes a pre-loaded image straight to `spawn_elf`.
    let image = match user {
        Some((program, _)) => {
            let bytes = user_program::image(program)?;
            Some(elf_loader::load(bytes)?)
        }
        None => None,
    };

    let ppid = current_task_pid();
    let cwd = manager
        .tasks
        .get(smp::current_index())
        .map(|task| task.cwd.clone())
        .unwrap_or_else(|| String::from("/"));
    // Wolfser persists its bookmarks in one root-level file. Give it a
    // path-scoped write window instead of the global CAP_FS_WRITE: the kernel
    // resolves the path and only `/sfs/wolfser.bookmarks` passes the check.
    let fs_write_prefix = user.and_then(|(program, _)| {
        if program == user_program::WOLFSER {
            Some(String::from("/sfs/wolfser.bookmarks"))
        } else {
            None
        }
    });
    let mut task = Task::new(
        id,
        entry,
        user,
        image,
        None,
        arguments,
        argv,
        envp,
        ppid,
        fs_write_prefix,
        None,
        Personality::Native,
    )?;
    task.cwd = cwd;
    task.cap_mode = super::current_task_capability_mode();

    manager.tasks.push(task);
    manager.next_id += 1;
    // A freshly spawned task starts Ready: queue it on the spawning CPU.
    let index = manager.tasks.len() - 1;
    if console_to_child {
        // Atomic console handoff. `SYS_SPAWN` returns to a caller that then has
        // to call `SYS_CONSOLE_SET`, and the child is already Ready: one timer
        // tick in that window lets a fast command (`cat` of a small file) run to
        // completion while it still owns no console, so its output is discarded
        // and the handoff afterwards fails because the task is a zombie. Doing
        // the handoff here closes the window: the child is the console owner
        // before it can be dispatched at all.
        let caller = current_task_id();
        for candidate in &mut manager.tasks {
            candidate.owns_console = candidate.id == id;
        }
        manager.console_parent = if caller == IDLE_TASK_ID {
            None
        } else {
            Some(caller)
        };
    }
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(id)
}

/// Self-capabilities a freshly spawned embedded program receives. Dynamic
/// ELF tasks start with none and must be delegated rights by their parent.
/// `CAP_MOUNT`/`CAP_POWER` are deliberately outside `CAP_ALL`, so a spawn
/// handle can never carry them.
pub(super) fn initial_self_rights(program: u64) -> Option<u32> {
    let rights = match program {
        user_program::SILVERD => {
            capability::CAP_ALL | capability::CAP_MOUNT | capability::CAP_POWER
        }
        // The UART driver owns the input path, including the Ctrl-Z console
        // stop request; without this right SYS_STOP_CONSOLE is denied.
        user_program::HOWLER => {
            capability::CAP_MMIO | capability::CAP_CONSOLE_GRANT
        }
        user_program::NET_DRIVER => capability::CAP_MMIO | capability::CAP_PCI,
        user_program::NET_SERVER => capability::CAP_MMIO,
        // The shell hands device rights to the tasks it spawns by hand
        // (`spawn net_driver`), and `CapTable::grant` only allows a task to
        // pass on a right it already holds.
        // The shell needs MMIO only for the commands that map a device window.
        // In the console product build it is also the trusted control plane,
        // so the kernel grants it the power privilege directly; a spawn handle
        // can never delegate CAP_POWER (it is outside CAP_ALL).
        // The shell is the trusted control plane of the console OS; it owns
        // the power privilege directly (a spawn handle can never delegate it).
        user_program::HOWL => {
            capability::CAP_MMIO
                | capability::CAP_IRQ
                | capability::CAP_FS_WRITE
                | capability::CAP_POWER
        }
        // Trusted filesystem stress helper (not an installed app bundle).
        user_program::BIGDIR | user_program::BIGFILE => capability::CAP_FS_WRITE,
        // Multi-call Unix toolbox: `cp` writes, the rest only reads.
        user_program::UTILS => capability::CAP_FS_WRITE,
        // Wolfser scans PCI for ivshmem, maps that surface and the network
        // data window.
        user_program::WOLFSER => capability::CAP_MMIO | capability::CAP_PCI,
        _ => return None,
    };
    Some(rights)
}

