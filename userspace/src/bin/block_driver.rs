#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const ASSIGNED_BAR0: u32 = 0x1000_0000;
const BAR_MAPPING_SIZE: u64 = 64 * 1024;

#[allow(dead_code)]
#[derive(Copy, Clone)]
struct VirtioCapability {
    bar: u8,
    offset: u32,
}

#[allow(dead_code)]
struct VirtioBlk {
    bus: u8,
    device: u8,
    function: u8,
    bar_base: u32,
    common_cfg: VirtioCapability,
    notify_cfg: Option<VirtioCapability>,
    device_cfg: Option<VirtioCapability>,
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("BLOCK DRIVER STARTED\r\n");

    let Some((virtual_address, _physical_address)) = syscall::alloc_dma(4 * 4096) else {
        syscall::exit_with_status(1);
    };
    unsafe {
        core::ptr::write_volatile(virtual_address as *mut u32, 0x5349_4C56);
        if core::ptr::read_volatile(virtual_address as *const u32) != 0x5349_4C56 {
            syscall::exit_with_status(1);
        }
    }

    if let Some(device) = find_virtio_blk() {
        syscall::print("FOUND VIRTIO-BLK AT BAR0: 0x");
        syscall::print_hex_u64(u64::from(device.bar_base));
        syscall::print("\r\n");
    } else {
        syscall::print("VIRTIO-BLK NOT FOUND\r\n");
    }

    loop {
        syscall::sleep(1000);
    }
}

fn pci_config_address(bus: u8, device: u8, function: u8, offset: u16) -> u64 {
    (u64::from(bus) << 20)
        | (u64::from(device) << 15)
        | (u64::from(function) << 12)
        | (u64::from(offset) & 0xFFC)
}

fn assign_bar0(bus: u8, device: u8, function: u8) -> Option<u32> {
    let address = pci_config_address(bus, device, function, 0x10);
    if syscall::pci_write32(address, ASSIGNED_BAR0) {
        Some(ASSIGNED_BAR0)
    } else {
        None
    }
}

fn find_virtio_blk() -> Option<VirtioBlk> {
    for bus in 0..=u8::MAX {
        for device in 0..32 {
            for function in 0..8 {
                let id = syscall::pci_read32(pci_config_address(bus, device, function, 0x00))?;
                let vendor_id = id as u16;
                let device_id = (id >> 16) as u16;
                if vendor_id != 0x1AF4 || !matches!(device_id, 0x1001 | 0x1042) {
                    continue;
                }

                let command_address = pci_config_address(bus, device, function, 0x04);
                let command = syscall::pci_read32(command_address)?;
                if !syscall::pci_write32(command_address, command | 0x6) {
                    return None;
                }

                let bar0 = syscall::pci_read32(pci_config_address(bus, device, function, 0x10))?;
                let bar_base = if bar0 == 0 || bar0 == u32::MAX {
                    assign_bar0(bus, device, function)?
                } else {
                    if bar0 & 0x1 != 0 {
                        continue;
                    }
                    bar0 & !0xF
                };
                let _mmio_base = syscall::map_phys(u64::from(bar_base), BAR_MAPPING_SIZE)?;
                let (common_cfg, notify_cfg, device_cfg) =
                    read_virtio_capabilities(bus, device, function)?;

                syscall::print("COMMON CFG: BAR ");
                syscall::print_dec_u64(u64::from(common_cfg.bar));
                syscall::print(", OFFSET 0x");
                syscall::print_hex_u64(u64::from(common_cfg.offset));
                syscall::print("\r\n");

                return Some(VirtioBlk {
                    bus,
                    device,
                    function,
                    bar_base,
                    common_cfg,
                    notify_cfg,
                    device_cfg,
                });
            }
        }
    }
    None
}

fn read_virtio_capabilities(
    bus: u8,
    device: u8,
    function: u8,
) -> Option<(
    VirtioCapability,
    Option<VirtioCapability>,
    Option<VirtioCapability>,
)> {
    let command_status = syscall::pci_read32(pci_config_address(bus, device, function, 0x04))?;
    let status = (command_status >> 16) as u16;
    if status & 0x10 == 0 {
        return None;
    }

    let mut cap_ptr =
        (syscall::pci_read32(pci_config_address(bus, device, function, 0x34))? & 0xFF) as u16;
    let mut common_cfg = None;
    let mut notify_cfg = None;
    let mut device_cfg = None;
    let mut visited = 0;

    while cap_ptr != 0 {
        if visited >= 48 || cap_ptr & 0x3 != 0 {
            return None;
        }
        let word = syscall::pci_read32(pci_config_address(bus, device, function, cap_ptr))?;
        let next_ptr = ((word >> 8) & 0xFF) as u16;
        let cap_id = word & 0xFF;
        let cap_len = (word >> 16) & 0xFF;
        if cap_id == 0x09 {
            if cap_len < 16 || cap_ptr > 0xF4 {
                return None;
            }
            let cfg_type = ((word >> 24) & 0xFF) as u8;
            let bar_word =
                syscall::pci_read32(pci_config_address(bus, device, function, cap_ptr + 4))?;
            let bar = (bar_word & 0xFF) as u8;
            let offset =
                syscall::pci_read32(pci_config_address(bus, device, function, cap_ptr + 8))?;
            let capability = VirtioCapability { bar, offset };
            match cfg_type {
                1 => common_cfg = Some(capability),
                2 => notify_cfg = Some(capability),
                4 => device_cfg = Some(capability),
                _ => {}
            }
        }
        cap_ptr = next_ptr;
        visited += 1;
    }

    Some((common_cfg?, notify_cfg, device_cfg))
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
