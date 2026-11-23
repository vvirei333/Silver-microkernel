//! Scheduling passes, ready queues and the context switch.
//!
//! A child module of `scheduler` like `task.rs`, `wait.rs` and `spawn.rs`:
//! the pass needs the parent's private task table, and the queues are reached
//! by `spawn` and the wake paths. `schedule`/`schedule_released`/`start` are
//! `pub`; the queue helpers are `pub(super)` because only `scheduler` and its
//! children call them.

use super::*;
pub fn start() -> ! {
    let saved_daif = interrupts::save_and_disable_irq();
    let manager = scheduler_manager().unwrap();
    let current_index = smp::current_index();
    let Some(first_task) = manager.tasks.get_mut(current_index) else {
        loop {
            interrupts::wait_for_irq();
        }
    };

    // The boot switch runs under the kernel lock; `silver_user_bootstrap`
    // drops it again right before `eret`-ing into the first user task, so
    // the acquire/release pair stays balanced.
    smp::bkl_acquire();
    first_task.state = TaskState::Running;
    first_task.running_on = Some(0);
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    SCHEDULER_STARTED.store(true, Ordering::Release);
    #[cfg(feature = "smp")]
    crate::watchdog::cpu_started(0);
    trace_switch(
        0,
        first_task.id,
        first_task.context.stack_pointer,
        first_task.context.elr,
    );
    let first_context = &first_task.context as *const TaskContext;
    let first_fp = first_task.fp;
    let boot_context = unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) };

    // Install the first task's vector register file; `silver_user_bootstrap`
    // restores it immediately before entering EL0.
    unsafe {
        *smp::this_cpu().fp.get() = first_fp;
    }

    mmu::switch_address_space(first_task.ttbr0);

    unsafe {
        silver_context_switch(boot_context, first_context);
    }

    interrupts::restore_irq(saved_daif);
    loop {
        interrupts::wait_for_irq();
    }
}

/// One scheduling pass for a task that already holds the kernel lock:
/// releases it (a blocked task must not pin the lock — another CPU has to be
/// able to wake tasks), runs the pass, and re-acquires before returning so
/// the caller keeps executing with the lock held.
pub fn schedule_released() {
    smp::bkl_release();
    schedule();
    smp::bkl_acquire();
}

