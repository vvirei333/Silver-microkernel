//! Boot hand-over parameters captured by the assembly entry stub.
//!
//! The stub runs before any Rust code and before the MMU, so it cannot call
//! into the kernel to publish what the bootloader passed. Instead it writes a
//! fixed-layout record into the initialized `.data` section (which the image
//! relocation copies along with the rest of the image) and `kernel_main` reads
//! it once the high-half mapping is live.
//!
//! The record is deliberately small and ABI-free: it is written with `str` at
//! fixed offsets, so the offsets below are the contract between `main.rs`'s
//! `global_asm!` and this module. Keep them in sync.

pub const BOOT_MAGIC: u64 = 0x3031_5245_564C_4953; // "SILVER10" tag (LE bytes)

/// Fixed-layout hand-over record. `repr(C)` + the field order define the
/// offsets the boot stub writes; do not reorder without updating the asm.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootParams {
    /// [`BOOT_MAGIC`] once the stub has written the record.
    pub magic: u64,
    /// Physical address the bootloader actually started the image at.
    pub actual_pc: u64,
    /// `CurrentEL` at entry (`4` = EL1, `8` = EL2).
    pub entry_el: u64,
    /// `SCTLR_EL1` as observed at entry (MMU/cache state evidence).
    pub sctlr: u64,
    /// `MPIDR_EL1` of the boot CPU.
    pub mpidr: u64,
    /// Physical DTB pointer from `x0` (`0` when the loader passed none).
    pub dtb_pa: u64,
    /// Physical base the image was relocated to (the link address).
    pub image_base: u64,
    /// DTB `totalsize` read from the header (0 when absent).
    pub dtb_size: u64,
    /// Loaded image span from the ARM64 header (`__kernel_end - _head`).
    pub image_size: u64,
    /// Difference `actual_pc - image_base`; zero means no relocation ran.
    pub load_delta: i64,
}

impl BootParams {
    pub const fn zero() -> Self {
        Self {
            // Non-zero so the compiler places the record in `.data`, not
            // `.bss`: the boot stub writes it before the BSS clear, and the
            // clear must not wipe the hand-over facts. `get()` still requires
            // BOOT_MAGIC, so this sentinel is never accepted as valid.
            magic: u64::MAX,
            actual_pc: 0,
            entry_el: 0,
            sctlr: 0,
            mpidr: 0,
            dtb_pa: 0,
            image_base: 0,
            dtb_size: 0,
            image_size: 0,
            load_delta: 0,
        }
    }

    pub fn has_dtb(&self) -> bool {
        self.dtb_pa != 0 && self.dtb_size != 0
    }
}

/// Single-claim word for the boot path. The first CPU to reach the stub
/// writes [`BOOT_CLAIM_VALUE`]; every other CPU that sees the claim parks.
/// This deliberately does not use `MPIDR` affinity zero: the bootloader's
/// choice of boot CPU is an unconfirmed parameter, so the claim is
/// identity-independent.
///
/// The claim is a plain load/store, not an exclusive monitor: with the MMU
/// off, data accesses are Device memory on real hardware, where exclusive
/// operations are not usable. It is best-effort and assumes the bootloader
/// starts a single CPU, which is the normal Qualcomm hand-over.
///
/// Initialised to a non-zero sentinel so the compiler places it in `.data`
/// (a zero-initialised static would land in `.bss`, which the stub clears).
#[no_mangle]
#[link_section = ".data"]
pub static mut BOOT_CLAIM: u32 = 0xDEAD_BEEF;

/// Written into [`BOOT_CLAIM`] by the first core. Referenced from the
/// assembly stub as a literal, so Rust sees no use here.
#[allow(dead_code)]
pub const BOOT_CLAIM_VALUE: u32 = 0x424F_4F54; // "BOOT"

/// Written by the assembly entry stub, read by `kernel_main`.
///
/// Explicitly placed in `.data`: the stub stores into it before zeroing BSS,
/// so `.bss` placement would erase the record a few instructions later.
#[no_mangle]
#[link_section = ".data"]
pub static mut BOOT_PARAMS: BootParams = BootParams::zero();

/// Magic of the relocation diagnostic record (ASCII `SLR2`, little-endian).
pub const RELOC_MAGIC: u32 = 0x3252_4C53;
/// Current layout version of [`RelocRecord`].
pub const RELOC_VERSION: u8 = 2;

/// Set once the stub enters the relocation path.
pub const RELOC_FLAG_ATTEMPTED: u16 = 1 << 0;
/// Set after the copy completed and the destination record was published.
pub const RELOC_FLAG_DONE: u16 = 1 << 1;
/// `sctlr_el1` was read successfully at K1.
pub const RELOC_FLAG_SCTLR_READ: u16 = 1 << 2;
/// `ctr_el0` was read successfully (EL2, or EL1 with TID2=0).
pub const RELOC_FLAG_CTR_READ: u16 = 1 << 3;
/// `hcr_el2` was read successfully (EL2 only).
pub const RELOC_FLAG_HCR_READ: u16 = 1 << 4;
/// The DTB pointer passed a bounded check and `dtb_size` was read.
pub const RELOC_FLAG_DTB_PRESENT: u16 = 1 << 5;

