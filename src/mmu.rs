//! ARMv8-A MMU with a high-half kernel and per-task user address spaces.
//!
//! TTBR1_EL1 maps physical memory at `KERNEL_VIRT_BASE + PA` for EL1 only:
//! kernel code/data/stacks, page tables, the kernel heap, UART and GIC all
//! live there. TTBR0_EL1 belongs to the current task and contains only its
//! independently loaded ELF image, sparse heap, user stack, private page,
//! explicitly authorized Device-nGnRnE MMIO and Normal Non-Cacheable DMA.
//! No kernel image or kernel data is shared with EL0.

use crate::{
    frame_allocator,
    memory::{phys_to_virt, virt_to_phys, DRAM_BASE, DRAM_END},
};

const ENTRIES: usize = 512;
const GRANULE: usize = 4096;
const TABLE_BYTES: usize = ENTRIES * 8;
const BLOCK_2M: usize = 0x20_0000;
const PHYSICAL_ADDRESS_LIMIT: usize = 1 << 40;

const DESC_TABLE: u64 = 0b11;
const DESC_BLOCK: u64 = 0b01;
const DESC_PAGE: u64 = 0b11;

/// Private 4 KB data window: L0 index 1, then L1/L2/L3 index 0.
pub const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;
const PRIVATE_WINDOW_L0_INDEX: usize = (PRIVATE_WINDOW_VA >> 39) & 0x1FF;
/// A sparse, per-task heap window immediately after the 2 MB ELF image.
/// Its L3 table is installed at spawn, while data pages are mapped by `brk`.
pub const USER_HEAP_VA: usize = 0x0000_0000_4020_0000;
pub const USER_HEAP_MAX_PAGES: usize = 512;
pub const USER_HEAP_MAX_SIZE: usize = USER_HEAP_MAX_PAGES * GRANULE;
const USER_HEAP_L2_INDEX: usize = (USER_HEAP_VA - DRAM_BASE) / BLOCK_2M;

/// Every task's own stack appears at this user VA, backed by different PAs.
pub const USER_STACK_VA: usize = 0x0000_0000_7FE0_0000;
pub const USER_STACK_PAGES: usize = 2;
pub const USER_STACK_SIZE: usize = USER_STACK_PAGES * GRANULE;
const USER_STACK_L2_INDEX: usize = (USER_STACK_VA - DRAM_BASE) / BLOCK_2M;

/// A 2 MB first-fit window for explicitly authorized user MMIO and DMA mappings.
pub const USER_MMIO_VA: usize = 0x0000_0000_8000_0000;
pub const USER_DMA_MAX_SIZE: usize = ENTRIES * GRANULE;
const USER_MMIO_L1_INDEX: usize = (USER_MMIO_VA >> 30) & 0x1FF;
const USER_MMIO_L2_INDEX: usize = 0;

pub const PCI_ECAM_BASE: usize = 0x0000_0040_1000_0000;
pub const PCI_ECAM_SIZE: usize = 0x1000_0000;
const PCI_ECAM_L1_INDEX: usize = (PCI_ECAM_BASE >> 30) & 0x1FF;
const PCI_ECAM_L2_INDEX: usize = ((PCI_ECAM_BASE >> 21) & 0x1FF) as usize;
const PCI_ECAM_L2_BLOCKS: usize = PCI_ECAM_SIZE / BLOCK_2M;

/// MAIR attribute indices. `ATTR_NORMAL` is cacheable write-back, `ATTR_DEVICE`
/// is Device-nGnRnE and `ATTR_NORMAL_NC` is Normal Non-Cacheable. These must
/// match the byte positions in `MAIR_VALUE` exactly.
const ATTR_NORMAL: u64 = 0;
const ATTR_DEVICE: u64 = 1;
const ATTR_NORMAL_NC: u64 = 2;

