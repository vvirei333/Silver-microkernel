// Excerpts from src/scheduler.rs: BKL/scheduler blocking (1900-1950),
// capability checks and delegation (4670-4780), physical mapping (4880-4945)
        .is_some();
    interrupts::restore_irq(saved_daif);
    notified
}

pub fn block_current_on(queue: &WaitQueue, link: WaitLink) {
    crate::smp::probe(4);
    let saved_daif = interrupts::save_and_disable_irq();
    let id = current_task_id();
    queue.enqueue(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(link);
        set_state_unless_dead(task, TaskState::Blocked);
    }
    schedule_released();
    if let Some(task) = current_task_mut() {
        task.waiting_on = None;
    }
    interrupts::restore_irq(saved_daif);
}

pub fn block_current_on_until(queue: &WaitQueue, link: WaitLink, deadline: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    let id = current_task_id();
    queue.enqueue(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = Some(link);
        set_state_unless_dead(task, TaskState::BlockedUntil(deadline));
    }
    schedule_released();
    queue.remove(id);
    if let Some(task) = current_task_mut() {
        task.waiting_on = None;
    }
    interrupts::restore_irq(saved_daif);
}

pub fn wake_task(id: u64) {
    let saved_daif = interrupts::save_and_disable_irq();
    smp::bkl_acquire();
    if let Some(manager) = scheduler_manager() {
        if let Some(index) = manager.tasks.iter().position(|task| task.id == id) {
            wake_blocked_index(manager, index, smp::this_cpu_id());
        }
    }
    smp::bkl_release();
    interrupts::restore_irq(saved_daif);
    // The woken task may be pickable by a CPU that is currently idle or
    // running something else: kick the others to re-evaluate their queues.
    smp::ipi_reschedule_others();
}

    }
    enqueue_ready(manager, smp::this_cpu_id(), index);
    Some(id)
}

pub fn capability_task_id(cap_id: capability::CapId) -> Option<u64> {
    capability_task_id_with_rights(cap_id, 0)
}

pub fn capability_task_id_with_rights(cap_id: capability::CapId, rights: u32) -> Option<u64> {
    let cap = current_task_mut()?.cap_table.get(cap_id)?;
    if cap.rights & rights != rights {
        return None;
    }
    Some(cap.target_task)
}

/// Delegates `rights` from the caller's `cap_id` to `recipient_task_id`.
///
/// The request is all-or-nothing. The source capability must cover every
/// requested bit, and a request that mixes global bits (`CAP_MMIO`, `CAP_IRQ`,
/// `CAP_PCI`, `CAP_MOUNT`) with target-scoped bits is rejected entirely when
/// the caller does not own *all* of the requested global bits itself. Nothing
/// is partially applied: a spawn handle over a child can pass on `CAP_KILL`,
/// but a `CAP_KILL | CAP_MMIO` request through that handle grants neither.
///
/// Global rights become the recipient's own self-capability; target-scoped
/// rights stay bound to the original capability's target.
pub fn grant_capability(cap_id: capability::CapId, recipient_task_id: u64, rights: u32) -> bool {
    let known = capability::CAP_ALL | capability::CAP_MOUNT | capability::CAP_POWER;
    if rights == 0 || rights & !known != 0 {
        return false;
    }
    let caller_id = current_task_id();
    let saved_daif = interrupts::save_and_disable_irq();
    let mut granted = false;
    if let Some((source_target, _)) =
        current_task_mut().and_then(|task| task.cap_table.grant(cap_id, rights))
    {
        let global = rights & capability::CAP_GLOBAL_BITS;
        let scoped = rights & capability::CAP_TARGET_BITS;
        // A process-wide right may only be delegated by a task that owns it
        // itself. A spawn handle over a child covers CAP_ALL bits but is not
        // an own privilege of the parent, so it cannot pass device/mount
        // rights on.
        let owns_global = global == 0
            || current_task_mut()
                .map(|task| task.cap_table.has_self_rights(caller_id, global))
                .unwrap_or(false);
        if owns_global {
            if let Some(manager) = scheduler_manager() {
                if let Some(recipient) = manager.tasks.iter_mut().find(|task| {
                    task.id == recipient_task_id
                        && !matches!(task.state, TaskState::Dead | TaskState::Zombie)
                }) {
                    // A repeated grant must not grow one task's table without
                    // limit; the whole request fails if there is no room.
                    let needed = usize::from(global != 0) + usize::from(scoped != 0);
                    if recipient.cap_table.has_capacity_for(needed) {
                        // Global rights become the recipient's own
                        // self-capability; target-scoped rights stay bound to
                        // the original target.
                        if global != 0 {
                            recipient.cap_table.insert(recipient_task_id, global);
                        }
                        if scoped != 0 {
                            recipient.cap_table.insert(source_target, scoped);
                        }
                        granted = true;
                    }
                }
            }
        }
    }
    interrupts::restore_irq(saved_daif);
    granted
}

