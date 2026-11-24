use super::*;
use super::table::*;
use super::kernel::*;
use super::user::*;

/// Boot-time proof of the reclamation invariants that do not need a running
/// task: a partial unmap is refused without touching the mapped page, the full
/// unmap succeeds, teardown releases the address space exactly once, and a
/// second teardown with the same root token is refused.
pub fn run_self_test() -> bool {
    let Some((private, private_generation)) = frame_allocator::alloc_page_tagged() else {
        return false;
    };
    let Some((data, data_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };
    let Some((root, space)) = create_task_space(private, 0, None, false) else {
        frame_allocator::free_page_tagged(data, data_generation);
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };

    let va = USER_MMAP_VA;
    let mapped = map_user_pages_at(root, va, data, 1, PROT_WRITE);
    // A two-page unmap reaches an unmapped page: the whole request must be
    // refused and the first page must still be mapped.
    let partial_refused = mapped && !unmap_user_pages_at(root, va, 2);
    let still_mapped = mapped && user_page_is_mapped(root, va);
    let unmapped = mapped && unmap_user_pages_at(root, va, 1);
    let gone = mapped && !user_page_is_mapped(root, va);

    // Exercise both sides of the 1 GiB L1 boundary and the former Denial
    // fault address. Each mapping must be independently visible and removable.
    let mut wide_ok = true;
    for high_va in [0x7fff_f000, 0x8000_0000, 0x8122_f000, 0xc000_0000] {
        let installed = map_user_pages_at(root, high_va, data, 1, PROT_WRITE);
        let visible = installed && user_physical_address(root, high_va) == Some(data);
        let removed = installed && unmap_user_pages_at(root, high_va, 1);
        wide_ok &= installed && visible && removed && !user_page_is_mapped(root, high_va);
    }

    frame_allocator::free_page_tagged(data, data_generation);
    let space_released = destroy_task_space(space, private, private_generation, asid_of(root));
    // Stale/double teardown: the registry record is gone, so this must fail
    // without freeing anything else. Quiet: this refusal is the expected
    // result, not a fault.
    let stale_refused = !destroy_task_space_inner(space, 0, 0, asid_of(root), false);

    mapped && partial_refused && still_mapped && unmapped && gone && wide_ok && space_released && stale_refused
}

/// Proves the enlarged user DMA window really spans more than one 2 MiB L2
/// group and that an oversized request is refused without side effects. The
/// display path needs two ~2.8 MiB surfaces, which only fits because the
/// first-fit allocator walks every group.
///
/// This runs on every platform, including fog, so a device boot that reaches
/// this point has exercised the same multi-group mapping arithmetic the
/// framebuffer path uses (the fog UI itself does not start userspace).
pub fn run_dma_window_self_test() -> bool {
    // One page into the second group: the mapping must cross a group boundary.
    let pages = ENTRIES + 1;
    let span = pages * GRANULE;
    let Some((physical, generation)) = frame_allocator::alloc_user_contiguous_pages_tagged(pages)
    else {
        return false;
    };
    let Some((private, private_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_contiguous_pages_tagged(physical, pages, generation);
        return false;
    };
    let Some((space, token)) = create_task_space(private, 0, None, false) else {
        frame_allocator::free_page_tagged(private, private_generation);
        frame_allocator::free_contiguous_pages_tagged(physical, pages, generation);
        return false;
    };

    let mapped = map_dma_to_user_space(space, physical, span);
    // The run may start in group 0 and continue across the boundary; what
    // matters is that the last page lives in a later group.
    let crossed_group = mapped
        .is_some_and(|va| va + span > USER_MMIO_VA + ENTRIES * GRANULE);
    // A request larger than the whole window must fail closed. It must not
    // install a partial run or disturb the mapping just published.
    let oversized_refused =
        map_dma_to_user_space(space, physical, USER_DMA_MAX_SIZE + GRANULE).is_none();

    let revoke = revoke_phys_range(space, physical, physical + span);
    let released = destroy_task_space(token, private, private_generation, asid_of(space));
    let freed = revoke.safe_to_free
        && revoke.unmapped == pages
        && frame_allocator::free_contiguous_pages_tagged(physical, pages, generation);

    mapped.is_some() && crossed_group && oversized_refused && freed && released
}

/// Boot-time proof that the CPU honours a translation-table edit: read a value
/// through TTBR0, remap the same VA to a second frame, and read it again. A
/// descriptor-only check cannot catch a missing TLB invalidation; this one
/// reads through the translation the CPU itself is using, so it also covers
/// `switch_address_space` and the `tlbi ...is` + `dsb ish` protocol.
pub fn run_tlb_self_test() -> bool {
    const FIRST_VALUE: u64 = 0x544C_4200_0000_0001;
    const SECOND_VALUE: u64 = 0x544C_4200_0000_0002;

    let Some((first, first_generation)) = frame_allocator::alloc_page_tagged() else {
        return false;
    };
    let Some((second, second_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(first, first_generation);
        return false;
    };
    let Some((private, private_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(second, second_generation);
        frame_allocator::free_page_tagged(first, first_generation);
        return false;
    };

    let Some((root, space)) = create_task_space(private, 0, None, false) else {
        frame_allocator::free_page_tagged(private, private_generation);
        frame_allocator::free_page_tagged(second, second_generation);
        frame_allocator::free_page_tagged(first, first_generation);
        return false;
    };
    let va = USER_MMAP_VA;


    // The test switches the running PE's own TTBR0. IRQs must stay masked:
    // an IRQ handler that touched user memory would see this scratch space.
    let saved_daif = crate::interrupts::save_and_disable_irq();
    let previous = current_ttbr0();
    switch_address_space(root);

    // Published frames are zeroed by `map_user_pages_at`, so seed the first
    // page through the *user* mapping and read it back.
    let mapped_first = map_user_pages_at(root, va, first, 1, PROT_WRITE);
    let first_read = if mapped_first {
        unsafe {
            (va as *mut u64).write_volatile(FIRST_VALUE);
            (va as *const u64).read_volatile()
        }
    } else {
        0
    };

    // Repoint the same VA at `second`, using the user map (which zeroes it) and
    // then seeding it through the kernel alias. A stale TLB entry for `first`
    // would still read FIRST_VALUE here, which is exactly the failure this
    // test must catch.
    let remapped =
        unmap_user_pages_at(root, va, 1) && map_user_pages_at(root, va, second, 1, PROT_WRITE);
    if remapped {
        unsafe {
            (phys_to_virt(second) as *mut u64).write_volatile(SECOND_VALUE);
        }
        clean_range_to_poc(second, GRANULE);
    }
    let second_read = if remapped {
        unsafe { (va as *const u64).read_volatile() }
    } else {
        0
    };

    switch_address_space(previous);
    crate::interrupts::restore_irq(saved_daif);

    // Tear the scratch space down exactly once. If a PTE survived, the frames
    // must be leaked rather than handed to the allocator under a live mapping.
    unmap_user_pages_at(root, va, 1);
    let mut safe_to_free = !user_page_is_mapped(root, va);
    if safe_to_free {
        safe_to_free &= frame_allocator::free_page_tagged(first, first_generation);
        safe_to_free &= frame_allocator::free_page_tagged(second, second_generation);
    }
    let released = destroy_task_space(space, private, private_generation, asid_of(root));

    first_read == FIRST_VALUE && second_read == SECOND_VALUE && safe_to_free && released
}

/// Proves that a recycled DMA frame is zeroed before it is published to EL0.
/// The frame is poisoned, then mapped through the same `map_dma_to_user_space`
/// path userspace DMA uses; any stale byte would still be readable afterwards.
///
/// With caches enabled the test also crosses the two aliases: the zeroes must
/// be visible through the Normal Non-Cacheable mapping (the clean in the
/// publish path is what puts them in memory), and a write through that mapping
/// must become visible to the cacheable kernel alias after an invalidate.
/// On hardware that models caches, removing either maintenance step makes this
/// check fail once `SCTLR_EL1.C` is on; QEMU does not model caches, so there
/// the probe passes either way and is HIL evidence rather than an emulated
/// discriminator.
pub fn run_dma_zero_self_test() -> bool {
    let Some((private, private_generation)) = frame_allocator::alloc_page_tagged() else {
        return false;
    };
    let Some((frame, frame_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };
    let Some((space, token)) = create_task_space(private, 0, None, false) else {
        frame_allocator::free_page_tagged(frame, frame_generation);
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };

    unsafe {
        core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0xA5, GRANULE);
    }
    clean_range_to_poc(frame, GRANULE);

    let mapped = map_dma_to_user_space(space, frame, GRANULE);
    let mut zeroed = true;
    for index in 0..GRANULE {
        let byte = unsafe { (phys_to_virt(frame) as *const u8).add(index).read() };
        if byte != 0 {
            zeroed = false;
            break;
        }
    }

    let mut coherent = false;
    if let Some(va) = mapped {
        let saved_daif = crate::interrupts::save_and_disable_irq();
        let previous = current_ttbr0();
        switch_address_space(space);
        // Direction 1: CPU wrote, device reads. The zeros were written through
        // the cacheable alias; the NC side must see them after the clean.
        let mut probe = [0xFF_u8; 8];
        let visible =
            crate::user_memory::copy_from_user(va as u64, &mut probe).is_ok()
                && probe == [0_u8; 8];
        // Direction 2: device wrote, CPU reads. The device updates memory
        // behind a clean cached line; only an invalidate can expose it.
        let wrote = crate::user_memory::copy_to_user(&[0x5A], va as u64).is_ok();
        invalidate_range(phys_to_virt(frame), phys_to_virt(frame) + GRANULE);
        let read_back = unsafe { (phys_to_virt(frame) as *const u8).read_volatile() };
        switch_address_space(previous);
        crate::interrupts::restore_irq(saved_daif);
        coherent = visible && wrote && read_back == 0x5A;
    }

    destroy_task_space(token, private, private_generation, asid_of(space));
    frame_allocator::free_page_tagged(frame, frame_generation);
    mapped.is_some() && zeroed && coherent
}

/// Proves the heap-L3 persistence contract exercised by `SYS_MUNMAP`/`brk`:
/// a generic unmap that empties the heap must not free the spawn-time L3
/// table, and a later heap map must succeed instead of following a freed
/// descriptor into physical address 0.
pub fn run_heap_table_self_test() -> bool {
    let Some((private, private_generation)) = frame_allocator::alloc_page_tagged() else {
        return false;
    };
    let Some((first, first_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };
    let Some((second, second_generation)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(first, first_generation);
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };
    let Some((space, token)) = create_task_space(private, 0, None, false) else {
        frame_allocator::free_page_tagged(second, second_generation);
        frame_allocator::free_page_tagged(first, first_generation);
        frame_allocator::free_page_tagged(private, private_generation);
        return false;
    };

    // Poison the first frame so a skipped zeroing step is observable.
    unsafe {
        core::ptr::write_bytes(phys_to_virt(first) as *mut u8, 0xA5, GRANULE);
    }
    clean_range_to_poc(first, GRANULE);

    let mut ok = map_user_heap_pages(space, 0, &[first]);
    // Generic unmap empties the heap L3; it belongs to the space and must
    // survive, otherwise the next map writes through a zero L2 descriptor.
    if ok {
        ok = unmap_user_pages_at(space, USER_HEAP_VA, 1);
    }
    if ok {
        ok = map_user_heap_pages(space, 0, &[second]);
    }
    if ok {
        ok = unmap_user_heap_page(space, 0) == Some(second);
    }

    destroy_task_space(token, private, private_generation, asid_of(space));
    frame_allocator::free_page_tagged(first, first_generation);
    frame_allocator::free_page_tagged(second, second_generation);
    ok
}

/// Boot-time proof that two address spaces are isolated by their ASID tag:
/// both map the same VA to different frames, and switching between them
/// **without any TLB invalidation** must return each space's own value. This
/// is exactly the property that lets [`switch_address_space`] skip the
/// domain-wide flush; without ASIDs the second read would keep the first
/// space's stale translation.
pub fn run_asid_self_test() -> bool {
    const VALUE_A: u64 = 0x4153_4944_0000_0001;
    const VALUE_B: u64 = 0x4153_4944_0000_0002;

    let Some((private_a, private_generation_a)) = frame_allocator::alloc_page_tagged() else {
        return false;
    };
    let Some((private_b, private_generation_b)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(private_a, private_generation_a);
        return false;
    };
    let Some((frame_a, frame_generation_a)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(private_b, private_generation_b);
        frame_allocator::free_page_tagged(private_a, private_generation_a);
        return false;
    };
    let Some((frame_b, frame_generation_b)) = frame_allocator::alloc_page_tagged() else {
        frame_allocator::free_page_tagged(frame_a, frame_generation_a);
        frame_allocator::free_page_tagged(private_b, private_generation_b);
        frame_allocator::free_page_tagged(private_a, private_generation_a);
        return false;
    };

    let Some((space_a, token_a)) = create_task_space(private_a, 0, None, false) else {
        frame_allocator::free_page_tagged(frame_b, frame_generation_b);
        frame_allocator::free_page_tagged(frame_a, frame_generation_a);
        frame_allocator::free_page_tagged(private_b, private_generation_b);
        frame_allocator::free_page_tagged(private_a, private_generation_a);
        return false;
    };
    let Some((space_b, token_b)) = create_task_space(private_b, 0, None, false) else {
        destroy_task_space(token_a, private_a, private_generation_a, asid_of(space_a));
        frame_allocator::free_page_tagged(frame_b, frame_generation_b);
        frame_allocator::free_page_tagged(frame_a, frame_generation_a);
        frame_allocator::free_page_tagged(private_b, private_generation_b);
        return false;
    };

    let va = USER_MMAP_VA;
    let mapped = map_user_pages_at(space_a, va, frame_a, 1, PROT_WRITE)
        && map_user_pages_at(space_b, va, frame_b, 1, PROT_WRITE);
    let distinct_asids =
        asid_of(space_a) != 0 && asid_of(space_b) != 0 && asid_of(space_a) != asid_of(space_b);

    if mapped {
        unsafe {
            (phys_to_virt(frame_a) as *mut u64).write_volatile(VALUE_A);
            (phys_to_virt(frame_b) as *mut u64).write_volatile(VALUE_B);
        }
        clean_range_to_poc(frame_a, GRANULE);
        clean_range_to_poc(frame_b, GRANULE);
    }

    // The test switches the running PE's own TTBR0. IRQs stay masked: a
    // handler that touched user memory would see a scratch space.
    let saved_daif = crate::interrupts::save_and_disable_irq();
    let previous = current_ttbr0();
    let mut read_a1 = 0_u64;
    let mut read_b = 0_u64;
    let mut read_a2 = 0_u64;
    if mapped {
        switch_address_space(space_a);
        read_a1 = unsafe { (va as *const u64).read_volatile() };
        // Deliberately no flush between these switches.
        switch_address_space(space_b);
        read_b = unsafe { (va as *const u64).read_volatile() };
        switch_address_space(space_a);
        read_a2 = unsafe { (va as *const u64).read_volatile() };
    }
    switch_address_space(previous);
    crate::interrupts::restore_irq(saved_daif);

    unmap_user_pages_at(space_a, va, 1);
    unmap_user_pages_at(space_b, va, 1);
    let released = destroy_task_space(token_a, private_a, private_generation_a, asid_of(space_a))
        & destroy_task_space(token_b, private_b, private_generation_b, asid_of(space_b));
    let freed = frame_allocator::free_page_tagged(frame_a, frame_generation_a)
        & frame_allocator::free_page_tagged(frame_b, frame_generation_b);

    distinct_asids
        && mapped
        && read_a1 == VALUE_A
        && read_b == VALUE_B
        && read_a2 == VALUE_A
        && released
        && freed
}
