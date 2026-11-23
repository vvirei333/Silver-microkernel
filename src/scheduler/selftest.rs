//! Boot self-tests and stress harnesses for the scheduler.
//!
//! Split out of `scheduler/mod.rs`, which had grown to 8 700 lines by mixing
//! the scheduler with every diagnostic its boot path can run. These functions
//! are the only callers of the probe task entries below, and nothing in the
//! scheduling core depends on them (feature-gated diagnostics call them from
//! `main.rs` / `smp.rs`). They stay a child module so they can still inspect
//! private `Task`/`TaskManager` state without widening any field visibility:
//! a descendant module may read its parent's private items.

use super::*;

/// Boot-time proof of the DMA quarantine window: a queued run stays allocated
/// for `DMA_QUARANTINE_EPOCHS` reclaim passes and is released on the next one.
/// The frame allocator is otherwise untouched by this test.
pub fn run_dma_quarantine_self_test() -> bool {
    const PAGES: usize = 2;
    let Some((physical, generation)) = frame_allocator::alloc_contiguous_pages_tagged(PAGES) else {
        return false;
    };
    if !quarantine_dma_run(physical, PAGES, generation) {
        frame_allocator::free_contiguous_pages_tagged(physical, PAGES, generation);
        return false;
    }
    let queued = frame_allocator::frame_in_use(physical);

    // One pass short of the window: the run must still be allocated.
    for _ in 0..(DMA_QUARANTINE_EPOCHS - 1) {
        reclaim_quarantined_dma();
    }
    let still_held = frame_allocator::frame_in_use(physical);
    reclaim_quarantined_dma();
    let released = !frame_allocator::frame_in_use(physical);

    queued && still_held && released
}

/// Boot-time proof of the lease gate: an armed range is reported busy at both
/// edges and becomes free once the handle drops.
pub fn run_dma_lease_self_test() -> bool {
    const PAGES: usize = 2;
    let Some((physical, generation)) = frame_allocator::alloc_contiguous_pages_tagged(PAGES) else {
        return false;
    };
    let span = PAGES * PAGE_SIZE;
    let Some(lease) = DmaLease::arm(physical, span) else {
        frame_allocator::free_contiguous_pages_tagged(physical, PAGES, generation);
        return false;
    };
    let busy = dma_range_in_flight(physical, span);
    let left_edge = dma_range_in_flight(physical - PAGE_SIZE, PAGES * PAGE_SIZE);
    let right_edge = dma_range_in_flight(physical + span - PAGE_SIZE, PAGE_SIZE);
    let disjoint = !dma_range_in_flight(physical + span, PAGE_SIZE);
    drop(lease);
    let released = !dma_range_in_flight(physical, span);
    let freed = frame_allocator::free_contiguous_pages_tagged(physical, PAGES, generation);
    busy && left_edge && right_edge && disjoint && released && freed
}

#[cfg(all(feature = "smp", feature = "smp-churn"))]
const CHURN_ROUNDS: usize = 4;
#[cfg(all(feature = "smp", feature = "smp-churn"))]
const CHURN_PER_ROUND: usize = 3;
/// Churn tasks get explicit ids above the user space (like AP idle tasks) and
/// the global `next_id` is restored around each spawn. User-space spawn logic
/// assumes task ids advance in lockstep with per-task cap-handle ids, so a
/// stress task must never shift the ids of a real child (SCOUT, block driver).
#[cfg(all(feature = "smp", feature = "smp-churn"))]
const CHURN_ID_BASE: u64 = 1 << 41;
#[cfg(all(feature = "smp", feature = "smp-churn"))]
static CHURN_EXITED: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(feature = "smp", feature = "smp-churn"))]
static CHURN_ON_CPU: [AtomicUsize; smp::MAX_CPUS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
AtomicUsize::new(0),
AtomicUsize::new(0),
AtomicUsize::new(0),
AtomicUsize::new(0),
];

