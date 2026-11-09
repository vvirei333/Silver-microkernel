//! ARMv8-A MMU with a high-half kernel and per-task user address spaces.
//!
//! TTBR1_EL1 maps physical memory at `KERNEL_VIRT_BASE + PA` for EL1 only:
//! kernel code/data/stacks, page tables, the kernel heap, UART and GIC all
//! live there. TTBR0_EL1 belongs to the current task and contains only its
//! independently loaded ELF image, sparse heap, user stack, private page,
//! explicitly authorized Device-nGnRnE MMIO and Normal Non-Cacheable DMA.
//! No kernel image or kernel data is shared with EL0.

use silver_microkernel::phys_range::{
    aligned_page_span, checked_range_end, page_range_within,
};
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
            crate::console::write_str(
                "PAGE TABLE REGISTRY: duplicate frame, invalid token, or out of memory; frame 0x",
            );
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" not registered\r\n");
            None
        }
    }
}

/// True only when `frame` is currently registered as a page table owned by
/// `owner`. Read-only, so it can validate an entire walk before the first
/// table mutation.
fn page_table_owned_by(frame: usize, owner: usize) -> bool {
    PAGE_TABLE_FRAMES.lock().owner(frame) == Some(owner)
}

/// A space root is a live frame whose registry owner is itself: that is the
/// same invariant `retire_space` checks, so a walker cannot treat another
/// space's child table as a root.
fn space_root_valid(ttbr0: usize) -> bool {
    let root = root_of(ttbr0);
    root % GRANULE == 0
        && frame_allocator::frame_in_use(root)
        && page_table_owned_by(root, root)
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
/// A sparse, per-task heap window after the ELF image. The engine-linked
/// browser image is larger than 2 MiB now, so the window starts at 8 MiB;
/// its L3 table is installed at spawn, while data pages are mapped by `brk`.
pub const USER_HEAP_VA: usize = USER_IMAGE_VA + 0x80_0000;
pub const USER_HEAP_MAX_PAGES: usize = 512;
pub const USER_HEAP_MAX_SIZE: usize = USER_HEAP_MAX_PAGES * GRANULE;
const USER_HEAP_L2_INDEX: usize = (USER_HEAP_VA - DRAM_BASE) / BLOCK_2M;

/// The lowest stack VA page is deliberately left unmapped as a guard. The
/// stack itself starts one page above it, so a downward overflow faults
/// instead of silently landing in the mmap window below.
///
/// Sixty-four pages (256 KiB) leave room for the guest TLS handshake and the
/// litehtml layout recursion. The old two-page stack faulted on the guard page
/// during the first HTTPS request; the browser engine needs even more.
pub const USER_STACK_GUARD_VA: usize = USER_IMAGE_VA + 0x3FE0_0000;
/// Every task's own stack appears at this user VA, backed by different PAs.
pub const USER_STACK_VA: usize = USER_STACK_GUARD_VA + GRANULE;
pub const USER_STACK_PAGES: usize = 64;
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

/// A multi-group first-fit window for explicitly authorized user MMIO and DMA
/// mappings. Eight L2 groups (16 MiB) cover the display path with headroom:
/// the compositor owns a ~2.8 MiB framebuffer, a ~2.8 MiB legacy offscreen
/// surface, and maps a ~2.8 MiB client surface for zero-copy scanout.
pub const USER_MMIO_VA: usize = USER_IMAGE_VA + 0x4000_0000;
const USER_MMIO_L2_GROUPS: usize = 8;
const _: () = assert!(USER_MMIO_L2_GROUPS >= 8);
pub const USER_DMA_MAX_SIZE: usize = ENTRIES * GRANULE * USER_MMIO_L2_GROUPS;
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
pub const USER_MMAP_VA: usize = USER_IMAGE_VA + 0xA0_0000;
/// Anonymous mappings stop below the stack guard page, so they can never
/// fill the hole that makes a stack overflow fault.
pub const USER_MMAP_MAX_VA: usize = USER_STACK_GUARD_VA;

/// Upper bound on the page count of a single anonymous mapping. User-supplied
/// lengths are rejected against this before any span arithmetic.
pub const USER_MMAP_MAX_PAGES: usize = (USER_MMAP_MAX_VA - USER_MMAP_VA) / GRANULE;

/// `prot` bits for anonymous mappings. Read is always granted; these flags
/// select write and execute in `user_anonymous_page`.
pub const PROT_READ: u64 = 1;
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

/// `TTBR0_EL1[63:48]` carries the address-space identifier. `TCR_EL1.AS` is
/// left at 0 (the ARMv8.0 baseline), so the architectural limit is 8 bits:
/// ASIDs `1..=255` are allocatable and `0` is reserved for the boot/kernel
/// TTBR0. Every task space gets a unique ASID until it is destroyed, which
/// lets a context switch keep both spaces' translations cached and lets a PTE
/// edit invalidate only the edited space.
const TTBR0_ASID_SHIFT: u32 = 48;
const ASID_BITS: usize = 8;
const ASID_WORDS: usize = (1usize << ASID_BITS) / 64;

/// Root physical address of a TTBR0 value (ASID bits removed). Every place
/// that treats a `ttbr0` argument as a frame address must go through this.
pub fn root_of(ttbr0: usize) -> usize {
    ttbr0 & ADDRESS_MASK as usize
}

/// ASID carried by a tagged TTBR0 value.
pub fn asid_of(ttbr0: usize) -> u16 {
    ((ttbr0 >> TTBR0_ASID_SHIFT) & 0xFF) as u16
}

fn tagged_ttbr0(root: usize, asid: u16) -> usize {
    root_of(root) | ((asid as usize) << TTBR0_ASID_SHIFT)
}

/// Live-ASID bitmap. Bit 0 (ASID 0) is permanently set; allocation scans the
/// remaining 255 bits. Allocation is rare and runs under the kernel lock, so
/// a linear scan under a small spinlock is cheaper than a free-list structure.
struct AsidPool {
    live: [u64; ASID_WORDS],
}

impl AsidPool {
    const fn new() -> Self {
        Self {
            live: [0; ASID_WORDS],
        }
    }
}

static ASIDS: SpinLock<AsidPool> = SpinLock::new(AsidPool::new());

fn alloc_asid() -> Option<u16> {
    let mut pool = ASIDS.lock();
    pool.live[0] |= 1; // reserve ASID 0 for the boot/kernel TTBR0
    for word in 0..ASID_WORDS {
        let free = !pool.live[word];
        if free != 0 {
            let bit = free.trailing_zeros() as usize;
            pool.live[word] |= 1u64 << bit;
            let asid = (word * 64 + bit) as u16;
            if asid != 0 {
                return Some(asid);
            }
        }
    }
    None
}

fn release_asid(asid: u16) {
    if asid == 0 {
        return;
    }
    let index = asid as usize;
    let mut pool = ASIDS.lock();
    pool.live[index / 64] &= !(1u64 << (index % 64));
}

/// Broadcast-invalidates every translation tagged with `asid`. The `is`
/// suffix covers the inner-shareable domain, so a stale entry on another CPU
/// is dropped too; the trailing `dsb ish` makes the invalidation complete
/// before the caller frees a table or reuses the ASID.
fn invalidate_asid(asid: u16) {
    if asid == 0 {
        return;
    }
    unsafe {
        core::arch::asm!(
            "tlbi aside1is, {op}",
            "dsb ish",
            "isb",
            op = in(reg) (asid as u64) << TTBR0_ASID_SHIFT,
            options(nostack),
        );
    }
}

/// Completes a batch of edits to the space named by `ttbr0`: one store-order
/// barrier, one ASID-scoped broadcast invalidation, one completion barrier.
/// ASID 0 (boot tables) falls back to the full broadcast.
fn finish_table_changes_for(ttbr0: usize) {
    let asid = asid_of(ttbr0);
    if asid == 0 {
        finish_table_changes();
        return;
    }
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi aside1is, {op}",
            "dsb ish",
            "isb",
            op = in(reg) (asid as u64) << TTBR0_ASID_SHIFT,
            options(nostack),
        );
    }
}

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

