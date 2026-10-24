//! ARMv8-A MMU: identity map of the low MMIO gigabyte and the DRAM gigabyte,
//! 4 KB granule, four-level translation tables with 2 MB block entries at
//! level 2. On top of that, per-task address spaces: every task gets its own
//! L0 with entry[0] sharing the kernel's identity chain and entry[1] pointing
//! at a private gigabyte window (starting at PRIVATE_WINDOW_VA) backed by
//! pages only that task can see through this VA.
//!
//! Coverage of the shared identity half (virtual == physical everywhere):
//!   0x0000_0000..0x4000_0000  device nGnRnE  (GIC @0x0800_0000, UART @0x0900_0000)
//!   0x4000_0000..0x8000_0000  normal WB      (DRAM starts at 0x4000_0000)
//! AP=EL1-RW everywhere: EL0 gets nothing yet, and the identity half means
//! tasks at EL1 can still reach all of kernel memory — real protection
//! arrives with the move to EL0.

use crate::{frame_allocator, memory::DRAM_BASE};

const ENTRIES: usize = 512;
const GRANULE: usize = 4096;
const TABLE_BYTES: usize = ENTRIES * 8;
const BLOCK_2M: usize = 0x20_0000;

const DESC_TABLE: u64 = 0b11;
const DESC_BLOCK: u64 = 0b01;
const DESC_PAGE: u64 = 0b11;

/// VA that every task maps privately: L0 index 1, L1/L2/L3 index 0.
/// Two tasks writing here hit two different physical pages.
pub const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;

/// MAIR_EL1 attribute indexes (see MAIR_VALUE below).
const ATTR_NORMAL: u64 = 0; // MAIR attr0 = 0xFF: Normal, inner+outer WB RW-allocate
const ATTR_DEVICE: u64 = 1; // MAIR attr1 = 0x00: Device-nGnRnE

const AP_EL1_RW: u64 = 0b00 << 6; // EL1 read/write, EL0 no access
const AF: u64 = 1 << 10; // Access Flag: never fault on access
const SH_NONE: u64 = 0b00 << 8; // non-shareable (device memory)
const SH_INNER: u64 = 0b11 << 8; // inner-shareable (normal memory)
const UXN: u64 = 1 << 54; // not executable at EL0 (PXN stays 0: EL1 may execute)

const ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// MAIR_EL1: attr0 = 0xFF (normal WB), attr1 = 0x00 (device-nGnRnE).
const MAIR_VALUE: u64 = 0x00FF;

/// TCR_EL1: 48-bit VA (T0SZ=16, full four-level walk), 4 KB granule,
/// inner-shareable walks, table walk caches WB RW-allocate, 40-bit PA,
/// TTBR1 walks disabled (we only use TTBR0).
const TCR_VALUE: u64 = 16 // T0SZ
    | (0b11 << 12) // SH0 = inner shareable
    | (0b01 << 10) // ORGN0 = WB RW-allocate
    | (0b01 << 8) // IRGN0 = WB RW-allocate
    | (1 << 23) // EPD1: TTBR1 disabled
    | (0b010 << 32); // IPS = 40-bit physical address
                     // TG0 = 0b00: 4 KB granule (zero bits [15:14])

/// One 4 KB translation table (512 × 8-byte descriptors).
/// The four tables are one array so cache maintenance covers them with a
/// single contiguous range.
#[repr(C, align(4096))]
struct Table {
    entries: [u64; ENTRIES],
}

const TABLE_COUNT: usize = 4;

const EMPTY_TABLE: Table = Table {
    entries: [0; ENTRIES],
};

static mut TABLES: [Table; TABLE_COUNT] = [EMPTY_TABLE; TABLE_COUNT];

const INDEX_LEVEL0: usize = 0;
const INDEX_LEVEL1: usize = 1;
const INDEX_LEVEL2_MMIO: usize = 2;
const INDEX_LEVEL2_DRAM: usize = 3;

/// Fills the tables and turns on translation. Call once, early in
/// kernel_main, before anything depends on mapped memory being "normal".
pub fn init() {
    unsafe {
        // Raw pointers: four disjoint tables, borrow checker cannot see that.
        let base = core::ptr::addr_of_mut!(TABLES) as *mut Table;
        let level0 = &mut *base.add(INDEX_LEVEL0);
        let level1 = &mut *base.add(INDEX_LEVEL1);
        let mmio = &mut *base.add(INDEX_LEVEL2_MMIO);
        let dram = &mut *base.add(INDEX_LEVEL2_DRAM);

        level0.entries[0] = table_descriptor(level1);
        level1.entries[0] = table_descriptor(mmio); // VA 0x0000_0000, 1 GB window
        level1.entries[1] = table_descriptor(dram); // VA 0x4000_0000, 1 GB window

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
    ((table as *const Table as u64) & ADDRESS_MASK) | DESC_TABLE
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

/// Pushes the freshly written tables out to the point of coherency. If the
/// data cache were enabled, the walker could otherwise read stale lines.
/// (Caches are off at this point in boot; this is the ritual that keeps
/// working when we turn them on.)
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
            line += 64; // 64-byte cache line on Cortex-A72
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Programs TTBR0_EL1 / MAIR_EL1 / TCR_EL1 and flips SCTLR_EL1.M.
///
/// Ordering rules that keep this free of TLB hazards:
/// - TTBR/TCR/MAIR writes are made visible with ISB *before* M goes on;
/// - identity mapping means the instruction fetch right after enabling
///   translates to the very same physical address — no far-jump dance;
/// - the TLB invalidate afterwards is belt-and-braces: nothing was ever
///   translated on this CPU, but any *future* table edit must follow the
///   same "modify tables → clean → tlbi → dsb → isb" ritual.
fn configure_and_enable() {
    let ttbr0 = (core::ptr::addr_of!(TABLES) as u64) & ADDRESS_MASK;
    let mut sctlr: u64 = 0;

    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "isb",
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, #1", // SCTLR_EL1.M: enable translation
            "msr sctlr_el1, {sctlr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr = in(reg) ttbr0,
            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) TCR_VALUE,
            sctlr = inout(reg) sctlr,
            options(nostack),
        );
    }
}