/// Spawns one churn task with an explicit high id, restoring `next_id` so a
/// real user spawn (SCOUT, block driver) keeps the id its cap handle expects.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
fn spawn_churn(explicit_id: u64) -> Option<u64> {
    let saved_next = {
        let manager = scheduler_manager()?;
        let saved = manager.next_id;
        manager.next_id = explicit_id;
        saved
    };
    let result = spawn_kernel(churn_task);
    if let Some(manager) = scheduler_manager() {
        manager.next_id = saved_next;
    }
    result
}

/// Kernel task entry used by the churn stress: exercise a yield (fast path)
/// or a timed block (timeout wake), count the CPU, then exit so another pass
/// reaps and tears the space down.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
extern "C" fn churn_task() -> ! {
    if smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    // A first dispatch inherits the dispatcher's masked DAIF (the scheduling
    // pass keeps IRQs disabled across the context switch).
    interrupts::enable_irq();
    let cpu = smp::this_cpu_id();
    if cpu < smp::MAX_CPUS {
        CHURN_ON_CPU[cpu].fetch_add(1, Ordering::Relaxed);
    }
    if current_task_id() & 1 == 0 {
        yield_current();
    } else {
        sleep(1);
    }
    CHURN_EXITED.fetch_add(1, Ordering::Release);
    exit_current();
}

/// SMP churn: spawn a batch of short-lived kernel tasks from the boot CPU,
/// release the BKL so the APs actually run them, then reap and require every
/// one to have left `Running`. Each spawn builds a fresh address space (ASID
/// allocation + 9 table frames) and each exit tears it down, so a run exercises
/// ready-queue dispatch, work stealing, ASID recycling and index-shifting
/// reaping under real concurrency. The per-pass invariant audit fails the boot
/// if the scheduler state drifts.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn run_smp_churn_selftest() -> bool {
    if smp::cpu_count() <= 1 {
        return true;
    }
    let total = CHURN_ROUNDS * CHURN_PER_ROUND;
    CHURN_EXITED.store(0, Ordering::Relaxed);
    for counter in &CHURN_ON_CPU {
        counter.store(0, Ordering::Relaxed);
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let first_id = CHURN_ID_BASE;
    for offset in 0..total {
        let Some(id) = spawn_churn(first_id + offset as u64) else {
            console::write_str("SMP SELFTEST: churn FAIL (spawn)\r\n");
            return false;
        };
        if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
            // A kernel probe must never take the demo console.
            task.owns_console = false;
        }
    }
    let last_id = first_id + total as u64 - 1;

    // The boot CPU holds the BKL. Release it so the APs can run the tasks;
    // the boot task itself stays Running and cannot be claimed. Keep IRQs
    // enabled while waiting so this CPU can still acknowledge a peer's
    // stop-the-world SGI and take reschedule IPIs. A timer taken here
    // re-enters `schedule`, which refuses the nested pass and returns,
    // leaving this CPU's pass untouched; global time comes from the system
    // counter, so deadlines advance whether or not this CPU takes its PPI.
    smp::bkl_release();
    interrupts::enable_irq();
    let mut spins: u32 = 0;
    while CHURN_EXITED.load(Ordering::Acquire) < total {
        spins += 1;
        if spins >= 50_000_000 {
            break;
        }
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
        }
    }
    interrupts::save_and_disable_irq();
    smp::bkl_acquire();

    let exited = CHURN_EXITED.load(Ordering::Acquire);

    // A Dead task still owned by a CPU is skipped by reap until that CPU
    // switches away, so drain in short windows with the lock released.
    let mut pending = total;
    for _ in 0..64 {
        reap_dead_tasks(manager);
        pending = manager
            .tasks
            .iter()
            .filter(|task| {
                task.id >= first_id && task.id <= last_id && task.state != TaskState::Zombie
            })
            .count();
        if pending == 0 {
            break;
        }
        smp::bkl_release();
        interrupts::enable_irq();
        for _ in 0..100_000 {
            unsafe {
                core::arch::asm!("yield", options(nomem, nostack, preserves_flags));
            }
        }
        interrupts::save_and_disable_irq();
        smp::bkl_acquire();
    }

    let ran_cpus = (0..smp::MAX_CPUS)
        .filter(|&cpu| CHURN_ON_CPU[cpu].load(Ordering::Relaxed) > 0)
        .count();
    // With only one AP available the boot CPU cannot take part (it is the
    // waiter), so require one peer; with three APs require at least two. Use
    // the online count, not the possible-slot count: `-smp 2` must not be
    // judged against a four-slot policy.
    let min_cpus = if smp::online_cpu_count() >= 3 { 2 } else { 1 };
    let ok = exited == total && pending == 0 && ran_cpus >= min_cpus;
    if ok {
        console::write_str("SMP SELFTEST: churn PASS (");
        console::write_dec_u64(total as u64);
        console::write_str(" tasks, ");
        console::write_dec_u64(ran_cpus as u64);
        console::write_str(" CPU(s) ran)\r\n");
    } else {
        console::write_str("SMP SELFTEST: churn FAIL (exited=");
        console::write_dec_u64(exited as u64);
        console::write_str("/");
        console::write_dec_u64(total as u64);
        console::write_str(" pending=");
        console::write_dec_u64(pending as u64);
        console::write_str(" cpus=");
        console::write_dec_u64(ran_cpus as u64);
        console::write_str(")\r\n");
    }
    ok
}