const AP_EL1_RW: u64 = 0b00 << 6;
const AP_EL0_RW: u64 = 0b01 << 6;
const AP_BOTH_RO: u64 = 0b11 << 6;
const AF: u64 = 1 << 10;
const SH_NONE: u64 = 0b00 << 8;
const SH_INNER: u64 = 0b11 << 8;
const UXN: u64 = 1 << 54;
const PXN: u64 = 1 << 53;

const ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// MAIR_EL1 attribute bytes, one per `ATTR_*` index:
/// * index 0 = `0xFF` Normal, Outer/Inner Write-Back Non-transient (cacheable)
/// * index 1 = `0x00` Device-nGnRnE (MMIO)
/// * index 2 = `0x44` Normal, Outer/Inner Non-Cacheable (DMA)
///
/// The byte order matters: a descriptor selects a byte with bits [4:2], so
/// index 1 must be the second byte of this value. Writing the two non-cacheable
/// attributes in the opposite order silently maps DMA as Device-nGnRnE and MMIO
/// as Normal Non-Cacheable.
const MAIR_VALUE: u64 = 0x44_00_FF;

/// 48-bit VAs on both TTBRs (T0SZ=T1SZ=16), 4 KB granules, 40-bit PAs,
/// inner-shareable WB table walks. TG0/TG1 are zero: 4 KB on both halves.
const TCR_VALUE: u64 = 16 // T0SZ
    | (16 << 16) // T1SZ
    | (0b11 << 12) // SH0
    | (0b01 << 10) // ORGN0
    | (0b01 << 8) // IRGN0
    | (0b010 << 32); // IPS

#[repr(C, align(4096))]
struct Table {
    entries: [u64; ENTRIES],
}

const TABLE_COUNT: usize = 6;
const EMPTY_TABLE: Table = Table {
    entries: [0; ENTRIES],
};

/// The assembly boot stub fills the first five tables before enabling the MMU.
/// The sixth is zeroed with .bss and populated for PCI ECAM in `init`.
#[no_mangle]
static mut TABLES: [Table; TABLE_COUNT] = [EMPTY_TABLE; TABLE_COUNT];

const INDEX_LOW_LEVEL0: usize = 0;
const INDEX_HIGH_LEVEL0: usize = 1;
const INDEX_LEVEL1: usize = 2;
const INDEX_LEVEL2_MMIO: usize = 3;
const INDEX_LEVEL2_DRAM: usize = 4;
const INDEX_LEVEL2_PCI: usize = 5;

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
        level1.entries[1] = table_descriptor(dram);
        level1.entries[PCI_ECAM_L1_INDEX] = table_descriptor(pci);

        for index in 0..ENTRIES {
            mmio.entries[index] = block_descriptor(index * BLOCK_2M, ATTR_DEVICE, SH_NONE);
            dram.entries[index] =
                block_descriptor(DRAM_BASE + index * BLOCK_2M, ATTR_NORMAL, SH_INNER);
        }
        for index in 0..PCI_ECAM_L2_BLOCKS {
            pci.entries[PCI_ECAM_L2_INDEX + index] =
                block_descriptor(PCI_ECAM_BASE + index * BLOCK_2M, ATTR_DEVICE, SH_NONE);
        }
    }

    clean_tables_to_point_of_coherency();
    configure_and_enable();
}

fn table_descriptor(table: &Table) -> u64 {
    (virt_to_phys(table as *const Table as usize) as u64) | DESC_TABLE
}

fn block_descriptor(output_address: usize, attr_index: u64, shareability: u64) -> u64 {
    ((output_address as u64) & !(BLOCK_2M as u64 - 1))
        | DESC_BLOCK
        | (attr_index << 2)
        | AP_EL1_RW
        | shareability
        | AF
        | UXN
}

