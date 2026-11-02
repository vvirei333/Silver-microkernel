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

/// Returns the record when the stub ran and filled it, otherwise `None`.
pub fn get() -> Option<BootParams> {
    // A plain read of the static; no reference is created, so the mutable
    // static never aliases a `&mut`.
    let params = unsafe { core::ptr::read(core::ptr::addr_of!(BOOT_PARAMS)) };
    (params.magic == BOOT_MAGIC).then_some(params)
}