/// Opt-in negative test for the stall watchdog: this task masks IRQs and
/// spins for longer than the watchdog grace window, exactly like a PE that
/// hangs in a non-scheduling loop. It then unmasks, yields and exits.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
extern "C" fn stall_probe_task() -> ! {
    // A first dispatch inherits the dispatcher's kernel-lock hold. Drop it:
    // the probe must simulate a CPU stuck with IRQs masked, not a CPU that
    // also owns the global lock (which would stall every other CPU and is a
    // different failure).
    if smp::bkl_depth() > 0 {
        smp::bkl_release();
    }
    crate::interrupts::save_and_disable_irq();
    let start = crate::timer::ticks();
    // 120 ticks = 1.2 s at 100 Hz: three 250 ms check rounds plus margin.
    while crate::timer::ticks().saturating_sub(start) < 120 {
        core::hint::spin_loop();
    }
    crate::interrupts::enable_irq();
    yield_current();
    exit_current()
}

/// Proves the monitor end to end: spawn a kernel task that masks IRQs and
/// spins, let an AP pick it, and require another PE to report the stall. The
/// probe is retired afterwards and the boot continues.
#[cfg(all(feature = "smp", feature = "smp-churn"))]
pub fn run_stall_selftest() -> bool {
    if smp::online_cpu_count() < 2 {
        return true;
    }
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    const STALL_PROBE_ID: u64 = 1 << 44;
    let saved_next = manager.next_id;
    manager.next_id = STALL_PROBE_ID;
    let probe = spawn_kernel(stall_probe_task);
    manager.next_id = saved_next;
    let Some(id) = probe else {
        return false;
    };
    if let Some(task) = manager.tasks.iter_mut().find(|task| task.id == id) {
        // A diagnostic probe must never take the demo console.
        task.owns_console = false;
    }
    crate::watchdog::reset_for_test();

    // Release the BKL so an AP can dispatch the probe, and keep our own IRQs
    // enabled so this CPU runs its own check rounds.
    smp::bkl_release();
    interrupts::enable_irq();
    let detect_deadline = timer::ticks().saturating_add(200);
    while crate::watchdog::stall_count() == 0 && timer::ticks() < detect_deadline {
        core::hint::spin_loop();
    }
    let reported = crate::watchdog::stall_count() > 0;

    // The probe stops masking on its own; give it time to yield and exit.
    let cleanup_deadline = timer::ticks().saturating_add(100);
    while timer::ticks() < cleanup_deadline {
        core::hint::spin_loop();
    }
    interrupts::save_and_disable_irq();
    smp::bkl_acquire();

    let mut pending = true;
    for _ in 0..64 {
        reap_dead_tasks(manager);
        pending = manager
            .tasks
            .iter()
            .any(|task| task.id == id && task.state != TaskState::Zombie);
        if !pending {
            break;
        }
        smp::bkl_release();
        interrupts::enable_irq();
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
        interrupts::save_and_disable_irq();
        smp::bkl_acquire();
    }
    if let Some(index) = manager
        .tasks
        .iter()
        .position(|task| task.id == id && task.state == TaskState::Zombie)
    {
        take_zombie(manager, index);
    }
    let cleaned = manager.tasks.iter().all(|task| task.id != id);
    let ok = reported && cleaned && !pending;
    if ok {
        console::write_str("SMP SELFTEST: watchdog stall PASS (masked-IRQ probe detected)\r\n");
    } else {
        console::write_str("SMP SELFTEST: watchdog stall FAIL (reported=");
        console::write_dec_u64(reported as u64);
        console::write_str(" cleaned=");
        console::write_dec_u64(cleaned as u64);
        console::write_str(" pending=");
        console::write_dec_u64(pending as u64);
        console::write_str(")\r\n");
    }
    ok
}

