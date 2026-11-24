//! Kernel block-device selection.
//!
//! SFS speaks 512-byte sectors, so the kernel needs one synchronous device
//! behind it and a name for the boot log. The first backend that initializes
//! wins; a missing device is not a boot failure.

use crate::fdt::Fdt;
use silver_microkernel::pci::{PciHost, PciHostKind};

pub const SECTOR_SIZE: usize = 512;

#[derive(Clone, Copy)]
pub struct Device {
    pub name: &'static str,
    pub sector_count: fn() -> u64,
    pub read_sector: fn(u64, &mut [u8; SECTOR_SIZE]) -> bool,
    pub write_sector: fn(u64, &[u8; SECTOR_SIZE]) -> bool,
}

static mut ACTIVE: Option<Device> = None;

/// Tries SDHCI first, then virtio-blk, and installs the survivor.
///
/// virtio-blk is only tried when the tree has an ECAM host, or when there is
/// no tree at all (the QEMU `-kernel`/ELF path). On a Pi 5 tree there is no
/// ECAM, so the kernel never touches QEMU's fixed ECAM address there.
pub fn init(dtb: Option<&[u8]>) -> bool {
    if crate::sdhci::init(dtb) {
        install(Device {
            name: "sdhci",
            sector_count: crate::sdhci::sector_count,
            read_sector: crate::sdhci::read_sector,
            write_sector: crate::sdhci::write_sector,
        });
        return true;
    }
    if ecam_available(dtb) && crate::virtio_blk::init() {
        install(Device {
            name: "virtio-blk",
            sector_count: crate::virtio_blk::sector_count,
            read_sector: crate::virtio_blk::read_sector,
            write_sector: crate::virtio_blk::write_sector,
        });
        return true;
    }
    false
}

fn install(device: Device) {
    unsafe {
        let slot = core::ptr::addr_of_mut!(ACTIVE);
        *slot = Some(device);
    }
    crate::console::write_str("BLOCK: ");
    crate::console::write_str(device.name);
    crate::console::write_str(" sectors=");
    crate::console::write_dec_u64((device.sector_count)());
    crate::console::write_str("\r\n");
}

pub fn active() -> Option<Device> {
    unsafe { core::ptr::addr_of!(ACTIVE).read() }
}

pub fn sector_count() -> u64 {
    active().map(|device| (device.sector_count)()).unwrap_or(0)
}

pub fn read_sector(sector: u64, buffer: &mut [u8; SECTOR_SIZE]) -> bool {
    active()
        .map(|device| (device.read_sector)(sector, buffer))
        .unwrap_or(false)
}

pub fn write_sector(sector: u64, buffer: &[u8; SECTOR_SIZE]) -> bool {
    active()
        .map(|device| (device.write_sector)(sector, buffer))
        .unwrap_or(false)
}

/// True when QEMU's fixed ECAM window is safe to probe.
fn ecam_available(dtb: Option<&[u8]>) -> bool {
    let Some(data) = dtb else {
        return true;
    };
    let Some(tree) = Fdt::new(data) else {
        return false;
    };
    let mut hosts = [PciHost::new(PciHostKind::Ecam, 0, 0, 0, 0); 4];
    let count = tree.pci_hosts(&mut hosts);
    hosts[..count]
        .iter()
        .any(|host| host.kind == PciHostKind::Ecam)
}