/// AP trampoline copies of the boot CPU's translation-control registers.
pub(crate) fn ap_boot_mair() -> u64 {
    MAIR_VALUE
}

pub(crate) fn ap_boot_tcr() -> u64 {
    TCR_VALUE
}

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
    // Caches were off while the boot stub and this function built and
    // published the tables, so the D-cache may still hold firmware lines.
    // `cache::init_boot` drops them and enables C/I; from that point the
    // table-clean and DMA-clean rituals are load-bearing rather than
    // ceremonial.
    crate::cache::init_boot();
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

/// Data-cache line size in bytes, from `CTR_EL0.DminLine`.
///
/// AArch64 encodes the minimum D-cache line as `4 << DminLine` bytes. Reading
/// it avoids baking a 64-byte assumption into the maintenance loops: every
/// range below is aligned and stepped with the value this CPU reports.
fn data_cache_line_size() -> usize {
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
fn clean_cache_range(start: usize, end: usize) {
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

fn clean_tables_to_point_of_coherency() {
    let start = core::ptr::addr_of!(TABLES) as usize;
    let end = start + TABLE_COUNT * TABLE_BYTES;
    clean_cache_range(start, end);
    unsafe {
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
fn finish_table_changes() {
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
fn clean_entry_line(table_start: usize, entry_index: usize) {
    let line_size = data_cache_line_size();
    let address = phys_to_virt(table_start) + entry_index * 8;
    let line = address & !(line_size - 1);
    clean_cache_range(line, line + line_size);
}

/// Pushes the dirty translation-table lines to the point of coherency.
///
/// Only the *table* is cleaned here: without it the table walker may fetch a
/// stale descriptor from memory even though the invalidating `tlbi` follows.
fn clean_table_lines(table_start: usize, table_bytes: usize) {
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
// ---------------------------------------------------------------------------
// Per-task user address spaces
// ---------------------------------------------------------------------------

/// Frames a freshly created address space owns: root, user L1/L2, image/heap/
/// stack L3s, and the private L1/L2/L3 trio. Charged to the task's memory
/// ledger as one flat cost; `mmap` adds its own L3 estimate separately.
pub const SPACE_TABLE_FRAMES: usize = 9;
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

    let [l0, l1_user, l2_user, l3_image, l3_heap, l3_stack, l1_priv, l2_priv, l3_priv] = pages;

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
            write_entry(l2_user, USER_STACK_L2_INDEX, l3_stack as u64 | DESC_TABLE);
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

fn map_user_heap_pages_inner(
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

/// Shared implementation of the two entry points above.
fn map_user_pages_at_inner(
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
                    finish_table_changes_for(ttbr0);
                    return false;
                };
                zero_page(page);
                if page_table_register(page, generation, root).is_none() {
                    // Registration refused: the L2 entry below is only written
                    // after a successful registration, so the frame is still
                    // unlinked and its own tag is correct.
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
                unmap_user_pages_at(ttbr0, virtual_address, done);
                finish_table_changes_for(ttbr0);
                return false;
            };
            if read_entry(l3, l3_index) != 0 {
                // F3: target slot already mapped.
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
        let l2_user = follow_table(l1_user, 1);
        let l2_index = (virtual_address - DRAM_BASE) / BLOCK_2M;
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
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
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
            clean_entry_line(l3, l3_index);
            // The heap and stack L3 tables are persistent: `brk` and the
            // fixed stack window reuse them after an unmap empties the last
            // page. Freeing them would leave map/unmap_user_heap_page
            // following a now-zero L2 descriptor into physical address 0.
            let persistent = l2_index == USER_HEAP_L2_INDEX
                || l2_index == USER_STACK_L2_INDEX;
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
            let descriptor = match kinds[index] {
                crate::elf::PageKind::Text => user_text_page(physical),
                crate::elf::PageKind::ReadOnly | crate::elf::PageKind::Cow => {
                    user_rodata_page(physical)
                }
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
fn alloc_registered_table(root: usize) -> Option<usize> {
    let (page, generation) = frame_allocator::alloc_page_tagged()?;
    unsafe {
        zero_page(page);
    }
    if page_table_register(page, generation, root).is_none() {
        frame_allocator::free_page_tagged(page, generation);
        return None;
    }
    Some(page)
}

#[cfg(feature = "linux-abi")]
fn rollback_linux_run(ttbr0: usize, virtual_address: usize, installed: usize) {
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
    /// `true` only when the whole address space was verified: the root and
    /// every intermediate table were registered to this space, and every
    /// non-empty L3 entry was a valid page descriptor. Any unverifiable
    /// structure or foreign owner forces `false`, and the caller must not free
    /// the frames.
    ///
    /// This is a **CPU-mapping** result only. It does not prove that a device
    /// has stopped touching the range: without an IOMMU or a device-reset
    /// contract, DMA can still be in flight after every PTE is gone. Callers
    /// may release the frames only when the device-quiescence precondition
    /// documented on [`map_dma_to_user`] is satisfied by the driver teardown.
    pub safe_to_free: bool,
}

/// Validates a descriptor that names the next-level page table before the
/// table frame is dereferenced, and returns the table's physical address.
///
/// A `DESC_TABLE` bit alone proves nothing about the safety of the address:
/// corrupted memory can name any location, and `read_entry` through it would
/// be a physical read of arbitrary memory. The address must therefore be a
/// page-aligned, currently allocated frame **and a member of the page-table
/// registry owned by `expected_owner`** (see [`PAGE_TABLE_FRAMES`]) — i.e. a
/// frame this kernel itself created as a table for the space being walked.
/// Contents-based validation is impossible: data can mimic a valid table
/// chain, so membership and ownership are checked, not inferred. Foreign
/// ownership is a hard refusal: an address space that points at another
/// space's table must never be modified, even if that table is otherwise
/// valid.
///
/// `None` means the address cannot be trusted: the caller must report the
/// address space as unverifiable (`safe_to_free = false`) without reading it.
fn follow_validated_table(descriptor: u64, expected_owner: usize) -> Option<usize> {
    if descriptor & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let physical = descriptor_address(descriptor) as usize;
    if physical % GRANULE != 0
        || !frame_allocator::frame_in_use(physical)
        || !page_table_owned_by(physical, expected_owner)
    {
        return None;
    }
    Some(physical)
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

fn map_user_pages(
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
fn rollback_user_pages(ttbr0: usize, first_slot: usize, written: usize) {
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

fn clean_dma_range(physical: usize, size: usize) {
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
fn user_mmio_table(ttbr0: usize, group: usize) -> Option<usize> {
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
fn destroy_task_space_inner(
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
pub fn user_page_is_mapped(ttbr0: usize, virtual_address: usize) -> bool {
    if virtual_address % GRANULE != 0 || virtual_address < DRAM_BASE {
        return false;
    }
    let l2_index = (virtual_address - DRAM_BASE) / BLOCK_2M;
    if l2_index >= ENTRIES {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
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

    frame_allocator::free_page_tagged(data, data_generation);
    let space_released = destroy_task_space(space, private, private_generation, asid_of(root));
    // Stale/double teardown: the registry record is gone, so this must fail
    // without freeing anything else. Quiet: this refusal is the expected
    // result, not a fault.
    let stale_refused = !destroy_task_space_inner(space, 0, 0, asid_of(root), false);

    mapped && partial_refused && still_mapped && unmapped && gone && space_released && stale_refused
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

/// Read-only DMA alias for a surface shared with another task (for example a
/// compositor scanning a client-owned buffer). EL0 may read the pixels but
/// cannot modify them; the owner keeps its writable alias.

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