/// Reason codes written before the stub parks on a refused relocation.
pub const RELOC_REASON_NONE: u16 = 0;
pub const RELOC_REASON_BAD_MAGIC: u16 = 2;
pub const RELOC_REASON_BAD_SIZE: u16 = 3;
pub const RELOC_REASON_BAD_ALIGN: u16 = 4;
pub const RELOC_REASON_BOUNDS: u16 = 5;
pub const RELOC_REASON_OVERLAP: u16 = 6;
pub const RELOC_REASON_FDT_REJECT: u16 = 7;

/// Checks completed before a refusal. Useful when only a raw DRAM image is
/// available after the unit parks.
pub const RELOC_CHECK_MAGIC: u16 = 1 << 0;
pub const RELOC_CHECK_SIZE: u16 = 1 << 1;
pub const RELOC_CHECK_ALIGN: u16 = 1 << 2;
pub const RELOC_CHECK_BOUNDS: u16 = 1 << 3;
pub const RELOC_CHECK_OVERLAP: u16 = 1 << 4;
pub const RELOC_CHECK_FDT: u16 = 1 << 5;

/// Compact relocation diagnostic record.
///
/// The assembly stub writes this **into the running source image** before it
/// touches the destination, so a refused relocation still leaves evidence in
/// DRAM. On success the same record is copied with the image and the stub
/// updates the destination copy with [`RELOC_FLAG_DONE`] and reason 0.
///
/// Offset contract with `main.rs`'s `global_asm!`:
/// `0x00` magic u32, `0x04` version u8, `0x05` entry_el u8, `0x06` flags u16,
/// `0x08` reason u16, `0x0A` checks u16, `0x0C` reserved u32,
/// `0x10` actual_pc, `0x18` dest_start, `0x20` dest_end, `0x28` image_size,
/// `0x30` dtb_pa, `0x38` dtb_size, `0x40` sctlr_el1, `0x48` ctr_el0,
/// `0x50` hcr_el2, `0x58` mpidr.
///
/// A zero field is not proof that the register was read. Check the matching
/// [`RELOC_FLAG_*`] bit first.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RelocRecord {
    pub magic: u32,
    pub version: u8,
    pub entry_el: u8,
    pub flags: u16,
    pub reason: u16,
    pub checks: u16,
    pub reserved0: u32,
    pub actual_pc: u64,
    pub dest_start: u64,
    pub dest_end: u64,
    pub image_size: u64,
    pub dtb_pa: u64,
    pub dtb_size: u64,
    pub sctlr_el1: u64,
    pub ctr_el0: u64,
    pub hcr_el2: u64,
    pub mpidr: u64,
    pub reserved: [u64; 4],
}

impl RelocRecord {
    pub const fn zero() -> Self {
        Self {
            magic: RELOC_MAGIC,
            version: RELOC_VERSION,
            entry_el: 0,
            flags: 0,
            reason: RELOC_REASON_NONE,
            checks: 0,
            reserved0: 0,
            actual_pc: 0,
            dest_start: 0,
            dest_end: 0,
            image_size: 0,
            dtb_pa: 0,
            dtb_size: 0,
            sctlr_el1: 0,
            ctr_el0: 0,
            hcr_el2: 0,
            mpidr: 0,
            reserved: [0; 4],
        }
    }
}

// The assembly stub hard-codes these offsets; keep the layout at 128 bytes.
const _: () = assert!(core::mem::size_of::<RelocRecord>() == 128);

/// Written by the assembly stub before the relocation copy. Like
/// [`BOOT_PARAMS`], it must live in `.data`, not `.bss`.
#[no_mangle]
#[link_section = ".data"]
pub static mut RELOC_RECORD: RelocRecord = RelocRecord::zero();

/// Returns the relocation record when the stub wrote a valid one.
///
/// This accessor is a memory read only. `fog::log_banner` prints a compact
/// summary to the console, but that is a UART channel and is not proven
/// visible on UART-less units; no panel channel exists yet. On a refused
/// relocation the useful copy is in the *source* image and is reachable only
/// through a debugger/ramdump until such a channel is added.
pub fn relocation_record() -> Option<RelocRecord> {
    let record = unsafe { core::ptr::read(core::ptr::addr_of!(RELOC_RECORD)) };
    (record.magic == RELOC_MAGIC && record.version == RELOC_VERSION).then_some(record)
}

/// Returns the record when the stub ran and filled it, otherwise `None`.
pub fn get() -> Option<BootParams> {
    // A plain read of the static; no reference is created, so the mutable
    // static never aliases a `&mut`.
    let params = unsafe { core::ptr::read(core::ptr::addr_of!(BOOT_PARAMS)) };
    (params.magic == BOOT_MAGIC).then_some(params)
}
