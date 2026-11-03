//! ARMv8-A MMU with a high-half kernel and per-task user address spaces.
//!
//! TTBR1_EL1 maps physical memory at `KERNEL_VIRT_BASE + PA` for EL1 only:
//! kernel code/data/stacks, page tables, the kernel heap, UART and GIC all
//! live there. TTBR0_EL1 belongs to the current task and contains only its
//! independently loaded ELF image, sparse heap, user stack, private page,
//! explicitly authorized Device-nGnRnE MMIO and Normal Non-Cacheable DMA.
//! No kernel image or kernel data is shared with EL0.

use silver_microkernel::table_registry::{
    RetireResult, SpaceRetire, TableRegistry, TableToken,
};

use crate::{
    frame_allocator,
    memory::{phys_to_virt, virt_to_phys, DRAM_BASE, DRAM_END},
    smp::SpinLock,
};

const ENTRIES: usize = 512;
const GRANULE: usize = 4096;
const TABLE_BYTES: usize = ENTRIES * 8;
const BLOCK_2M: usize = 0x20_0000;
const PHYSICAL_ADDRESS_LIMIT: usize = 1 << 40;
/// L1 slot that maps the first DRAM gigabyte; identity (VA == PA) at the
/// right index. On the tablet a second entry (`DRAM_L1_INDEX + 1`) maps the
/// second gigabyte, so the frame-allocator region at 0xD510_0000 is covered.
const DRAM_L1_INDEX: usize = DRAM_BASE >> 30;

const DESC_TABLE: u64 = 0b11;
const DESC_BLOCK: u64 = 0b01;
const DESC_PAGE: u64 = 0b11;

/// Registry of physical frames whose contents are page tables.
///
/// There is no allocator-level notion of "page-table frame": every table is a
/// tagged `alloc_page_tagged()` result. The registry stores the frame, its
/// generation and the owning address space (the physical root frame), and
/// hands out a [`TableToken`] at registration. Freeing a table requires the
/// token, so a stale caller that only remembers the old address cannot free a
/// frame that was recycled as another table: the generation in its token no
/// longer matches the live record and the release is refused (the frame leaks,
/// which is the safe failure).
///
/// This registry is also the ground truth for revoke's table validation: a
/// frame is registered exactly where a table is created (`create_task_space`,
/// `map_user_pages_at`, `user_mmio_table`, `install_kernel_stack_arena`) and
/// retired exactly where it is freed. A descriptor naming any frame outside
/// the registry — freed, data, or corrupted — cannot be distinguished from a
/// table by its contents alone, so validation fails closed
/// (`safe_to_free = false`, frames leak) instead of reading attacker- or
/// corruption-controlled data as a page table.
///
/// The registry lives behind a spinlock: two CPUs create tables (and retire
/// them) concurrently, and a torn push/swap_remove here would defeat the
/// revoke validation. All registry sites run with the kernel lock held, so
/// the spin is bounded.
///
/// Invariants, all enforced loudly rather than silently:
/// - at most one registry record per frame: registration happens on fresh
///   `alloc_page_tagged()` results only (a registered frame is used, so the
///   allocator cannot hand it out again), and the registration path refuses
///   a duplicate with a diagnostic and a debug assertion;
/// - a frame is linked into a page table only after a successful
///   registration: every creation site registers before its `write_entry`,
///   and on registration failure frees the still-unlinked frame directly;
/// - a table is freed only through a token (exact generation match) or a
///   walk of the owning space (`retire_owned`); neither path can free a
///   record whose generation/owner does not match.
static PAGE_TABLE_FRAMES: SpinLock<TableRegistry> = SpinLock::new(TableRegistry::new());

/// Registers a freshly created table frame with the generation tag from its
/// allocation and the owning space root. Returns `None` — leaving the frame
/// unregistered — on duplicate registration (kernel bug) or when the registry
/// cannot grow (out of memory). The caller must then not link the frame into
/// any table and release it with the tagged allocation handle.
fn page_table_register(frame: usize, generation: u32, owner: usize) -> Option<TableToken> {
    let mut frames = PAGE_TABLE_FRAMES.lock();
    match frames.register(frame, generation, owner) {
        Some(token) => Some(token),
        None => {
            crate::console::write_str("PAGE TABLE REGISTRY: duplicate frame or out of memory, frame 0x");
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" not registered\r\n");
            None
        }
    }
}

