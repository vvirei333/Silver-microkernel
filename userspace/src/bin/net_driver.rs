#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

/// QEMU exposes virtio-net as a transitional (`0x1000`) or modern-only
/// (`0x1041`) PCI function. Both are driven through the modern virtio
/// capabilities; the legacy I/O BAR is never touched because this kernel has
/// no port I/O interface.
const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
const VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;
const VIRTIO_NET_MODERN: u16 = 0x1041;

const BAR_MAPPING_SIZE: u64 = 64 * 1024;
/// BARs of the `virt` machine start unassigned because nothing runs firmware
/// before the kernel. The block driver claims `0x1010_0000` for its BAR4, so
/// the network driver uses the next 64 KiB slot.
const ASSIGNED_BAR_BASE: u64 = 0x1020_0000;

/// Modern virtio-pci common configuration layout (virtio 1.2, 4.1.4.3).
const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00;
const COMMON_DEVICE_FEATURE: u64 = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: u64 = 0x08;
const COMMON_DRIVER_FEATURE: u64 = 0x0C;
const COMMON_DEVICE_STATUS: u64 = 0x14;
const COMMON_QUEUE_SELECT: u64 = 0x16;
const COMMON_QUEUE_SIZE: u64 = 0x18;
const COMMON_QUEUE_ENABLE: u64 = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const COMMON_QUEUE_DESC: u64 = 0x20;
const COMMON_QUEUE_DRIVER: u64 = 0x28;
const COMMON_QUEUE_DEVICE: u64 = 0x30;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

/// Virtio-net queue indices: queue 0 receives, queue 1 transmits.
const QUEUE_RX: u16 = 0;
const QUEUE_TX: u16 = 1;

/// Init failure stages. A failed driver exits with the stage in its status,
/// which the kernel always prints, so failures stay visible even though this
/// task does not own the console for long enough to rely on `print`.
const INIT_NOT_FOUND: u8 = 1;
const INIT_RESET: u8 = 2;
const INIT_BAD_VERSION: u8 = 3;
const INIT_FEATURES: u8 = 4;
const INIT_NO_QUEUE: u8 = 5;
const INIT_QUEUE_SETUP: u8 = 6;
const INIT_DMA: u8 = 7;
const INIT_MAC: u8 = 8;

/// Silverd spawns this task and hands the console over for one second, but the
/// scheduler may run the new task before the parent executes that handoff.
/// Sleeping first makes the MAC line land in the window where this task owns
/// the console; it does not affect device initialization.
const CONSOLE_HANDOFF_DELAY_MS: u64 = 100;

#[allow(dead_code)]
#[derive(Copy, Clone)]
struct VirtioCapability {
    bar: u8,
    offset: u32,
}

#[allow(dead_code)]
struct VirtioNet {
    bus: u8,
    device: u8,
    function: u8,
    common_cfg: VirtioCapability,
    common_cfg_addr: u64,
    notify_cfg: VirtioCapability,
    notify_bar_vaddr: u64,
    notify_off_multiplier: u32,
    device_cfg: VirtioCapability,
    device_cfg_addr: u64,
}

#[allow(dead_code)]
struct Virtqueue {
    index: u16,
    size: u16,
    notify_addr: u64,
    descriptors_vaddr: u64,
    available_vaddr: u64,
    used_vaddr: u64,
}

/// Kept live for the RX/TX path that follows in Phase 4. Setup already
/// programs both queues; no buffers are published yet.
#[allow(dead_code)]
static mut RX_QUEUE: Option<Virtqueue> = None;
#[allow(dead_code)]
static mut TX_QUEUE: Option<Virtqueue> = None;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::sleep(CONSOLE_HANDOFF_DELAY_MS);
    syscall::print("NET DRIVER STARTED\r\n");

    let Some(device) = find_virtio_net() else {
        fail(INIT_NOT_FOUND, "VIRTIO-NET NOT FOUND\r\n");
    };

    syscall::print("VIRTIO-NET FOUND ON PCI ");
    print_bdf(&device);
    syscall::print("\r\n");

    match unsafe { initialize(&device) } {
        Ok(mac) => {
            syscall::print("VIRTIO-NET INITIALIZED AND READY\r\n");
            print_mac(&mac);
        }
        Err(stage) => fail(stage, "VIRTIO-NET INITIALIZATION FAILED\r\n"),
    }

    // No interrupts or IPC yet: keep the task alive (and the device configured)
    // so the next phase can attach handlers without redoing setup.
    loop {
        syscall::sleep(1000);
    }
}

