use super::*;
use super::table::*;
use super::kernel::*;

// ---------------------------------------------------------------------------
// Per-task user address spaces
// ---------------------------------------------------------------------------

/// Frames a freshly created address space owns: root, user L1/L2, the high
/// L2 for the stack's 1 GiB block, image/heap/stack L3s, and the private
/// L1/L2/L3 trio. Charged to the task's memory ledger as one flat cost;
/// `mmap` adds its own L3 estimate separately.
pub const SPACE_TABLE_FRAMES: usize = 10;
/// One L3 table covers 2 MiB. The ELF loader's window is 8 MiB, so an image
/// may need up to four image L3 tables (the first one is preallocated).
const IMAGE_L3_TABLES: usize = crate::elf::USER_IMAGE_SIZE / BLOCK_2M;

/// Builds a user TTBR0. `user_stack_start == 0` creates no stack mapping
/// (used for the EL1 idle task). A loaded ELF supplies independently owned
/// image pages instead of the old shared kernel text/rodata alias. The heap
/// starts empty: its L3 table exists, but data frames are added by `brk`.
///
/// Returns the root frame (the TTBR0 value) together with its [`TableToken`].
/// The token is the only proof that a later teardown still owns this address
/// space: after the root frame is recycled, the stale token is refused.
pub fn create_task_space(
    private_page: usize,
    user_stack_start: usize,
    image: Option<&crate::elf::LoadedImage>,
    cow: bool,
) -> Option<(usize, TableToken)> {
    if image.is_some_and(|image| {
        image.page_count == 0 || image.page_count > ENTRIES * IMAGE_L3_TABLES
    }) {
        return None;
    }
    let Some(asid) = alloc_asid() else {
        crate::console::write_str("MMU: no free ASID, space creation refused\r\n");
        return None;
    };
    let mut pages = [0_usize; SPACE_TABLE_FRAMES];
    let mut generations = [0_u32; SPACE_TABLE_FRAMES];
    for index in 0..pages.len() {
        let Some((page, generation)) = frame_allocator::alloc_page_tagged() else {
            // Nothing is registered or linked yet: release the raw runs.
            for prior in 0..index {
                frame_allocator::free_page_tagged(pages[prior], generations[prior]);
            }
            release_asid(asid);
            return None;
        };
        pages[index] = page;
        generations[index] = generation;
    }

    // All nine frames are allocated before any is linked. Register them with
    // the root frame as owner; the root itself is registered as its own
    // space root, which is what `retire_space` validates.
    let mut tokens = [TableToken::empty(); SPACE_TABLE_FRAMES];
    for index in 0..pages.len() {
        let Some(token) = page_table_register(pages[index], generations[index], pages[0]) else {
            // The refused frame is still unlinked; prior frames are
            // registered but also still unlinked.
            frame_allocator::free_page_tagged(pages[index], generations[index]);
            for prior in &tokens[..index] {
                free_table_frame(*prior);
            }
            release_asid(asid);
            return None;
        };
        tokens[index] = token;
    }

    // The image is larger than one 2 MiB L3 table. Allocate the remaining
    // tables up front and register them with the same root owner, so the
    // existing `retire_space` teardown walk frees them with the fixed nine.
    let image_groups = image.map_or(0, |image| image.page_count.div_ceil(ENTRIES));
    let extra_count = image_groups.saturating_sub(1);
    let mut extra_frames = [0usize; IMAGE_L3_TABLES];
    let mut extra_tokens = [TableToken::empty(); IMAGE_L3_TABLES];
    for index in 0..extra_count {
        let Some((page, generation)) = frame_allocator::alloc_page_tagged() else {
            for prior in &extra_tokens[..index] {
                let _ = free_table_frame(*prior);
            }
            for token in &tokens {
                let _ = free_table_frame(*token);
            }
            release_asid(asid);
            return None;
        };
        let Some(token) = page_table_register(page, generation, pages[0]) else {
            frame_allocator::free_page_tagged(page, generation);
            for prior in &extra_tokens[..index] {
                let _ = free_table_frame(*prior);
            }
            for token in &tokens {
                let _ = free_table_frame(*token);
            }
            release_asid(asid);
            return None;
        };
        extra_frames[index] = page;
        extra_tokens[index] = token;
    }

    let [l0, l1_user, l2_user, l2_stack_high, l3_image, l3_heap, l3_stack, l1_priv, l2_priv, l3_priv] =
        pages;

    unsafe {
        for &page in &pages {
            zero_page(page);
        }
        for index in 0..extra_count {
            zero_page(extra_frames[index]);
        }

        write_entry(l0, 0, l1_user as u64 | DESC_TABLE);
        write_entry(l0, PRIVATE_WINDOW_L0_INDEX, l1_priv as u64 | DESC_TABLE);
        write_entry(l1_user, 1, l2_user as u64 | DESC_TABLE);
        write_entry(l2_user, 0, l3_image as u64 | DESC_TABLE);
        write_entry(l2_user, USER_HEAP_L2_INDEX, l3_heap as u64 | DESC_TABLE);

        if let Some(image) = image {
            for group in 0..image_groups {
                let table = if group == 0 {
                    l3_image
                } else {
                    let page = extra_frames[group - 1];
                    write_entry(l2_user, group, page as u64 | DESC_TABLE);
                    page
                };
                let start = group * ENTRIES;
                let end = core::cmp::min(start + ENTRIES, image.page_count);
                for index in start..end {
                    let physical = image.physical_start + index * GRANULE;
                    let descriptor = match image.kinds[index] {
                        crate::elf::PageKind::Text => user_text_page(physical),
                        crate::elf::PageKind::ReadOnly | crate::elf::PageKind::Cow => {
                            user_rodata_page(physical)
                        }
                        crate::elf::PageKind::Data => user_data_page(physical),
                    };
                    write_entry(table, index - start, descriptor);
                }
            }
        }

        if user_stack_start != 0 {
            // The stack block lives in its own L2 above the first 1 GiB: the
            // mmap window below stays wide (see USER_STACK_GUARD_VA).
            write_entry(l1_user, USER_STACK_L1_INDEX, l2_stack_high as u64 | DESC_TABLE);
            write_entry(l2_stack_high, USER_STACK_L2_INDEX, l3_stack as u64 | DESC_TABLE);
            // Slot 0 remains zero: that is the guard page below the stack.
            for index in 0..USER_STACK_PAGES {
                let physical = user_stack_start + index * GRANULE;
                // The stack is EL0-visible: never expose a recycled frame that
                // still contains another task's or the kernel's data. A COW
                // child shares the parent's live stack, so it must neither be
                // zeroed nor mapped writable.
                if !cow {
                    zero_page(physical);
                }
                write_entry(
                    l3_stack,
                    USER_STACK_L3_INDEX + index,
                    if cow {
                        user_rodata_page(physical)
                    } else {
                        user_data_page(physical)
                    },
                );
            }
            debug_assert_eq!(
                read_entry(l3_stack, USER_STACK_L3_INDEX - 1),
                0,
                "user stack guard page must stay unmapped"
            );
        }

        // The private 4 KiB window is writable and readable from EL0, so it
        // gets the same guarantee as the stack and heap. In a COW child the
        // frame is shared and lives read-only.
        if !cow {
            zero_page(private_page);
        }
        write_entry(l1_priv, 0, l2_priv as u64 | DESC_TABLE);
        write_entry(l2_priv, 0, l3_priv as u64 | DESC_TABLE);
        write_entry(
            l3_priv,
            0,
            if cow {
                user_rodata_page(private_page)
            } else {
                user_data_page(private_page)
            },
        );
    }

    for page in pages {
        flush_after_table_change(page, GRANULE);
    }
    for index in 0..extra_count {
        flush_after_table_change(extra_frames[index], GRANULE);
    }
    // The space starts life with a unique ASID, so a later context switch can
    // install TTBR0 without flushing every CPU's TLB.
    Some((tagged_ttbr0(l0, asid), tokens[0]))
}