fn page_table_registered(frame: usize) -> bool {
    PAGE_TABLE_FRAMES.lock().contains(frame)
}

/// Frees a page-table frame only when `token` is the live registration.
///
/// Fail-closed on every refusal: a stale token, an already-retired frame or a
/// mis-owned record leaves the frame allocated and registered, so a leaked
/// frame can never be reallocated as data while a table still references it.
fn free_table_frame(token: TableToken) -> bool {
    let result = PAGE_TABLE_FRAMES.lock().retire(token);
    match result {
        RetireResult::Retired { generation } => {
            if frame_allocator::free_page_tagged(token.frame(), generation) {
                true
            } else {
                crate::console::write_str("PAGE TABLE REGISTRY: allocator refused frame 0x");
                crate::console::write_hex_u64(token.frame() as u64);
                crate::console::write_str(" - leaked\r\n");
                false
            }
        }
        RetireResult::NotRegistered => {
            crate::console::write_str("PAGE TABLE REGISTRY: frame 0x");
            crate::console::write_hex_u64(token.frame() as u64);
            crate::console::write_str(" is not registered - leaked, not freed\r\n");
            false
        }
        RetireResult::StaleToken => {
            crate::console::write_str("PAGE TABLE REGISTRY: stale token for frame 0x");
            crate::console::write_hex_u64(token.frame() as u64);
            crate::console::write_str(" - leaked, not freed\r\n");
            false
        }
        RetireResult::WrongOwner => {
            crate::console::write_str("PAGE TABLE REGISTRY: owner mismatch for frame 0x");
            crate::console::write_hex_u64(token.frame() as u64);
            crate::console::write_str(" - leaked, not freed\r\n");
            false
        }
    }
}

/// Frees a table frame reached by walking `owner`'s page tables. The owner
/// check refuses a frame that a different (recycled) address space owns.
fn free_table_frame_owned(frame: usize, owner: usize) -> bool {
    let result = PAGE_TABLE_FRAMES.lock().retire_owned(frame, owner);
    match result {
        RetireResult::Retired { generation } => {
            frame_allocator::free_page_tagged(frame, generation)
        }
        RetireResult::NotRegistered => {
            crate::console::write_str("PAGE TABLE REGISTRY: walked frame 0x");
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" is not registered - leaked\r\n");
            false
        }
        RetireResult::WrongOwner => {
            crate::console::write_str("PAGE TABLE REGISTRY: walked frame 0x");
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" belongs to another space - leaked\r\n");
            false
        }
        RetireResult::StaleToken => false,
    }
}

/// Private 4 KB data window: L0 index 1, then L1/L2/L3 index 0.
pub const PRIVATE_WINDOW_VA: usize = 0x0000_0080_0000_0000;
const PRIVATE_WINDOW_L0_INDEX: usize = (PRIVATE_WINDOW_VA >> 39) & 0x1FF;
/// Physical base of the user address space (the 2 MB user ELF image loads here).
/// In QEMU this is the DRAM base (0x4000_0000); on the tablet it is 0x8000_0000.
pub const USER_IMAGE_VA: usize = DRAM_BASE;
/// A sparse, per-task heap window immediately after the 2 MB ELF image.
/// Its L3 table is installed at spawn, while data pages are mapped by `brk`.
pub const USER_HEAP_VA: usize = USER_IMAGE_VA + 0x20_0000;
pub const USER_HEAP_MAX_PAGES: usize = 512;
pub const USER_HEAP_MAX_SIZE: usize = USER_HEAP_MAX_PAGES * GRANULE;
const USER_HEAP_L2_INDEX: usize = (USER_HEAP_VA - DRAM_BASE) / BLOCK_2M;

