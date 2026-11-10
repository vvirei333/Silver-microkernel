//! QEMU ivshmem surface for Linux compositors.
//!
//! The host viewer maps the same shared-memory file and expects the `WIN1`
//! header the native Wolfser publishes. A Linux process cannot program a PCI
//! BAR or map physical memory, so the kernel performs the tiny amount of PCI
//! setup once and exposes the BAR through `/dev/ivshmem`; `mmap` on that
//! device maps the host-shared pages Non-Cacheable.
//!
//! QEMU `virt` is the only platform where the Linux personality runs today.
//! The device is located through ECAM, so the address does not depend on a
//! hard-coded bus/device pair; the BAR target is the same 0x1040_0000 window
//! Wolfser uses.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::memory::phys_to_virt;

/// BAR2 (offset 0x18) holds the shared-memory window on QEMU's ivshmem.
const BAR_OFFSET: usize = 0x18;
const COMMAND_OFFSET: usize = 0x04;
const VENDOR_RED_HAT: u32 = 0x1AF4;
const DEVICE_IVSHMEM: u32 = 0x1110;

/// Window published to `/dev/ivshmem` and the host viewer.
pub const BAR_ADDRESS: usize = 0x1040_0000;
pub const BAR_SIZE: usize = 4 * 1024 * 1024;

static READY: AtomicBool = AtomicBool::new(false);

fn config_address(bus: u8, device: u8, function: u8, offset: usize) -> usize {
    let address = (usize::from(bus) << 20)
        | (usize::from(device) << 15)
        | (usize::from(function) << 12)
        | (offset & 0xFFC);
    phys_to_virt(crate::mmu::PCI_ECAM_BASE + address)
}

unsafe fn read32(address: usize) -> u32 {
    core::ptr::read_volatile(address as *const u32)
}

unsafe fn write32(address: usize, value: u32) {
    core::ptr::write_volatile(address as *mut u32, value);
}

/// Programs and enables the ivshmem BAR. Returns the guest-physical base, or
/// `None` when the machine has no ivshmem device. Idempotent and safe to call
/// from the Linux `open` path (the caller holds the kernel lock).
pub fn ensure() -> Option<usize> {
    if READY.load(Ordering::Acquire) {
        return Some(BAR_ADDRESS);
    }
    for bus in 0..=u8::MAX {
        for device in 0..32u8 {
            for function in 0..8u8 {
                let id = unsafe { read32(config_address(bus, device, function, 0)) };
                if id & 0xFFFF != VENDOR_RED_HAT || id >> 16 != DEVICE_IVSHMEM {
                    continue;
                }
                unsafe {
                    let command =
                        config_address(bus, device, function, COMMAND_OFFSET);
                    write32(command, read32(command) | 0x2);
                    let bar = config_address(bus, device, function, BAR_OFFSET);
                    let high = config_address(bus, device, function, BAR_OFFSET + 4);
                    let saved = read32(bar);
                    let flags = saved & 0xF;
                    write32(bar, 0xFFFF_FFFF);
                    write32(high, 0xFFFF_FFFF);
                    let mask = (u64::from(read32(bar) & 0xFFFF_FFF0))
                        | (u64::from(read32(high)) << 32);
                    let size = (!mask).wrapping_add(1);
                    write32(bar, saved);
                    if size < BAR_SIZE as u64 {
                        continue;
                    }
                    write32(bar, (BAR_ADDRESS as u32) | flags);
                    write32(high, (BAR_ADDRESS >> 32) as u32);
                }
                READY.store(true, Ordering::Release);
                return Some(BAR_ADDRESS);
            }
        }
    }
    None
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}