/// Replaces the AP space's empty user-image L3 with one EL1-only 2 MiB
/// identity block covering `[DRAM_BASE, DRAM_BASE + 2 MiB)`. QEMU-only
/// (`smp` feature).
#[cfg(feature = "smp")]
///
/// PSCI starts an AP with the MMU off at its physical entry address. The AP
/// trampoline must keep fetching at that low address until it has enabled the
/// MMU, so its page has to be mapped in the AP's TTBR0. The block is EL1-only
/// (`AP_EL1_RW`, `UXN`) and the AP space is kernel-only (`image == None`), so
/// this does not expose the kernel image to EL0. The replaced L3 table stays
/// registered and is reclaimed with the space.
pub fn install_ap_identity(ttbr0: usize) -> bool {
    let root = root_of(ttbr0);
    if !space_root_valid(root) {
        return false;
    }
    unsafe {
        let root_entry = read_entry(root, 0);
        let Some(l1_user) = follow_validated_table(root_entry, root) else {
            return false;
        };
        let l2_entry = read_entry(l1_user, 1);
        let Some(l2_user) = follow_validated_table(l2_entry, root) else {
            return false;
        };
        write_entry(
            l2_user,
            0,
            block_descriptor(DRAM_BASE, ATTR_NORMAL, SH_INNER),
        );
        flush_after_table_change(l2_user, GRANULE);
    }
    true
}

/// Maps a run of zeroed, writable and non-executable heap pages starting at
/// heap index `first_index`. `physical_pages[i]` backs index `first_index + i`.
///
/// All-or-nothing: every index is bounds-checked and every target slot is
/// proven free before the first page is zeroed or published, so a rejected
/// batch leaves the address space unchanged and the caller owns all frames.
/// The whole run shares one barrier and one broadcast invalidation instead of
/// paying a full table clean plus `dsb`/`tlbi` per page — `brk` can grow the
/// heap by hundreds of pages at a time.
pub fn map_user_heap_pages(ttbr0: usize, first_index: usize, physical_pages: &[usize]) -> bool {
    map_user_heap_pages_inner(ttbr0, first_index, physical_pages, false)
}

/// Maps already-populated heap pages for a forked child as read-only COW.
pub fn map_shared_user_heap_pages(
    ttbr0: usize,
    first_index: usize,
    physical_pages: &[usize],
) -> bool {
    map_user_heap_pages_inner(ttbr0, first_index, physical_pages, true)
}

pub(super) fn map_user_heap_pages_inner(
    ttbr0: usize,
    first_index: usize,
    physical_pages: &[usize],
    cow: bool,
) -> bool {
    let count = physical_pages.len();
    let root = root_of(ttbr0);
    let Some(last) = first_index.checked_add(count) else {
        return false;
    };
    if count == 0
        || last > USER_HEAP_MAX_PAGES
        || physical_pages.iter().any(|physical| physical % GRANULE != 0)
    {
        return false;
    }
    unsafe {
        let l1_user = follow_table(root, 0);
        let l2_user = follow_table(l1_user, 1);
        // F2: the heap L3 is created with the space and is never freed by a
        // generic unmap, but never trust the descriptor kind. A zero or a
        // block descriptor here must fail, not be dereferenced as a table
        // (physical address 0 or an unrelated frame).
        let l2_entry = read_entry(l2_user, USER_HEAP_L2_INDEX);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3_heap = descriptor_address(l2_entry) as usize;
        debug_assert!(l3_heap != 0 && l3_heap % GRANULE == 0);
        for index in first_index..last {
            if read_entry(l3_heap, index) != 0 {
                return false;
            }
        }
        for (offset, &physical) in physical_pages.iter().enumerate() {
            let index = first_index + offset;
            if cow {
                write_entry(l3_heap, index, user_rodata_page(physical));
            } else {
                zero_page(physical);
                write_entry(l3_heap, index, user_data_page(physical));
            }
            clean_entry_line(l3_heap, index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Removes a heap mapping and returns its physical frame to the caller.
pub fn unmap_user_heap_page(ttbr0: usize, index: usize) -> Option<usize> {
    if index >= USER_HEAP_MAX_PAGES {
        return None;
    }
    let root = root_of(ttbr0);
    let physical;
    unsafe {
        let l1_user = follow_table(root, 0);
        let l2_user = follow_table(l1_user, 1);
        let l2_entry = read_entry(l2_user, USER_HEAP_L2_INDEX);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return None;
        }
        let l3_heap = descriptor_address(l2_entry) as usize;
        if l3_heap == 0 {
            return None;
        }
        let entry = read_entry(l3_heap, index);
        if entry & DESC_PAGE != DESC_PAGE {
            return None;
        }
        write_entry(l3_heap, index, 0);
        clean_entry_line(l3_heap, index);
        physical = descriptor_address(entry) as usize;
    }
    finish_table_changes_for(ttbr0);
    Some(physical)
}

/// Maps `page_count` anonymous pages at `virtual_address` into the current
/// task's TTBR0, allocating intermediate L3 tables on demand. The physical
/// frames must already be allocated contiguously at `physical_start`; each page
/// is described with the `prot`-derived permissions (read always, write/execute
/// per the flags).
///
/// Every data frame is cleared **before** its descriptor is published, so a
/// recycled frame can never leak another task's or the kernel's contents to
/// EL0. Page-table frames are zeroed separately when allocated.
///
/// All-or-nothing contract: on any failure every partially installed mapping
/// is unrolled (including page-table frames allocated for this call) before
/// `false` is returned, so the caller's data-frame release is safe and the
/// address space is left unchanged.
///
/// Maps an anonymous run, zeroing every frame before publishing it.
pub fn map_user_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    map_user_pages_at_inner(ttbr0, virtual_address, physical_start, page_count, prot, true, false)
}

/// Maps frames that already contain live data (a forked child's shared COW
/// pages): no zeroing, and writable `prot` becomes read-only so the first
/// write traps into the COW handler.
pub fn map_shared_user_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    map_user_pages_at_inner(ttbr0, virtual_address, physical_start, page_count, prot, false, true)
}

/// Maps frames that already contain live data and are owned by another
/// object (a memfd's shared run). No zeroing, and writable `prot` stays
/// writable: this is a true `MAP_SHARED` mapping whose writes must be visible
/// to every other mapping of the same run.
pub fn map_shared_writable_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    map_user_pages_at_inner(ttbr0, virtual_address, physical_start, page_count, prot, false, false)
}

/// Maps one frame that already holds live data at `virtual_address`.
///
/// File-backed segments allocate a frame per page (a large library cannot be
/// satisfied by one contiguous run), so they cannot use
/// [`map_user_pages_at`]. The frame is not cleared: the caller filled it from
/// the file before publishing the descriptor.
pub fn map_user_page_at(
    ttbr0: usize,
    virtual_address: usize,
    physical: usize,
    prot: u64,
) -> bool {
    map_user_pages_at_inner(ttbr0, virtual_address, physical, 1, prot, false, false)
}