/// The lowest stack VA page is deliberately left unmapped as a guard. The
/// stack itself starts one page above it, so a downward overflow faults
/// instead of silently landing in the mmap window below.
pub const USER_STACK_GUARD_VA: usize = USER_IMAGE_VA + 0x3FE0_0000;
/// Every task's own stack appears at this user VA, backed by different PAs.
pub const USER_STACK_VA: usize = USER_STACK_GUARD_VA + GRANULE;
pub const USER_STACK_PAGES: usize = 2;
pub const USER_STACK_SIZE: usize = USER_STACK_PAGES * GRANULE;
const USER_STACK_L2_INDEX: usize = (USER_STACK_VA - DRAM_BASE) / BLOCK_2M;
/// L3 slot of the first mapped stack page. Slot 0 is the guard page; the
/// stack table is zeroed on allocation, so the guard entry stays invalid.
const USER_STACK_L3_INDEX: usize = (USER_STACK_VA >> 12) & 0x1FF;
const _: () = assert!(USER_STACK_L3_INDEX == 1);
const _: () = assert!((USER_STACK_GUARD_VA >> 12) + 1 == (USER_STACK_VA >> 12));
const _: () = assert!(USER_STACK_L3_INDEX + USER_STACK_PAGES <= ENTRIES);
const _: () = assert!(USER_STACK_VA % GRANULE == 0);
const _: () = assert!(USER_STACK_GUARD_VA % GRANULE == 0);

/// A 2 MB first-fit window for explicitly authorized user MMIO and DMA mappings.
pub const USER_MMIO_VA: usize = USER_IMAGE_VA + 0x4000_0000;
pub const USER_DMA_MAX_SIZE: usize = ENTRIES * GRANULE;
const USER_MMIO_L1_INDEX: usize = (USER_MMIO_VA >> 30) & 0x1FF;
const USER_MMIO_L2_INDEX: usize = 0;

pub const PCI_ECAM_BASE: usize = 0x0000_0040_1000_0000;
pub const PCI_ECAM_SIZE: usize = 0x1000_0000;
const PCI_ECAM_L1_INDEX: usize = (PCI_ECAM_BASE >> 30) & 0x1FF;
const PCI_ECAM_L2_INDEX: usize = ((PCI_ECAM_BASE >> 21) & 0x1FF) as usize;
const PCI_ECAM_L2_BLOCKS: usize = PCI_ECAM_SIZE / BLOCK_2M;

/// Anonymous `mmap` region: a large window between the sparse heap and the user
/// stack where `SYS_MMAP` places kernel-chosen mappings. It spans the free
/// `l2_user` slots, so page tables are only allocated once a mapping is placed.
pub const USER_MMAP_VA: usize = USER_IMAGE_VA + 0x40_0000;
/// Anonymous mappings stop below the stack guard page, so they can never
/// fill the hole that makes a stack overflow fault.
pub const USER_MMAP_MAX_VA: usize = USER_STACK_GUARD_VA;

/// Upper bound on the page count of a single anonymous mapping. User-supplied
/// lengths are rejected against this before any span arithmetic.
pub const USER_MMAP_MAX_PAGES: usize = (USER_MMAP_MAX_VA - USER_MMAP_VA) / GRANULE;

/// `prot` bits for anonymous mappings. Read is always granted; these flags
/// select write and execute in `user_anonymous_page`.
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;

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

/// QEMU maps one DRAM gigabyte; the tablet needs two (0x8000_0000..0x1_0000_0000).
#[cfg(feature = "tablet")]
const TABLE_COUNT: usize = 7;
#[cfg(not(feature = "tablet"))]
const TABLE_COUNT: usize = 6;
const EMPTY_TABLE: Table = Table {
    entries: [0; ENTRIES],
};

/// The assembly boot stub fills the first five tables before enabling the MMU.
/// The sixth is zeroed with .bss and populated for PCI ECAM in `init`; on the
/// tablet the seventh holds the second DRAM gigabyte, also populated in `init`.
#[no_mangle]
static mut TABLES: [Table; TABLE_COUNT] = [EMPTY_TABLE; TABLE_COUNT];

const INDEX_LOW_LEVEL0: usize = 0;
const INDEX_HIGH_LEVEL0: usize = 1;
const INDEX_LEVEL1: usize = 2;
const INDEX_LEVEL2_MMIO: usize = 3;
const INDEX_LEVEL2_DRAM: usize = 4;
const INDEX_LEVEL2_PCI: usize = 5;
#[cfg(feature = "tablet")]
const INDEX_LEVEL2_DRAM_HIGH: usize = 6;

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

