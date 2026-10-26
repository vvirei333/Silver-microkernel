#![no_std]
#![no_main]

#[path = "../ipc.rs"]
mod ipc;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const ASSIGNED_BAR0: u32 = 0x1000_0000;
const ASSIGNED_BAR4: u32 = 0x1010_0000;
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
    bar4_base: u32,
    common_cfg: VirtioCapability,
    common_cfg_addr: u64,
    notify_cfg: Option<VirtioCapability>,
    notify_bar_vaddr: u64,
    notify_off_multiplier: u32,
    device_cfg: Option<VirtioCapability>,
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

#[repr(C)]
struct VirtioBlkRequest {
    request_type: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
struct VirtqDescriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvailable {
    flags: u16,
    idx: u16,
    ring: [u16; 0],
}

#[repr(C)]
struct VirtqUsedElement {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElement; 0],
}

static mut QUEUE: Option<Virtqueue> = None;
static mut REQUEST_BUFFER: Option<(u64, u64)> = None;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::print("BLOCK DRIVER STARTED\r\n");

    if let Some((mut device, cfg_bar_vaddr, notify_bar_vaddr)) = find_virtio_blk() {
        device.common_cfg_addr = cfg_bar_vaddr + u64::from(device.common_cfg.offset);
        device.notify_bar_vaddr = notify_bar_vaddr;
        syscall::print("FOUND VIRTIO-BLK AT BAR0: 0x");
        syscall::print_hex_u64(u64::from(device.bar_base));
        syscall::print("\r\n");

        unsafe {
            let device_status = (device.common_cfg_addr + 0x14) as *mut u8;
            core::ptr::write_volatile(device_status, 0);
            core::ptr::write_volatile(device_status, 1);
            core::ptr::write_volatile(device_status, 3);

            let feature_select = device.common_cfg_addr as *mut u32;
            core::ptr::write_volatile(feature_select, 0);
            let features_low =
                core::ptr::read_volatile((device.common_cfg_addr + 0x04) as *const u32);
            core::ptr::write_volatile(feature_select, 1);
            let features_high =
                core::ptr::read_volatile((device.common_cfg_addr + 0x04) as *const u32);
            let features = (u64::from(features_high) << 32) | u64::from(features_low);
            syscall::print("DEVICE FEATURES: 0x");
            syscall::print_hex_u64(features);
            syscall::print("\r\n");

            if features_high & 1 == 0 {
                syscall::print("VIRTIO_F_VERSION_1 NOT SUPPORTED\r\n");
            } else if !negotiate_features(&device) {
                syscall::print("VIRTIO FEATURE NEGOTIATION FAILED\r\n");
            } else if let Some(queue) = prepare_virtqueue(&device) {
                syscall::print("VIRTQUEUE 0 FOUND: SIZE ");
                syscall::print_dec_u64(u64::from(queue.size));
                syscall::print(", NOTIFY 0x");
                syscall::print_hex_u64(queue.notify_addr);
                syscall::print("\r\n");
                if let Some(queue) = initialize_virtqueue(&device, queue) {
                    syscall::print("VIRTIO-BLK INITIALIZED AND READY\r\n");
                    QUEUE = Some(queue);
                } else {
                    syscall::print("VIRTQUEUE 0 INITIALIZATION FAILED\r\n");
                }
            } else {
                syscall::print("VIRTQUEUE 0 NOT AVAILABLE\r\n");
            }
        }
    } else {
        syscall::print("VIRTIO-BLK NOT FOUND\r\n");
    }

    serve_ipc()
}

fn serve_ipc() -> ! {
    unsafe { REQUEST_BUFFER = syscall::alloc_dma(4096) };
    if unsafe { (*core::ptr::addr_of!(REQUEST_BUFFER)).is_none() } {
        syscall::print("BLOCK DRIVER REQUEST DMA ALLOCATION FAILED\r\n");
    }

    loop {
        let mut request = ipc::Message::new(0, &[]);
        if !syscall::receive(&mut request) || request.sender != 3 {
            continue;
        }

        let mut sector_bytes = [0_u8; 8];
        sector_bytes.copy_from_slice(&request.payload[..8]);
        let sector = u64::from_le_bytes(sector_bytes);
        let queue = unsafe { (*core::ptr::addr_of!(QUEUE)).as_ref() };
        let response = if let Some(queue) = queue {
            let mut data = [0_u8; 512];
            if unsafe { read_sector(queue, sector, &mut data) } {
                let (_, request_paddr) = unsafe { (*core::ptr::addr_of!(REQUEST_BUFFER)).unwrap() };
                let data_paddr = request_paddr + 16;
                let mut payload = [0_u8; 11];
                payload[0] = 1;
                payload[1..9].copy_from_slice(&data_paddr.to_le_bytes());
                payload[9..11].copy_from_slice(&512_u16.to_le_bytes());
                ipc::Message::new(0, &payload)
            } else {
                ipc::Message::new(0, &[0])
            }
        } else {
            ipc::Message::new(0, &[2])
        };
        let shared_buffer = response.payload[0] == 1;
        if syscall::send(request.sender, &response) && shared_buffer {
            loop {
                let mut acknowledgement = ipc::Message::new(0, &[]);
                if syscall::receive(&mut acknowledgement)
                    && acknowledgement.sender == request.sender
                    && acknowledgement.payload[0] == 0xA5
                {
                    break;
                }
            }
        }
    }
}