fn clean_tables_to_point_of_coherency() {
    let start = core::ptr::addr_of!(TABLES) as usize;
    let end = start + TABLE_COUNT * TABLE_BYTES;
    unsafe {
        let mut line = start;
        while line < end {
            core::arch::asm!(
                "dc cvac, {line}",
                line = in(reg) line,
                options(nostack, preserves_flags),
            );
            line += 64;
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Installs empty-low/high-kernel TTBRs. The boot stub has already enabled
/// translation, so this runs entirely through the high alias.
fn configure_and_enable() {
    let table_base = core::ptr::addr_of!(TABLES) as usize;
    let ttbr0 = (virt_to_phys(table_base + INDEX_LOW_LEVEL0 * TABLE_BYTES) as u64) & ADDRESS_MASK;
    let ttbr1 = (virt_to_phys(table_base + INDEX_HIGH_LEVEL0 * TABLE_BYTES) as u64) & ADDRESS_MASK;

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
pub fn flush_after_table_change(table_start: usize, table_bytes: usize) {
    clean_table_lines(table_start, table_bytes);
    unsafe {
        core::arch::asm!(
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
}

/// Pushes the dirty translation-table lines to the point of coherency.
///
/// Only the *table* is cleaned here: without it the table walker may fetch a
/// stale descriptor from memory even though the invalidating `tlbi` follows.
fn clean_table_lines(table_start: usize, table_bytes: usize) {
    let start = phys_to_virt(table_start) & !63;
    let end = (phys_to_virt(table_start) + table_bytes + 63) & !63;
    unsafe {
        let mut line = start;
        while line < end {
            core::arch::asm!(
                "dc cvac, {line}",
                line = in(reg) line,
                options(nostack, preserves_flags),
            );
            line += 64;
        }
        core::arch::asm!("dsb ishst", options(nostack));
    }
}

/// Invalidates the EL1&0 translation of one 4 KB user page by address.
///
/// `tlbi vmalle1` would be equally correct, but it discards every cached
/// translation system-wide on each mapping request. The address form keeps the
/// rest of the TLB warm and, unlike a block-wide flush, cannot be skipped by
/// accident when a task maps into a page table that was just installed.
fn invalidate_user_page(virtual_address: usize) {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            // VA[55:12] is what VAE1IS takes; the low 12 bits are the offset.
            "tlbi vaae1is, {va}",
            "dsb ish",
            "isb",
            va = in(reg) (virtual_address as u64) >> 12,
            options(nostack),
        );
    }
}

// ---------------------------------------------------------------------------
// Per-task user address spaces
// ---------------------------------------------------------------------------

/// Builds a user TTBR0. `user_stack_start == 0` creates no stack mapping
/// (used for the EL1 idle task). A loaded ELF supplies independently owned
/// image pages instead of the old shared kernel text/rodata alias. The heap
/// starts empty: its L3 table exists, but data frames are added by `brk`.
pub fn create_task_space(
    private_page: usize,
    user_stack_start: usize,
    image: Option<&crate::elf_loader::UserImage>,
) -> Option<usize> {
    let mut pages = [0_usize; 9];
    for (index, slot) in pages.iter_mut().enumerate() {
        let Some(page) = frame_allocator::alloc_page() else {
            for prior in &pages[..index] {
                frame_allocator::free_page(*prior);
            }
            return None;
        };
        *slot = page;
    }
    let [l0, l1_user, l2_user, l3_image, l3_heap, l3_stack, l1_priv, l2_priv, l3_priv] = pages;

    unsafe {
        for &page in &pages {
            zero_page(page);
        }

        write_entry(l0, 0, l1_user as u64 | DESC_TABLE);
        write_entry(l0, PRIVATE_WINDOW_L0_INDEX, l1_priv as u64 | DESC_TABLE);
        write_entry(l1_user, 1, l2_user as u64 | DESC_TABLE);
        write_entry(l2_user, 0, l3_image as u64 | DESC_TABLE);
        write_entry(l2_user, USER_HEAP_L2_INDEX, l3_heap as u64 | DESC_TABLE);

        if let Some(image) = image {
            for index in 0..image.page_count {
                let physical = image.physical_start + index * GRANULE;
                let descriptor = match image.kinds[index] {
                    crate::elf_loader::PageKind::Text => user_text_page(physical),
                    crate::elf_loader::PageKind::ReadOnly => user_rodata_page(physical),
                    crate::elf_loader::PageKind::Data => user_data_page(physical),
                };
                write_entry(l3_image, index, descriptor);
            }
        }

        if user_stack_start != 0 {
            write_entry(l2_user, USER_STACK_L2_INDEX, l3_stack as u64 | DESC_TABLE);
            for index in 0..USER_STACK_PAGES {
                write_entry(
                    l3_stack,
                    index,
                    user_data_page(user_stack_start + index * GRANULE),
                );
            }
        }

        write_entry(l1_priv, 0, l2_priv as u64 | DESC_TABLE);
        write_entry(l2_priv, 0, l3_priv as u64 | DESC_TABLE);
        write_entry(l3_priv, 0, user_data_page(private_page));
    }

    for page in pages {
        flush_after_table_change(page, GRANULE);
    }
    Some(l0)
}

/// Maps one zeroed, writable and non-executable heap page. The task records
/// the frame for `brk` rollback/shrink; `destroy_task_space` ultimately frees it.
pub fn map_user_heap_page(ttbr0: usize, index: usize, physical: usize) -> bool {
    if index >= USER_HEAP_MAX_PAGES || physical % GRANULE != 0 {
        return false;
    }
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
        let l3_heap = follow_table(l2_user, USER_HEAP_L2_INDEX);
        if read_entry(l3_heap, index) != 0 {
            return false;
        }
        zero_page(physical);
        write_entry(l3_heap, index, user_data_page(physical));
        flush_after_table_change(l3_heap, GRANULE);
    }
    true
}

/// Removes a heap mapping and returns its physical frame to the caller.
pub fn unmap_user_heap_page(ttbr0: usize, index: usize) -> Option<usize> {
    if index >= USER_HEAP_MAX_PAGES {
        return None;
    }
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
        let l3_heap = follow_table(l2_user, USER_HEAP_L2_INDEX);
        let entry = read_entry(l3_heap, index);
        if entry & DESC_PAGE != DESC_PAGE {
            return None;
        }
        write_entry(l3_heap, index, 0);
        flush_after_table_change(l3_heap, GRANULE);
        Some(descriptor_address(entry) as usize)
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

    let physical_start = phys_addr & !(GRANULE - 1);
    let physical_offset = phys_addr - physical_start;
    let mapped_size = physical_offset.checked_add(size)?;
    let page_count = mapped_size.div_ceil(GRANULE);
    if page_count == 0 || page_count > ENTRIES {
        return None;
    }
    map_user_pages(
        task,
        physical_start,
        page_count,
        physical_offset,
        user_device_page,
    )
}

/// Maps newly allocated DMA frames as Normal Non-Cacheable, never as Device or
/// cacheable RAM. The range must be page-aligned and wholly inside known DRAM.
pub fn map_dma_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    if size == 0 || phys_addr % GRANULE != 0 || size % GRANULE != 0 {
        return None;
    }
    let physical_end = phys_addr.checked_add(size)?;
    if phys_addr < DRAM_BASE || physical_end > DRAM_END {
        return None;
    }
    let page_count = size / GRANULE;
    if page_count > ENTRIES {
        return None;
    }

    // The allocator and zeroing path use the cacheable kernel alias. Push those
    // lines to the point of coherency before the device can observe the buffer.
    clean_dma_range(phys_addr, size);
    map_user_pages(task, phys_addr, page_count, 0, user_dma_page)
}

/// Maps a registered DMA allocation shared by another task. The scheduler
/// validates ownership of the physical range before calling this function.
pub fn map_shared_dma_to_user(
    task: &crate::scheduler::Task,
    phys_addr: usize,
    size: usize,
) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let physical_end = phys_addr.checked_add(size)?;
    if phys_addr < DRAM_BASE || physical_end > DRAM_END {
        return None;
    }

    let physical_start = phys_addr & !(GRANULE - 1);
    let physical_offset = phys_addr - physical_start;
    let mapped_size = physical_offset.checked_add(size)?;
    let page_count = mapped_size.div_ceil(GRANULE);
    if page_count == 0 || page_count > ENTRIES {
        return None;
    }
    map_user_pages(
        task,
        physical_start,
        page_count,
        physical_offset,
        user_dma_page,
    )
}

fn map_user_pages(
    task: &crate::scheduler::Task,
    physical_start: usize,
    page_count: usize,
    physical_offset: usize,
    descriptor: fn(usize) -> u64,
) -> Option<usize> {
    unsafe {
        let l3_mmio = user_mmio_table(task.ttbr0)?;
        let mut first = ENTRIES;
        let mut run_length = 0;
        for index in 0..ENTRIES {
            if read_entry(l3_mmio, index) == 0 {
                if run_length == 0 {
                    first = index;
                }
                run_length += 1;
                if run_length == page_count {
                    break;
                }
            } else {
                run_length = 0;
            }
        }
        if run_length != page_count {
            return None;
        }

        for page_index in 0..page_count {
            write_entry(
                l3_mmio,
                first + page_index,
                descriptor(physical_start + page_index * GRANULE),
            );
        }
        // The descriptor lives in memory, so it has to reach the point of
        // coherency before the stale translation is dropped; otherwise the
        // walker can still load the old (empty) slot and the task keeps seeing
        // the previous mapping of this window.
        clean_table_lines(l3_mmio, GRANULE);
        let virtual_address = USER_MMIO_VA + first * GRANULE;
        for page_index in 0..page_count {
            invalidate_user_page(virtual_address + page_index * GRANULE);
        }
        Some(virtual_address + physical_offset)
    }
}

fn clean_dma_range(physical: usize, size: usize) {
    let start = phys_to_virt(physical) & !63;
    let end = (phys_to_virt(physical) + size + 63) & !63;
    unsafe {
        let mut line = start;
        while line < end {
            core::arch::asm!("dc cvac, {}", in(reg) line, options(nostack));
            line += 64;
        }
        core::arch::asm!("dsb ishst", options(nostack));
    }
}

fn user_mmio_table(ttbr0: usize) -> Option<usize> {
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let existing = read_entry(l1_user, USER_MMIO_L1_INDEX);
        if existing & DESC_TABLE == DESC_TABLE {
            let l2_mmio = descriptor_address(existing) as usize;
            let l3_entry = read_entry(l2_mmio, USER_MMIO_L2_INDEX);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                return None;
            }
            return Some(descriptor_address(l3_entry) as usize);
        }
        if existing != 0 {
            return None;
        }

        let l2_mmio = frame_allocator::alloc_page()?;
        zero_page(l2_mmio);
        let Some(l3_mmio) = frame_allocator::alloc_page() else {
            frame_allocator::free_page(l2_mmio);
            return None;
        };
        zero_page(l3_mmio);
        write_entry(l2_mmio, USER_MMIO_L2_INDEX, l3_mmio as u64 | DESC_TABLE);
        write_entry(l1_user, USER_MMIO_L1_INDEX, l2_mmio as u64 | DESC_TABLE);
        flush_after_table_change(l3_mmio, GRANULE);
        flush_after_table_change(l2_mmio, GRANULE);
        flush_after_table_change(l1_user, GRANULE);
        Some(l3_mmio)
    }
}