/// Shared implementation of the two entry points above.
pub(super) fn map_user_pages_at_inner(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
    zero: bool,
    cow: bool,
) -> bool {
    if page_count == 0 || virtual_address % GRANULE != 0 || physical_start % GRANULE != 0 {
        return false;
    }
    // Defense in depth for W^X: reject before any table mutation even if a
    // future caller forgets the scheduler-level validation.
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return false;
    }
    // checked_mul: `page_count` is bounded by the callers, but this function
    // must not depend on that for overflow safety.
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < USER_MMAP_VA || end > USER_MMAP_MAX_VA {
        return false;
    }

    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        // All-or-nothing: `done` counts fully installed pages; every failure
        // path unrolls them before returning. The rollback cannot refuse:
        // each of the `done` slots was written by this call, so its L2 link
        // and L3 descriptor are present (F1-created tables included).
        let mut done = 0usize;
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l1_entry = read_entry(l1_user, l1_index);
            let l2_user = if l1_entry == 0 {
                let Some(table) = alloc_registered_table(root) else {
                    if done != 0 { unmap_user_pages_at(ttbr0, virtual_address, done); }
                    finish_table_changes_for(ttbr0);
                    return false;
                };
                write_entry(l1_user, l1_index, table as u64 | DESC_TABLE);
                clean_entry_line(l1_user, l1_index);
                table
            } else if l1_entry & DESC_TABLE == DESC_TABLE {
                descriptor_address(l1_entry) as usize
            } else {
                if done != 0 { unmap_user_pages_at(ttbr0, virtual_address, done); }
                finish_table_changes_for(ttbr0);
                return false;
            };
            let l2_entry = read_entry(l2_user, l2_index);
            let l3 = if l2_entry == 0 {
                let Some((page, generation)) = frame_allocator::alloc_page_tagged() else {
                    // F1: no frame left for a new L3 table.
                    mmu_fail_record(va, 1, done as u64);
                    unmap_user_pages_at(ttbr0, virtual_address, done);
                    finish_table_changes_for(ttbr0);
                    return false;
                };
                zero_page(page);
                if page_table_register(page, generation, root).is_none() {
                    // Registration refused: the L2 entry below is only written
                    // after a successful registration, so the frame is still
                    // unlinked and its own tag is correct.
                    mmu_fail_record(va, 2, done as u64);
                    frame_allocator::free_page_tagged(page, generation);
                    unmap_user_pages_at(ttbr0, virtual_address, done);
                    finish_table_changes_for(ttbr0);
                    return false;
                }
                // Registered before the first link; the rollback paths below
                // free it through `unmap_user_pages_at`, which retires it as a
                // table owned by this space. The barrier is deferred: the
                // single `finish_table_changes` at the end (or before an error
                // return) publishes every line this call cleaned.
                write_entry(l2_user, l2_index, page as u64 | DESC_TABLE);
                clean_entry_line(l2_user, l2_index);
                page
            } else if l2_entry & DESC_TABLE == DESC_TABLE {
                descriptor_address(l2_entry) as usize
            } else {
                // F2: unexpected descriptor kind in the user L2.
                mmu_fail_record(va, 3, l2_entry);
                unmap_user_pages_at(ttbr0, virtual_address, done);
                finish_table_changes_for(ttbr0);
                return false;
            };
            if read_entry(l3, l3_index) != 0 {
                // F3: target slot already mapped.
                mmu_fail_record(va, 4, read_entry(l3, l3_index));
                unmap_user_pages_at(ttbr0, virtual_address, done);
                finish_table_changes_for(ttbr0);
                return false;
            }
            // Zero before publication: the frame is recycled allocator memory
            // and becomes EL0-readable the moment the descriptor is written.
            // A concurrent walk (or the returning task itself) must never be
            // able to observe leftover data, so the PTE is installed only
            // after the whole granule has been cleared. Shared COW pages
            // already hold live content and must not be cleared.
            if zero {
                zero_page(physical_start + index * GRANULE);
            }
            let page_prot = if cow { prot & !PROT_WRITE } else { prot };
            write_entry(
                l3,
                l3_index,
                user_anonymous_page(physical_start + index * GRANULE, page_prot),
            );
            clean_entry_line(l3, l3_index);
            done += 1;
        }
    }
    // One barrier + broadcast for the whole range. The caller runs under the
    // kernel lock and the target space is not executing on another CPU, so no
    // translation can be observed between the individual entry writes.
    finish_table_changes_for(ttbr0);
    true
}

/// Re-points one already-mapped user page at a different frame, keeping its
/// permission bits. Returns `false` (and changes nothing) when the slot is not
/// a live 4 KiB page mapping inside the anonymous window.
///
/// The edit is published exactly like [`map_user_pages_at`]: clean the
/// descriptor line, then one `dsb ishst` + broadcast `tlbi vmalle1is` +
/// `dsb ish`. Unlike a production map this deliberately runs **while another
/// PE may be executing the address space**; the caller must own the
/// coordination that keeps the peer from touching `virtual_address` until
/// [`replace_user_page`] returns. [`crate::smp`]'s cross-PE TLB self-test is
/// the only user and relies on that contract to prove the invalidation really
/// crosses the inner-shareable domain.
#[cfg(feature = "smp-selftest")]
pub fn replace_user_page(
    ttbr0: usize,
    virtual_address: usize,
    physical: usize,
    prot: u64,
) -> bool {
    if virtual_address % GRANULE != 0 || physical % GRANULE != 0 {
        return false;
    }
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return false;
    }
    let Some(end) = virtual_address.checked_add(GRANULE) else {
        return false;
    };
    if virtual_address < USER_MMAP_VA || end > USER_MMAP_MAX_VA {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        let l1_entry = read_entry(l1_user, (virtual_address >> 30) & (ENTRIES - 1));
        if l1_entry & DESC_TABLE != DESC_TABLE { return false; }
        let l2_user = descriptor_address(l1_entry) as usize;
        let l2_index = (virtual_address >> 21) & (ENTRIES - 1);
        let l3_index = (virtual_address >> 12) & 0x1FF;
        let l2_entry = read_entry(l2_user, l2_index);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
            return false;
        }
        write_entry(l3, l3_index, user_anonymous_page(physical, prot));
        clean_entry_line(l3, l3_index);
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Changes the permission bits of an already-mapped range inside the
/// anonymous window, leaving every physical page in place. Used by
/// `mprotect(2)`, in particular for RELRO pages that move from RW to RO after
/// the dynamic loader has applied relocations.
///
/// All-or-nothing: the whole range is checked for a live 4 KiB mapping before
/// the first descriptor changes, so a hole does not leave a half-applied
/// protection. W^X is enforced here as well as in [`map_user_pages_at`].
pub fn protect_user_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    if page_count == 0 || virtual_address % GRANULE != 0 {
        return false;
    }
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    // The image lives below the anonymous window (MAIN_BIAS/INTERP_BIAS);
    // `mprotect` must work there too, not only on `mmap` regions. The stack
    // sits above the guard page, so the upper bound is the stack end.
    if virtual_address < GRANULE || end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }

    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        // Walk L1/L2/L3 generically: the image at 0x1000_0000/0x2000_0000 is
        // below `DRAM_BASE`, so the old `(va - DRAM_BASE) / BLOCK_2M` indexing
        // used by the mmap window cannot be reused here.
        // Validate every target slot first: a refusal must not leave the
        // range half-protected.
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                return false;
            }
        }
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2 = descriptor_address(read_entry(l1_user, l1_index)) as usize;
            let l3 = descriptor_address(read_entry(l2, l2_index)) as usize;
            let physical = descriptor_address(read_entry(l3, l3_index)) as usize;
            write_entry(l3, l3_index, user_anonymous_page(physical, prot));
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Like [`protect_user_pages_at`], but pages that are not backed yet are
/// skipped instead of rejected. `mprotect` on a lazily reserved range covers
/// pages that will only get a frame on first access.
pub fn protect_user_pages_at_lenient(
    ttbr0: usize,
    virtual_address: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    if page_count == 0 || virtual_address % GRANULE != 0 {
        return false;
    }
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < GRANULE || end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }

    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                continue;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                continue;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            let entry = read_entry(l3, l3_index);
            if entry & DESC_PAGE != DESC_PAGE {
                continue;
            }
            let physical = descriptor_address(entry) as usize;
            write_entry(l3, l3_index, user_anonymous_page(physical, prot));
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Makes every `Cow` page in a shared image read-only in `ttbr0`. Text and
/// read-only pages are already immutable.
#[cfg(feature = "linux-abi")]
pub fn protect_cow_image(
    ttbr0: usize,
    image: &crate::linux_elf::LinuxImage,
) -> bool {
    for run in &image.runs {
        for (index, kind) in run.kinds.iter().enumerate() {
            if *kind == crate::elf::PageKind::Cow
                && !protect_user_pages_at(
                    ttbr0,
                    run.virtual_address + index * GRANULE,
                    1,
                    PROT_READ,
                )
            {
                return false;
            }
        }
    }
    true
}