/// A 4 KiB kernel page: cacheable, EL1 RW, non-executable. Used for the
/// dedicated kernel stack arena after its 2 MiB block is split.
fn kernel_page_descriptor(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_EL1_RW
        | SH_INNER
        | AF
        | UXN
        | PXN
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
/// TTBR1 value of the shared kernel high half, recorded at boot so APs can
/// install the identical kernel mapping without rebuilding it.
static KERNEL_TTBR1: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The kernel-high-half TTBR1 value; valid after [`init`].
pub fn kernel_ttbr1() -> usize {
    KERNEL_TTBR1.load(core::sync::atomic::Ordering::Acquire)
}

fn configure_and_enable() {
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
    unsafe {
        core::arch::asm!(
            "tlbi vmalle1is",
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
///
/// Returns the root frame (the TTBR0 value) together with its [`TableToken`].
/// The token is the only proof that a later teardown still owns this address
/// space: after the root frame is recycled, the stale token is refused.
pub fn create_task_space(
    private_page: usize,
    user_stack_start: usize,
    image: Option<&crate::elf::LoadedImage>,
) -> Option<(usize, TableToken)> {
    let mut pages = [0_usize; 9];
    let mut generations = [0_u32; 9];
    for index in 0..pages.len() {
        let Some((page, generation)) = frame_allocator::alloc_page_tagged() else {
            // Nothing is registered or linked yet: release the raw runs.
            for prior in 0..index {
                frame_allocator::free_page_tagged(pages[prior], generations[prior]);
            }
            return None;
        };
        pages[index] = page;
        generations[index] = generation;
    }

    // All nine frames are allocated before any is linked. Register them with
    // the root frame as owner; the root itself is registered as its own
    // space root, which is what `retire_space` validates.
    let mut tokens = [TableToken::empty(); 9];
    for index in 0..pages.len() {
        let Some(token) = page_table_register(pages[index], generations[index], pages[0]) else {
            // The refused frame is still unlinked; prior frames are
            // registered but also still unlinked.
            frame_allocator::free_page_tagged(pages[index], generations[index]);
            for prior in &tokens[..index] {
                free_table_frame(*prior);
            }
            return None;
        };
        tokens[index] = token;
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
                    crate::elf::PageKind::Text => user_text_page(physical),
                    crate::elf::PageKind::ReadOnly => user_rodata_page(physical),
                    crate::elf::PageKind::Data => user_data_page(physical),
                };
                write_entry(l3_image, index, descriptor);
            }
        }

        if user_stack_start != 0 {
            write_entry(l2_user, USER_STACK_L2_INDEX, l3_stack as u64 | DESC_TABLE);
            // Slot 0 remains zero: that is the guard page below the stack.
            for index in 0..USER_STACK_PAGES {
                let physical = user_stack_start + index * GRANULE;
                // The stack is EL0-visible: never expose a recycled frame that
                // still contains another task's or the kernel's data.
                zero_page(physical);
                write_entry(
                    l3_stack,
                    USER_STACK_L3_INDEX + index,
                    user_data_page(physical),
                );
            }
            debug_assert_eq!(
                read_entry(l3_stack, USER_STACK_L3_INDEX - 1),
                0,
                "user stack guard page must stay unmapped"
            );
        }

        // The private 4 KiB window is writable and readable from EL0, so it
        // gets the same guarantee as the stack and heap.
        zero_page(private_page);
        write_entry(l1_priv, 0, l2_priv as u64 | DESC_TABLE);
        write_entry(l2_priv, 0, l3_priv as u64 | DESC_TABLE);
        write_entry(l3_priv, 0, user_data_page(private_page));
    }

    for page in pages {
        flush_after_table_change(page, GRANULE);
    }
    Some((l0, tokens[0]))
}

/// Maps one zeroed, writable and non-executable heap page. The task records
/// the frame and its generation for `brk` rollback, shrink and teardown.
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
/// Only the user image/heap/stack window (`l2_user`, `[DRAM_BASE, DRAM_BASE+1G)`)
/// is traversed, so `virtual_address` must lie inside it.
pub fn map_user_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
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
    if virtual_address < DRAM_BASE || end > DRAM_BASE + 0x4000_0000 {
        return false;
    }

    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
        // All-or-nothing: `done` counts fully installed pages; every failure
        // path unrolls them before returning. The rollback cannot refuse:
        // each of the `done` slots was written by this call, so its L2 link
        // and L3 descriptor are present (F1-created tables included).
        let mut done = 0usize;
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l2_index = (va - DRAM_BASE) / BLOCK_2M;
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l2_user, l2_index);
            let l3 = if l2_entry == 0 {
                let Some((page, generation)) = frame_allocator::alloc_page_tagged() else {
                    // F1: no frame left for a new L3 table.
                    unmap_user_pages_at(ttbr0, virtual_address, done);
                    return false;
                };
                zero_page(page);
                if page_table_register(page, generation, ttbr0).is_none() {
                    // Registration refused: the L2 entry below is only written
                    // after a successful registration, so the frame is still
                    // unlinked and its own tag is correct.
                    frame_allocator::free_page_tagged(page, generation);
                    unmap_user_pages_at(ttbr0, virtual_address, done);
                    return false;
                }
                // Registered before the first link; the rollback paths below
                // free it through `unmap_user_pages_at`, which retires it as a
                // table owned by this space.
                write_entry(l2_user, l2_index, page as u64 | DESC_TABLE);
                flush_after_table_change(l2_user, GRANULE);
                page
            } else if l2_entry & DESC_TABLE == DESC_TABLE {
                descriptor_address(l2_entry) as usize
            } else {
                // F2: unexpected descriptor kind in the user L2.
                unmap_user_pages_at(ttbr0, virtual_address, done);
                return false;
            };
            if read_entry(l3, l3_index) != 0 {
                // F3: target slot already mapped.
                unmap_user_pages_at(ttbr0, virtual_address, done);
                return false;
            }
            // Zero before publication: the frame is recycled allocator memory
            // and becomes EL0-readable the moment the descriptor is written.
            // A concurrent walk (or the returning task itself) must never be
            // able to observe leftover data, so the PTE is installed only
            // after the whole granule has been cleared.
            zero_page(physical_start + index * GRANULE);
            write_entry(
                l3,
                l3_index,
                user_anonymous_page(physical_start + index * GRANULE, prot),
            );
            flush_after_table_change(l3, GRANULE);
            done += 1;
        }
    }
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
    if end > DRAM_BASE + 0x4000_0000 {
        return false;
    }
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);

        // Pre-scan: no mutation until every page in the range is proven to be
        // a live 4 KiB mapping in this address space.
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l2_index = (va - DRAM_BASE) / BLOCK_2M;
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
            let l2_index = (va - DRAM_BASE) / BLOCK_2M;
            let l3_index = (va >> 12) & 0x1FF;
            let l3 = descriptor_address(read_entry(l2_user, l2_index)) as usize;
            write_entry(l3, l3_index, 0);
            flush_after_table_change(l3, GRANULE);
            if table_empty(l3) {
                write_entry(l2_user, l2_index, 0);
                flush_after_table_change(l2_user, GRANULE);
                free_table_frame_owned(l3, ttbr0);
            }
        }
    }
    true
}

