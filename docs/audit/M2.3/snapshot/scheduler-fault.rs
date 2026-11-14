// Excerpts from src/scheduler.rs: cow_copy_run (3160-3260), fork_current (3960-4040),
// lazy_page_fault + cow_fault (6560-6800)
    prot: u64,
    fixed: Option<usize>,
    physical_start: usize,
    owner: crate::linux_fd::MappedFile,
) -> Option<usize> {
    with_group_owner(|task| task.mmap_shared_file(length, prot, fixed, physical_start, owner))?
}

/// Changes protections on an existing mapping of the current task.
pub fn mprotect(address: usize, length: usize, prot: u64) -> bool {
    with_group_owner(|task| task.mprotect_range(address, length, prot)).unwrap_or(false)
}

/// Returns the current task's kernel-provided signal restorer page, creating
/// it on first use.
#[cfg(feature = "linux-abi")]
pub fn signal_trampoline() -> Option<usize> {
    with_group_owner(|task| task.signal_trampoline())?
}

/// COW write-fault entry point. Returns `true` when the faulting page was
/// copied and remapped, so the exception trampoline can retry the instruction.
#[cfg(feature = "linux-abi")]
pub fn cow_fault(address: usize, esr: u64) -> bool {
    with_group_owner(|task| task.cow_fault(address, esr)).unwrap_or(false)
}

/// Allocates a fresh run and copies an old shared run into it.
#[cfg(feature = "linux-abi")]
fn cow_copy_run(
    old_physical: usize,
    page_count: usize,
    old_generation: u32,
) -> Option<(usize, u32)> {
    let (physical, generation) =
        frame_allocator::alloc_user_contiguous_pages_tagged(page_count)?;
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(old_physical) as *const u8,
            phys_to_virt(physical) as *mut u8,
            page_count * PAGE_SIZE,
        );
    }
    let _ = old_generation;
    Some((physical, generation))
}

/// Unmaps an anonymous region previously returned by `mmap_anonymous`.
pub fn munmap(address: usize, length: usize) -> bool {
    with_group_owner(|task| task.munmap(address, length)).unwrap_or(false)
}

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

}