pub fn schedule() {
    if !SCHEDULER_READY.load(Ordering::Acquire) {
        // Boot: no valid current task yet (see `SCHEDULER_READY`).
        return;
    }
    let Some(manager) = scheduler_manager() else {
        return;
    };
    if manager.tasks.is_empty() {
        return;
    }

    #[cfg(feature = "smp")]
    if smp::this_cpu_id() != 0
        && !smp::AP_TASK_SCHEDULING.load(core::sync::atomic::Ordering::Acquire)
    {
        // Phase 1: APs stay parked in `wait_for_irq` after GIC/timer bring-up.
        return;
    }

    // A pass is not re-entrant on one CPU. The snapshot/remap pair below and
    // the `running_on = None` -> publish-new-index transition are only
    // consistent once the pass completes; a timer IRQ that ran a second pass
    // in between would leave the outer pass publishing a stale index when it
    // resumes. Mask IRQs for the whole pass and refuse a nested entry (the
    // churn harness re-enables IRQs on purpose while it waits, so the mask
    // alone is not a complete guard). The mask is dropped again only after
    // the kernel lock has been released, right before returning or resuming.
    let pass_cpu = smp::this_cpu_id();
    #[cfg(feature = "smp")]
    crate::watchdog::note_progress(pass_cpu);
    if SCHED_PASS_ACTIVE[pass_cpu].swap(true, Ordering::AcqRel) {
        return;
    }
    let saved_daif = interrupts::save_and_disable_irq();

    // The kernel lock is held for the whole pass. If this call switches
    // away, the outgoing task is suspended before the matching release and
    // the incoming task resumes inside its own suspended `schedule`, so the
    // CPU keeps the lock across the switch without a transfer protocol.
    crate::smp::probe(5);
    smp::bkl_acquire();
    {
        static SCHED_SEEN: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !SCHED_SEEN.swap(true, core::sync::atomic::Ordering::AcqRel) {
            crate::console::write_str("SMP: schedule() first entry\r\n");
        }
    }

    let live_task_id = manager
        .tasks
        .get(smp::current_index())
        .map(|task| task.id)
        .unwrap_or(IDLE_TASK_ID);
    let live_is_dead = manager
        .tasks
        .get(smp::current_index())
        .map(|task| matches!(task.state, TaskState::Dead | TaskState::Zombie))
        .unwrap_or(false);

    reap_dead_tasks(manager);
    // The reap path removes tasks (shifting indices) and remaps every CPU's
    // current index. Verify the state machine immediately afterwards: this is
    // the riskiest mutation in the scheduler, and `smp-selftest` turns a silent
    // `running_on`/`current_index` drift into a loud boot failure instead of a
    // task resuming with another task's address space.
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    if SCHEDULER_STARTED.load(Ordering::Acquire) {
        if let Err(reason) = verify_scheduling_invariants(manager) {
            console::write_str("SMP SELFTEST: sched invariant FAIL (");
            console::write_str(reason);
            console::write_str(")\r\n");
            panic!("scheduling invariant violated");
        }
    }
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    report_ready_queues_once();
    wake_expired_tasks(manager, timer::ticks());

    if AP_BOOT_PENDING.swap(false, core::sync::atomic::Ordering::AcqRel) {
        #[cfg(feature = "smp")]
        smp::boot_secondary_cpus();
    }

    let current_index = smp::current_index();
    let had_current = current_index < manager.tasks.len();
    // Boot window: `current_index` may already name idle while the CPU is
    // still on the boot stack (a timer IRQ during `scheduler::init`). Saving
    // that SP into idle's context corrupts it. `start()` performs the real
    // first switch; until then `schedule` must not switch at all. An AP's
    // first pass has no current task (`current_index == usize::MAX`) and is
    // handled by the dummy-context path below.
    if !live_is_dead && had_current {
        let sp_now: usize;
        unsafe {
            core::arch::asm!("mov {}, sp", out(reg) sp_now, options(nostack));
        }
        let current = &manager.tasks[current_index];
        let physical = sp_now.wrapping_sub(crate::memory::KERNEL_VIRT_BASE);
        if !current.stack.contains(physical.saturating_sub(1)) {
            SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
            smp::bkl_release();
            interrupts::restore_irq(saved_daif);
            return;
        }
    }
    if had_current && manager.tasks[current_index].state == TaskState::Running {
        manager.tasks[current_index].state = TaskState::Ready;
    }

    let scan_base = if had_current { current_index } else { 0 };
    let Some(next_index) = find_next_task(manager, smp::this_cpu_id(), scan_base) else {
        if had_current && manager.tasks[current_index].state == TaskState::Ready {
            manager.tasks[current_index].state = TaskState::Running;
        }
        SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
        smp::bkl_release();
        interrupts::restore_irq(saved_daif);
        return;
    };
    // A runnable task must not be punished with a full idle round trip.
    // `find_next_task` only sees queued tasks, and the outgoing task is enqueued
    // *after* this decision, so when it is Ready on this CPU (it yielded, or a
    // tick preempted it) and the only candidate is an idle task, the "switch"
    // would park the CPU in `wfi` until the next tick. That capped a `yield`
    // loop at the tick rate: `bench sched` measured ~200 yields/s (5 ms each,
    // half of the 10 ms tick) instead of a same-CPU resumption. Idle still runs
    // whenever nothing is runnable, i.e. when the outgoing task is not Ready.
    if had_current
        && !live_is_dead
        && manager.tasks[next_index].is_idle
        && manager.tasks[current_index].state == TaskState::Ready
        && !manager.tasks[current_index].is_idle
    {
        manager.tasks[current_index].state = TaskState::Running;
        SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
        smp::bkl_release();
        interrupts::restore_irq(saved_daif);
        return;
    }

    if had_current && next_index == current_index && !live_is_dead {
        // Self-continuation: this CPU keeps running the same task and keeps
        // holding the kernel lock for it. The probe above marked it Ready so
        // `find_next_task` could consider it; restore Running before another
        // CPU can mistake it for runnable.
        manager.tasks[current_index].state = TaskState::Running;
        SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
        smp::bkl_release();
        interrupts::restore_irq(saved_daif);
        return;
    }

    // Switching away: the outgoing task stops running on this CPU. An AP's
    // first pass has no outgoing task, so there is no state to clear.
    if had_current {
        let outgoing_ready = match manager.tasks.get_mut(current_index) {
            Some(outgoing) => {
                outgoing.running_on = None;
                // The outgoing task may be `Ready` (it yielded, or a wake moved
                // it back to Ready while it still owned this CPU). Now that no
                // CPU owns it, put it on this CPU's ready queue.
                outgoing.state == TaskState::Ready
            }
            None => {
                console::write_str("SCHEDULE: current_index out of range: ");
                console::write_dec_u64(current_index as u64);
                console::write_str("\r\n");
                false
            }
        };
        if outgoing_ready {
            enqueue_ready(manager, smp::this_cpu_id(), current_index);
        }
    }

    // From here to the context switch the code only uses the raw `tasks`
    // pointer: `next_task` used to be a `&mut Task` minted from the vector
    // while `manager` kept being reborrowed (`tasks.len()`), which invalidates
    // the reference under Stacked Borrows — exactly the class of UB the
    // optimizer turned into a bare `b .` self-loop elsewhere.
    let tasks_len = manager.tasks.len();
    let tasks = manager.tasks.as_mut_ptr();
    let next_task = unsafe { &mut *tasks.add(next_index) };
    let saved_stack_pointer = next_task.context.stack_pointer as usize;
    if !next_task
        .stack
        .contains(virt_to_phys(saved_stack_pointer) - 1)
    {
        console::write_str("BAD SP task=");
        console::write_dec_u64(next_task.id);
        console::write_str(" sp=");
        console::write_hex_u64(next_task.context.stack_pointer);
        console::write_str(" stack=[");
        console::write_hex_u64(next_task.stack.start as u64);
        console::write_str(",");
        console::write_hex_u64(next_task.stack.end as u64);
        console::write_str("] cur_idx=");
        console::write_dec_u64(current_index as u64);
        console::write_str(" live_id=");
        console::write_dec_u64(live_task_id);
        console::write_str(" dead=");
        console::write_dec_u64(u64::from(live_is_dead));
        console::write_str(" next_idx=");
        console::write_dec_u64(next_index as u64);
        console::write_str("\r\n");
        dump_switch_trace();
        panic!("bad saved stack pointer");
    }

    let next_id = next_task.id;
    let next_ttbr0 = next_task.ttbr0;
    let next_context_stack_pointer = next_task.context.stack_pointer;
    let next_context_elr = next_task.context.elr;
    next_task.state = TaskState::Running;
    next_task.running_on = Some(smp::this_cpu_id());
    smp::set_current_task(next_index, next_id);

    let has_current = current_index < tasks_len;
    let current_context = if live_is_dead {
        unsafe { &mut *core::ptr::addr_of_mut!(BOOT_CONTEXT) }
    } else if has_current {
        unsafe { &mut (*tasks.add(current_index)).context }
    } else {
        // An AP's first pass has no current task: its outgoing registers are
        // parked in a dummy context that is never restored (the AP's boot
        // stack frame is abandoned once it switches into a real task).
        unsafe { &mut AP_DUMMY_CONTEXTS[smp::this_cpu_id()] }
    };
    let from_id = if live_is_dead || !has_current {
        live_task_id
    } else {
        unsafe { (*tasks.add(current_index)).id }
    };
    // An AP before its first pick has no current task and no current TTBR0:
    // force the address-space switch to the first task it runs.
    let current_ttbr0 = if has_current {
        unsafe { (*tasks.add(current_index)).ttbr0 }
    } else {
        0
    };

    if live_is_dead || next_ttbr0 != current_ttbr0 {
        mmu::switch_address_space(next_ttbr0);
    }

    trace_switch(
        from_id,
        next_id,
        next_context_stack_pointer,
        next_context_elr,
    );

    // FP/SIMD file: the outgoing task's live copy lives in `PerCpu::fp`
    // (its last EL0 entry saved it there). Park it in the task record and
    // install the incoming task's file on this CPU; the incoming task's
    // exception epilogue restores the vector registers before `eret`.
    let cpu = smp::this_cpu();
    unsafe {
        if has_current && !live_is_dead {
            (*tasks.add(current_index)).fp = *cpu.fp.get();
        }
        *cpu.fp.get() = (*tasks.add(next_index)).fp;
    }

    // The pass is complete: the new `current_index` is published, so a later
    // pass (including a preemption right after the resume) starts from a
    // consistent cut. IRQs stay masked across the switch itself; the incoming
    // task re-enables them from its own saved state.
    SCHED_PASS_ACTIVE[pass_cpu].store(false, Ordering::Release);
    unsafe {
        silver_context_switch(current_context, &(*tasks.add(next_index)).context);
    }

    // Resumed here as the incoming task, inside our own suspended `schedule`:
    // complete the release of this pass's acquire (see the locking contract
    // in `smp::bkl_acquire`). A task that never ran before enters through
    // `silver_user_bootstrap` instead, whose `bkl_release_asm` is the matching
    // release for a first dispatch; this line only executes on the resumed
    // path, so the kernel lock never stays pinned while a task sleeps.
    smp::bkl_release();
    interrupts::restore_irq(saved_daif);
}

