//! Task death and waiting: exit, kill, zombies and reparenting.
//!
//! A child module of `scheduler` on purpose: it needs the parent's private
//! task manager and wake helpers, and nothing outside the scheduler should
//! reach into the death/wake state machine. The functions the parent still
//! calls are `pub(super)`; the ones other subsystems use (`exit_current`,
//! `kill_task`, `wait_for_task`, `exit_group`, ...) stay `pub` and are
//! re-exported by `scheduler`, so every existing call path is unchanged.

use super::*;
/// True when the scheduler may add another task record: an active slot is
/// free and the zombie/record metadata budget is not exhausted.
pub(super) fn has_task_capacity(manager: &TaskManager) -> bool {
    if manager.tasks.len() >= MAX_TASK_RECORDS {
        return false;
    }
    manager
        .tasks
        .iter()
        .filter(|task| task.state != TaskState::Zombie)
        .count()
        < MAX_TASKS
}

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
        // Assembled into one line and emitted once: the trace used to be seven
        // console calls, so a task printing on another CPU could land in the
        // middle of it.
        let mut line = console::Line::new();
        line.str("EXIT: task=").dec(id);
        line.str(" program=").dec(program);
        line.str(" status=").dec(status);
        // Packed reports (see the filesystem server's mount failure) are
        // unreadable in decimal, so the same value is repeated in hex.
        line.str(" (0x").hex_u64(status).str(")\r\n");
        line.emit();
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
        release_bound_irq(task);
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
    release_bound_irq(&mut manager.tasks[index]);
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