/// Boot proof of partial `munmap`: map four pages, remove the middle two,
/// require the surviving ends to stay mapped and the hole to be gone, refill
/// the hole (first-fit), then release every remaining range and require the
/// ledger to return to its baseline.
pub fn run_mmap_split_selftest() -> bool {
    let Some(baseline) = current_task_mem_frames() else {
        return false;
    };
    let Some(ttbr0) = current_task_mut().map(|task| task.ttbr0) else {
        return false;
    };
    let page = PAGE_SIZE;
    let Some(base) = mmap_anonymous(4 * page, mmu::PROT_WRITE) else {
        return false;
    };
    let split = munmap(base + page, 2 * page);
    let split_regions = current_task_mmap_regions();
    let holes_gone = !mmu::user_page_is_mapped(ttbr0, base + page)
        && !mmu::user_page_is_mapped(ttbr0, base + 2 * page);
    let ends_kept = mmu::user_page_is_mapped(ttbr0, base)
        && mmu::user_page_is_mapped(ttbr0, base + 3 * page);
    // First-fit must put the next single page back into the hole.
    let refilled = mmap_anonymous(page, mmu::PROT_WRITE) == Some(base + page);
    let mut freed = refilled;
    if refilled {
        freed &= munmap(base + page, page);
        freed &= munmap(base, page);
        freed &= munmap(base + 3 * page, page);
    }
    let empty = current_task_mem_frames() == Some(baseline);
    let ok = split && holes_gone && ends_kept && refilled && freed && empty;
    #[cfg(not(feature = "tablet"))]
    {
        crate::console::write_str(if ok {
            "MMAP SPLIT PASS (regions="
        } else {
            "MMAP SPLIT FAIL (regions="
        });
        crate::console::write_dec_u64(split_regions as u64);
        crate::console::write_str(", ledger=");
        crate::console::write_dec_u64(current_task_mem_frames().unwrap_or(0) as u64);
        crate::console::write_str(")\r\n");
    }
    ok
}

/// Boot proof that the per-task ledger is wired: a two-frame charge fits a
/// forced limit exactly, one more frame is refused without changing the
/// counter, releasing restores the original value, and clearing the override
/// reopens the gate.
pub fn run_task_ledger_selftest() -> bool {
    let Some(before) = current_task_mem_frames() else {
        return false;
    };
    let limit = task_frame_limit();
    TASK_LIMIT_OVERRIDE.store(before.saturating_add(2), Ordering::Relaxed);
    let charged = charge_current_frames(2);
    let refused = !charge_current_frames(1);
    release_current_frames(2);
    let restored = current_task_mem_frames() == Some(before);
    TASK_LIMIT_OVERRIDE.store(usize::MAX, Ordering::Relaxed);
    let reopened = charge_current_frames(1);
    if reopened {
        release_current_frames(1);
    }
    let ok = charged && refused && restored && reopened && current_task_mem_frames() == Some(before);
    #[cfg(not(feature = "tablet"))]
    {
        crate::console::write_str(if ok {
            "TASK LEDGER PASS (limit="
        } else {
            "TASK LEDGER FAIL (limit="
        });
        crate::console::write_dec_u64(limit as u64);
        crate::console::write_str(" frames, before=");
        crate::console::write_dec_u64(before as u64);
        crate::console::write_str(")\r\n");
    }
    ok
}