/// True when `index` names a task this scheduler pass may dispatch right now:
/// in range, not terminal, not owned by a CPU, and not an idle task (idle is
/// only ever chosen by the global fallback so real work always wins).
pub(super) fn ready_for_queue(manager: &TaskManager, index: usize) -> bool {
    manager.tasks.get(index).is_some_and(|task| {
        !task.is_idle && task.state == TaskState::Ready && task.running_on.is_none()
    })
}

/// Appends `index` to `cpu`'s ready queue if it is dispatchable and not
/// already present. Deduplicating keeps the queues from growing when a task is
/// woken repeatedly without being dispatched.
pub(super) fn enqueue_ready(manager: &mut TaskManager, cpu: usize, index: usize) {
    if !ready_for_queue(manager, index) {
        return;
    }
    let task = &manager.tasks[index];
    let id = task.id;
    let priority = task.priority;
    let cpu = cpu.min(smp::MAX_CPUS - 1);
    if !manager.ready[cpu]
        .iter()
        .any(|entry| entry.index == index && entry.id == id)
    {
        // Back of the queue: newest last, so same-class work stays FIFO. The
        // age starts at zero; a freshly readied task has not waited yet.
        manager.ready[cpu].push_back(ReadyEntry {
            id,
            index,
            priority,
            age: 0,
        });
    }
}