fn fail(stage: u8, message: &str) -> ! {
    syscall::print(message);
    syscall::print("VIRTIO-NET FAILURE STAGE ");
    syscall::print_dec_u64(u64::from(stage));
    syscall::print("\r\n");
    syscall::exit_with_status(u64::from(stage))
}

fn print_bdf(device: &VirtioNet) {
    syscall::print_dec_u64(u64::from(device.bus));
    syscall::print(":");
    syscall::print_dec_u64(u64::from(device.device));
    syscall::print(".");
    syscall::print_dec_u64(u64::from(device.function));
}

fn print_mac(mac: &[u8; 6]) {
    syscall::print("MAC: ");
    for (index, byte) in mac.iter().enumerate() {
        if index > 0 {
            syscall::print(":");
        }
        print_hex_byte(*byte);
    }
    syscall::print("\r\n");
}

fn print_hex_byte(value: u8) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut text = [0_u8; 2];
    text[0] = DIGITS[usize::from(value >> 4)];
    text[1] = DIGITS[usize::from(value & 0xF)];
    syscall::print_bytes(&text);
}

unsafe fn initialize(device: &VirtioNet) -> Result<[u8; 6], u8> {
    let common = device.common_cfg_addr;
    unsafe { reset_device(common)? };

    unsafe {
        core::ptr::write_volatile(
            (common + COMMON_DEVICE_STATUS) as *mut u8,
            VIRTIO_STATUS_ACKNOWLEDGE,
        );
        core::ptr::write_volatile(
            (common + COMMON_DEVICE_STATUS) as *mut u8,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
        );

        core::ptr::write_volatile((common + COMMON_DEVICE_FEATURE_SELECT) as *mut u32, 0);
        let features_low = core::ptr::read_volatile((common + COMMON_DEVICE_FEATURE) as *const u32);
        core::ptr::write_volatile((common + COMMON_DEVICE_FEATURE_SELECT) as *mut u32, 1);
        let features_high =
            core::ptr::read_volatile((common + COMMON_DEVICE_FEATURE) as *const u32);
        let features = (u64::from(features_high) << 32) | u64::from(features_low);
        syscall::print("DEVICE FEATURES: 0x");
        syscall::print_hex_u64(features);
        syscall::print("\r\n");

        // VIRTIO_F_VERSION_1 is bit 32. Without it the device cannot be driven
        // through the modern common configuration.
        if features_high & 1 == 0 {
            return Err(INIT_BAD_VERSION);
        }

        core::ptr::write_volatile((common + COMMON_DRIVER_FEATURE_SELECT) as *mut u32, 0);
        core::ptr::write_volatile((common + COMMON_DRIVER_FEATURE) as *mut u32, 0);
        core::ptr::write_volatile((common + COMMON_DRIVER_FEATURE_SELECT) as *mut u32, 1);
        core::ptr::write_volatile((common + COMMON_DRIVER_FEATURE) as *mut u32, 1);

        let status = core::ptr::read_volatile((common + COMMON_DEVICE_STATUS) as *const u8);
        core::ptr::write_volatile(
            (common + COMMON_DEVICE_STATUS) as *mut u8,
            status | VIRTIO_STATUS_FEATURES_OK,
        );
        if core::ptr::read_volatile((common + COMMON_DEVICE_STATUS) as *const u8)
            & VIRTIO_STATUS_FEATURES_OK
            == 0
        {
            return Err(INIT_FEATURES);
        }

        let rx = setup_virtqueue(device, QUEUE_RX)?;
        let tx = setup_virtqueue(device, QUEUE_TX)?;

        syscall::print("QUEUE 0 (RX): SIZE ");
        syscall::print_dec_u64(u64::from(rx.size));
        syscall::print(", NOTIFY 0x");
        syscall::print_hex_u64(rx.notify_addr);
        syscall::print("\r\n");
        syscall::print("QUEUE 1 (TX): SIZE ");
        syscall::print_dec_u64(u64::from(tx.size));
        syscall::print(", NOTIFY 0x");
        syscall::print_hex_u64(tx.notify_addr);
        syscall::print("\r\n");

        // Both queues are programmed: only now may the device be marked ready.
        let status = core::ptr::read_volatile((common + COMMON_DEVICE_STATUS) as *const u8);
        core::ptr::write_volatile(
            (common + COMMON_DEVICE_STATUS) as *mut u8,
            status | VIRTIO_STATUS_DRIVER_OK,
        );

        RX_QUEUE = Some(rx);
        TX_QUEUE = Some(tx);

        read_mac(device)
    }
}

