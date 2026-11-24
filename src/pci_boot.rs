//! Boot-time PCIe host discovery and endpoint logging.
//!
//! The policy and the walk live in the shared `pci` module; this file only
//! supplies the two MMIO backends the kernel has today: QEMU's ECAM window and
//! the BCM2712 index/data registers. It deliberately does not claim a driver:
//! finding RP1 is the first step, not a storage stack.

use silver_microkernel::pci::{self, ConfigSpace, PciDevice, PciHost, PciHostKind};

use crate::console;
use crate::fdt::Fdt;
use crate::memory::phys_to_virt;
use crate::mmu;

const MAX_HOSTS: usize = 4;
const MAX_DEVICES: usize = 16;

/// One mapped host bridge.
struct MmioConfig {
    host: PciHost,
    base: usize,
}

impl MmioConfig {
    fn bcm_link_up(&self) -> bool {
        // PCIE_MISC_PCIE_STATUS: DL active (bit 5) and PHY link up (bit 4).
        let status = unsafe { read32(self.base + 0x4068) };
        status & 0x30 == 0x30
    }
}

impl ConfigSpace for MmioConfig {
    fn read32(&mut self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        let offset = offset & 0xffc;
        match self.host.kind {
            PciHostKind::Ecam => {
                if !self.host.contains_bus(bus) {
                    return 0xffff_ffff;
                }
                let relative = (usize::from(bus - self.host.bus_start) << 20)
                    | (usize::from(device) << 15)
                    | (usize::from(function) << 12)
                    | offset;
                if relative >= self.host.size as usize {
                    return 0xffff_ffff;
                }
                unsafe { read32(self.base + relative) }
            }
            PciHostKind::Bcm2712 => {
                if bus == 0 {
                    // Bus 0, function 0 is the root complex's own config
                    // header, reached directly; other root-bus devfns are not
                    // devices and must not go through the index register.
                    if device == 0 && function == 0 {
                        return unsafe { read32(self.base + offset) };
                    }
                    return 0xffff_ffff;
                }
                if !self.bcm_link_up() {
                    return 0xffff_ffff;
                }
                let index = (u32::from(bus) << 20)
                    | (u32::from(device) << 15)
                    | (u32::from(function) << 12);
                unsafe {
                    write32(self.base + 0x9000, index);
                    core::arch::asm!("dsb sy", options(nostack, preserves_flags));
                    read32(self.base + 0x8000 + offset)
                }
            }
        }
    }
}

unsafe fn read32(address: usize) -> u32 {
    core::ptr::read_volatile(address as *const u32)
}

unsafe fn write32(address: usize, value: u32) {
    core::ptr::write_volatile(address as *mut u32, value);
}

/// Maps a host's register window into the kernel's high-half alias.
fn mapped_base(host: PciHost) -> Option<usize> {
    let base = usize::try_from(host.base).ok()?;
    let size = usize::try_from(host.size).ok()?;
    match host.kind {
        PciHostKind::Ecam if base == mmu::PCI_ECAM_BASE => {
            (size <= mmu::PCI_ECAM_SIZE).then(|| phys_to_virt(base))
        }
        PciHostKind::Bcm2712
            if base >= mmu::BCM_PERIPH_BASE
                && base
                    .checked_add(size)
                    .is_some_and(|end| end <= mmu::BCM_PERIPH_BASE + mmu::BCM_PERIPH_SIZE) =>
        {
            Some(phys_to_virt(base))
        }
        _ => None,
    }
}

fn write_hex(value: u64, digits: usize) {
    let mut buffer = [0u8; 16];
    let digits = digits.min(buffer.len());
    for (index, slot) in buffer[..digits].iter_mut().enumerate() {
        let shift = (digits - 1 - index) * 4;
        let digit = ((value >> shift) & 0xf) as u8;
        *slot = match digit {
            0..=9 => b'0' + digit,
            _ => b'A' + digit - 10,
        };
    }
    console::write_bytes(&buffer[..digits]);
}

fn log_host(host: PciHost) {
    console::write_str("PCI: host=");
    console::write_str(match host.kind {
        PciHostKind::Ecam => "ECAM",
        PciHostKind::Bcm2712 => "BCM2712",
    });
    console::write_str(" base=0x");
    write_hex(host.base, 10);
    console::write_str(" buses=");
    console::write_dec_u64(u64::from(host.bus_start));
    console::write_str("..");
    console::write_dec_u64(u64::from(host.bus_end));
    console::write_str("\r\n");
}

fn log_device(device: &PciDevice) {
    console::write_str("PCI ");
    write_hex(u64::from(device.bus), 2);
    console::write_str(":");
    write_hex(u64::from(device.device), 2);
    console::write_str(".");
    write_hex(u64::from(device.function), 1);
    console::write_str(" ");
    write_hex(u64::from(device.vendor), 4);
    console::write_str(":");
    write_hex(u64::from(device.device_id), 4);
    console::write_str(" class=");
    write_hex(u64::from(device.class), 8);
    console::write_str("\r\n");
}

/// Binds the enabled host bridges from the live tree and enumerates them.
///
/// A tree is required: there is no board-name fallback for PCIe, because
/// guessing a controller address on unknown firmware is how a boot turns into
/// a bus abort. QEMU's ECAM host is still found through its own tree node.
pub fn init(dtb: Option<&[u8]>) {
    let Some(data) = dtb else {
        return;
    };
    let Some(tree) = Fdt::new(data) else {
        return;
    };
    let mut hosts = [PciHost::new(PciHostKind::Ecam, 0, 0, 0, 0); MAX_HOSTS];
    let host_count = tree.pci_hosts(&mut hosts);
    if host_count == 0 {
        return;
    }
    for host in &hosts[..host_count] {
        log_host(*host);
        let Some(base) = mapped_base(*host) else {
            console::write_str("PCI: window not mapped\r\n");
            continue;
        };
        let mut config = MmioConfig { host: *host, base };
        if host.kind == PciHostKind::Bcm2712 && !config.bcm_link_up() {
            console::write_str("PCI: link down\r\n");
            continue;
        }
        let mut devices = [PciDevice::default(); MAX_DEVICES];
        let count = pci::scan(&mut config, *host, &mut devices);
        console::write_str("PCI: devices=");
        console::write_dec_u64(count as u64);
        console::write_str("\r\n");
        for device in &devices[..count] {
            log_device(device);
        }
        if let Some(rp1) = pci::find_rp1(&devices[..count]) {
            console::write_str("PCI: RP1 at ");
            write_hex(u64::from(rp1.bus), 2);
            console::write_str(":");
            write_hex(u64::from(rp1.device), 2);
            console::write_str(".");
            write_hex(u64::from(rp1.function), 1);
            console::write_str("\r\n");
        }
    }
}