/// Pops the next dispatchable task from `cpu`'s queue, discarding stale
/// entries: a task that was killed, blocked again, claimed by another CPU, or
/// whose index was shifted by a removal (the stored id no longer matches).
///
/// Each call is one aging round: every entry that stays queued gains one
/// pass, so a low-class task is promoted even while high-class arrivals keep
/// the queue non-empty.
pub(super) fn dequeue_ready(manager: &mut TaskManager, cpu: usize) -> Option<usize> {
    let cpu = cpu.min(smp::MAX_CPUS - 1);
    for entry in manager.ready[cpu].iter_mut() {
        entry.age = entry.age.saturating_add(1);
    }
    loop {
        let entry = pop_best(&mut manager.ready[cpu])?;
        let same_task = manager
            .tasks
            .get(entry.index)
            .map(|task| task.id == entry.id)
            .unwrap_or(false);
        if same_task && ready_for_queue(manager, entry.index) {
            return Some(entry.index);
        }
    }
}

/// Removes and returns the entry the policy would dispatch: the FIFO-first
/// entry with the best (lowest) effective class.
pub(super) fn pop_best(queue: &mut VecDeque<ReadyEntry>) -> Option<ReadyEntry> {
    let mut best = None;
    let mut best_class = u8::MAX;
    for (position, entry) in queue.iter().enumerate() {
        let class = sched_policy::effective(entry.priority, entry.age);
        if class < best_class {
            best = Some(position);
            best_class = class;
        }
    }
    queue.remove(best?)
}

/// Rebuilds every queue from the task table. Required after any operation that
/// shifts indices (`Vec::remove` / `retain`); also used to seed the queues
/// after a state sweep.
pub(super) fn rebuild_ready_queues(manager: &mut TaskManager) {
    for queue in &mut manager.ready {
        queue.clear();
    }
    let cpus = smp::cpu_count().max(1);
    // Split borrows: `tasks` is read while `ready` is written.
    let tasks = &manager.tasks;
    let ready = &mut manager.ready;
    for index in 0..tasks.len() {
        let task = &tasks[index];
        if !task.is_idle && task.state == TaskState::Ready && task.running_on.is_none() {
            let cpu = index % cpus;
            ready[cpu].push_back(ReadyEntry {
                id: task.id,
                index,
                priority: task.priority,
                age: 0,
            });
        }
    }
}