/// Makes the private 4 KiB window read-only. `fork` uses it after sharing the
/// private page with a child.
pub fn protect_private_page(ttbr0: usize) -> bool {
    let root = root_of(ttbr0);
    unsafe {
        let l0_entry = read_entry(root, PRIVATE_WINDOW_L0_INDEX);
        if l0_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l1 = descriptor_address(l0_entry) as usize;
        let l1_entry = read_entry(l1, 0);
        if l1_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l2 = descriptor_address(l1_entry) as usize;
        let l2_entry = read_entry(l2, 0);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        let entry = read_entry(l3, 0);
        if entry & DESC_PAGE != DESC_PAGE {
            return false;
        }
        let physical = descriptor_address(entry) as usize;
        write_entry(l3, 0, user_rodata_page(physical));
        clean_entry_line(l3, 0);
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Re-points the private 4 KiB window at a fresh frame with RW permissions.
pub fn remap_private_page(ttbr0: usize, physical: usize) -> bool {
    if physical % GRANULE != 0 {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l0_entry = read_entry(root, PRIVATE_WINDOW_L0_INDEX);
        if l0_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l1 = descriptor_address(l0_entry) as usize;
        let l1_entry = read_entry(l1, 0);
        if l1_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l2 = descriptor_address(l1_entry) as usize;
        let l2_entry = read_entry(l2, 0);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        if read_entry(l3, 0) & DESC_PAGE != DESC_PAGE {
            return false;
        }
        write_entry(l3, 0, user_data_page(physical));
        clean_entry_line(l3, 0);
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Re-points an existing user range at a new physical run with one uniform
/// protection. Used by the COW fault path after it copied a stack/heap/mmap
/// run. All target slots must already be live pages.
pub fn remap_user_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    if page_count == 0
        || virtual_address % GRANULE != 0
        || physical_start % GRANULE != 0
        || (prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0)
    {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < GRANULE || end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                return false;
            }
        }
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2 = descriptor_address(read_entry(l1_user, l1_index)) as usize;
            let l3 = descriptor_address(read_entry(l2, l2_index)) as usize;
            write_entry(
                l3,
                l3_index,
                user_anonymous_page(physical_start + index * GRANULE, prot),
            );
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Re-points an existing image range at a new physical run, preserving each
/// page's permission class. The COW path uses this after copying a whole
/// image run and promoting `Cow` pages back to `Data`.
pub fn remap_user_kinds_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    kinds: &[crate::elf::PageKind],
) -> bool {
    if kinds.is_empty() || virtual_address % GRANULE != 0 || physical_start % GRANULE != 0 {
        return false;
    }
    let Some(span) = kinds.len().checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < GRANULE || end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        for index in 0..kinds.len() {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                return false;
            }
        }
        for (index, kind) in kinds.iter().enumerate() {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2 = descriptor_address(read_entry(l1_user, l1_index)) as usize;
            let l3 = descriptor_address(read_entry(l2, l2_index)) as usize;
            let physical = physical_start + index * GRANULE;
            let descriptor = match kind {
                crate::elf::PageKind::Text => user_text_page(physical),
                crate::elf::PageKind::ReadOnly | crate::elf::PageKind::Cow => {
                    user_rodata_page(physical)
                }
                crate::elf::PageKind::Data => user_data_page(physical),
            };
            write_entry(l3, l3_index, descriptor);
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Removes `page_count` pages starting at `virtual_address`, freeing an L3 table
/// once its last entry is gone. The physical frames are returned to the caller
/// by address (tracked separately by the scheduler), so this only edits tables.
///
/// All-or-nothing: the whole range is validated before the first entry is
/// cleared. A range with one unmapped hole returns `false` with every mapping
/// still in place, so a caller that reacts by freeing the backing run cannot
/// leave a live PTE pointing at a recycled frame.
///
/// `true` means **the range is no longer mapped**, not that every page-table
/// frame used by the range was returned to the allocator. An L3 is released
/// only when it became empty and its registry owner still matches; a refused
/// table free leaks that table rather than risking a foreign frame. Callers
/// must therefore treat `true` as permission to release the data frames, not
/// as a page-table accounting result.
/// Like [`unmap_user_pages_at`], but pages that were never backed are
/// skipped. A lazily reserved range has holes by design, and `munmap` or a
/// `MAP_FIXED` replacement must be able to cover them.
pub fn unmap_user_pages_at_lenient(
    ttbr0: usize,
    virtual_address: usize,
    page_count: usize,
) -> bool {
    if page_count == 0 || virtual_address % GRANULE != 0 || virtual_address < DRAM_BASE {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l1_entry = read_entry(l1_user, l1_index);
            if l1_entry & DESC_TABLE != DESC_TABLE { continue; }
            let l2_user = descriptor_address(l1_entry) as usize;
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l2_user, l2_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                continue;
            }
            let l3 = descriptor_address(l2_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                continue;
            }
            write_entry(l3, l3_index, 0);
            clean_entry_line(l3, l3_index);
            let persistent = (l1_index == 1 && l2_index == USER_HEAP_L2_INDEX)
                || (l1_index == USER_STACK_L1_INDEX && l2_index == USER_STACK_L2_INDEX);
            if !persistent && table_empty(l3) {
                write_entry(l2_user, l2_index, 0);
                clean_entry_line(l2_user, l2_index);
                finish_table_changes_for(ttbr0);
                free_table_frame_owned(l3, root);
            }
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

pub fn unmap_user_pages_at(ttbr0: usize, virtual_address: usize, page_count: usize) -> bool {
    if page_count == 0 || virtual_address % GRANULE != 0 || virtual_address < DRAM_BASE {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);

        // Pre-scan: no mutation until every page in the range is proven to be
        // a live 4 KiB mapping in this address space.
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_entry = read_entry(l1_user, (va >> 30) & (ENTRIES - 1));
            if l1_entry & DESC_TABLE != DESC_TABLE { return false; }
            let l2_user = descriptor_address(l1_entry) as usize;
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l2_user, l2_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l3 = descriptor_address(l2_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                return false;
            }
        }

        // Apply. State cannot change between the passes: every caller runs
        // with the kernel lock held and the task is not executing.
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_user = descriptor_address(read_entry(l1_user, l1_index)) as usize;
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l3 = descriptor_address(read_entry(l2_user, l2_index)) as usize;
            write_entry(l3, l3_index, 0);
            clean_entry_line(l3, l3_index);
            // The heap and stack L3 tables are persistent: `brk` and the
            // fixed stack window reuse them after an unmap empties the last
            // page. Freeing them would leave map/unmap_user_heap_page
            // following a now-zero L2 descriptor into physical address 0.
            let persistent = (l1_index == 1 && l2_index == USER_HEAP_L2_INDEX)
                || (l1_index == USER_STACK_L1_INDEX && l2_index == USER_STACK_L2_INDEX);
            if !persistent && table_empty(l3) {
                write_entry(l2_user, l2_index, 0);
                clean_entry_line(l2_user, l2_index);
                // Publish the removal before the frame can be handed to a new
                // owner: a walker on another PE must never read a recycled
                // table. Tables empty rarely, so a flush per retired table is
                // cheaper than tracking them across the loop.
                finish_table_changes_for(ttbr0);
                free_table_frame_owned(l3, root);
            }
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Maps one Linux image run into `ttbr0` at its own virtual address.
///
/// This is the spawn-time counterpart of [`map_user_pages_at`]: it accepts any
/// user virtual address below the fixed stack window, allocates the per-2 MiB
/// L3 tables (and a per-1 GiB L2 table) on demand, and registers them with the
/// space root so the normal teardown frees them. It does **not** zero the
/// pages: the Linux loader fills and zeroes the whole run before the space
/// exists, and zeroing here would erase the code it just copied.
///
/// Runs under the kernel lock, for a space that is not executing. On failure
/// the pages installed by this call are unmapped again; tables allocated by it
/// stay registered with the space and are reclaimed if the space is destroyed.
#[cfg(feature = "linux-abi")]
pub fn install_linux_run(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    kinds: &[crate::elf::PageKind],
) -> bool {
    if page_count == 0
        || kinds.len() != page_count
        || virtual_address % GRANULE != 0
        || physical_start % GRANULE != 0
    {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < GRANULE || end > USER_STACK_GUARD_VA {
        return false;
    }
    let root = root_of(ttbr0);
    if !space_root_valid(root) {
        return false;
    }

    unsafe {
        let l1_user = follow_table(root, 0);
        let mut installed = 0usize;
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & (ENTRIES - 1);

            // The L2 table for this 1 GiB span, created on first use.
            let l2_entry = read_entry(l1_user, l1_index);
            let l2 = if l2_entry == 0 {
                let Some(table) = alloc_registered_table(root) else {
                    rollback_linux_run(ttbr0, virtual_address, installed);
                    return false;
                };
                write_entry(l1_user, l1_index, table as u64 | DESC_TABLE);
                clean_entry_line(l1_user, l1_index);
                table
            } else if l2_entry & DESC_TABLE == DESC_TABLE {
                descriptor_address(l2_entry) as usize
            } else {
                rollback_linux_run(ttbr0, virtual_address, installed);
                return false;
            };

            // The L3 table for this 2 MiB block, created on first use.
            let l3_entry = read_entry(l2, l2_index);
            let l3 = if l3_entry == 0 {
                let Some(table) = alloc_registered_table(root) else {
                    rollback_linux_run(ttbr0, virtual_address, installed);
                    return false;
                };
                write_entry(l2, l2_index, table as u64 | DESC_TABLE);
                clean_entry_line(l2, l2_index);
                table
            } else if l3_entry & DESC_TABLE == DESC_TABLE {
                descriptor_address(l3_entry) as usize
            } else {
                rollback_linux_run(ttbr0, virtual_address, installed);
                return false;
            };

            if read_entry(l3, l3_index) != 0 {
                rollback_linux_run(ttbr0, virtual_address, installed);
                return false;
            }
            let physical = physical_start + index * GRANULE;
            // The software page watch (debug::arm_soft_log) needs the watched
            // data page mapped read-only at exec time, so its first write
            // faults into the logging path instead of passing silently.
            #[cfg(feature = "gdb-stub")]
            let watched = crate::debug::soft_page_is(va as u64)
                && kinds[index] == crate::elf::PageKind::Data;
            #[cfg(not(feature = "gdb-stub"))]
            let watched = false;
            if watched {
                crate::console::write_str("WATCH-EXEC protected va=");
                crate::console::write_hex_u64(va as u64);
                crate::console::write_str("\r\n");
            }
            let descriptor = match kinds[index] {
                crate::elf::PageKind::Text => user_text_page(physical),
                crate::elf::PageKind::ReadOnly | crate::elf::PageKind::Cow => {
                    user_rodata_page(physical)
                }
                crate::elf::PageKind::Data if watched => user_rodata_page(physical),
                crate::elf::PageKind::Data => user_data_page(physical),
            };
            write_entry(l3, l3_index, descriptor);
            clean_entry_line(l3, l3_index);
            installed += 1;
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Maps every run of a foreign image. See [`install_linux_run`].
#[cfg(feature = "linux-abi")]
pub fn install_linux_image(ttbr0: usize, image: &crate::linux_elf::LinuxImage) -> bool {
    for run in &image.runs {
        if !install_linux_run(
            ttbr0,
            run.virtual_address,
            run.physical_start,
            run.page_count,
            &run.kinds,
        ) {
            return false;
        }
    }
    true
}

#[cfg(feature = "linux-abi")]
pub(super) fn rollback_linux_run(ttbr0: usize, virtual_address: usize, installed: usize) {
    unsafe {
        let root = root_of(ttbr0);
        let l1_user = follow_table(root, 0);
        for index in 0..installed {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & (ENTRIES - 1);
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                continue;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                continue;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            write_entry(l3, l3_index, 0);
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
}

/// Read-only quiescence check: does `ttbr0` currently map any page whose
/// physical address lies in `[phys_base, phys_end)`?
///
/// `true` is conservative: a malformed or foreign table also returns `true`,
/// because the caller must not revoke/free a range it cannot prove absent
/// from a running CPU's address space. A missing level or an empty L3 means
/// the range is not mapped and returns `false`.
pub fn range_mapped_by(ttbr0: usize, phys_base: usize, phys_end: usize) -> bool {
    if !page_range_within(phys_base, phys_end, GRANULE, DRAM_BASE, DRAM_END)
        || !space_root_valid(ttbr0)
    {
        return true;
    }
    let root = root_of(ttbr0);
    unsafe {
        let root_entry = read_entry(root, 0);
        if root_entry == 0 {
            return false;
        }
        let Some(l1_user) = follow_validated_table(root_entry, root) else {
            return true;
        };
        let l1_entry = read_entry(l1_user, USER_MMIO_L1_INDEX);
        if l1_entry == 0 {
            return false;
        }
        let Some(l2_mmio) = follow_validated_table(l1_entry, root) else {
            return true;
        };
        for group in 0..USER_MMIO_L2_GROUPS {
            let l2_entry = read_entry(l2_mmio, USER_MMIO_L2_INDEX + group);
            if l2_entry == 0 {
                continue;
            }
            let Some(l3_mmio) = follow_validated_table(l2_entry, root) else {
                return true;
            };
            for index in 0..ENTRIES {
                let entry = read_entry(l3_mmio, index);
                if entry == 0 {
                    continue;
                }
                if entry & DESC_PAGE != DESC_PAGE {
                    return true;
                }
                let physical = descriptor_address(entry) as usize;
                if physical >= phys_base && physical < phys_end {
                    return true;
                }
            }
        }
    }
    false
}

/// Revokes every page mapping whose physical address lies in the half-open
/// range `[phys_base, phys_end)` from the MMIO window of one address space.
///
/// Read-only until a matching mapping is found: the root must be a registered
/// space root and every intermediate level is validated before use (the
/// descriptor must be a table **and** the table's physical address must be a
/// page-aligned, currently allocated frame registered to **this** space; a
/// zero entry means the level is absent and the task is trivially clean) and
/// no table is ever allocated. Only valid page descriptors (`DESC_PAGE`)
/// whose physical address falls inside the range are removed; anything
/// non-empty but not a valid page, or a table owned by another space, makes
/// the whole window unverifiable (`safe_to_free = false`). Foreign mappings
/// outside the range are kept.
///
/// The range must lie inside `[DRAM_BASE, DRAM_END)`; addresses outside DRAM
/// are refused before any walk. `safe_to_free` covers CPU mappings only — see
/// [`RevokeResult::safe_to_free`] and the device-quiescence precondition on
/// [`map_dma_to_user`].
///
/// Never allocates and never frees physical frames. Clearing a descriptor is
/// followed by table cache maintenance and a full TLB invalidation before
/// the caller may free the data frames.
pub fn revoke_phys_range(ttbr0: usize, phys_base: usize, phys_end: usize) -> RevokeResult {
    if !page_range_within(phys_base, phys_end, GRANULE, DRAM_BASE, DRAM_END) {
        return RevokeResult {
            unmapped: 0,
            safe_to_free: false,
        };
    }
    if phys_base == phys_end {
        return RevokeResult {
            unmapped: 0,
            safe_to_free: true,
        };
    }

    let root = root_of(ttbr0);
    unsafe {
        if !space_root_valid(ttbr0) {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        }
        let root_entry = read_entry(root, 0);
        if root_entry == 0 {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: true,
            };
        }
        let Some(l1_user) = follow_validated_table(root_entry, root) else {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        };
        let l1_entry = read_entry(l1_user, USER_MMIO_L1_INDEX);
        if l1_entry == 0 {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: true,
            };
        }
        let Some(l2_mmio) = follow_validated_table(l1_entry, root) else {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        };

        let mut unmapped = 0usize;
        let mut changed = false;
        for group in 0..USER_MMIO_L2_GROUPS {
            let l2_entry = read_entry(l2_mmio, USER_MMIO_L2_INDEX + group);
            if l2_entry == 0 {
                continue;
            }
            let Some(l3_mmio) = follow_validated_table(l2_entry, root) else {
                if changed {
                    finish_table_changes_for(ttbr0);
                }
                return RevokeResult {
                    unmapped,
                    safe_to_free: false,
                };
            };
            for index in 0..ENTRIES {
                let entry = read_entry(l3_mmio, index);
                if entry == 0 {
                    continue;
                }
                if entry & DESC_PAGE != DESC_PAGE {
                    if changed {
                        finish_table_changes_for(ttbr0);
                    }
                    return RevokeResult {
                        unmapped,
                        safe_to_free: false,
                    };
                }
                let physical = descriptor_address(entry) as usize;
                if physical >= phys_base && physical < phys_end {
                    write_entry(l3_mmio, index, 0);
                    clean_entry_line(l3_mmio, index);
                    unmapped += 1;
                    changed = true;
                }
            }
        }
        if changed {
            finish_table_changes_for(ttbr0);
        }
        RevokeResult {
            unmapped,
            safe_to_free: true,
        }
    }
}

/// Maps an MMIO range into a free run in the task's 2 MB device window.
/// Physical DRAM is rejected so this interface cannot expose kernel memory.
pub fn map_phys_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let physical_end = phys_addr.checked_add(size)?;
    if physical_end > PHYSICAL_ADDRESS_LIMIT || (phys_addr < DRAM_END && physical_end > DRAM_BASE) {
        return None;
    }

    let (physical_start, physical_offset, page_count) =
        aligned_page_span(phys_addr, size, GRANULE)?;
    // Device windows share the same multi-group VA window as DMA mappings: a
    // BAR (or an ivshmem surface) can be larger than one 2 MiB group.
    if page_count > ENTRIES * USER_MMIO_L2_GROUPS {
        return None;
    }
    map_user_pages(
        task.ttbr0,
        physical_start,
        page_count,
        physical_offset,
        user_device_page,
    )
}

/// Maps a device-window range as Normal Non-Cacheable user memory.
///
/// The ivshmem BAR is RAM that the guest shares with the host, not a device
/// register bank: Device attributes would forbid the unaligned accesses and
/// write combining a framebuffer needs. The range must lie outside DRAM, as
/// for [`map_phys_to_user`].
pub fn map_phys_nc_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let physical_end = phys_addr.checked_add(size)?;
    if physical_end > PHYSICAL_ADDRESS_LIMIT || (phys_addr < DRAM_END && physical_end > DRAM_BASE) {
        return None;
    }
    let (physical_start, physical_offset, page_count) =
        aligned_page_span(phys_addr, size, GRANULE)?;
    if page_count > ENTRIES * USER_MMIO_L2_GROUPS {
        return None;
    }
    map_user_pages(
        task.ttbr0,
        physical_start,
        page_count,
        physical_offset,
        user_dma_page,
    )
}

/// Maps newly allocated DMA frames as Normal Non-Cacheable, never as Device or
/// cacheable RAM. The range must be page-aligned and wholly inside known DRAM.
///
/// Cache protocol: the allocator/owner initialises the buffer through the
/// cacheable kernel alias, so this function cleans the range to the point of
/// coherency *before* the user (and therefore the device) can observe it.
/// Afterwards the buffer must be touched only through Normal Non-Cacheable
/// aliases; if any cacheable alias is written again, the owner must clean it
/// before the next ownership handover.
///
/// Device-quiescence precondition: freeing a DMA range requires that the
/// device can no longer issue DMA to it (queue stopped or device reset, with
/// completion observed). `revoke_phys_range` proves only that CPU mappings are
/// gone; without an IOMMU it cannot stop a device that is still running. An
/// in-kernel driver must hold a [`crate::scheduler::DmaLease`] across the armed
/// window: the revoke path leaks any range with a live lease instead of
/// reusing it.
pub fn map_dma_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    map_dma_to_user_space(task.ttbr0, phys_addr, size)
}

/// [`map_dma_to_user`] for a raw address space. Exposed so the boot self-test
/// exercises the same zero -> clean -> publish sequence without constructing a
/// scheduler task.
pub fn map_dma_to_user_space(ttbr0: usize, phys_addr: usize, size: usize) -> Option<usize> {
    if phys_addr % GRANULE != 0 || size % GRANULE != 0 {
        return None;
    }
    checked_range_end(phys_addr, size, DRAM_BASE, DRAM_END)?;
    let page_count = size / GRANULE;
    if page_count > ENTRIES * USER_MMIO_L2_GROUPS {
        return None;
    }

    // Every DMA frame is recycled allocator memory. Clear it through the
    // cacheable kernel alias before it becomes EL0/device-visible, then push
    // those lines to the point of coherency. Clean is not a substitute for
    // zeroing: stale bytes must never cross a task boundary.
    unsafe {
        core::ptr::write_bytes(phys_to_virt(phys_addr) as *mut u8, 0, size);
    }
    clean_dma_range(phys_addr, size);
    map_user_pages(ttbr0, phys_addr, page_count, 0, user_dma_page)
}

/// Maps a registered DMA allocation shared by another task. The scheduler
/// validates that the physical range lies inside some task's `dma_pages` run
/// before calling this function.
///
/// Cache protocol: this path assumes the buffer was already published by
/// [`map_dma_to_user`] (which cleaned the initialising writes) and that every
/// participant accesses it exclusively through its Normal Non-Cacheable
/// alias. No clean is needed while that holds; if a cacheable alias is written
/// again, the writer must clean/invalidate around the ownership handover.
///
/// The physical address is a bearer capability: any task with `CAP_MMIO` that
/// learns it can map the window. Grant checks belong to the IPC protocol that
/// distributes the address, not to this function.
///
/// Device-quiescence precondition: same as [`map_dma_to_user`] — a CPU
/// mapping being gone does not mean the device has stopped.
/// [`map_shared_dma_to_user`] for surfaces the CPU writes and the device
/// reads. The mapping is Normal Cacheable so ordinary libc code (including
/// `dc zva`) works; every transfer cleans the range first.
pub fn map_shared_normal_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    checked_range_end(phys_addr, size, DRAM_BASE, DRAM_END)?;
    let (physical_start, physical_offset, page_count) =
        aligned_page_span(phys_addr, size, GRANULE)?;
    if page_count > ENTRIES * USER_MMIO_L2_GROUPS {
        return None;
    }
    map_user_pages(
        task.ttbr0,
        physical_start,
        page_count,
        physical_offset,
        user_shared_normal_page,
    )
}

pub fn map_shared_dma_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    checked_range_end(phys_addr, size, DRAM_BASE, DRAM_END)?;
    let (physical_start, physical_offset, page_count) =
        aligned_page_span(phys_addr, size, GRANULE)?;
    // Shared mappings use the same multi-group window as owner mappings. The
    // earlier one-group bound silently rejected every full-size display
    // surface (710 pages), so cross-task sharing could never work.
    if page_count > ENTRIES * USER_MMIO_L2_GROUPS {
        return None;
    }
    map_user_pages(
        task.ttbr0,
        physical_start,
        page_count,
        physical_offset,
        user_dma_page,
    )
}

pub(super) fn map_user_pages(
    ttbr0: usize,
    physical_start: usize,
    page_count: usize,
    physical_offset: usize,
    descriptor: fn(usize) -> u64,
) -> Option<usize> {
    unsafe {
        let total = ENTRIES * USER_MMIO_L2_GROUPS;
        if page_count == 0 || page_count > total {
            return None;
        }
        let mut first_slot = usize::MAX;
        let mut run_length = 0usize;
        'search: for group in 0..USER_MMIO_L2_GROUPS {
            let Some(table) = user_mmio_table(ttbr0, group) else {
                run_length = 0;
                continue;
            };
            for index in 0..ENTRIES {
                if read_entry(table, index) == 0 {
                    if run_length == 0 {
                        first_slot = group * ENTRIES + index;
                    }
                    run_length += 1;
                    if run_length == page_count {
                        break 'search;
                    }
                } else {
                    run_length = 0;
                }
            }
        }
        if run_length != page_count {
            return None;
        }
        let mut written = 0usize;
        while written < page_count {
            let slot = first_slot + written;
            let group = slot / ENTRIES;
            let index = slot % ENTRIES;
            let Some(table) = user_mmio_table(ttbr0, group) else {
                rollback_user_pages(ttbr0, first_slot, written);
                return None;
            };
            write_entry(
                table,
                index,
                descriptor(physical_start + written * GRANULE),
            );
            clean_entry_line(table, index);
            written += 1;
        }
        finish_table_changes_for(ttbr0);
        Some(USER_MMIO_VA + first_slot * GRANULE + physical_offset)
    }
}

/// Undoes a partially written map run so a failed mapping never leaves
/// half a window visible to EL0.
pub(super) fn rollback_user_pages(ttbr0: usize, first_slot: usize, written: usize) {
    unsafe {
        for page_index in 0..written {
            let slot = first_slot + page_index;
            let group = slot / ENTRIES;
            let index = slot % ENTRIES;
            let Some(table) = user_mmio_table(ttbr0, group) else {
                continue;
            };
            write_entry(table, index, 0);
            clean_entry_line(table, index);
        }
    }
    finish_table_changes_for(ttbr0);
}

pub(super) fn clean_dma_range(physical: usize, size: usize) {
    let start = phys_to_virt(physical);
    clean_cache_range(start, start.saturating_add(size));
    unsafe {
        // Device-visible publication needs full-system ordering, not only
        // inner-shareable store completion.
        core::arch::asm!("dsb sy", options(nostack));
    }
}

/// Returns (allocating on first use) the L3 table behind the task's device
/// window. The root and every existing level are validated with the same
/// owner-aware checks as [`revoke_phys_range`]: a foreign or corrupted
/// descriptor is refused before it is dereferenced or modified, and the
/// mapping path never writes into another space's table.
pub(super) fn user_mmio_table(ttbr0: usize, group: usize) -> Option<usize> {
    if group >= USER_MMIO_L2_GROUPS {
        return None;
    }
    let root = root_of(ttbr0);
    unsafe {
        if !space_root_valid(root) {
            return None;
        }
        let root_entry = read_entry(root, 0);
        let l1_user = follow_validated_table(root_entry, root)?;
        let existing = read_entry(l1_user, USER_MMIO_L1_INDEX);
        let l2_mmio = if existing & DESC_TABLE == DESC_TABLE {
            follow_validated_table(existing, root)?
        } else if existing == 0 {
            let (l2, generation) = frame_allocator::alloc_page_tagged()?;
            zero_page(l2);
            if page_table_register(l2, generation, root).is_none() {
                frame_allocator::free_page_tagged(l2, generation);
                return None;
            }
            // Publish the zeroed contents to the point of coherency *before*
            // the parent entry can make a walker read this table. Cleaning
            // only the parent descriptor would let a real CPU fetch stale
            // lines from the recycled frame.
            flush_after_table_change(l2, GRANULE);
            write_entry(l1_user, USER_MMIO_L1_INDEX, l2 as u64 | DESC_TABLE);
            clean_entry_line(l1_user, USER_MMIO_L1_INDEX);
            l2
        } else {
            return None;
        };

        let index = USER_MMIO_L2_INDEX + group;
        let entry = read_entry(l2_mmio, index);
        if entry & DESC_TABLE == DESC_TABLE {
            return follow_validated_table(entry, root);
        }
        if entry != 0 {
            return None;
        }
        let (l3, generation) = frame_allocator::alloc_page_tagged()?;
        zero_page(l3);
        if page_table_register(l3, generation, root).is_none() {
            frame_allocator::free_page_tagged(l3, generation);
            return None;
        }
        flush_after_table_change(l3, GRANULE);
        write_entry(l2_mmio, index, l3 as u64 | DESC_TABLE);
        clean_entry_line(l2_mmio, index);
        finish_table_changes_for(ttbr0);
        Some(l3)
    }
}

/// Tears down a whole address space. `space` is the root token returned by
/// [`create_task_space`]; a stale token is refused before anything is freed,
/// so a recycled root frame can never destroy the new owner's tables.
///
/// All tables are drained from the registry by owner in one step, then each
/// frame is released with the generation stored at its registration. Heap
/// data pages are released by the task before this call, because only the task
/// owns their generation tags.
pub fn destroy_task_space(
    space: TableToken,
    private_page: usize,
    private_page_generation: u32,
    asid: u16,
) -> bool {
    destroy_task_space_inner(space, private_page, private_page_generation, asid, true)
}

/// `diagnose = false` is used only by the boot self-test for its intentional
/// stale teardown; production callers always take the diagnostic path.
pub(super) fn destroy_task_space_inner(
    space: TableToken,
    private_page: usize,
    private_page_generation: u32,
    asid: u16,
    diagnose: bool,
) -> bool {
    // Quiescence proof: a space still current on an online CPU may have live
    // translations and a walker inside its tables. Refuse and leak rather
    // than retire it underneath that CPU.
    if crate::scheduler::space_is_running(space.frame()) {
        if diagnose {
            crate::console::write_str("MMU: destroy refused, space still running on a CPU 0x");
            crate::console::write_hex_u64(space.frame() as u64);
            crate::console::write_str(" - leaked\r\n");
        }
        return false;
    }
    // Drop every translation tagged with this ASID before the tables go away,
    // and before the ASID can be handed to a new space. Broadcast: the space
    // may have run on another CPU earlier.
    invalidate_asid(asid);
    let retired = PAGE_TABLE_FRAMES.lock().retire_space(space);
    let tokens = match retired {
        SpaceRetire::Retired(tokens) => tokens,
        SpaceRetire::NotRegistered => {
            if diagnose {
                crate::console::write_str("MMU: destroy refused, root frame 0x");
                crate::console::write_hex_u64(space.frame() as u64);
                crate::console::write_str(" is not a live table space - leaked\r\n");
            }
            return false;
        }
        SpaceRetire::StaleToken => {
            if diagnose {
                crate::console::write_str("MMU: destroy refused, stale root token for frame 0x");
                crate::console::write_hex_u64(space.frame() as u64);
                crate::console::write_str(" - leaked\r\n");
            }
            return false;
        }
        SpaceRetire::NotSpaceRoot => {
            if diagnose {
                crate::console::write_str("MMU: destroy refused, frame 0x");
                crate::console::write_hex_u64(space.frame() as u64);
                crate::console::write_str(" is not a space root - leaked\r\n");
            }
            return false;
        }
        SpaceRetire::OutOfMemory => {
            if diagnose {
                crate::console::write_str("MMU: destroy refused, registry out of memory - leaked\r\n");
            }
            return false;
        }
    };

    let mut complete = true;
    for token in tokens {
        if !frame_allocator::free_page_tagged(token.frame(), token.generation()) {
            if diagnose {
                crate::console::write_str("MMU: table frame 0x");
                crate::console::write_hex_u64(token.frame() as u64);
                crate::console::write_str(" was already free - leaked\r\n");
            }
            complete = false;
        }
    }
    if !frame_allocator::free_page_tagged(private_page, private_page_generation) {
        if diagnose {
            crate::console::write_str("MMU: private page 0x");
            crate::console::write_hex_u64(private_page as u64);
            crate::console::write_str(" was already free - leaked\r\n");
        }
        complete = false;
    }
    // The space is retired and its ASID was invalidated above, so the value is
    // safe to reissue even if a frame-accounting anomaly kept `complete` false.
    release_asid(asid);
    complete
}

/// Read-only check used by the address-space self-test: is the 4 KiB page at
/// `virtual_address` present in `ttbr0`?
/// Translates a user virtual address through `ttbr0` and returns the
/// physical address backing it (page descriptor + intra-page offset), or
/// `None` when the page is not mapped. Diagnostic use only.
pub fn user_physical_address(ttbr0: usize, virtual_address: usize) -> Option<usize> {
    if virtual_address < DRAM_BASE {
        return None;
    }
    let l2_index = (virtual_address >> 21) & (ENTRIES - 1);
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        if !plausible_table_address(l1_user) {
            return None;
        }
        let l1_entry = read_entry(l1_user, (virtual_address >> 30) & (ENTRIES - 1));
        if l1_entry & DESC_TABLE != DESC_TABLE { return None; }
        let l2_user = descriptor_address(l1_entry) as usize;
        if !plausible_table_address(l2_user) {
            return None;
        }
        let l2_entry = read_entry(l2_user, l2_index);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return None;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        if !plausible_table_address(l3) {
            return None;
        }
        let entry = read_entry(l3, (virtual_address >> 12) & 0x1FF);
        if entry & DESC_PAGE != DESC_PAGE {
            return None;
        }
        let frame = (entry & ADDRESS_MASK) as usize;
        Some(frame | (virtual_address & (GRANULE - 1)))
    }
}

