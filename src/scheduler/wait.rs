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
    let (tgid, is_thread) = current_task_mut()
        .map(|task| (task.tgid, task.is_thread))
        .unwrap_or((id, false));
    // A leader that exits with live siblings must not take the shared address
    // space, descriptor table or signal dispositions with it. The leader record
    // stays Dead (so `with_group_owner` and `fd_table_owner` still resolve)
    // until the last thread has left the space; only then does the reaper
    // release the group resources and turn the leader into a waitable Zombie.
    let group_alive = scheduler_manager()
        .map(|manager| {
            manager.tasks.iter().any(|task| {
                task.id != id
                    && task.tgid == tgid
                    && task.state != TaskState::Zombie
                    // A Dead sibling whose CPU has not switched away yet still
                    // owns a slice of the shared context until `running_on`
                    // clears; keep the tables alive for it.
                    && (task.state != TaskState::Dead || task.running_on.is_some())
            })
        })
        .unwrap_or(false);
    let defer_group = !is_thread && group_alive;
    #[cfg(feature = "linux-abi")]
    let clear_tid = current_task_mut().and_then(|task| task.clear_child_tid);
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
    // These tables are keyed by the group leader. A leader that leaves a live
    // worker behind keeps them until the reaper retires the whole group; a
    // thread id has no table of its own, so closing it here is a no-op.
    if !defer_group {
        #[cfg(feature = "linux-abi")]
        crate::linux_fd::close_task(id);
        #[cfg(feature = "linux-abi")]
        crate::linux_signal::close_task(id);
        vfs::close_task(id);
    }
    #[cfg(feature = "linux-abi")]
    crate::linux_abi::futex_cleanup(id);
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
        // A process is not over while a worker still runs in its space: its
        // children keep their PPID and the parent gets SIGCHLD only when the
        // reaper retires the last member. A thread has no children of its own.
        if !defer_group && !is_thread {
            reparent_children(manager, id);
        }
        wake_process_waiters(manager);
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
    //
    // Linux signals target a thread group. Killing only the leader would leave
    // its workers running in freed page tables and close the descriptor table
    // underneath them, so a fatal signal retires every live member. A caller
    // inside the same group cannot mark itself Dead here; it must take the
    // `exit_group` path instead.
    let target_tgid = manager.tasks[index].tgid;
    let group_member = manager.tasks[index].is_thread
        || manager
            .tasks
            .iter()
            .any(|task| task.id != id && task.tgid == target_tgid);
    let current = current_task_id();
    if group_member
        && manager
            .tasks
            .iter()
            .any(|task| task.id == current && task.tgid == target_tgid)
    {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    let mut victims = Vec::new();
    for task in manager.tasks.iter_mut() {
        if group_member {
            if task.tgid != target_tgid {
                continue;
            }
        } else if task.id != id {
            continue;
        }
        if matches!(task.state, TaskState::Dead | TaskState::Zombie) {
            continue;
        }
        task.state = TaskState::Dead;
        release_bound_irq(task);
        task.exit_status = Some(1);
        victims.push(task.id);
    }
    if victims.is_empty() {
        interrupts::restore_irq(saved_daif);
        return false;
    }
    for &victim in &victims {
        crate::ipc::remove_task(victim);
        #[cfg(feature = "linux-abi")]
        crate::linux_abi::futex_cleanup(victim);
    }
    for &victim in &victims {
        recover_console(manager, victim);
    }
    // The group's tables and address space stay live until the reaper sees the
    // last victim leave its CPU; a worker running on another CPU is kicked so
    // it can obey the Dead state instead of touching a dying space.
    wake_process_waiters(manager);
    smp::ipi_reschedule_others();
    schedule_released();
    interrupts::restore_irq(saved_daif);
    true
}

pub(super) fn recover_console(manager: &mut TaskManager, exited: u64) {
    let Some((exited_tgid, owns_console)) = manager
        .tasks
        .iter()
        .find(|task| task.id == exited)
        .map(|task| (task.tgid, task.owns_console))
    else {
        return;
    };
    if !owns_console {
        return;
    }
    // The terminal belongs to the process, not to the one Task record that
    // happened to call `set_console_owner`. If a worker is still running in
    // this group, hand the console to it before falling back to the shell.
    let successor = manager
        .tasks
        .iter()
        .find(|task| {
            task.id != exited
                && task.tgid == exited_tgid
                && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
        })
        .map(|task| task.id);
    for task in &mut manager.tasks {
        if task.id == exited {
            task.owns_console = false;
        }
        if Some(task.id) == successor {
            task.owns_console = true;
        }
    }
    if successor.is_some() {
        return;
    }
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

/// `exit_group(2)`: mark every member Dead, let the reaper retire them, then
/// exit the caller and release the shared address space. A non-leader uses the
/// same path; the leader record stays Dead until the last worker has left, so
/// the space and descriptor table cannot disappear underneath a sibling.
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