/// True when `cpu` is online and currently runs its idle task. Read under the
/// kernel lock, which serialises every `current_index` mutation.
#[cfg(feature = "smp")]
pub(super) fn cpu_is_idle(manager: &TaskManager, cpu: usize) -> bool {
    if !smp::cpu_online(cpu) {
        return false;
    }
    let index = smp::current_index_of(cpu);
    manager
        .tasks
        .get(index)
        .is_some_and(|task| task.is_idle)
}

/// Moves one queued task from `from` to an idle peer, if the policy finds a
/// worthwhile target. Called after a successful local dispatch: a CPU that
/// still holds work shares it instead of letting an idle PE sleep through a
/// timer window. The entry keeps its age, so a migrated low-class task does
/// not restart its aging.
#[cfg(feature = "smp")]
pub(super) fn push_migrate(manager: &mut TaskManager, from: usize) -> bool {
    let cpus = smp::cpu_count().clamp(1, smp::MAX_CPUS);
    let mut idle = [false; smp::MAX_CPUS];
    for cpu in 0..cpus {
        idle[cpu] = cpu_is_idle(manager, cpu);
    }
    let moved = push_migrate_with(manager, from, &idle[..cpus]);
    if let Some(target) = moved {
        // The target may be parked in WFI or spinning on the kernel lock;
        // without an IPI the migrated task waits for the next timer tick.
        if let Some(mpidr) = smp::mpidr_of(target) {
            crate::gic::send_sgi_to(mpidr, crate::gic::RESCHEDULE_IRQ);
        }
        #[cfg(all(feature = "smp", feature = "smp-selftest"))]
        READY_PUSH_PICKS[target].fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// [`push_migrate`] with an explicit idle view. Kept separate so the boot test
/// can exercise the queue move deterministically: a live AP is usually parked
/// on the kernel lock with no current task at that moment, so "really idle"
/// cannot be observed from the boot CPU.
#[cfg(feature = "smp")]
pub(super) fn push_migrate_with(manager: &mut TaskManager, from: usize, idle: &[bool]) -> Option<usize> {
    let from = from.min(smp::MAX_CPUS - 1);
    let cpus = idle.len().clamp(1, smp::MAX_CPUS);
    let mut loads = [0usize; smp::MAX_CPUS];
    for (cpu, load) in loads.iter_mut().enumerate().take(cpus) {
        *load = manager.ready[cpu].len();
    }
    let target = sched_policy::push_target(&loads[..cpus], idle, from)?;
    let entry = manager.ready[from].pop_back()?;
    manager.ready[target].push_back(entry);
    Some(target)
}

pub(super) fn find_next_task(manager: &mut TaskManager, cpu: usize, scan_base: usize) -> Option<usize> {
    // 1. This CPU's own queue — the common case, O(1).
    if let Some(index) = dequeue_ready(manager, cpu) {
        #[cfg(all(feature = "smp", feature = "smp-selftest"))]
        note_pick(&READY_LOCAL_PICKS, cpu);
        #[cfg(feature = "smp")]
        push_migrate(manager, cpu);
        return Some(index);
    }
    // 2. Work stealing: take one dispatchable task from another CPU's queue.
    //    This can only move a task that is not currently owned by a CPU, so it
    //    never preempts a running task.
    for other in 0..smp::cpu_count() {
        if other != cpu {
            if let Some(index) = dequeue_ready(manager, other) {
                #[cfg(all(feature = "smp", feature = "smp-selftest"))]
                note_pick(&READY_STEAL_PICKS, cpu);
                return Some(index);
            }
        }
    }
    // 3. Fallback full scan: covers idle tasks and any Ready task whose enqueue
    //    was missed. Same "non-idle first" policy as before the queues.
    let task_count = manager.tasks.len();

    for offset in 1..=task_count {
        let index = (scan_base + offset) % task_count;
        let task = &manager.tasks[index];
        if !task.is_idle && task.state == TaskState::Ready && task.running_on.is_none() {
            #[cfg(all(feature = "smp", feature = "smp-selftest"))]
            note_pick(&READY_SCAN_PICKS, cpu);
            return Some(index);
        }
    }

    for offset in 1..=task_count {
        let index = (scan_base + offset) % task_count;
        let task = &manager.tasks[index];
        if task.state == TaskState::Ready && task.running_on.is_none() {
            #[cfg(all(feature = "smp", feature = "smp-selftest"))]
            note_pick(&READY_SCAN_PICKS, cpu);
            return Some(index);
        }
    }

    None
}