unsafe fn negotiate_features(device: &VirtioBlk) -> bool {
    let common = device.common_cfg_addr;
    let driver_feature_select = (common + 0x08) as *mut u32;
    let driver_features = (common + 0x0C) as *mut u32;
    unsafe {
        core::ptr::write_volatile(driver_feature_select, 0);
        core::ptr::write_volatile(driver_features, 0);
        core::ptr::write_volatile(driver_feature_select, 1);
        core::ptr::write_volatile(driver_features, 1);

        let device_status = (common + 0x14) as *mut u8;
        let status = core::ptr::read_volatile(device_status);
        core::ptr::write_volatile(device_status, status | 0x08);
        core::ptr::read_volatile(device_status) & 0x08 != 0
    }
}

unsafe fn prepare_virtqueue(device: &VirtioBlk) -> Option<Virtqueue> {
    let common = device.common_cfg_addr;
    let notify_cfg = device.notify_cfg?;
    if device.notify_off_multiplier == 0 {
        return None;
    }

    let num_queues = unsafe { core::ptr::read_volatile((common + 0x12) as *const u16) };
    if num_queues == 0 {
        return None;
    }

    unsafe { core::ptr::write_volatile((common + 0x16) as *mut u16, 0) };
    let queue_size = unsafe { core::ptr::read_volatile((common + 0x18) as *const u16) };
    let queue_notify_off = unsafe { core::ptr::read_volatile((common + 0x1E) as *const u16) };
    if queue_size == 0 {
        return None;
    }

    let notify_addr = device.notify_bar_vaddr
        + u64::from(notify_cfg.offset)
        + u64::from(queue_notify_off) * u64::from(device.notify_off_multiplier);

    Some(Virtqueue {
        index: 0,
        size: queue_size,
        notify_addr,
        descriptors_vaddr: 0,
        available_vaddr: 0,
        used_vaddr: 0,
    })
}

unsafe fn initialize_virtqueue(device: &VirtioBlk, mut queue: Virtqueue) -> Option<Virtqueue> {
    const DMA_PAGE_SIZE: u64 = 4096;
    let align_page = |size: u64| {
        size.checked_add(DMA_PAGE_SIZE - 1)
            .map(|size| size & !(DMA_PAGE_SIZE - 1))
    };
    let queue_size = u64::from(queue.size);
    let descriptor_size = queue_size.checked_mul(16)?;
    let available_size = queue_size.checked_mul(2)?.checked_add(6)?;
    let used_size = queue_size.checked_mul(8)?.checked_add(6)?;
    let descriptor_span = align_page(descriptor_size)?;
    let available_span = align_page(available_size)?;
    let used_span = align_page(used_size)?;
    let used_offset = descriptor_span.checked_add(available_span)?;
    let total_size = used_offset.checked_add(used_span)?;

    let (vaddr, paddr) = syscall::alloc_dma(total_size)?;
    unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, total_size as usize) };

    queue.descriptors_vaddr = vaddr;
    queue.available_vaddr = vaddr + descriptor_span;
    queue.used_vaddr = vaddr + used_offset;

    let common = device.common_cfg_addr;
    unsafe {
        core::ptr::write_volatile((common + 0x16) as *mut u16, queue.index);
        core::ptr::write_volatile((common + 0x20) as *mut u64, paddr);
        core::ptr::write_volatile((common + 0x28) as *mut u64, paddr + descriptor_span);
        core::ptr::write_volatile((common + 0x30) as *mut u64, paddr + used_offset);
        core::arch::asm!("dsb sy", options(nostack));
        core::ptr::write_volatile((common + 0x1C) as *mut u16, 1);
        if core::ptr::read_volatile((common + 0x1C) as *const u16) != 1 {
            return None;
        }

        let device_status = (common + 0x14) as *mut u8;
        let status = core::ptr::read_volatile(device_status);
        core::ptr::write_volatile(device_status, status | 0x04);
        if core::ptr::read_volatile(device_status) & 0x04 == 0 {
            return None;
        }
    }

    Some(queue)
}

