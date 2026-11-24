//! ARMv8-A MMU with a high-half kernel and per-task user address spaces.
//!
//! TTBR1_EL1 maps physical memory at `KERNEL_VIRT_BASE + PA` for EL1 only:
//! kernel code/data/stacks, page tables, the kernel heap, UART and GIC all
//! live there. TTBR0_EL1 belongs to the current task and contains only its
//! independently loaded ELF image, sparse heap, user stack, private page,
//! explicitly authorized Device-nGnRnE MMIO and Normal Non-Cacheable DMA.
//! No kernel image or kernel data is shared with EL0.
//!
//! The implementation is one module per layer: table.rs owns the page-table
//! registry, the ASID pool and descriptor/entry access; kernel.rs the TTBR1
//! map, boot control registers and cache maintenance; user.rs per-task spaces
//! and their mappings; selftest.rs the boot self-tests.

pub(super) use silver_microkernel::phys_range::{
    aligned_page_span, checked_range_end, page_range_within,
};
pub(super) use silver_microkernel::table_registry::{
    RetireResult, SpaceRetire, TableRegistry, TableToken,
};

pub(super) use crate::{
    frame_allocator,
    memory::{phys_to_virt, virt_to_phys, DRAM_BASE, DRAM_END},
    smp::SpinLock,
};

mod kernel;
mod selftest;
mod table;
mod user;

#[allow(unused_imports)]
pub use kernel::{
    init, install_kernel_stack_arena, kernel_page_is_mapped, set_kernel_stack_pages,
    invalidate_range, kernel_ttbr1, flush_after_table_change, current_ttbr0,
    clean_range_to_poc, ap_enable, switch_address_space,
};
#[allow(unused_imports)]
pub use table::{
    root_of, asid_of,
};
#[allow(unused_imports)]
pub use user::{
    create_task_space, map_user_heap_pages, map_shared_user_heap_pages,
    unmap_user_heap_page, map_user_pages_at, map_shared_user_pages_at,
    map_shared_writable_pages_at, map_user_page_at, protect_user_pages_at,
    protect_user_pages_at_lenient, protect_private_page, remap_private_page,
    remap_user_pages_at, remap_user_kinds_at, unmap_user_pages_at_lenient,
    unmap_user_pages_at, range_mapped_by, revoke_phys_range, map_phys_to_user,
    map_phys_nc_to_user, map_dma_to_user, map_dma_to_user_space,
    map_shared_normal_to_user, map_shared_dma_to_user, destroy_task_space,
    user_physical_address, user_page_is_mapped, valid_user_slice,
    valid_user_slice_mut, SPACE_TABLE_FRAMES,
};
#[cfg(feature = "smp")]
#[allow(unused_imports)]
pub use user::install_ap_identity;
#[cfg(feature = "smp-selftest")]
#[allow(unused_imports)]
pub use user::replace_user_page;
#[cfg(feature = "linux-abi")]
#[allow(unused_imports)]
pub use user::protect_cow_image;
#[cfg(feature = "linux-abi")]
#[allow(unused_imports)]
pub use user::install_linux_run;
#[cfg(feature = "linux-abi")]
#[allow(unused_imports)]
pub use user::install_linux_image;
#[allow(unused_imports)]
pub use selftest::{
    run_self_test, run_dma_window_self_test, run_tlb_self_test,
    run_dma_zero_self_test, run_heap_table_self_test, run_asid_self_test
};
#[allow(unused_imports)]
pub(crate) use kernel::{ap_boot_mair, ap_boot_tcr};

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
///
/// The stack lives in its own 1 GiB L1 block (0xF0000000..) instead of the
/// top of the first L2 table: the mmap window below it must stay wide enough
/// for Mesa/LLVM arena reservations (~193 MiB per GPU worker context).
pub const USER_STACK_GUARD_VA: usize = USER_IMAGE_VA + 0xB000_0000;
/// Every task's own stack appears at this user VA, backed by different PAs.
pub const USER_STACK_VA: usize = USER_STACK_GUARD_VA + GRANULE;
pub const USER_STACK_PAGES: usize = 64;
pub const USER_STACK_SIZE: usize = USER_STACK_PAGES * GRANULE;
/// L1 slot of the stack's 1 GiB block.
const USER_STACK_L1_INDEX: usize = (USER_STACK_VA >> 30) & 0x1FF;
/// L2 slot inside that block.
const USER_STACK_L2_INDEX: usize = (USER_STACK_VA >> 21) & 0x1FF;
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

