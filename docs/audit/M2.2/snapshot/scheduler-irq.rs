// Excerpts from src/scheduler.rs: wait_irq/irq_is_bound/wake_bound_irq (1810-1865),
// exit_current_with_status (2040-2200), bind_irq (4970-5020)
}

pub fn wait_irq() -> bool {
    let saved_daif = interrupts::save_and_disable_irq();
    if current_task_mut().and_then(|task| task.bound_irq).is_none() {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    loop {
        let Some(task) = current_task_mut() else {
            interrupts::restore_irq(saved_daif);
            return false;
        };
        if task.irq_pending {
            task.irq_pending = false;
            if let Some(intid) = task.bound_irq {
                crate::gic::enable_irq(intid);
            }
            interrupts::restore_irq(saved_daif);
            return true;
        }
        if let Some(intid) = task.bound_irq {
            crate::gic::enable_irq(intid);
        }
        set_state_unless_dead(task, TaskState::Blocked);
        schedule_released();
    }
}

pub fn irq_is_bound(intid: u32) -> bool {
    scheduler_manager()
        .map(|manager| {
            manager.tasks.iter().any(|task| {
                !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && task.bound_irq == Some(intid)
            })
        })
        .unwrap_or(false)
}

pub fn wake_bound_irq(intid: u32) -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(index) = manager.tasks.iter().position(|task| {
        !matches!(task.state, TaskState::Dead | TaskState::Zombie) && task.bound_irq == Some(intid)
    }) else {
        return false;
    };
    manager.tasks[index].irq_pending = true;
    wake_blocked_index(manager, index, smp::this_cpu_id());
    true
}

/// Task that owns the PL011 interrupt, i.e. the userspace console sink.
///

/// more entries than the 128 MiB DRAM can back; this cap bounds the metadata
/// Vec itself, not just the frames.
const MAX_MMAP_REGIONS: usize = 1024;

/// Per-task cap on mapping bytes. The mmap window is ~1 GiB but DRAM is
/// shared by every task, so one EL0 task must not be able to claim the whole
/// machine and starve the scheduler, IPC and the kernel heap. The GTK3
/// library set is ~80 MiB of segments; 160 MiB leaves room for its heap while
/// the global frame reserve stays the last line of defence.
// A large value because lazily reserved ranges (glibc's 128 MiB heap probe)
// count toward it while committing no frames. The real bound on memory is the
// per-task frame ledger, charged one page at a time.
const MAX_MMAP_BYTES_PER_TASK: usize = 1024 * 1024 * 1024;

pub fn exit_current() -> ! {
    exit_current_with_status(0)
}

pub fn exit_current_with_status(raw_status: u64) -> ! {
    // Reserve the top three u64 values for SYS_WAIT_EVENT sentinels
    // (ERROR/STOPPED/ECHILD). A user exit status must never alias them.
    let status = raw_status.min(u64::MAX - 4);
    // The syscall path already holds the kernel lock; the EL0 fault vector
    // enters without it. Acquire only when missing so the teardown below
    // (`ipc::remove_task` takes the IPC mutex, which asserts the BKL) is valid
    // in both paths. `schedule_released` balances the extra hold.
    if smp::bkl_depth() == 0 {
        smp::bkl_acquire();
    }
    let id = current_task_id();
    #[cfg(feature = "linux-abi")]
    let parent = current_task_mut().and_then(|task| task.ppid);
    #[cfg(feature = "linux-abi")]
    let (tgid, clear_tid) = current_task_mut()
        .map(|task| (task.tgid, task.clear_child_tid))
        .unwrap_or((0, None));
    // Kernel-side trace: syscall::print is discarded for tasks that do not own
    // the console, so a task dying silently leaves no user-visible evidence.
    // console writes here are raw MMIO pokes and stay visible for every task.
    let program = current_task_mut().map_or(u64::MAX, |task| task.program);
    // Kernel tasks (program == u64::MAX) are internal probes; their exit is
    // not user-visible evidence and would only flood the console.
    if program != u64::MAX {
        console::write_str("EXIT: task=");
        console::write_dec_u64(id);
        console::write_str(" program=");
        console::write_dec_u64(program);
        console::write_str(" status=");
        console::write_dec_u64(status);
        // Packed reports (see the filesystem server's mount failure) are
        // unreadable in decimal, so the same value is repeated in hex.
        console::write_str(" (0x");
        console::write_hex_u64(status);
        console::write_str(")\r\n");
    }
    #[cfg(feature = "linux-abi")]
    if let Some(pointer) = clear_tid {
        let zero = 0_u32.to_le_bytes();
        let _ = crate::user_memory::copy_to_user(&zero, pointer);
        crate::linux_abi::futex_wake_address(pointer, tgid, 1);
    }
    crate::ipc::remove_task(id);
    #[cfg(feature = "linux-abi")]
    crate::linux_fd::close_task(id);
    #[cfg(feature = "linux-abi")]
    crate::linux_signal::close_task(id);
    #[cfg(feature = "linux-abi")]
    crate::linux_abi::futex_cleanup(id);
    vfs::close_task(id);
    let saved_daif = interrupts::save_and_disable_irq();
    if let Some(task) = current_task_mut() {
        task.state = TaskState::Dead;
        task.bound_irq = None;
        // The status survives the Dead -> Zombie transition in `reap`; the
        // executable resources are released there, after this CPU has left
        // the context (see `reap_dead_tasks`).
        task.exit_status = Some(status);
    }
    if let Some(manager) = scheduler_manager() {
        recover_console(manager, id);
        // A dying parent must not leave its children pointing at a dead PID.
        reparent_children(manager, id);
        wake_process_waiters(manager);
    }
    #[cfg(feature = "linux-abi")]
    if let Some(parent) = parent {
        if task_alive(parent) && crate::linux_signal::has_table(parent) {
            crate::linux_signal::raise(parent, crate::linux_signal::SIGCHLD);
        }
    }

    schedule_released();
    // A dead task must not park while holding the kernel lock. `schedule`
    // normally switches away here; when every other task is already running
    // (for example all idle tasks are busy) the pass returns without a switch,
    // and `schedule_released` has just re-acquired the hold. Drop whatever
    // remains before waiting for an interrupt, otherwise this CPU pins the
    // lock while it is parked.
    while smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    interrupts::restore_irq(saved_daif);
    loop {
        interrupts::wait_for_irq();
    }
}

