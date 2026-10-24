//! ARMv8-A MMU with a high-half kernel and per-task user address spaces.
//!
//! TTBR1_EL1 maps physical memory at `KERNEL_VIRT_BASE + PA` for EL1 only:
//! kernel code/data/stacks, page tables, the kernel heap, UART and GIC all
//! live there. TTBR0_EL1 belongs to the current task and contains only its
//! independently loaded ELF image, its own user stack and one private page.
//! No kernel image, kernel data or MMIO pages are shared with EL0.

use crate::{
    frame_allocator,
    memory::{phys_to_virt, virt_to_phys, DRAM_BASE},
};

const ENTRIES: usize = 512;
const GRANULE: usize = 4096;
const TABLE_BYTES: usize = ENTRIES * 8;
const BLOCK_2M: usize = 0x20_0000;

const DESC_TABLE: u64 = 0b11;
const DESC_BLOCK: u64 = 0b01;
const DESC_PAGE: u64 = 0b11;

/// Private 4 KB data window: L0 index 1, then L1/L2/L3 index 0.
pub const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;
const PRIVATE_WINDOW_L0_INDEX: usize = (PRIVATE_WINDOW_VA >> 39) & 0x1FF;
/// Every task's own stack appears at this user VA, backed by different PAs.
pub const USER_STACK_VA: usize = 0x0000_0000_7FE0_0000;
pub const USER_STACK_PAGES: usize = 2;
pub const USER_STACK_SIZE: usize = USER_STACK_PAGES * GRANULE;
const USER_STACK_L2_INDEX: usize = (USER_STACK_VA - DRAM_BASE) / BLOCK_2M;

const ATTR_NORMAL: u64 = 0;
const ATTR_DEVICE: u64 = 1;

const AP_EL1_RW: u64 = 0b00 << 6;
const AP_EL0_RW: u64 = 0b01 << 6;
const AP_BOTH_RO: u64 = 0b11 << 6;
const AF: u64 = 1 << 10;
const SH_NONE: u64 = 0b00 << 8;
const SH_INNER: u64 = 0b11 << 8;
const UXN: u64 = 1 << 54;
const PXN: u64 = 1 << 53;

const ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_F000;
const MAIR_VALUE: u64 = 0x00FF;

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

const TABLE_COUNT: usize = 5;
const EMPTY_TABLE: Table = Table {
    entries: [0; ENTRIES],
};

/// Referenced by the assembly boot stub, which fills these five tables before
/// enabling the MMU. Keep the indices in sync with main.rs.
#[no_mangle]
static mut TABLES: [Table; TABLE_COUNT] = [EMPTY_TABLE; TABLE_COUNT];

const INDEX_LOW_LEVEL0: usize = 0;
const INDEX_HIGH_LEVEL0: usize = 1;
const INDEX_LEVEL1: usize = 2;
const INDEX_LEVEL2_MMIO: usize = 3;
const INDEX_LEVEL2_DRAM: usize = 4;

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

        // We execute through TTBR1 now; the low identity alias is no longer
        // needed by the kernel. User TTBR0s are installed by the scheduler.
        low0.entries[0] = 0;
        high0.entries[0] = table_descriptor(level1);
        level1.entries[0] = table_descriptor(mmio);
        level1.entries[1] = table_descriptor(dram);

        for index in 0..ENTRIES {
            mmio.entries[index] = block_descriptor(index * BLOCK_2M, ATTR_DEVICE, SH_NONE);
            dram.entries[index] =
                block_descriptor(DRAM_BASE + index * BLOCK_2M, ATTR_NORMAL, SH_INNER);
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
            "bic {sctlr}, {sctlr}, #(1 << 22)",
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
        core::arch::asm!(
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
}

// ---------------------------------------------------------------------------
// Per-task user address spaces
// ---------------------------------------------------------------------------

/// Builds a user TTBR0. `user_stack_start == 0` creates no stack mapping
/// (used for the EL1 idle task). A loaded ELF supplies independently owned
/// image pages instead of the old shared kernel text/rodata alias.
pub fn create_task_space(
    private_page: usize,
    user_stack_start: usize,
    image: Option<&crate::elf_loader::UserImage>,
) -> Option<usize> {
    let mut pages = [0_usize; 8];
    for (index, slot) in pages.iter_mut().enumerate() {
        let Some(page) = frame_allocator::alloc_page() else {
            for prior in &pages[..index] {
                frame_allocator::free_page(*prior);
            }
            return None;
        };
        *slot = page;
    }
    let [l0, l1_user, l2_user, l3_image, l3_stack, l1_priv, l2_priv, l3_priv] = pages;

    unsafe {
        for &page in &pages {
            zero_page(page);
        }

        write_entry(l0, 0, l1_user as u64 | DESC_TABLE);
        write_entry(l0, PRIVATE_WINDOW_L0_INDEX, l1_priv as u64 | DESC_TABLE);
        write_entry(l1_user, 1, l2_user as u64 | DESC_TABLE);
        write_entry(l2_user, 0, l3_image as u64 | DESC_TABLE);

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

pub fn destroy_task_space(ttbr0: usize) {
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
        let image_table_entry = read_entry(l2_user, 0);
        if image_table_entry & DESC_TABLE == DESC_TABLE {
            frame_allocator::free_page(descriptor_address(image_table_entry) as usize);
        }
        let stack_table_entry = read_entry(l2_user, USER_STACK_L2_INDEX);
        if stack_table_entry & DESC_TABLE == DESC_TABLE {
            frame_allocator::free_page(descriptor_address(stack_table_entry) as usize);
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

/// Accepts only pages actually mapped in the current task's TTBR0. This
/// covers its own ELF image, fixed-VA stack and private page without baking
/// linker addresses or image sizes into the syscall layer.
pub fn valid_user_slice(pointer: u64, length: u64) -> bool {
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
        if !current_user_page_present(page) {
            return false;
        }
        if page == last_page {
            return true;
        }
        page += GRANULE as u64;
    }
}

fn current_user_page_present(virtual_address: u64) -> bool {
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
        read_entry(table, index) & DESC_PAGE == DESC_PAGE
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