/// Asserts the SYS_STOP_CONSOLE policy: the syscall requires
/// `CAP_CONSOLE_GRANT` on the caller. The console owner (HOWLER) receives it
/// at spawn, while a plain driver does not, so a driver is denied and the
/// owner is allowed. This guards the grant map; the syscall branch itself is
/// `current_task_has_rights(CAP_CONSOLE_GRANT)`.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_console_cap_selftest() -> bool {
    let allowed = initial_self_rights(user_program::HOWLER)
        .is_some_and(|rights| rights & capability::CAP_CONSOLE_GRANT != 0);
    let denied = initial_self_rights(user_program::NET_DRIVER)
        .is_some_and(|rights| rights & capability::CAP_CONSOLE_GRANT == 0);
    allowed && denied
}

/// Proves the ready queue is FIFO, not a stack. Three probes are spawned in
/// order and every queue is drained: the probes must appear in spawn order.
/// The old `Vec` + `pop()` pair dispatched the newest entry first, so with
/// three or more runnable tasks the oldest one was never dispatched at all.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn ready_queue_probe_task() -> ! {
    exit_current()
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_ready_queue_selftest() -> bool {
    const PROBE_BASE: u64 = 1 << 43;
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let saved_next = manager.next_id;
    let mut ids = [0u64; 3];
    let mut spawned = 0usize;
    for (offset, slot) in ids.iter_mut().enumerate() {
        // Explicit ids keep `next_id` untouched: user-space spawn logic
        // assumes task ids and per-task cap-handle ids advance in lockstep.
        manager.next_id = PROBE_BASE + offset as u64;
        let Some(id) = spawn_kernel(ready_queue_probe_task) else {
            break;
        };
        *slot = id;
        spawned += 1;
    }
    manager.next_id = saved_next;

    // Pop every queue through the real `dequeue_ready` path (not `drain`,
    // which always walks front to back and would hide a stack-shaped queue),
    // record the dispatch order, then restore the snapshot.
    let mut seen = [usize::MAX; 3];
    let mut duplicates = false;
    let mut position = 0usize;
    for cpu in 0..smp::MAX_CPUS {
        let snapshot: Vec<ReadyEntry> = manager.ready[cpu].iter().copied().collect();
        while let Some(index) = dequeue_ready(manager, cpu) {
            let id = manager.tasks[index].id;
            if let Some(slot) = ids[..spawned].iter().position(|probe| *probe == id) {
                if seen[slot] != usize::MAX {
                    duplicates = true;
                }
                seen[slot] = position;
            }
            position += 1;
        }
        manager.ready[cpu].clear();
        manager.ready[cpu].extend(snapshot);
    }

    let ordered = spawned == 3
        && !duplicates
        && seen.iter().all(|&slot| slot != usize::MAX)
        && seen.windows(2).all(|pair| pair[0] < pair[1]);

    // The probes never ran; retire them exactly like a task that died before
    // its first dispatch (reap, then remove the zombie record).
    for &id in &ids[..spawned] {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            manager.tasks[index].state = TaskState::Dead;
            manager.tasks[index].running_on = None;
        }
    }
    reap_dead_tasks(manager);
    for &id in &ids[..spawned] {
        if let Some(index) = manager
            .tasks
            .iter()
            .position(|task| task.id == id && task.state == TaskState::Zombie)
        {
            take_zombie(manager, index);
        }
    }

    if ordered {
        console::write_str("SMP SELFTEST: ready fifo PASS (3 probes in spawn order)\r\n");
    } else {
        console::write_str("SMP SELFTEST: ready fifo FAIL (spawned=");
        console::write_dec_u64(spawned as u64);
        console::write_str(" positions=");
        console::write_dec_u64(seen[0] as u64);
        console::write_str(",");
        console::write_dec_u64(seen[1] as u64);
        console::write_str(",");
        console::write_dec_u64(seen[2] as u64);
        console::write_str(")\r\n");
    }
    ordered
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn priority_probe_task() -> ! {
    exit_current()
}