/// Forks the current Linux task (`clone` with `SIGCHLD`, no shared VM).
///
/// The child shares the parent's image, heap, anonymous mappings, private
/// page and user stack copy-on-write: frames gain a refcount, writable pages
/// are mapped read-only in both spaces, and the first write copies the
/// containing run through `cow_fault`. The kernel stack is new and starts with
/// a copy of the parent's SVC frame whose `x0` is zero, so the child returns
/// from `clone` as a new process. File descriptors and the Linux fd table are
/// duplicated, and the child's parent is the caller.
#[cfg(feature = "linux-abi")]
pub fn fork_current(frame: &[u64; crate::syscall::FRAME_QUADWORDS]) -> Option<u64> {
    let manager = scheduler_manager()?;
    if !has_task_capacity(manager) {
        return None;
    }
    // A plain `fork` may be called from a thread. The child is a single-thread
    // process whose address space is a copy of the *process* (the group
    // leader), not of the calling thread, and the leader owns the image,
    // heap and mmap regions. The resume frame and TLS still come from the
    // caller.
    let parent_id = current_tgid();
    let parent_index = manager
        .tasks
        .iter()
        .position(|task| task.id == parent_id)?;

    // Snapshot everything needed from the parent before mutating the table.
    let (
        parent_image,
        parent_stack,
        parent_stack_generation,
        parent_private,
        parent_private_generation,
        parent_heap,
        parent_mmap,
        parent_cwd,
        parent_argv,
        parent_envp,
        parent_arguments,
        parent_break,
        parent_mmap_bytes,
        parent_trampoline,
        parent_prefix,
        parent_read_prefix,
        parent_tls,
        parent_quota,
        parent_priority,
        parent_fp,
    ) = {
        let parent = manager.tasks.get(parent_index)?;
        if parent.personality != Personality::Linux || parent.user_image.is_some() { return None;
        }
        (
            match parent.linux_image.as_ref()?.share() {
                Some(image) => image,
                None => { return None;
                }
            },
            parent.user_stack,
            parent.user_stack_generation,
            parent.private_page,
            parent.private_page_generation,
            parent.heap_pages.clone(),
            parent.mmap_regions.clone(),
            parent.cwd.clone(),
            parent.argv.clone(),
            parent.envp.clone(),
            parent.arguments.clone(),
            parent.program_break,
            parent.mmap_bytes,
            parent.signal_trampoline,
            parent.fs_write_prefix.clone(),
            parent.read_prefix.clone(),
            current_user_tls(),
            parent.fs_quota,
            parent.priority,
            // The caller is inside its own syscall, so the live file is the
            // parent's state at the `clone` call, not the older copy parked
            // in the task record.


    /// Returns an RX page containing `mov x8, #139; svc #0`, the user trampoline
    /// used when a `sigaction` has no `SA_RESTORER`. It is created once per
    /// task and cached in `signal_trampoline`; because it is an ordinary mmap
    /// region, fork copies it and teardown frees its frame.
    #[cfg(feature = "linux-abi")]
    fn signal_trampoline(&mut self) -> Option<usize> {
        if let Some(address) = self.signal_trampoline {
            return Some(address);
        }
        // mov x8, #139 (rt_sigreturn); svc #0; brk #0.
        const CODE: [u8; 12] = [
            0x68, 0x11, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4, 0x00, 0x00, 0x20, 0xd4,
        ];
        let address =
            self.mmap_region(PAGE_SIZE, mmu::PROT_EXEC, None, Some(&CODE), None, false)?;
        self.signal_trampoline = Some(address);
        Some(address)
    }

    /// Backs one page of a lazy anonymous region. `esr` decides whether a
    /// write is permitted; untouched pages are zero-filled.
    #[cfg(feature = "linux-abi")]
    fn lazy_page_fault(&mut self, index: usize, page: usize, esr: u64) -> bool {
        const WNR: u64 = 1 << 6;
        let prot = self.mmap_regions[index].prot;
        let write = esr & WNR != 0;
        if prot == 0 || (write && prot & mmu::PROT_WRITE == 0) {
            return false;
        }
        let offset = ((page - self.mmap_regions[index].virtual_address) / PAGE_SIZE) as u32;
        if self.mmap_regions[index].pages.contains_key(&offset) {
            return false;
        }
        let source = self.mmap_regions[index].file.clone();
        if !self.charge_frames(1) {
            return false;
        }
        let Some((physical, generation)) = frame_allocator::alloc_user_page_tagged() else {
            self.release_frames(1);
            return false;
        };
        unsafe {
            core::ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, PAGE_SIZE);
        }
        if let Some((path, file_offset)) = source {
            // The faulting thread waits for one page of disk I/O; that is the
            // price of not copying an unused library into frames. VFS reads
            // are polled and take the VFS lock, which no path holds while
            // waiting for the scheduler lock.
            let start = file_offset + offset as usize * PAGE_SIZE;
            match crate::vfs::read_path_at(&path, start, PAGE_SIZE) {
                Ok(bytes) => {
                    let count = bytes.len().min(PAGE_SIZE);
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            phys_to_virt(physical) as *mut u8,
                            count,
                        );
                    }
                }
                Err(()) => {
                    // Mapping zeros would silently turn an I/O error into an
                    // empty page; fail the access instead.
                    let _ = frame_allocator::free_page_tagged(physical, generation);
                    self.release_frames(1);
                    return false;
                }
            }
        }
        if !mmu::map_user_page_at(self.ttbr0, page, physical, prot) {
            frame_allocator::free_page_tagged(physical, generation);
            self.release_frames(1);
            return false;
        }
        self.mmap_regions[index]
            .pages
            .insert(offset, (physical, generation));
        true
    }

    /// Handles a write to a page shared by `fork`. Copies the containing run,
    /// remaps it writable in this address space and drops this task's reference
    /// to the old run. A write to a genuinely read-only page returns `false`,
    /// which the fault path turns into the normal task kill.
    #[cfg(feature = "linux-abi")]
    fn cow_fault(&mut self, address: usize, esr: u64) -> bool {
        const WNR: u64 = 1 << 6;
        if self.ttbr0 == 0 {
            return false;
        }
        let page = address & !(PAGE_SIZE - 1);
        let ttbr0 = self.ttbr0;

        // A lazily reserved anonymous page backs itself on first access,
        // read or write. This is how a large `PROT_NONE` reservation from
        // glibc becomes a usable heap after `mprotect`.
        if let Some(index) = self.mmap_regions.iter().position(|region| {
            region.lazy
                && page >= region.virtual_address
                && page < region.virtual_address + region.page_count * PAGE_SIZE
        }) {
            return self.lazy_page_fault(index, page, esr);
        }

        if esr & WNR == 0 {
            return false;
        }

        if let Some(image) = self.linux_image.as_mut() {
            if let Some(run) = image.runs.iter_mut().find(|run| {
                page >= run.virtual_address
                    && page < run.virtual_address + run.page_count * PAGE_SIZE
            }) {
                let index = (page - run.virtual_address) / PAGE_SIZE;
                if !matches!(
                    run.kinds[index],
                    crate::elf::PageKind::Cow | crate::elf::PageKind::Data
                ) {
                    return false;
                }
                let old_physical = run.physical_start;
                let old_generation = run.generation;
                let count = run.page_count;
                let virtual_address = run.virtual_address;
                let Some((new_physical, new_generation)) =
                    cow_copy_run(old_physical, count, old_generation)
                else {
                    return false;
                };
                let mut kinds = run.kinds.clone();
                for kind in &mut kinds {
                    if *kind == crate::elf::PageKind::Cow {
                        *kind = crate::elf::PageKind::Data;
                    }
                }
                if !mmu::remap_user_kinds_at(ttbr0, virtual_address, new_physical, &kinds) {
                    frame_allocator::free_contiguous_pages_tagged(
                        new_physical,
                        count,
                        new_generation,
                    );
                    return false;
                }
                frame_allocator::free_contiguous_pages_tagged(
                    old_physical,
                    count,
                    old_generation,
                );
                run.physical_start = new_physical;
                run.generation = new_generation;
                run.kinds = kinds;
                return true;
            }
        }

        if page >= mmu::USER_STACK_VA && page < mmu::USER_STACK_VA + TASK_USER_STACK_SIZE {
            let old_physical = self.user_stack.start;
            let old_generation = self.user_stack_generation;
            let Some((new_physical, new_generation)) =
                cow_copy_run(old_physical, TASK_USER_STACK_PAGES, old_generation)
            else {
                return false;
            };
            if !mmu::remap_user_pages_at(
                ttbr0,
                mmu::USER_STACK_VA,
                new_physical,
                TASK_USER_STACK_PAGES,
                mmu::PROT_WRITE,
            ) {
                frame_allocator::free_contiguous_pages_tagged(
                    new_physical,
                    TASK_USER_STACK_PAGES,
                    new_generation,
                );
                return false;
            }
            frame_allocator::free_contiguous_pages_tagged(
                old_physical,
                TASK_USER_STACK_PAGES,
                old_generation,
            );
            self.user_stack = MemoryRegion::new(
                new_physical,
                new_physical + TASK_USER_STACK_SIZE,
            );
            self.user_stack_generation = new_generation;
            return true;
        }

        if page == mmu::PRIVATE_WINDOW_VA {
            let old_physical = self.private_page;
            let old_generation = self.private_page_generation;
            let Some((new_physical, new_generation)) =
                cow_copy_run(old_physical, 1, old_generation)
            else {
                return false;
            };
            if !mmu::remap_private_page(ttbr0, new_physical) {
                frame_allocator::free_contiguous_pages_tagged(
                    new_physical,
                    1,
                    new_generation,
                );
                return false;
            }
            frame_allocator::free_contiguous_pages_tagged(old_physical, 1, old_generation);
            self.private_page = new_physical;
            self.private_page_generation = new_generation;
            return true;
        }

        if page >= mmu::USER_HEAP_VA {
            let index = (page - mmu::USER_HEAP_VA) / PAGE_SIZE;
            if index < self.heap_pages.len() {
                let (old_physical, old_generation) = self.heap_pages[index];
                let Some((new_physical, new_generation)) =
                    cow_copy_run(old_physical, 1, old_generation)
                else {
                    return false;
                };
                if !mmu::remap_user_pages_at(
                    ttbr0,
                    mmu::USER_HEAP_VA + index * PAGE_SIZE,
                    new_physical,
                    1,
                    mmu::PROT_WRITE,
                ) {
                    frame_allocator::free_contiguous_pages_tagged(
                        new_physical,
                        1,
                        new_generation,
                    );
                    return false;
                }
                frame_allocator::free_contiguous_pages_tagged(
                    old_physical,
                    1,
                    old_generation,