unsafe fn read_sector(queue: &Virtqueue, sector: u64, buffer: &mut [u8; 512]) -> bool {
    const REQUEST_DMA_SIZE: usize = 4096;
    const DATA_OFFSET: u64 = 16;
    const STATUS_OFFSET: u64 = DATA_OFFSET + 512;
    if queue.size < 3 {
        return false;
    }

    let Some((request_vaddr, request_paddr)) = (*core::ptr::addr_of!(REQUEST_BUFFER)) else {
        return false;
    };
    core::ptr::write_bytes(request_vaddr as *mut u8, 0, REQUEST_DMA_SIZE);
    core::ptr::write_volatile(
        request_vaddr as *mut VirtioBlkRequest,
        VirtioBlkRequest {
            request_type: 0,
            reserved: 0,
            sector,
        },
    );

    let descriptors = queue.descriptors_vaddr as *mut VirtqDescriptor;
    core::ptr::write_volatile(
        descriptors,
        VirtqDescriptor {
            addr: request_paddr,
            len: 16,
            flags: 1,
            next: 1,
        },
    );
    core::ptr::write_volatile(
        descriptors.add(1),
        VirtqDescriptor {
            addr: request_paddr + DATA_OFFSET,
            len: 512,
            flags: 1 | 2,
            next: 2,
        },
    );
    core::ptr::write_volatile(
        descriptors.add(2),
        VirtqDescriptor {
            addr: request_paddr + STATUS_OFFSET,
            len: 1,
            flags: 2,
            next: 0,
        },
    );

    let available = queue.available_vaddr as *mut VirtqAvailable;
    let used = queue.used_vaddr as *const VirtqUsed;
    let available_idx = core::ptr::read_volatile(core::ptr::addr_of!((*available).idx));
    let previous_used_idx = core::ptr::read_volatile(core::ptr::addr_of!((*used).idx));
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*available).flags), 0);
    let available_ring = core::ptr::addr_of_mut!((*available).ring).cast::<u16>();
    core::ptr::write_volatile(
        available_ring.add(usize::from(available_idx % queue.size)),
        0,
    );
    core::arch::asm!("dsb sy", options(nostack));
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*available).idx),
        available_idx.wrapping_add(1),
    );
    core::arch::asm!("dsb sy", options(nostack));
    core::ptr::write_volatile(queue.notify_addr as *mut u16, queue.index);

    let mut remaining = 10_000_000_u32;
    while core::ptr::read_volatile(core::ptr::addr_of!((*used).idx)) == previous_used_idx {
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        core::hint::spin_loop();
    }
    core::arch::asm!("dsb sy", options(nostack));

    let used_ring = core::ptr::addr_of!((*used).ring).cast::<VirtqUsedElement>();
    let used_element =
        core::ptr::read_volatile(used_ring.add(usize::from(previous_used_idx % queue.size)));
    if used_element.id != 0 {
        return false;
    }

    let status = (request_vaddr + STATUS_OFFSET) as *const u8;
    if core::ptr::read_volatile(status) != 0 {
        return false;
    }

    let data = (request_vaddr + DATA_OFFSET) as *const u8;
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = core::ptr::read_volatile(data.add(index));
    }
    true
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

fn assign_bar4(bus: u8, device: u8, function: u8) -> Option<u32> {
    let address = pci_config_address(bus, device, function, 0x20);
    let bar4 = syscall::pci_read32(address)?;
    let bar4_base = bar4 & !0xF;
    if bar4_base == 0 || bar4_base == u32::MAX & !0xF {
        if syscall::pci_write32(address, ASSIGNED_BAR4) {
            Some(ASSIGNED_BAR4)
        } else {
            None
        }
    } else if bar4 & 0x1 == 0 {
        Some(bar4_base)
    } else {
        None
    }
}

fn find_virtio_blk() -> Option<(VirtioBlk, u64, u64)> {
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
                let bar0_vaddr = syscall::map_phys(u64::from(bar_base), BAR_MAPPING_SIZE)?;
                let (common_cfg, notify_cfg, device_cfg, notify_off_multiplier) =
                    read_virtio_capabilities(bus, device, function)?;
                let notify_cfg = notify_cfg?;
                let notify_off_multiplier = notify_off_multiplier?;

                syscall::print("COMMON CFG: BAR ");
                syscall::print_dec_u64(u64::from(common_cfg.bar));
                syscall::print(", OFFSET 0x");
                syscall::print_hex_u64(u64::from(common_cfg.offset));
                syscall::print("\r\n");

                let needs_bar4 = common_cfg.bar == 4 || notify_cfg.bar == 4;
                let (bar4_base, bar4_vaddr) = if needs_bar4 {
                    let bar4_base = assign_bar4(bus, device, function)?;
                    let bar4_vaddr = syscall::map_phys(u64::from(bar4_base), BAR_MAPPING_SIZE)?;
                    (bar4_base, Some(bar4_vaddr))
                } else {
                    (0, None)
                };
                let cfg_bar_vaddr = match common_cfg.bar {
                    0 => bar0_vaddr,
                    4 => bar4_vaddr?,
                    _ => return None,
                };
                let notify_bar_vaddr = match notify_cfg.bar {
                    0 => bar0_vaddr,
                    4 => bar4_vaddr?,
                    _ => return None,
                };

                return Some((
                    VirtioBlk {
                        bus,
                        device,
                        function,
                        bar_base,
                        bar4_base,
                        common_cfg,
                        common_cfg_addr: 0,
                        notify_cfg: Some(notify_cfg),
                        notify_bar_vaddr: 0,
                        notify_off_multiplier,
                        device_cfg,
                    },
                    cfg_bar_vaddr,
                    notify_bar_vaddr,
                ));
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