pub fn destroy_task_space(ttbr0: usize) {
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
        let image_table_entry = read_entry(l2_user, 0);
        if image_table_entry & DESC_TABLE == DESC_TABLE {
            frame_allocator::free_page(descriptor_address(image_table_entry) as usize);
        }
        let heap_table_entry = read_entry(l2_user, USER_HEAP_L2_INDEX);
        if heap_table_entry & DESC_TABLE == DESC_TABLE {
            let l3_heap = descriptor_address(heap_table_entry) as usize;
            for index in 0..USER_HEAP_MAX_PAGES {
                let entry = read_entry(l3_heap, index);
                if entry & DESC_PAGE == DESC_PAGE {
                    frame_allocator::free_page(descriptor_address(entry) as usize);
                }
            }
            frame_allocator::free_page(l3_heap);
        }
        let stack_table_entry = read_entry(l2_user, USER_STACK_L2_INDEX);
        if stack_table_entry & DESC_TABLE == DESC_TABLE {
            frame_allocator::free_page(descriptor_address(stack_table_entry) as usize);
        }
        let mmio_table_entry = read_entry(l1_user, USER_MMIO_L1_INDEX);
        if mmio_table_entry & DESC_TABLE == DESC_TABLE {
            let l2_mmio = descriptor_address(mmio_table_entry) as usize;
            let l3_entry = read_entry(l2_mmio, USER_MMIO_L2_INDEX);
            if l3_entry & DESC_TABLE == DESC_TABLE {
                frame_allocator::free_page(descriptor_address(l3_entry) as usize);
            }
            frame_allocator::free_page(l2_mmio);
        }

        let l1_priv = follow_table(ttbr0, 1);
        let l2_priv = follow_table(l1_priv, 0);
        let l3_priv = follow_table(l2_priv, 0);
        let private_page = descriptor_address(read_entry(l3_priv, 0));
        frame_allocator::free_page(private_page as usize);
        frame_allocator::free_page(l3_priv);
        frame_allocator::free_page(l2_priv);
        frame_allocator::free_page(l1_priv);
        frame_allocator::free_page(l2_user);
        frame_allocator::free_page(l1_user);
        frame_allocator::free_page(ttbr0);
    }
}