pub(super) fn recover_console(manager: &mut TaskManager, exited: u64) {
    let Some(exited_task) = manager.tasks.iter_mut().find(|task| task.id == exited) else {
        return;
    };
    if !exited_task.owns_console {
        return;
    }
    exited_task.owns_console = false;
    manager.console_parent = None;
    if cfg!(feature = "demo-tasks") {
        return;
    }
    let fallback = manager
        .tasks
        .iter()
        .find(|task| {
            !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                && task.program == user_program::HOWL
        })
        .map(|task| task.id);
    for task in &mut manager.tasks {
        task.owns_console = Some(task.id) == fallback;
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitEvent {
    Exited(u64),
    Stopped,
}

pub fn wait_for_task(id: u64) -> Option<u64> {
    match wait_inner(id, false, true) {
        Some(WaitEvent::Exited(status)) => Some(status),
        _ => None,
    }
}

pub fn wait_for_task_event(id: u64) -> Option<WaitEvent> {
    wait_inner(id, true, true)
}

/// First phase of a transactional `SYS_WAITPID`: block until `id` is a zombie
/// and return its exit status **without consuming the record**.
///
/// The target must be a child of the caller or covered by a target-scoped
/// `CAP_WAIT`; a foreign PID is reported as ECHILD exactly like a consumed
/// zombie, so existence does not leak.
///
/// The caller (the syscall layer) must hold the kernel lock on return, copy the
/// status to user memory, and then call [`consume_zombie`] in the same
/// non-blocking critical section. No `schedule`/BKL release may happen in
/// between; otherwise another waiter could consume the same zombie.
pub fn wait_status_for_pid(id: u64) -> Option<u64> {
    if id == IDLE_TASK_ID || id == current_task_id() || !current_task_can_wait(id) {
        return None;
    }
    match wait_inner(id, false, false) {
        Some(WaitEvent::Exited(status)) => Some(status),
        _ => None,
    }
}

/// Second phase of a transactional `SYS_WAITPID`: consume the zombie that was
/// peeked by [`wait_status_for_pid`].
///
/// Re-validates pid, state and status so a stale peek can never remove a
/// different task or report a mismatched status. Runs with interrupts masked and
/// the kernel lock held; it never blocks.
pub fn consume_zombie(id: u64, expected_status: u64) -> bool {
    // The transaction contract: peek and consume must be one BKL-protected
    // step so no other waiter can slip in between. This is a no-op in release.
    smp::bkl_assert_held();
    let saved_daif = interrupts::save_and_disable_irq();
    let consumed = scheduler_manager()
        .and_then(|manager| {
            let index = manager.tasks.iter().position(|task| {
                task.id == id
                    && task.state == TaskState::Zombie
                    && task.exit_status.unwrap_or(0) == expected_status
            })?;
            take_zombie(manager, index);
            Some(())
        })
        .is_some();
    interrupts::restore_irq(saved_daif);
    consumed
}

pub(super) fn current_task_can_wait(id: u64) -> bool {
    let caller = current_task_id();
    if caller == id {
        return false;
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    if manager
        .tasks
        .iter()
        .any(|task| task.id == id && task.ppid == Some(caller))
    {
        return true;
    }
    manager
        .tasks
        .iter()
        .find(|task| task.id == caller)
        .map(|task| task.cap_table.has_target_rights(id, capability::CAP_WAIT))
        .unwrap_or(false)
}

pub(super) fn wait_inner(id: u64, report_stopped: bool, consume: bool) -> Option<WaitEvent> {
    if id == IDLE_TASK_ID || id == current_task_id() {
        return None;
    }
    let waiter = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    loop {
        let Some(manager) = scheduler_manager() else {
            interrupts::restore_irq(saved_daif);
            return None;
        };
        let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
            // Never existed, or the zombie record was already consumed here:
            // the caller maps this to ECHILD.
            interrupts::restore_irq(saved_daif);
            return None;
        };
        match manager.tasks[index].state {
            TaskState::Zombie => {
                if !consume {
                    // Transactional peek: report the status but keep the
                    // zombie available until the caller has copied it out.
                    let status = manager.tasks[index].exit_status.unwrap_or(0);
                    interrupts::restore_irq(saved_daif);
                    return Some(WaitEvent::Exited(status));
                }
                let status = take_zombie(manager, index);
                interrupts::restore_irq(saved_daif);
                return Some(WaitEvent::Exited(status));
            }
            TaskState::Stopped if report_stopped => {
                interrupts::restore_irq(saved_daif);
                return Some(WaitEvent::Stopped);
            }
            _ => {}
        }
        // Alive (including Dead-but-not-yet-reaped): block until the zombie
        // transition broadcasts on the process wait queue.
        PROCESS_WAIT_QUEUE.enqueue(waiter);
        if let Some(task) = current_task_mut() {
            task.waiting_on = Some(WaitLink::Immortal(&PROCESS_WAIT_QUEUE));
            set_state_unless_dead(task, TaskState::Blocked);
        }
        schedule_released();
        PROCESS_WAIT_QUEUE.remove(waiter);
        if let Some(task) = current_task_mut() {
            task.waiting_on = None;
        }
    }
}

/// Removes the zombie at `index` from the task table and returns its status.
///
/// Removal shifts every later index, so the current-task IDs of all online
/// CPUs are snapshotted first and remapped afterwards. Capability records that
/// target the dead task are dropped from the caller's table.
pub(super) fn take_zombie(manager: &mut TaskManager, index: usize) -> u64 {
    let id = manager.tasks[index].id;
    debug_assert!(
        manager.tasks[index].running_on.is_none(),
        "removing a zombie that is still running on a CPU"
    );
    let status = manager.tasks[index].exit_status.unwrap_or(0);
    let cpu_ids = snapshot_current_ids(manager);
    let live_id = manager.tasks.get(smp::current_index()).map(|task| task.id);
    manager.tasks.remove(index);
    remap_current_indices(manager, &cpu_ids, live_id);
    // Every later task moved down one slot, so every queued index now names a
    // different task. Rebuild from the table instead of trying to shift.
    rebuild_ready_queues(manager);
    if let Some(current) = manager.tasks.get_mut(smp::current_index()) {
        current.cap_table.remove_for_target(id);
    }
    status
}

/// Moves every child of `dead_id` to the live reaper. Safe at any point: the
/// dead task may be a zombie already or only marked Dead.
pub(super) fn reparent_children(manager: &mut TaskManager, dead_id: u64) {
    let reaper = reaper_pid(manager, dead_id);
    for task in &mut manager.tasks {
        if task.ppid == Some(dead_id) && task.id != dead_id {
            task.ppid = Some(reaper);
        }
    }
}

/// Chooses a live reaper without assuming PID 1 exists: the supervisor
/// (`silverd`) first, then any live user task, finally the always-present
/// idle task (0), which keeps orphans bounded via [`ZOMBIE_LIMIT`].
pub(super) fn reaper_pid(manager: &TaskManager, excluded: u64) -> u64 {
    if let Some(task) = manager
        .tasks
        .iter()
        .filter(|task| {
            task.id != excluded
                && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                && task.program == user_program::SILVERD
        })
        .min_by_key(|task| task.id)
    {
        return task.id;
    }
    if let Some(task) = manager
        .tasks
        .iter()
        .filter(|task| {
            task.id != excluded
                && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                && task.program != u64::MAX
        })
        .min_by_key(|task| task.id)
    {
        return task.id;
    }
    IDLE_TASK_ID
}

/// `exit_group(2)` from the thread-group leader: mark every sibling Dead,
/// let the reaper retire them, then exit the leader and release the shared
/// address space. A non-leader falls back to exiting only itself; transferring
/// process ownership safely is a later refactor.
#[cfg(feature = "linux-abi")]
pub fn exit_group(status: u64) -> ! {
    let current = current_task_id();
    let tgid = current_tgid();
    let is_leader = scheduler_manager()
        .and_then(|manager| manager.tasks.iter().find(|task| task.id == current))
        .map(|task| !task.is_thread)
        .unwrap_or(false);
    // Kill every other member of the group. The leader's record carries the
    // group status (this is what a parent waits for), and the leader owns the
    // shared address space, so the reaper releases it after the leader dies.
    // This is also the path a non-leader takes: it must not release the space
    // itself, but the group must still terminate.
    let mut wake = Vec::new();
    if let Some(manager) = scheduler_manager() {
        for task in manager.tasks.iter_mut() {
            if task.id != current
                && task.tgid == tgid
                && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
            {
                task.state = TaskState::Dead;
                release_bound_irq(task);
                task.exit_status = Some(if task.id == tgid { status } else { 1 });
                wake.push(task.id);
            }
        }
    }
    for id in wake {
        wake_for_signal(id);
    }
    // A sibling Running on another CPU does not become wakeable (state Dead
    // is not Sleeping/Blocked); kick every CPU so its own `schedule` observes
    // the death and switches away instead of faulting on the dying space.
    smp::ipi_reschedule_others();
    if !is_leader {
        // The leader is now Dead; its reaper releases the shared space. This
        // thread only has to leave its own context.
        exit_current_with_status(status);
    }
    // Wait until every sibling is reaped. They were just woken, so a blocked
    // futex waiter or sleeper cannot delay this indefinitely.
    loop {
        let done = scheduler_manager()
            .map(|manager| {
                manager.tasks.iter().all(|task| {
                    task.id == current
                        || task.tgid != tgid
                        || matches!(task.state, TaskState::Zombie)
                })
            })
            .unwrap_or(true);
        if done {
            break;
        }
        sleep(1);
    }
    exit_current_with_status(status)
}