/// Call after ANY change to translation tables:
/// clean the edited pages, invalidate affected TLB entries, and serialize.
pub fn flush_after_table_change(table_start: usize, table_bytes: usize) {
    let start = table_start & !63;
    let end = (table_start + table_bytes + 63) & !63;
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
// Per-task address spaces
// ---------------------------------------------------------------------------

/// Builds a fresh address space for one task. Layout of its private L0:
///   entry[0] → the SHARED level-1 table of the kernel identity half
///              (kernel, devices, heap, stacks: identical in every task);
///   entry[1] → the task's own L1→L2→L3 chain, mapping PRIVATE_WINDOW_VA to
///              `private_page` (one 4 KB page, EL1-RW, normal WB).
/// Table pages come from the frame allocator; they are reachable through
/// the kernel identity half, so filling them by physical address is safe.
///
/// Returns the physical base to load into TTBR0_EL1.
pub fn create_task_space(private_page: usize) -> Option<usize> {
    let l0 = frame_allocator::alloc_page()?;
    let Some(l1) = frame_allocator::alloc_page() else {
        frame_allocator::free_page(l0);
        return None;
    };
    let Some(l2) = frame_allocator::alloc_page() else {
        frame_allocator::free_page(l1);
        frame_allocator::free_page(l0);
        return None;
    };
    let Some(l3) = frame_allocator::alloc_page() else {
        frame_allocator::free_page(l2);
        frame_allocator::free_page(l1);
        frame_allocator::free_page(l0);
        return None;
    };

    unsafe {
        zero_page(l0);
        zero_page(l1);
        zero_page(l2);
        zero_page(l3);

        write_entry(l0, 0, descriptor_address(identity_l1_address() as u64) | DESC_TABLE);
        write_entry(l0, 1, descriptor_address(l1 as u64) | DESC_TABLE);
        write_entry(l1, 0, descriptor_address(l2 as u64) | DESC_TABLE);
        write_entry(l2, 0, descriptor_address(l3 as u64) | DESC_TABLE);
        write_entry(l3, 0, page_descriptor(private_page));
    }

    for page in [l0, l1, l2, l3] {
        flush_after_table_change(page, GRANULE);
    }
    Some(l0)
}

/// Frees every page of a task space created by `create_task_space`,
/// including the private data page (recovered from the L3 descriptor).
pub fn destroy_task_space(ttbr0: usize) {
    unsafe {
        let l1 = follow_table(ttbr0, 1);
        let l2 = follow_table(l1, 0);
        let l3 = follow_table(l2, 0);
        let private_page = descriptor_address(read_entry(l3, 0));
        frame_allocator::free_page(private_page as usize);
        frame_allocator::free_page(l3);
        frame_allocator::free_page(l2);
        frame_allocator::free_page(l1);
        frame_allocator::free_page(ttbr0);
    }
}

/// Loads TTBR0_EL1 and flushes the TLB. Call only on the context-switch
/// path. Safe mid-switch because everything this code touches until the
/// register/SP restore lives in the identity half, which is identical in
/// both address spaces.
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

fn identity_l1_address() -> usize {
    core::ptr::addr_of!(TABLES) as usize + INDEX_LEVEL1 * TABLE_BYTES
}

fn descriptor_address(descriptor_or_address: u64) -> u64 {
    descriptor_or_address & ADDRESS_MASK
}

fn page_descriptor(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_EL1_RW
        | SH_INNER
        | AF
        | UXN
}

unsafe fn zero_page(base: usize) {
    unsafe { core::ptr::write_bytes(base as *mut u8, 0, GRANULE) };
}

unsafe fn write_entry(base: usize, index: usize, descriptor: u64) {
    unsafe { core::ptr::write_volatile((base as *mut u64).add(index), descriptor) };
}

unsafe fn read_entry(base: usize, index: usize) -> u64 {
    unsafe { core::ptr::read_volatile((base as *const u64).add(index)) }
}

unsafe fn follow_table(base: usize, index: usize) -> usize {
    descriptor_address(read_entry(base, index)) as usize
}