fn table_empty(table: usize) -> bool {
    unsafe {
        let entries = phys_to_virt(table) as *const u64;
        for index in 0..ENTRIES {
            if core::ptr::read_volatile(entries.add(index)) != 0 {
                return false;
            }
        }
        true
    }
}

/// Result of a shared-DMA revoke scan over one address space.
#[derive(Copy, Clone)]
pub struct RevokeResult {
    /// Number of page mappings removed from this address space.
    pub unmapped: usize,
    /// `true` only when the whole address space was verified: every
    /// intermediate descriptor was a valid table descriptor and every
    /// non-empty L3 entry was a valid page descriptor. Any unverifiable
    /// structure forces `false`, and the caller must not free the frames.
    pub safe_to_free: bool,
}

/// Validates a descriptor that names the next-level page table before the
/// table frame is dereferenced, and returns the table's physical address.
///
/// A `DESC_TABLE` bit alone proves nothing about the safety of the address:
/// corrupted memory can name any location, and `read_entry` through it would
/// be a physical read of arbitrary memory. The address must therefore be a
/// page-aligned, currently allocated frame **and a member of the page-table
/// registry** (see [`PAGE_TABLE_FRAMES`]) — i.e. a frame this kernel itself
/// created as a page table. Contents-based validation is impossible: data can
/// mimic a valid table chain, so membership is checked, not inferred.
///
/// `None` means the address cannot be trusted: the caller must report the
/// address space as unverifiable (`safe_to_free = false`) without reading it.
fn follow_validated_table(descriptor: u64) -> Option<usize> {
    if descriptor & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let physical = descriptor_address(descriptor) as usize;
    if physical % GRANULE != 0
        || !frame_allocator::frame_in_use(physical)
        || !page_table_registered(physical)
    {
        return None;
    }
    Some(physical)
}