/// Proves the priority/aging contract on the real ready queue:
/// * a high-class probe enqueued last is dispatched before lower classes;
/// * two normal probes keep FIFO order;
/// * a low-class probe sitting behind a stream of fresh high-class arrivals
///   is promoted after two aging windows, so it cannot starve.
///
/// The queue is treated as scratch space: it is snapshotted before the probes
/// are spawned and restored afterwards, then the probes are retired.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_priority_selftest() -> bool {
    const PROBE_BASE: u64 = 1 << 45;
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let cpu = smp::this_cpu_id().min(smp::MAX_CPUS - 1);
    let snapshot: Vec<ReadyEntry> = manager.ready[cpu].iter().copied().collect();

    let saved_next = manager.next_id;
    let mut ids = [0u64; 4];
    let mut spawned = 0usize;
    for (offset, slot) in ids.iter_mut().enumerate() {
        // Explicit ids keep `next_id` (and the user-space task-id/cap-handle
        // lockstep) untouched.
        manager.next_id = PROBE_BASE + offset as u64;
        let Some(id) = spawn_kernel(priority_probe_task) else {
            break;
        };
        *slot = id;
        spawned += 1;
    }
    manager.next_id = saved_next;

    let mut ok = spawned == 4;
    // Classes: low, normal, normal, high.
    if ok {
        ok = set_task_priority(ids[0], sched_policy::PRIORITY_LOW)
            && set_task_priority(ids[1], sched_policy::PRIORITY_NORMAL)
            && set_task_priority(ids[2], sched_policy::PRIORITY_NORMAL)
            && set_task_priority(ids[3], sched_policy::PRIORITY_HIGH);
    }

    // 1. Priority beats arrival order; same-class entries stay FIFO.
    let mut order = [0u64; 4];
    if ok {
        manager.ready[cpu].clear();
        for (slot, &id) in ids.iter().enumerate() {
            let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
                ok = false;
                break;
            };
            let priority = match slot {
                0 => sched_policy::PRIORITY_LOW,
                3 => sched_policy::PRIORITY_HIGH,
                _ => sched_policy::PRIORITY_NORMAL,
            };
            manager.ready[cpu].push_back(ReadyEntry {
                id,
                index,
                priority,
                age: 0,
            });
        }
        for slot in order.iter_mut() {
            let Some(index) = dequeue_ready(manager, cpu) else {
                ok = false;
                break;
            };
            *slot = manager.tasks[index].id;
        }
        ok &= order == [ids[3], ids[1], ids[2], ids[0]];
    }

    // 2. Bounded waiting: the low probe must win after two aging windows
    //    while a fresh high probe keeps being re-enqueued behind it.
    let mut aged_round = 0u32;
    if ok {
        let Some(low_index) = manager.tasks.iter().position(|task| task.id == ids[0]) else {
            return false;
        };
        let Some(high_index) = manager.tasks.iter().position(|task| task.id == ids[3]) else {
            return false;
        };
        manager.ready[cpu].clear();
        manager.ready[cpu].push_back(ReadyEntry {
            id: ids[0],
            index: low_index,
            priority: sched_policy::PRIORITY_LOW,
            age: 0,
        });
        manager.ready[cpu].push_back(ReadyEntry {
            id: ids[3],
            index: high_index,
            priority: sched_policy::PRIORITY_HIGH,
            age: 0,
        });
        for round in 1..=32u32 {
            let Some(index) = dequeue_ready(manager, cpu) else {
                ok = false;
                break;
            };
            if manager.tasks[index].id == ids[0] {
                aged_round = round;
                break;
            }
            // The high probe ran and yielded: put it back at the tail as a
            // fresh arrival, so pressure on the low probe stays constant.
            manager.ready[cpu].push_back(ReadyEntry {
                id: ids[3],
                index: high_index,
                priority: sched_policy::PRIORITY_HIGH,
                age: 0,
            });
        }
        ok &= aged_round > 0
            && aged_round <= (sched_policy::AGING_PASSES as u32 * 2 + 1);
    }

    // Restore the real queue and retire the probes.
    manager.ready[cpu].clear();
    manager.ready[cpu].extend(snapshot);
    for &id in &ids[..spawned] {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            manager.tasks[index].state = TaskState::Dead;
            manager.tasks[index].running_on = None;
        }
    }
    reap_dead_tasks(manager);
    for &id in &ids[..spawned] {
        if let Some(index) = manager
            .tasks
            .iter()
            .position(|task| task.id == id && task.state == TaskState::Zombie)
        {
            take_zombie(manager, index);
        }
    }

    if ok {
        console::write_str("SMP SELFTEST: priority PASS (high first, FIFO in class, low aged in ");
        console::write_dec_u64(aged_round as u64);
        console::write_str(" round(s))\r\n");
    } else {
        console::write_str("SMP SELFTEST: priority FAIL (spawned=");
        console::write_dec_u64(spawned as u64);
        console::write_str(" order=");
        console::write_dec_u64(order[0]);
        console::write_str(",");
        console::write_dec_u64(order[1]);
        console::write_str(",");
        console::write_dec_u64(order[2]);
        console::write_str(",");
        console::write_dec_u64(order[3]);
        console::write_str(" aged_round=");
        console::write_dec_u64(aged_round as u64);
        console::write_str(")\r\n");
    }
    ok
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
extern "C" fn push_probe_task() -> ! {
    exit_current()
}