unsafe fn reset_device(common: u64) -> Result<(), u8> {
    let status = (common + COMMON_DEVICE_STATUS) as *mut u8;
    unsafe { core::ptr::write_volatile(status, 0) };
    let mut remaining = 1_000_000_u32;
    while unsafe { core::ptr::read_volatile(status) } != 0 {
        if remaining == 0 {
            return Err(INIT_RESET);
        }
        remaining -= 1;
        core::hint::spin_loop();
    }
    Ok(())
}

unsafe fn setup_virtqueue(device: &VirtioNet, index: u16) -> Result<Virtqueue, u8> {
    let common = device.common_cfg_addr;
    unsafe {
        core::ptr::write_volatile((common + COMMON_QUEUE_SELECT) as *mut u16, index);
        core::arch::asm!("dsb sy", options(nostack));
    }

    let size = unsafe { core::ptr::read_volatile((common + COMMON_QUEUE_SIZE) as *const u16) };
    if size == 0 {
        return Err(INIT_NO_QUEUE);
    }
    let notify_offset =
        unsafe { core::ptr::read_volatile((common + COMMON_QUEUE_NOTIFY_OFF) as *const u16) };

    const DMA_PAGE_SIZE: u64 = 4096;
    let align_page = |bytes: u64| {
        bytes
            .checked_add(DMA_PAGE_SIZE - 1)
            .map(|value| value & !(DMA_PAGE_SIZE - 1))
    };
    let descriptor_span = align_page(u64::from(size) * 16).ok_or(INIT_DMA)?;
    let available_span = align_page(u64::from(size) * 2 + 6).ok_or(INIT_DMA)?;
    let used_span = align_page(u64::from(size) * 8 + 6).ok_or(INIT_DMA)?;
    let available_offset = descriptor_span;
    let used_offset = descriptor_span
        .checked_add(available_span)
        .ok_or(INIT_DMA)?;
    let total_size = used_offset.checked_add(used_span).ok_or(INIT_DMA)?;

    let (vaddr, paddr) = syscall::alloc_dma(total_size).ok_or(INIT_DMA)?;
    unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, total_size as usize) };

    let notify_addr = device.notify_bar_vaddr
        + u64::from(device.notify_cfg.offset)
        + u64::from(notify_offset) * u64::from(device.notify_off_multiplier);

    unsafe {
        core::ptr::write_volatile((common + COMMON_QUEUE_DESC) as *mut u64, paddr);
        core::ptr::write_volatile(
            (common + COMMON_QUEUE_DRIVER) as *mut u64,
            paddr + available_offset,
        );
        core::ptr::write_volatile(
            (common + COMMON_QUEUE_DEVICE) as *mut u64,
            paddr + used_offset,
        );
        core::arch::asm!("dsb sy", options(nostack));

        core::ptr::write_volatile((common + COMMON_QUEUE_ENABLE) as *mut u16, 1);
        if core::ptr::read_volatile((common + COMMON_QUEUE_ENABLE) as *const u16) != 1 {
            return Err(INIT_QUEUE_SETUP);
        }
    }

    Ok(Virtqueue {
        index,
        size,
        notify_addr,
        descriptors_vaddr: vaddr,
        available_vaddr: vaddr + available_offset,
        used_vaddr: vaddr + used_offset,
    })
}

/// The MAC is `virtio_net_config.mac`, the first field of the device-specific
/// configuration window. Legacy virtio-pci places that window at BAR0 + 0x14;
/// a modern device exposes the same window through its device-config
/// capability, so `device_cfg_addr` already includes that shift and the MAC is
/// read at offset 0 from it.
unsafe fn read_mac(device: &VirtioNet) -> Result<[u8; 6], u8> {
    let mut mac = [0_u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        *byte = unsafe {
            core::ptr::read_volatile((device.device_cfg_addr + index as u64) as *const u8)
        };
    }
    // All-zero or all-ones means the window was not decoded, not a real MAC.
    if mac == [0_u8; 6] || mac == [0xFF_u8; 6] {
        return Err(INIT_MAC);
    }
    Ok(mac)
}

fn pci_config_address(bus: u8, device: u8, function: u8, offset: u16) -> u64 {
    (u64::from(bus) << 20)
        | (u64::from(device) << 15)
        | (u64::from(function) << 12)
        | (u64::from(offset) & 0xFFC)
}