pub fn kill_task(id: u64) -> bool {
    if id == IDLE_TASK_ID || id == current_task_id() {
        return false;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let Some(manager) = scheduler_manager() else {
        interrupts::restore_irq(saved_daif);
        return false;
    };
    let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
        interrupts::restore_irq(saved_daif);
        return false;
    };
    if matches!(
        manager.tasks[index].state,
        TaskState::Dead | TaskState::Zombie
    ) {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    // Side effects only after the target is proven live: a bogus CAP_KILL
    // target must not broadcast mailbox/console wakeups across the system.
    let parent = manager.tasks[index].ppid;
    crate::ipc::remove_task(id);
    #[cfg(feature = "linux-abi")]
    crate::linux_fd::close_task(id);
    #[cfg(feature = "linux-abi")]
    crate::linux_signal::close_task(id);
    #[cfg(feature = "linux-abi")]
    crate::linux_abi::futex_cleanup(id);
    vfs::close_task(id);
    manager.tasks[index].state = TaskState::Dead;
    manager.tasks[index].bound_irq = None;
    manager.tasks[index].exit_status = Some(1);
    recover_console(manager, id);
    reparent_children(manager, id);
    wake_process_waiters(manager);
    #[cfg(feature = "linux-abi")]
    if let Some(parent) = parent {
        if task_alive(parent) && crate::linux_signal::has_table(parent) {
            crate::linux_signal::raise(parent, crate::linux_signal::SIGCHLD);
        }
    }
    schedule_released();
    interrupts::restore_irq(saved_daif);
    true
}

fn recover_console(manager: &mut TaskManager, exited: u64) {
    let Some(exited_task) = manager.tasks.iter_mut().find(|task| task.id == exited) else {
        return;
    };
    if !exited_task.owns_console {
        return;

        };
        task.dma_pages.push((physical, page_count, generation));
        Some((virtual_address, physical, generation))
    });
    interrupts::restore_irq(saved_daif);
    result
}

pub fn bind_irq(intid: u32) -> bool {
    // Only SPIs are handed to userspace. SGIs/PPIs are banked per PE and
    // include the kernel's reschedule SGI and timer PPI; the capability
    // check below is necessary but not sufficient on its own.
    if !crate::irq_policy::is_user_bindable(intid, crate::gic::timer_irq()) {
        return false;
    }
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager()
        .map(|manager| {
            let current_index = smp::current_index();
            let Some(current) = manager.tasks.get(current_index) else {
                return false;
            };
            if !current
                .cap_table
                .has_self_rights(current.id, capability::CAP_IRQ)
            {
                return false;
            }
            if manager.tasks.iter().enumerate().any(|(index, task)| {
                index != current_index
                    && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                    && task.bound_irq == Some(intid)
            }) {
                return false;
            }
            let Some(task) = manager.tasks.get_mut(current_index) else {
                return false;
            };
            task.bound_irq = Some(intid);
            task.irq_pending = false;
            crate::gic::enable_irq(intid);
            true
        })
        .unwrap_or(false);
    interrupts::restore_irq(saved_daif);
    result
}

pub fn spawn_user_with_args_and_cap(
    program: u64,
    argument: u64,