/// BCM2712 peripheral window. The three PCIe root complexes start at
/// 0x10_0010_0000, and the SDHCI host used by the Pi 5 SD card is at
/// 0x10_00ff_f000; both are inside the first 16 MiB. This is Device memory,
/// not RAM, and the PCIe host bridge is not ECAM: it is reached through its
/// own index/data registers at `+0x9000` and `+0x8000`.
pub const BCM_PERIPH_BASE: usize = 0x0000_0010_0000_0000;
pub const BCM_PERIPH_SIZE: usize = 0x100_0000;
const BCM_PERIPH_L1_INDEX: usize = (BCM_PERIPH_BASE >> 30) & 0x1FF;
const BCM_PERIPH_L2_INDEX: usize = ((BCM_PERIPH_BASE >> 21) & 0x1FF) as usize;
const BCM_PERIPH_L2_BLOCKS: usize = BCM_PERIPH_SIZE / BLOCK_2M;

/// True when the kernel already maps `[start, start + size)` as Device MMIO.
///
/// This is the policy the SDHCI core uses before touching a controller named
/// by the tree: a node can describe a window, but only a window the kernel
/// actually mapped is safe to read. Low 1 GiB is the boot-stub MMIO map; the
/// other two are installed by [`kernel::init`].
pub fn device_window_mapped(start: usize, size: usize) -> bool {
    let Some(end) = start.checked_add(size) else {
        return false;
    };
    if size == 0 {
        return false;
    }
    [
        (0usize, 0x4000_0000usize),
        (PCI_ECAM_BASE, PCI_ECAM_SIZE),
        (BCM_PERIPH_BASE, BCM_PERIPH_SIZE),
    ]
    .iter()
    .any(|(base, length)| start >= *base && end <= base.saturating_add(*length))
}

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
/// Non-global: this translation belongs to the ASID in TTBR0. Without it the
/// per-space flushes (`tlbi ... aside1`) would not invalidate the entry and a
/// translation cached for another address space could satisfy this VA.
const NG: u64 = 1 << 11;
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
const TABLE_COUNT: usize = 8;
#[cfg(not(feature = "tablet"))]
const TABLE_COUNT: usize = 7;
const EMPTY_TABLE: Table = Table {
    entries: [0; ENTRIES],
};

/// The assembly boot stub fills the first five tables before enabling the MMU.
/// The sixth is zeroed with .bss and populated for PCI ECAM in `init`; the
/// seventh maps the BCM2712 peripheral window. On the tablet the second DRAM
/// gigabyte keeps its table at index six and the BCM window moves to the last
/// slot.
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
#[cfg(feature = "tablet")]
const INDEX_LEVEL2_BCM: usize = 7;
#[cfg(not(feature = "tablet"))]
const INDEX_LEVEL2_BCM: usize = 6;

/// Installs empty-low/high-kernel TTBRs. The boot stub has already enabled
/// translation, so this runs entirely through the high alias.
/// TTBR1 value of the shared kernel high half, recorded at boot so APs can
/// install the identical kernel mapping without rebuilding it.
static KERNEL_TTBR1: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Failure diagnostics ring for `map_user_pages_at_inner`:
/// `(faulting page VA, arm id, extra, task)`. Arms: 1 = L3 frame alloc
/// refused, 2 = table registration refused, 3 = unexpected L2 descriptor
/// (extra = the raw descriptor), 4 = target PTE already live (extra = the
/// existing entry). Read from a pmemsave dump after the kill; the ring makes
/// the "reason=map" LAZY_FAIL record specific.
pub const MMU_FAIL_LEN: usize = 32;
static MMU_FAIL_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut MMU_FAIL: [[u64; 4]; MMU_FAIL_LEN] = [[0; 4]; MMU_FAIL_LEN];

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