/// Switches only the user half. The executing high-half kernel code and its
/// stack remain visible through TTBR1_EL1 for every task.
pub fn switch_address_space(ttbr0: usize) {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr = in(reg) ttbr0 as u64,
            options(nostack),
        );
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

fn valid_user_access(pointer: u64, length: u64, write: bool) -> bool {
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

fn current_user_page_access(virtual_address: u64, write: bool) -> bool {
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

fn descriptor_address(descriptor_or_address: u64) -> u64 {
    descriptor_or_address & ADDRESS_MASK
}

/// User ELF text: readable/executable at EL0, never executable at EL1.
fn user_text_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_BOTH_RO
        | SH_INNER
        | AF
        | PXN
}

/// User ELF rodata: read-only and never executable at either level.
fn user_rodata_page(output_address: usize) -> u64 {
    user_text_page(output_address) | UXN
}

/// Task-owned writable memory: user stack or private page, never executable.
fn user_data_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_EL0_RW
        | SH_INNER
        | AF
        | UXN
        | PXN
}

/// User MMIO: MAIR index 1 is 0x00, Device-nGnRnE, never executable.
fn user_device_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_DEVICE << 2)
        | AP_EL0_RW
        | SH_NONE
        | AF
        | UXN
        | PXN
}

/// User DMA: MAIR index 2 is 0x44, Normal Non-Cacheable, never executable.
fn user_dma_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL_NC << 2)
        | AP_EL0_RW
        | SH_INNER
        | AF
        | UXN
        | PXN
}

unsafe fn zero_page(physical: usize) {
    unsafe { core::ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, GRANULE) };
}

unsafe fn write_entry(physical: usize, index: usize, descriptor: u64) {
    unsafe {
        core::ptr::write_volatile((phys_to_virt(physical) as *mut u64).add(index), descriptor)
    };
}

unsafe fn read_entry(physical: usize, index: usize) -> u64 {
    unsafe { core::ptr::read_volatile((phys_to_virt(physical) as *const u64).add(index)) }
}

unsafe fn follow_table(physical: usize, index: usize) -> usize {
    descriptor_address(read_entry(physical, index)) as usize
}
