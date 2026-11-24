use super::*;
use super::table::*;

/// AP trampoline copies of the boot CPU's translation-control registers.
pub(crate) fn ap_boot_mair() -> u64 {
    MAIR_VALUE
}

pub(crate) fn ap_boot_tcr() -> u64 {
    TCR_VALUE
}

/// Replaces the temporary boot identity TTBR0 with an empty kernel TTBR0 and
/// retains the high TTBR1 map. User TTBR0s are installed by the scheduler.
pub fn init() {
    unsafe {
        let base = core::ptr::addr_of_mut!(TABLES) as *mut Table;
        let low0 = &mut *base.add(INDEX_LOW_LEVEL0);
        let high0 = &mut *base.add(INDEX_HIGH_LEVEL0);
        let level1 = &mut *base.add(INDEX_LEVEL1);
        let mmio = &mut *base.add(INDEX_LEVEL2_MMIO);
        let dram = &mut *base.add(INDEX_LEVEL2_DRAM);
        let pci = &mut *base.add(INDEX_LEVEL2_PCI);

        // We execute through TTBR1 now; the low identity alias is no longer
        // needed by the kernel. User TTBR0s are installed by the scheduler.
        low0.entries[0] = 0;
        high0.entries[0] = table_descriptor(level1);
        level1.entries[0] = table_descriptor(mmio);
        level1.entries[DRAM_L1_INDEX] = table_descriptor(dram);
        level1.entries[PCI_ECAM_L1_INDEX] = table_descriptor(pci);

        for index in 0..ENTRIES {
            mmio.entries[index] = block_descriptor(index * BLOCK_2M, ATTR_DEVICE, SH_NONE);
            dram.entries[index] =
                block_descriptor(DRAM_BASE + index * BLOCK_2M, ATTR_NORMAL, SH_INNER);
        }
        // The boot stub only fills the first DRAM gigabyte. The tablet's frame
        // allocator works in the second one (0xD510_0000..0xD800_0000), so a
        // second L2 is installed here, after the MMU is running.
        #[cfg(feature = "tablet")]
        {
            let dram_high = &mut *base.add(INDEX_LEVEL2_DRAM_HIGH);
            level1.entries[DRAM_L1_INDEX + 1] = table_descriptor(dram_high);
            for index in 0..ENTRIES {
                dram_high.entries[index] = block_descriptor(
                    DRAM_BASE + 0x4000_0000 + index * BLOCK_2M,
                    ATTR_NORMAL,
                    SH_INNER,
                );
            }
        }
        for index in 0..PCI_ECAM_L2_BLOCKS {
            pci.entries[PCI_ECAM_L2_INDEX + index] =
                block_descriptor(PCI_ECAM_BASE + index * BLOCK_2M, ATTR_DEVICE, SH_NONE);
        }
    }

    clean_tables_to_point_of_coherency();
    configure_and_enable();
    // Caches were off while the boot stub and this function built and
    // published the tables, so the D-cache may still hold firmware lines.
    // `cache::init_boot` drops them and enables C/I; from that point the
    // table-clean and DMA-clean rituals are load-bearing rather than
    // ceremonial.
    crate::cache::init_boot();
}

/// Replaces the 2 MiB block containing `arena_pa` with a 4 KiB page table
/// whose entries are initially invalid, and returns the table's physical
/// address.
///
/// The caller must own the entire 2 MiB block. The kernel stack arena is
/// allocated as one aligned run for exactly this reason; splitting a block
/// that also contains live kernel data would unmap it. Concurrent init calls
/// are serialized in `kernel_stack::init`; the descriptor-type check below is
/// the second line of defence against splitting the same block twice.
pub fn install_kernel_stack_arena(arena_pa: usize) -> Option<usize> {
    if arena_pa % BLOCK_2M != 0 {
        return None;
    }
    let offset = arena_pa.checked_sub(DRAM_BASE)?;
    let l1_index = DRAM_L1_INDEX + offset / 0x4000_0000;
    if l1_index >= ENTRIES {
        return None;
    }
    let l2_index = (offset % 0x4000_0000) / BLOCK_2M;

    unsafe {
        let base = core::ptr::addr_of!(TABLES) as *const Table;
        let level1 = &*base.add(INDEX_LEVEL1);
        let l1_entry = level1.entries[l1_index];
        if l1_entry & DESC_TABLE != DESC_TABLE {
            return None;
        }
        let l2_pa = descriptor_address(l1_entry) as usize;
        let l2_entry = read_entry(l2_pa, l2_index);
        if l2_entry & 0b11 != DESC_BLOCK {
            // Already split or invalid: do not reuse someone else's table.
            return None;
        }

        let (l3_pa, l3_generation) = frame_allocator::alloc_page_tagged()?;
        // Owner 0: the kernel stack arena table is permanent and never
        // retired, but it is still a table for revoke validation.
        if page_table_register(l3_pa, l3_generation, 0).is_none() {
            frame_allocator::free_page_tagged(l3_pa, l3_generation);
            return None;
        }
        zero_page(l3_pa);
        // Publish the zeroed L3 contents to the point of coherency *before*
        // the L2 entry can make the walker read the table. Cleaning only the
        // L2 entry first would let a real CPU fetch stale L3 lines after the
        // old block translation is dropped.
        flush_after_table_change(l3_pa, GRANULE);
        write_entry(l2_pa, l2_index, l3_pa as u64 | DESC_TABLE);
        flush_after_table_change(l2_pa, GRANULE);
        Some(l3_pa)
    }
}

