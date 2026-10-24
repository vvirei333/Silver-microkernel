//! ARMv8-A MMU with a high-half kernel and per-task user address spaces.
//!
//! TTBR1_EL1 maps physical memory at `KERNEL_VIRT_BASE + PA` for EL1 only:
//! kernel code/data/stacks, page tables, the kernel heap, UART and GIC all
//! live there. TTBR0_EL1 belongs to the current task and contains only what
//! EL0 needs: a read-only low alias of the built-in text/rodata image, the
//! task's own user stack, and one private data page. There are no kernel
//! data or MMIO mappings in a task's TTBR0.
//!
//! The text/rodata alias is a deliberate compatibility measure until user
//! programs are separate ELF binaries. It is read-only and contains no
//! writable kernel state; the ELF loader is the step that removes it.

use crate::{
    frame_allocator,
    memory::{phys_to_virt, virt_to_phys, MemoryRegion, DRAM_BASE},
};

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __data_start: u8;
    static __kernel_start: u8;
}

const ENTRIES: usize = 512;
const GRANULE: usize = 4096;
const TABLE_BYTES: usize = ENTRIES * 8;
const BLOCK_2M: usize = 0x20_0000;

const DESC_TABLE: u64 = 0b11;
const DESC_BLOCK: u64 = 0b01;
const DESC_PAGE: u64 = 0b11;

/// Private 4 KB data window: L0 index 1, then L1/L2/L3 index 0.
pub const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;
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

const TABLE_COUNT: usize = 6;
const EMPTY_TABLE: Table = Table {
    entries: [0; ENTRIES],
};

/// Referenced by the assembly boot stub, which fills the first five tables
/// before enabling the MMU. Keep the indices in sync with main.rs.
#[no_mangle]
static mut TABLES: [Table; TABLE_COUNT] = [EMPTY_TABLE; TABLE_COUNT];

const INDEX_LOW_LEVEL0: usize = 0;
const INDEX_HIGH_LEVEL0: usize = 1;
const INDEX_LEVEL1: usize = 2;
const INDEX_LEVEL2_MMIO: usize = 3;
const INDEX_LEVEL2_DRAM: usize = 4;
const INDEX_USER_IMAGE_L3: usize = 5;

/// Replaces the temporary boot identity TTBR0 with an empty kernel TTBR0,
/// retains the high TTBR1 map, and builds the shared read-only user image.
/// Called from kernel_main while already executing at a high PC.
pub fn init() {
    unsafe {
        let base = core::ptr::addr_of_mut!(TABLES) as *mut Table;
        let low0 = &mut *base.add(INDEX_LOW_LEVEL0);
        let high0 = &mut *base.add(INDEX_HIGH_LEVEL0);
        let level1 = &mut *base.add(INDEX_LEVEL1);
        let mmio = &mut *base.add(INDEX_LEVEL2_MMIO);
        let dram = &mut *base.add(INDEX_LEVEL2_DRAM);
        let user_image = &mut *base.add(INDEX_USER_IMAGE_L3);

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
        fill_user_image_l3(user_image);
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
/// (used for the EL1 idle task); the private page is still allocated/freed
/// uniformly for every task.
///
/// Owned pages: L0, L1_user, L2_user, L3_stack, and three private-window
/// levels. The read-only image L3 is shared and never freed per task.
pub fn create_task_space(private_page: usize, user_stack_start: usize) -> Option<usize> {
    let mut pages = [0_usize; 7];
    for (index, slot) in pages.iter_mut().enumerate() {
        let Some(page) = frame_allocator::alloc_page() else {
            for prior in &pages[..index] {
                frame_allocator::free_page(*prior);
            }
            return None;
        };
        *slot = page;
    }
    let [l0, l1_user, l2_user, l3_stack, l1_priv, l2_priv, l3_priv] = pages;

    unsafe {
        for &page in &pages {
            zero_page(page);
        }

        write_entry(l0, 0, l1_user as u64 | DESC_TABLE);
        write_entry(l0, 1, l1_priv as u64 | DESC_TABLE);
        write_entry(l1_user, 1, l2_user as u64 | DESC_TABLE);

        let shared_image =
            virt_to_phys(core::ptr::addr_of!(TABLES) as usize + INDEX_USER_IMAGE_L3 * TABLE_BYTES);
        write_entry(l2_user, 0, shared_image as u64 | DESC_TABLE);

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

/// Shared low user alias: only .text and .rodata are present. Kernel data,
/// stacks, heap, MMIO and every other DRAM page are deliberately unmapped.
unsafe fn fill_user_image_l3(table: &mut Table) {
    let text_start = virt_to_phys(core::ptr::addr_of!(__text_start) as usize);
    let text_end = virt_to_phys(core::ptr::addr_of!(__text_end) as usize);
    let data_start = virt_to_phys(core::ptr::addr_of!(__data_start) as usize);

    for index in 0..ENTRIES {
        let address = DRAM_BASE + index * GRANULE;
        table.entries[index] = if address >= text_start && address < text_end {
            kernel_text_page(address)
        } else if address >= text_end && address < data_start {
            kernel_rodata_page(address)
        } else {
            0
        };
    }
}

pub fn destroy_task_space(ttbr0: usize) {
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
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

/// Accepts only memory a user task can legitimately pass to the kernel:
/// the shared read-only image, its own fixed-VA stack, or its private page.
pub fn valid_user_slice(pointer: u64, length: u64) -> bool {
    if length > 4096 {
        return false;
    }
    let Some(last) = pointer.checked_add(length) else {
        return false;
    };
    let pointer = pointer as usize;
    let last = last as usize;
    let stack = MemoryRegion::new(USER_STACK_VA, USER_STACK_VA + USER_STACK_SIZE);
    let private = MemoryRegion::new(PRIVATE_WINDOW_VA, PRIVATE_WINDOW_VA + GRANULE);
    let image = user_image_range();
    [image, stack, private]
        .iter()
        .any(|region| pointer >= region.start && last <= region.end)
}

fn user_image_range() -> MemoryRegion {
    MemoryRegion::new(
        virt_to_phys(core::ptr::addr_of!(__kernel_start) as usize),
        virt_to_phys(core::ptr::addr_of!(__data_start) as usize),
    )
}

fn descriptor_address(descriptor_or_address: u64) -> u64 {
    descriptor_or_address & ADDRESS_MASK
}

/// Built-in .text, mapped read-only and executable at EL0 and EL1. RO at EL0
/// also avoids the ARM rule that an EL0-writable page is EL1-execute-never.
fn kernel_text_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_BOTH_RO
        | SH_INNER
        | AF
}

fn kernel_rodata_page(output_address: usize) -> u64 {
    kernel_text_page(output_address) | UXN | PXN
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
