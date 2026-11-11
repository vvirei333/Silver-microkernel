//! virtio-gpu discovery.
//!
//! `build_and_qemu.sh --gpu` attaches a virtio-gpu. The full driver (control
//! queue, resources, DRM, virgl) is not written yet; this module finds the
//! device and reports its BARs so the next step can be checked against a real
//! device. It is deliberately read-only: it enables the PCI memory window and
//! prints what the device exposes.

use crate::console;

const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
/// Modern virtio-gpu: 0x1050. Transitional: 0x0010.
const PCI_DEVICE_GPU_MODERN: u16 = 0x1050;
const PCI_DEVICE_GPU_TRANSITIONAL: u16 = 0x0010;

fn pci_address(bus: u8, device: u8, function: u8, offset: usize) -> Option<usize> {
    if offset % 4 != 0 || offset >= 0x1000 {
        return None;
    }
    Some(crate::memory::phys_to_virt(
        crate::mmu::PCI_ECAM_BASE
            + ((bus as usize) << 20)
            + ((device as usize) << 15)
            + ((function as usize) << 12)
            + offset,
    ))
}

fn read32(bus: u8, device: u8, function: u8, offset: usize) -> Option<u32> {
    let address = pci_address(bus, device, function, offset)?;
    Some(unsafe { core::ptr::read_volatile(address as *const u32) })
}

fn write32(bus: u8, device: u8, function: u8, offset: usize, value: u32) -> bool {
    match pci_address(bus, device, function, offset) {
        Some(address) => {
            unsafe { core::ptr::write_volatile(address as *mut u32, value) };
            true
        }
        None => false,
    }
}

fn write_hex(value: u32) {
    console::write_str("0x");
    console::write_hex_u64(value as u64);
}

/// Scans PCI for a virtio-gpu and prints its BARs. Returns true when a device
/// is present.
pub fn probe() -> bool {
    for device in 0..32 {
        for function in 0..8 {
            let Some(id) = read32(0, device, function, 0x00) else {
                continue;
            };
            if id == 0xFFFF_FFFF {
                continue;
            }
            let vendor = id as u16;
            let product = (id >> 16) as u16;
            if vendor != PCI_VENDOR_VIRTIO
                || (product != PCI_DEVICE_GPU_MODERN
                    && product != PCI_DEVICE_GPU_TRANSITIONAL)
            {
                continue;
            }

            console::write_str("VIRTIO-GPU: device ");
            console::write_dec_u64(device as u64);
            console::write_str(".");
            console::write_dec_u64(function as u64);
            console::write_str(" id ");
            write_hex(product as u32);
            console::write_str("\r\n");

            // Enable memory space + bus mastering for the future driver.
            if let Some(command) = read32(0, device, function, 0x04) {
                let _ = write32(0, device, function, 0x04, command | 0x6);
            }

            for (index, name) in [
                (0x10, "BAR0"),
                (0x14, "BAR1"),
                (0x18, "BAR2"),
                (0x1C, "BAR3"),
                (0x20, "BAR4"),
                (0x24, "BAR5"),
            ] {
                if let Some(bar) = read32(0, device, function, index) {
                    if bar != 0 && bar != 0xFFFF_FFFF {
                        console::write_str("VIRTIO-GPU: ");
                        console::write_str(name);
                        console::write_str(" ");
                        write_hex(bar);
                        console::write_str("\r\n");
                    }
                }
            }
            return true;
        }
    }
    false
}