/// Read-only check used by the kernel-stack arena self-test.
pub fn kernel_page_is_mapped(table_pa: usize, index: usize) -> bool {
    index < ENTRIES && (unsafe { read_entry(table_pa, index) } & 0b11) == DESC_PAGE
}

/// Maps or unmaps `page_count` consecutive 4 KiB pages in a kernel stack
/// arena table. One table clean + TLB flush covers the whole run.
pub fn set_kernel_stack_pages(
    table_pa: usize,
    first_index: usize,
    physical_base: usize,
    page_count: usize,
    mapped: bool,
) -> bool {
    if page_count == 0 || first_index.checked_add(page_count).map_or(true, |end| end > ENTRIES) {
        return false;
    }
    for index in 0..page_count {
        let descriptor = if mapped {
            kernel_page_descriptor(physical_base + index * GRANULE)
        } else {
            0
        };
        unsafe { write_entry(table_pa, first_index + index, descriptor) };
    }
    flush_after_table_change(table_pa, GRANULE);
    true
}

/// Data-cache line size in bytes, from `CTR_EL0.DminLine`.
///
/// AArch64 encodes the minimum D-cache line as `4 << DminLine` bytes. Reading
/// it avoids baking a 64-byte assumption into the maintenance loops: every
/// range below is aligned and stepped with the value this CPU reports.
pub(super) fn data_cache_line_size() -> usize {
    let ctr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {}, ctr_el0",
            out(reg) ctr,
            options(nostack, preserves_flags),
        );
    }
    4usize << ((ctr >> 16) & 0xF)
}

/// Cleans (write-back, no invalidate) every data-cache line overlapping
/// `[start, end)`. The caller follows this with the barrier its protocol
/// requires (`dsb ishst` for table publication, `dsb sy` for boot tables).
pub(super) fn clean_cache_range(start: usize, end: usize) {
    let line_size = data_cache_line_size();
    let mask = line_size - 1;
    let mut line = start & !mask;
    let end = end.saturating_add(mask) & !mask;
    unsafe {
        while line < end {
            core::arch::asm!(
                "dc cvac, {line}",
                line = in(reg) line,
                options(nostack, preserves_flags),
            );
            line += line_size;
        }
    }
}