pub fn user_page_is_mapped(ttbr0: usize, virtual_address: usize) -> bool {
    if virtual_address % GRANULE != 0 || virtual_address < DRAM_BASE {
        return false;
    }
    let l2_index = (virtual_address >> 21) & (ENTRIES - 1);
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        if !plausible_table_address(l1_user) {
            return false;
        }
        let l1_entry = read_entry(l1_user, (virtual_address >> 30) & (ENTRIES - 1));
        if l1_entry & DESC_TABLE != DESC_TABLE { return false; }
        let l2_user = descriptor_address(l1_entry) as usize;
        if !plausible_table_address(l2_user) {
            return false;
        }
        let l2_entry = read_entry(l2_user, l2_index);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        if !plausible_table_address(l3) {
            return false;
        }
        read_entry(l3, (virtual_address >> 12) & 0x1FF) & DESC_PAGE == DESC_PAGE
    }
}

/// Validates an EL0-readable buffer through the current task's TTBR0.
pub fn valid_user_slice(pointer: u64, length: u64) -> bool {
    valid_user_access(pointer, length, false)
}

/// Validates an EL0-writable buffer. This rejects rodata as well as unmapped
/// pages and is required before copyout or a potentially blocking receive.
pub fn valid_user_slice_mut(pointer: u64, length: u64) -> bool {
    valid_user_access(pointer, length, true)
}

