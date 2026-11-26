//! PL031 real-time clock: the wall-clock half of the time ABI.
//!
//! `timer::ticks()` counts upwards from zero at boot, so the Linux
//! `CLOCK_REALTIME` path used to report 1970 plus uptime. Certificate
//! validity and build stamps need a real epoch, so the kernel reads the
//! PL031 data register once at boot and keeps the offset from there.
//!
//! Only the live device tree selects the device: QEMU `virt` describes an
//! `arm,pl031` node at 0x0901_0000, and a machine without that node simply
//! leaves the wall clock unavailable instead of touching an unmapped address.

use core::sync::atomic::{AtomicU64, Ordering};

/// PL031 `DR` (data) register offset from the node's `reg` base.
const DATA_REGISTER: usize = 0;

/// Unix seconds sampled at boot; zero means "no RTC".
static BOOT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Binds the RTC named by the device tree and samples its epoch.
pub fn init(dtb: Option<&[u8]>) -> bool {
    // QEMU `-kernel` does not always hand over a DTB, so the emulated board
    // keeps the documented `virt` address as a fallback. Any other board
    // without a PL031 node stays clockless instead of probing MMIO.
    let base = dtb
        .and_then(crate::fdt::Fdt::new)
        .and_then(|tree| tree.first_enabled_reg("arm,pl031"))
        .map(|region| region.start as usize)
        .or_else(|| {
            (crate::platform::detect() == crate::platform::Kind::Qemu)
                .then_some(0x0901_0000)
        });
    let Some(base) = base else {
        return false;
    };
    // The 1 GiB device window is mapped device-nGnRnE by `mmu::init`, so
    // this is a plain volatile register read.
    // Kernel MMIO is mapped only at `KERNEL_VIRT_BASE + PA` (TTBR1); the
    // physical address alone is not a valid kernel pointer.
    let data = crate::memory::phys_to_virt(base + DATA_REGISTER) as *const u32;
    let seconds = unsafe { core::ptr::read_volatile(data) };
    if seconds == 0 {
        return false;
    }
    BOOT_EPOCH.store(u64::from(seconds), Ordering::Release);
    true
}

/// Current Unix time as `(seconds, microseconds)`, or `None` without an RTC.
pub fn now() -> Option<(u64, u64)> {
    let epoch = BOOT_EPOCH.load(Ordering::Acquire);
    if epoch == 0 {
        return None;
    }
    let ticks = crate::timer::ticks();
    // The scheduler tick is 100 Hz, which is also the clock resolution the
    // ABI reports through `clock_getres`.
    Some((epoch + ticks / 100, (ticks % 100) * 10_000))
}