/// Invalidates (no write-back) every data-cache line overlapping
/// `[start, end)`.
///
/// This is the receive half of the DMA handover: after a device (or another
/// non-cacheable agent) wrote the buffer, a clean line may still hold the old
/// contents. Invalidating by VA discards that copy so the next load observes
/// memory. Never call this on lines that hold the only copy of dirty data.
pub fn invalidate_range(start: usize, end: usize) {
    let line_size = data_cache_line_size();
    let mask = line_size - 1;
    let mut line = start & !mask;
    let end = end.saturating_add(mask) & !mask;
    unsafe {
        while line < end {
            core::arch::asm!(
                "dc ivac, {line}",
                line = in(reg) line,
                options(nostack, preserves_flags),
            );
            line += line_size;
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

pub(super) fn clean_tables_to_point_of_coherency() {
    let start = core::ptr::addr_of!(TABLES) as usize;
    let end = start + TABLE_COUNT * TABLE_BYTES;
    clean_cache_range(start, end);
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// The kernel-high-half TTBR1 value; valid after [`init`].
pub fn kernel_ttbr1() -> usize {
    KERNEL_TTBR1.load(core::sync::atomic::Ordering::Acquire)
}

pub(super) fn configure_and_enable() {
    let table_base = core::ptr::addr_of!(TABLES) as usize;
    let ttbr0 = (virt_to_phys(table_base + INDEX_LOW_LEVEL0 * TABLE_BYTES) as u64) & ADDRESS_MASK;
    let ttbr1 = (virt_to_phys(table_base + INDEX_HIGH_LEVEL0 * TABLE_BYTES) as u64) & ADDRESS_MASK;
    KERNEL_TTBR1.store(
        ttbr1 as usize,
        core::sync::atomic::Ordering::Release,
    );

    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {low}",
            "msr ttbr1_el1, {high}",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "isb",
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, #1",
            "orr {sctlr}, {sctlr}, #(1 << 22)",
            // UCT, DZE and UCI: EL0 may read CTR_EL0, use DC ZVA and run
            // cache maintenance by VA (glibc memset and JIT clear_cache).
            "orr {sctlr}, {sctlr}, #(1 << 15)",
            "orr {sctlr}, {sctlr}, #(1 << 14)",
            "orr {sctlr}, {sctlr}, #(1 << 26)",
            "msr sctlr_el1, {sctlr}",
            "isb",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            low = in(reg) ttbr0,
            high = in(reg) ttbr1,
            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) TCR_VALUE,
            sctlr = out(reg) _,
            options(nostack),
        );
    }
}

/// Clean and invalidate after changing a task's tables. `table_start` is a
/// physical table address; the CPU accesses it through the high alias.
///
/// TLB shootdown contract: the `is` suffix broadcasts the invalidation to
/// every PE in the inner-shareability domain, and the trailing `dsb ish`
/// makes the issuing CPU wait until the broadcast has been *observed* by
/// all of them (ARM DDI 0487: completion of a DSB after an IS-scope TLBI
/// guarantees the invalidation is complete across the domain). Together
/// with the preceding `clean_table_lines` + `dsb ishst` (table write made
/// visible before the walk can re-fetch it) and the kernel lock excluding
/// concurrent table editors, this is the full shootdown protocol: no CPU
/// can hold or re-create a translation for a removed entry past this call.
pub fn flush_after_table_change(table_start: usize, table_bytes: usize) {
    clean_table_lines(table_start, table_bytes);
    finish_table_changes();
}

/// Completes a batch of table edits made since the last call: makes every
/// cleaned descriptor visible (`dsb ishst`) and performs one broadcast
/// invalidation for the whole batch.
///
/// This is the "edit the range, flush once" order: a caller that publishes
/// many PTEs cleans each touched cache line as it writes (see
/// [`clean_entry_line`]) and calls this once at the end, instead of paying a
/// full `dsb`/`tlbi`/`dsb` per page.
pub(super) fn finish_table_changes() {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
}

/// Cleans the cache line holding one 8-byte table entry, without a barrier.
/// Paired with [`finish_table_changes`] for batched publication.
pub(super) fn clean_entry_line(table_start: usize, entry_index: usize) {
    let line_size = data_cache_line_size();
    let address = phys_to_virt(table_start) + entry_index * 8;
    let line = address & !(line_size - 1);
    clean_cache_range(line, line + line_size);
}

/// Pushes the dirty translation-table lines to the point of coherency.
///
/// Only the *table* is cleaned here: without it the table walker may fetch a
/// stale descriptor from memory even though the invalidating `tlbi` follows.
pub(super) fn clean_table_lines(table_start: usize, table_bytes: usize) {
    let start = phys_to_virt(table_start);
    clean_cache_range(start, start.saturating_add(table_bytes));
    unsafe {
        core::arch::asm!("dsb ishst", options(nostack));
    }
}

/// Invalidates the EL1&0 translation of one 4 KB user page by address.
///
/// `tlbi vmalle1` would be equally correct, but it discards every cached
/// translation system-wide on each mapping request. The address form keeps the
/// rest of the TLB warm and, unlike a block-wide flush, cannot be skipped by
/// accident when a task maps into a page table that was just installed.
/// Reads the TTBR0 value installed on this CPU (used only by the boot TLB
/// self-test, which restores it after switching to a scratch space).
pub fn current_ttbr0() -> usize {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) value, options(nomem, nostack));
    }
    value as usize
}

/// Pushes `size` bytes at a kernel virtual (== physical, low identity)
/// address to the point of coherency. Used to publish AP boot parameters to
/// a CPU that starts with cold caches.
pub fn clean_range_to_poc(start: usize, size: usize) {
    clean_cache_range(start, start.saturating_add(size));
    unsafe {
        core::arch::asm!("dsb ish", options(nostack));
    }
}

/// Enables the MMU on an application CPU: the shared kernel high half
/// (`kernel_ttbr1`) plus a pre-built task-space root whose L0[0] identity-
/// maps DRAM, so the AP keeps executing its low-address code and reaches the
/// kernel through the high alias.
///
/// Runs with the MMU off: every address this function touches is PA == VA
/// (the kernel is linked at its load address). The caller must have cleaned
/// `ap_ttbr0` and the boot parameters to the point of coherency.
pub fn ap_enable(ap_ttbr0: usize) {
    let high = kernel_ttbr1() as u64;
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {low}",
            "msr ttbr1_el1, {high}",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "isb",
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, #1",
            "orr {sctlr}, {sctlr}, #(1 << 22)",
            "msr sctlr_el1, {sctlr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            low = in(reg) ap_ttbr0 as u64,
            high = in(reg) high,
            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) TCR_VALUE,
            sctlr = out(reg) _,
            options(nostack),
        );
    }
}

/// Switches only the user half. The executing high-half kernel code and its
/// stack remain visible through TTBR1_EL1 for every task.
///
/// No TLB invalidation is needed: every space carries a unique ASID in
/// `TTBR0_EL1[63:48]`, so the old space's entries stay tagged and valid while
/// the new space's entries are selected by the new ASID. An ASID is only
/// reused after [`invalidate_asid`] broadcast-invalidated it on destroy.
pub fn switch_address_space(ttbr0: usize) {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "isb",
            ttbr = in(reg) ttbr0 as u64,
            options(nostack),
        );
    }
}