/// Revokes every page mapping whose physical address lies in the half-open
/// range `[phys_base, phys_end)` from the MMIO window of one address space.
///
/// Read-only until a matching mapping is found: every intermediate level is
/// validated before use (the descriptor must be a table **and** the table's
/// physical address must be a page-aligned, currently allocated frame that
/// the kernel itself registered as a page table — `ttbr0` itself included; a
/// zero entry means the level is absent and the task is trivially clean) and
/// no table is ever allocated. Only valid page descriptors (`DESC_PAGE`)
/// whose physical address falls inside the range are removed; anything
/// non-empty but not a valid page makes the whole window unverifiable
/// (`safe_to_free = false`). Foreign mappings outside the range are kept.
///
/// Never allocates and never frees physical frames. Clearing a descriptor is
/// followed by table cache maintenance and a full TLB invalidation before
/// the caller may free the data frames.
pub fn revoke_phys_range(ttbr0: usize, phys_base: usize, phys_end: usize) -> RevokeResult {
    if phys_base > phys_end
        || phys_base % GRANULE != 0
        || phys_end % GRANULE != 0
        || phys_end > DRAM_END
    {
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

    unsafe {
        // The root table named by `ttbr0` gets the same validation as every
        // other level, registry included (`create_task_space` registers it).
        // A `ttbr0` that fails any check means corrupted task state:
        // unverifiable, and not read.
        if ttbr0 % GRANULE != 0
            || !frame_allocator::frame_in_use(ttbr0)
            || !page_table_registered(ttbr0)
        {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        }

        // L0 → L1_user. A zero entry means the user half is absent: nothing
        // in this address space can map the range.
        let root_entry = read_entry(ttbr0, 0);
        if root_entry == 0 {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: true,
            };
        }
        let Some(l1_user) = follow_validated_table(root_entry) else {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        };

        // L1 → mmio window. Zero means the task never opened an MMIO window.
        let l1_entry = read_entry(l1_user, USER_MMIO_L1_INDEX);
        if l1_entry == 0 {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: true,
            };
        }
        let Some(l2_mmio) = follow_validated_table(l1_entry) else {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        };

        let l2_entry = read_entry(l2_mmio, USER_MMIO_L2_INDEX);
        if l2_entry == 0 {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: true,
            };
        }
        let Some(l3_mmio) = follow_validated_table(l2_entry) else {
            return RevokeResult {
                unmapped: 0,
                safe_to_free: false,
            };
        };

        let mut unmapped = 0usize;
        for index in 0..ENTRIES {
            let entry = read_entry(l3_mmio, index);
            if entry == 0 {
                continue;
            }
            if entry & DESC_PAGE != DESC_PAGE {
                // Not a valid page descriptor: this window cannot be fully
                // verified, so the caller must not free the frames.
                return RevokeResult {
                    unmapped,
                    safe_to_free: false,
                };
            }
            let physical = descriptor_address(entry) as usize;
            if physical >= phys_base && physical < phys_end {
                write_entry(l3_mmio, index, 0);
                flush_after_table_change(l3_mmio, GRANULE);
                unmapped += 1;
            }
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
    let virtual_address = map_user_pages(task, phys_addr, page_count, 0, user_dma_page)?;
    invalidate_user_page(virtual_address);
    Some(virtual_address)
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
        let mut iter_count = 0;
        for index in 0..ENTRIES {
            iter_count += 1;
            if iter_count > 1000 {
                panic!("MMU: infinite loop!");
            }
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

        let mut iter_count2 = 0;
        for page_index in 0..page_count {
            iter_count2 += 1;
            if iter_count2 > 1000 {
                panic!("MMU: infinite loop!");
            }
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
        let mut iter_count3 = 0;
        for page_index in 0..page_count {
            iter_count3 += 1;
            if iter_count3 > 1000 {
                panic!("MMU: infinite loop!");
            }
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

        let (l2_mmio, l2_generation) = frame_allocator::alloc_page_tagged()?;
        zero_page(l2_mmio);
        let Some(l2_token) = page_table_register(l2_mmio, l2_generation, ttbr0) else {
            // Registration refused: `l1_user` is only linked after both
            // registrations succeed, so the frame is still unlinked.
            frame_allocator::free_page_tagged(l2_mmio, l2_generation);
            return None;
        };
        let Some((l3_mmio, l3_generation)) = frame_allocator::alloc_page_tagged() else {
            free_table_frame(l2_token);
            return None;
        };
        zero_page(l3_mmio);
        if page_table_register(l3_mmio, l3_generation, ttbr0).is_none() {
            // `l3_mmio` was never linked (its L2 entry is written below);
            // `l2_mmio` is registered but not yet linked into `l1_user`.
            frame_allocator::free_page_tagged(l3_mmio, l3_generation);
            free_table_frame(l2_token);
            return None;
        }
        write_entry(l2_mmio, USER_MMIO_L2_INDEX, l3_mmio as u64 | DESC_TABLE);
        write_entry(l1_user, USER_MMIO_L1_INDEX, l2_mmio as u64 | DESC_TABLE);
        flush_after_table_change(l3_mmio, GRANULE);
        flush_after_table_change(l2_mmio, GRANULE);
        flush_after_table_change(l1_user, GRANULE);
        Some(l3_mmio)
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
) -> bool {
    destroy_task_space_inner(space, private_page, private_page_generation, true)
}

/// `diagnose = false` is used only by the boot self-test for its intentional
/// stale teardown; production callers always take the diagnostic path.
fn destroy_task_space_inner(
    space: TableToken,
    private_page: usize,
    private_page_generation: u32,
    diagnose: bool,
) -> bool {
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
    complete
}

/// Read-only check used by the address-space self-test: is the 4 KiB page at
/// `virtual_address` present in `ttbr0`?
pub fn user_page_is_mapped(ttbr0: usize, virtual_address: usize) -> bool {
    if virtual_address % GRANULE != 0 || virtual_address < DRAM_BASE {
        return false;
    }
    let l2_index = (virtual_address - DRAM_BASE) / BLOCK_2M;
    if l2_index >= ENTRIES {
        return false;
    }
    unsafe {
        let l1_user = follow_table(ttbr0, 0);
        let l2_user = follow_table(l1_user, 1);
        let l2_entry = read_entry(l2_user, l2_index);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        read_entry(l3, (virtual_address >> 12) & 0x1FF) & DESC_PAGE == DESC_PAGE
    }
}

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
    let Some((root, space)) = create_task_space(private, 0, None) else {
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

    frame_allocator::free_page_tagged(data, data_generation);
    let space_released = destroy_task_space(space, private, private_generation);
    // Stale/double teardown: the registry record is gone, so this must fail
    // without freeing anything else. Quiet: this refusal is the expected
    // result, not a fault.
    let stale_refused = !destroy_task_space_inner(space, 0, 0, false);

    mapped && partial_refused && still_mapped && unmapped && gone && space_released && stale_refused
}

/// Pushes `size` bytes at a kernel virtual (== physical, low identity)
/// address to the point of coherency. Used to publish AP boot parameters to
/// a CPU that starts with cold caches.
pub fn clean_range_to_poc(start: usize, size: usize) {
    let start = start & !63;
    let end = (start + size + 63) & !63;
    unsafe {
        let mut line = start;
        while line < end {
            core::arch::asm!("dc cvac, {}", in(reg) line, options(nostack));
            line += 64;
        }
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
pub fn switch_address_space(ttbr0: usize) {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            // IS scope: other CPUs do not use this address space, but the
            // broadcast is cheap and keeps every invalidation in the kernel
            // domain-wide by contract.
            "tlbi vmalle1is",
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

/// Anonymous `mmap` page: always readable, writable/executable per `prot`, and
/// never executable from EL1. Write-only combinations fall back to read-only
/// (read permission is assumed present for every mapping). Callers must reject
/// `PROT_WRITE | PROT_EXEC` before reaching this helper; `map_user_pages_at`
/// also enforces that W^X rule defensively.
fn user_anonymous_page(output_address: usize, prot: u64) -> u64 {
    let mut descriptor = descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | SH_INNER
        | AF
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