pub(super) fn valid_user_access(pointer: u64, length: u64, write: bool) -> bool {
    if length > 4096 || pointer >= (1_u64 << 48) {
        return false;
    }
    if length == 0 {
        return true;
    }
    let Some(last) = pointer.checked_add(length - 1) else {
        return false;
    };
    if last >= (1_u64 << 48) {
        return false;
    }

    let mut page = pointer & !(GRANULE as u64 - 1);
    let last_page = last & !(GRANULE as u64 - 1);
    loop {
        if !current_user_page_access(page, write) {
            return false;
        }
        if page == last_page {
            return true;
        }
        page += GRANULE as u64;
    }
}

pub(super) fn current_user_page_access(virtual_address: u64, write: bool) -> bool {
    let ttbr0: u64;
    unsafe {
        core::arch::asm!("mrs {value}, ttbr0_el1", value = out(reg) ttbr0, options(nostack));
        let mut table = descriptor_address(ttbr0) as usize;
        for shift in [39, 30, 21] {
            let index = ((virtual_address >> shift) & 0x1FF) as usize;
            let descriptor = read_entry(table, index);
            if descriptor & DESC_TABLE != DESC_TABLE {
                return false;
            }
            table = descriptor_address(descriptor) as usize;
            if !plausible_table_address(table) {
                return false;
            }
        }
        let index = ((virtual_address >> 12) & 0x1FF) as usize;
        let descriptor = read_entry(table, index);
        if descriptor & DESC_PAGE != DESC_PAGE {
            return false;
        }
        let access_permissions = (descriptor >> 6) & 0b11;
        if write {
            access_permissions == 0b01
        } else {
            access_permissions == 0b01 || access_permissions == 0b11
        }
    }
}