fn find_virtio_net() -> Option<VirtioNet> {
    for bus in 0..=u8::MAX {
        for device in 0..32 {
            for function in 0..8 {
                let Some(id) = syscall::pci_read32(pci_config_address(bus, device, function, 0x00))
                else {
                    continue;
                };
                let vendor_id = id as u16;
                let device_id = (id >> 16) as u16;
                if vendor_id != VIRTIO_VENDOR_ID
                    || !matches!(device_id, VIRTIO_NET_TRANSITIONAL | VIRTIO_NET_MODERN)
                {
                    continue;
                }

                // Memory decode plus bus mastering: the device needs both to
                // reach the queues in DRAM.
                let command_address = pci_config_address(bus, device, function, 0x04);
                let Some(command) = syscall::pci_read32(command_address) else {
                    continue;
                };
                if !syscall::pci_write32(command_address, command | 0x6) {
                    continue;
                }

                let Some((common_cfg, notify_cfg, device_cfg, notify_off_multiplier)) =
                    read_virtio_capabilities(bus, device, function)
                else {
                    continue;
                };
                let (Some(notify_cfg), Some(device_cfg), Some(notify_off_multiplier)) =
                    (notify_cfg, device_cfg, notify_off_multiplier)
                else {
                    continue;
                };

                let mut bars = BarCache::new();
                let Some(common_bar_vaddr) = bars.vaddr(bus, device, function, common_cfg.bar)
                else {
                    continue;
                };
                let Some(notify_bar_vaddr) = bars.vaddr(bus, device, function, notify_cfg.bar)
                else {
                    continue;
                };
                let Some(device_bar_vaddr) = bars.vaddr(bus, device, function, device_cfg.bar)
                else {
                    continue;
                };

                return Some(VirtioNet {
                    bus,
                    device,
                    function,
                    common_cfg,
                    common_cfg_addr: common_bar_vaddr + u64::from(common_cfg.offset),
                    notify_cfg,
                    notify_bar_vaddr,
                    notify_off_multiplier,
                    device_cfg,
                    device_cfg_addr: device_bar_vaddr + u64::from(device_cfg.offset),
                });
            }
        }
    }
    None
}

/// Maps one BAR per PCI function at most once, so the common, notify and
/// device capabilities that share QEMU's modern BAR4 do not consume three
/// separate windows.
struct BarCache {
    entries: [(u8, u64); 6],
    len: usize,
}

impl BarCache {
    const fn new() -> Self {
        Self {
            entries: [(0, 0); 6],
            len: 0,
        }
    }

    fn vaddr(&mut self, bus: u8, device: u8, function: u8, bar: u8) -> Option<u64> {
        for &(cached_bar, vaddr) in self.entries.iter().take(self.len) {
            if cached_bar == bar {
                return Some(vaddr);
            }
        }
        if self.len >= self.entries.len() {
            return None;
        }
        let vaddr = map_bar(bus, device, function, bar)?;
        self.entries[self.len] = (bar, vaddr);
        self.len += 1;
        Some(vaddr)
    }
}

/// Maps a memory BAR, assigning an address below 4 GiB first when the machine
/// left it at zero. I/O BARs are rejected: the kernel offers no port I/O.
fn map_bar(bus: u8, device: u8, function: u8, bar: u8) -> Option<u64> {
    if bar > 5 {
        return None;
    }
    let address = pci_config_address(bus, device, function, 0x10 + u16::from(bar) * 4);
    let low = syscall::pci_read32(address)?;
    if low & 0x1 != 0 {
        return None;
    }

    let is_64_bit = low & 0x4 != 0 && bar < 5;
    let mut base = u64::from(low & !0xF);
    if base == 0 || base == 0xFFFF_FFF0 {
        let assigned = ASSIGNED_BAR_BASE + u64::from(bar) * BAR_MAPPING_SIZE;
        if !syscall::pci_write32(address, assigned as u32 | (low & 0xF)) {
            return None;
        }
        if is_64_bit && !syscall::pci_write32(address + 4, (assigned >> 32) as u32) {
            return None;
        }
        base = assigned;
    } else if is_64_bit {
        base |= u64::from(syscall::pci_read32(address + 4)?) << 32;
    }

    syscall::map_phys(base, BAR_MAPPING_SIZE)
}

fn read_virtio_capabilities(
    bus: u8,
    device: u8,
    function: u8,
) -> Option<(
    VirtioCapability,
    Option<VirtioCapability>,
    Option<VirtioCapability>,
    Option<u32>,
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
    let mut notify_off_multiplier = None;
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
                2 => {
                    if cap_len < 20 || cap_ptr > 0xEC {
                        return None;
                    }
                    notify_off_multiplier = Some(syscall::pci_read32(pci_config_address(
                        bus,
                        device,
                        function,
                        cap_ptr + 16,
                    ))?);
                    notify_cfg = Some(capability);
                }
                4 => device_cfg = Some(capability),
                _ => {}
            }
        }
        cap_ptr = next_ptr;
        visited += 1;
    }

    Some((common_cfg?, notify_cfg, device_cfg, notify_off_multiplier))
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