pub fn current_task_has_rights(rights: u32) -> bool {
    current_task_mut()
        .map(|task| {
            let id = task.id;
            task.cap_table.has_self_rights(id, rights)
        })
        .unwrap_or(false)
}

pub fn current_resource_counts() -> Option<(usize, usize, usize, usize, usize)> {
    current_task_mut().map(|task| {
        let dma_pages = task.dma_pages.iter().map(|&(_, pages, _)| pages).sum();
        (task.mem_frames, task.heap_pages.len(), task.mmap_bytes, dma_pages, task.cap_table.len())
    })
}

/// True when the current task may write `path`: it either holds the global
/// `CAP_FS_WRITE`, or its bundle manifest granted a matching prefix.
pub fn fs_write_allowed(path: &str) -> bool {
    current_task_mut()
        .map(|task| {
            let id = task.id;
            if task.cap_table.has_self_rights(id, capability::CAP_FS_WRITE) {
                return true;
            }
            let Some(scope) = task.fs_write_prefix.as_deref() else {
                return false;
            };
            path == scope
                || (path.starts_with(scope) && path.as_bytes().get(scope.len()) == Some(&b'/'))
        })
        .unwrap_or(false)
}

pub fn map_phys_nc_to_current(phys_addr: usize, size: usize) -> Option<usize> {
    map_phys_to_current_mode(phys_addr, size, true)
}

fn map_phys_to_current_mode(phys_addr: usize, size: usize, non_cacheable: bool) -> Option<usize> {
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager().and_then(|manager| {
        let shared_dma = manager.tasks.iter().any(|owner| {
            owner.dma_pages.iter().any(|&(base, page_count, _)| {
                base.checked_add(page_count * PAGE_SIZE)
                    .is_some_and(|dma_end| {
                        phys_addr >= base
                            && phys_addr
                                .checked_add(size)
                                .is_some_and(|requested_end| requested_end <= dma_end)
                    })
            })
        });
        let task = manager.tasks.get_mut(smp::current_index())?;
        let task_id = task.id;
        if !task
            .cap_table
            .has_self_rights(task_id, capability::CAP_MMIO)
        {
            return None;
        }
        if !task.cap_table.has_capacity() {
            return None;
        }
        let virtual_address = if shared_dma {
            mmu::map_shared_dma_to_user(task, phys_addr, size)?
        } else if non_cacheable {
            mmu::map_phys_nc_to_user(task, phys_addr, size)?
        } else {
            mmu::map_phys_to_user(task, phys_addr, size)?
        };
        task.cap_table
            .insert(virtual_address as u64, capability::CAP_MMIO);
        Some(virtual_address)
    });
    interrupts::restore_irq(saved_daif);
    result
}

pub fn alloc_dma_for_current(size: usize) -> Option<(usize, usize, u32)> {
    alloc_dma_run(size)
}

/// Shared implementation for explicit DMA allocation. The capability is
/// checked before any frame is charged, so a refused request has no side
/// effects.
fn alloc_dma_run(size: usize) -> Option<(usize, usize, u32)> {
    if size == 0 || size > mmu::USER_DMA_MAX_SIZE {
        return None;
    }
    let page_count = size.div_ceil(PAGE_SIZE);
    let mapped_size = page_count * PAGE_SIZE;
    let saved_daif = interrupts::save_and_disable_irq();
    let result = scheduler_manager().and_then(|manager| {
        let task = manager.tasks.get_mut(smp::current_index())?;
        let task_id = task.id;
        if !task
            .cap_table
            .has_self_rights(task_id, capability::CAP_MMIO)
        {
            console::write_str("DMA: FAIL - missing MMIO right\r\n");