/// User ELF text: readable/executable at EL0, never executable at EL1.
pub(super) fn user_text_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_BOTH_RO
        | SH_INNER
        | AF
        | NG
        | PXN
}

/// User ELF rodata: read-only and never executable at either level.
pub(super) fn user_rodata_page(output_address: usize) -> u64 {
    user_text_page(output_address) | UXN
}

/// Task-owned writable memory: user stack or private page, never executable.
pub(super) fn user_data_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_EL0_RW
        | SH_INNER
        | AF
        | NG
        | UXN
        | PXN
}

/// Anonymous `mmap` page: always readable, writable/executable per `prot`, and
/// never executable from EL1. Write-only combinations fall back to read-only
/// (read permission is assumed present for every mapping). Callers must reject
/// `PROT_WRITE | PROT_EXEC` before reaching this helper; `map_user_pages_at`
/// also enforces that W^X rule defensively.
/// Cacheable builder for shared DMA surfaces. `dc zva` (which glibc's
/// `memset` uses for large zeroing) is only architecturally defined on
/// Normal Cacheable memory, so a Non-Cacheable mapping makes ordinary libc
/// code fault. The device sees the data because the owner cleans the range
/// before every transfer.
pub(super) fn user_shared_normal_page(output_address: usize) -> u64 {
    user_anonymous_page(output_address, PROT_READ | PROT_WRITE)
}

pub(super) fn user_anonymous_page(output_address: usize, prot: u64) -> u64 {
    let mut descriptor = descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | SH_INNER
        | AF
        | NG
        | PXN;
    if prot & PROT_WRITE != 0 {
        descriptor |= AP_EL0_RW;
    } else {
        descriptor |= AP_BOTH_RO;
    }
    if prot & PROT_EXEC == 0 {
        descriptor |= UXN;
    }
    descriptor
}

/// User MMIO: MAIR index 1 is 0x00, Device-nGnRnE, never executable.
pub(super) fn user_device_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_DEVICE << 2)
        | AP_EL0_RW
        | SH_NONE
        | AF
        | NG
        | UXN
        | PXN
}

/// User DMA: MAIR index 2 is 0x44, Normal Non-Cacheable, never executable.
pub(super) fn user_dma_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL_NC << 2)
        | AP_EL0_RW
        | SH_INNER
        | AF
        | NG
        | UXN
        | PXN
}