/// Proves push migration on the real queue: with one probe queued locally and
/// an idle, empty peer, `push_migrate` must move the probe to that peer. Skips
/// (returns true) when no peer is idle, which is legitimate for a profile that
/// keeps every CPU busy.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_push_selftest() -> bool {
    const PUSH_PROBE_ID: u64 = 1 << 46;
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let me = smp::this_cpu_id();
    // Pick an online peer and mark it idle for the test. A live AP is usually
    // parked on the kernel lock with no current task here, so observing real
    // idleness would make this test timing-dependent; the real `push_migrate`
    // path uses `cpu_is_idle` and is visible in the `push=` dispatch counter.
    let Some(target) = (0..smp::MAX_CPUS)
        .find(|&cpu| cpu != me && cpu < smp::cpu_count() && smp::cpu_online(cpu))
    else {
        console::write_str("SMP SELFTEST: push SKIP (no online peer)\r\n");
        return true;
    };
    // Set the peer's queue aside (we hold the kernel lock, so it cannot drain
    // it) and restore it after the probe.
    let saved_target_queue: Vec<ReadyEntry> = manager.ready[target].drain(..).collect();

    let saved_next = manager.next_id;
    manager.next_id = PUSH_PROBE_ID;
    let probe = spawn_kernel(push_probe_task);
    manager.next_id = saved_next;
    let Some(id) = probe else {
        return false;
    };
    // A pass on another CPU cannot run while this CPU holds the kernel lock,
    // so the queue is stable: drop any copy spawn made and install exactly
    // one entry at the back of the local queue.
    for cpu in 0..smp::MAX_CPUS {
        manager.ready[cpu].retain(|entry| entry.id != id);
    }
    let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
        return false;
    };
    manager.ready[me].push_back(ReadyEntry {
        id,
        index,
        priority: manager.tasks[index].priority,
        age: 0,
    });

    let cpus = smp::cpu_count().clamp(1, smp::MAX_CPUS);
    let mut idle = [false; smp::MAX_CPUS];
    idle[target] = true;
    let moved = push_migrate_with(manager, me, &idle[..cpus]).is_some();
    let on_target = manager.ready[target].iter().any(|entry| entry.id == id);
    let off_source = !manager.ready[me].iter().any(|entry| entry.id == id);

    // Undo the test's view of the target queue: drop the probe (it is retired
    // below) and put the peer's own entries back in their original order.
    manager.ready[target].retain(|entry| entry.id != id);
    manager.ready[target].extend(saved_target_queue);

    // Retire the probe before it can run.
    if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
        manager.tasks[index].state = TaskState::Dead;
        manager.tasks[index].running_on = None;
    }
    reap_dead_tasks(manager);
    if let Some(index) = manager
        .tasks
        .iter()
        .position(|task| task.id == id && task.state == TaskState::Zombie)
    {
        take_zombie(manager, index);
    }

    let ok = moved && on_target && off_source;
    if ok {
        console::write_str("SMP SELFTEST: push PASS (probe moved to CPU");
        console::write_dec_u64(target as u64);
        console::write_str(")\r\n");
    } else {
        console::write_str("SMP SELFTEST: push FAIL (target=");
        console::write_dec_u64(target as u64);
        console::write_str(" moved=");
        console::write_dec_u64(moved as u64);
        console::write_str(" on_target=");
        console::write_dec_u64(on_target as u64);
        console::write_str(" off_source=");
        console::write_dec_u64(off_source as u64);
        console::write_str(")\r\n");
    }
    ok
}

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_remap_sentinel_selftest() -> bool {
    let Some(manager) = scheduler_manager() else {
        return false;
    };
    let Some(cpu) = (1..smp::MAX_CPUS)
        .find(|&cpu| smp::cpu_online(cpu) && smp::current_index_of(cpu) == usize::MAX)
    else {
        // Single CPU, or every AP already picked: nothing to prove here.
        return true;
    };

    // The probe must not shift `next_id`: user-space spawn logic assumes a
    // task id and its per-task cap-handle id advance in lockstep. Give the
    // probe an explicit id above the user space, like the churn tasks.
    const REMAP_PROBE_ID: u64 = 1 << 42;
    let saved_next = manager.next_id;
    manager.next_id = REMAP_PROBE_ID;
    let probe = spawn_kernel(remap_probe_task);
    manager.next_id = saved_next;
    let Some(id) = probe else {
        return false;
    };
    let Some(index) = manager.tasks.iter().position(|task| task.id == id) else {
        return false;
    };
    // Kill the probe before it can run, exactly like a task that died while
    // the AP was still parked, then force both remap paths: reap (no index
    // shift) and zombie removal (indices shift).
    manager.tasks[index].state = TaskState::Dead;
    manager.tasks[index].running_on = None;
    reap_dead_tasks(manager);
    let after_reap = smp::current_index_of(cpu);
    if let Some(zombie) = manager
        .tasks
        .iter()
        .position(|task| task.id == id && task.state == TaskState::Zombie)
    {
        take_zombie(manager, zombie);
    }
    let after_remove = smp::current_index_of(cpu);
    let ok = after_reap == usize::MAX && after_remove == usize::MAX;
    if ok {
        console::write_str("SMP SELFTEST: remap sentinel PASS (AP");
        console::write_dec_u64(cpu as u64);
        console::write_str(" kept no-current-task)\r\n");
    } else {
        console::write_str("SMP SELFTEST: remap sentinel FAIL (cpu=");
        console::write_dec_u64(cpu as u64);
        console::write_str(" after_reap=");
        console::write_dec_u64(after_reap as u64);
        console::write_str(" after_remove=");
        console::write_dec_u64(after_remove as u64);
        console::write_str(")\r\n");
    }
    ok
}
