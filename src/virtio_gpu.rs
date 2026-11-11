//! virtio-gpu driver: control queue + display discovery.
//!
//! This is the first slice of the GPU stack. It claims the device, brings up
//! the control queue and asks for the display info, which proves the virtio
//! transport and the GPU command path. Resource creation, scanout, DRM and the
//! virgl 3D context build on the same queue.

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;

const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
const PCI_DEVICE_GPU_MODERN: u16 = 0x1050;
const PCI_DEVICE_GPU_TRANSITIONAL: u16 = 0x0010;

/// A different 1 GiB device window than virtio-blk's 0x1010_0000.
const GPU_BAR4_PA: usize = 0x1020_0000;
const NOTIFY_OFF: usize = 0x3000;
const DEVICE_CFG_OFF: usize = 0x2000;
const QUEUE_SIZE: usize = 64;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const CTRL_HDR_LEN: usize = 24;
const DISPLAY_INFO_LEN: usize = CTRL_HDR_LEN + 16 * 24;

struct Queue {
    descriptors: usize,
    available: usize,
    used: usize,
    notify: usize,
    last_used: u16,
}

static mut QUEUE: Option<Queue> = None;

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

fn pci_read32(bus: u8, device: u8, function: u8, offset: usize) -> Option<u32> {
    let address = pci_address(bus, device, function, offset)?;
    Some(unsafe { core::ptr::read_volatile(address as *const u32) })
}

fn pci_write32(bus: u8, device: u8, function: u8, offset: usize, value: u32) -> bool {
    match pci_address(bus, device, function, offset) {
        Some(address) => {
            unsafe { core::ptr::write_volatile(address as *mut u32, value) };
            true
        }
        None => false,
    }
}

fn find_device() -> Option<(u8, u8, u8)> {
    for device in 0..32 {
        for function in 0..8 {
            let Some(id) = pci_read32(0, device, function, 0x00) else {
                continue;
            };
            if id == 0xFFFF_FFFF {
                continue;
            }
            let vendor = id as u16;
            let product = (id >> 16) as u16;
            if vendor == PCI_VENDOR_VIRTIO
                && (product == PCI_DEVICE_GPU_MODERN
                    || product == PCI_DEVICE_GPU_TRANSITIONAL)
            {
                return Some((0, device as u8, function as u8));
            }
        }
    }
    None
}

fn claim_device(bus: u8, device: u8, function: u8) -> bool {
    let Some(command) = pci_read32(bus, device, function, 0x04) else {
        return false;
    };
    if !pci_write32(bus, device, function, 0x04, command | 0x6) {
        return false;
    }
    // BAR4 is a 64-bit memory BAR on QEMU's virtio-gpu.
    if !pci_write32(bus, device, function, 0x20, GPU_BAR4_PA as u32) {
        return false;
    }
    let _ = pci_write32(bus, device, function, 0x24, 0);
    let Some(assigned) = pci_read32(bus, device, function, 0x20) else {
        return false;
    };
    if (assigned & !0xF) != GPU_BAR4_PA as u32 {
        console::write_str("VIRTIO-GPU: BAR4 relocation failed\r\n");
        return false;
    }
    true
}

fn common(offset: usize) -> usize {
    phys_to_virt(GPU_BAR4_PA) + offset
}

unsafe fn read8(offset: usize) -> u8 {
    core::ptr::read_volatile(common(offset) as *const u8)
}

unsafe fn read16(offset: usize) -> u16 {
    core::ptr::read_volatile(common(offset) as *const u16)
}

unsafe fn read32(offset: usize) -> u32 {
    core::ptr::read_volatile(common(offset) as *const u32)
}

unsafe fn write8(offset: usize, value: u8) {
    core::ptr::write_volatile(common(offset) as *mut u8, value);
}

unsafe fn write16(offset: usize, value: u16) {
    core::ptr::write_volatile(common(offset) as *mut u16, value);
}

unsafe fn write32(offset: usize, value: u32) {
    core::ptr::write_volatile(common(offset) as *mut u32, value);
}

unsafe fn write64(offset: usize, value: u64) {
    core::ptr::write_volatile(common(offset) as *mut u64, value);
}

unsafe fn clean(address: usize, length: usize) {
    let line = 64usize;
    let mut current = address & !(line - 1);
    let end = address + length;
    while current < end {
        core::arch::asm!("dc cvac, {}", in(reg) current, options(nostack, preserves_flags));
        current += line;
    }
    core::arch::asm!("dsb sy", options(nostack));
}

unsafe fn invalidate(address: usize, length: usize) {
    let line = 64usize;
    let mut current = address & !(line - 1);
    let end = address + length;
    while current < end {
        core::arch::asm!("dc ivac, {}", in(reg) current, options(nostack, preserves_flags));
        current += line;
    }
    core::arch::asm!("dsb sy", options(nostack));
}

unsafe fn init_device() -> Option<Queue> {
    write8(0x14, 0); // reset
    write8(0x14, 1); // ACKNOWLEDGE
    write8(0x14, 3); // ACKNOWLEDGE | DRIVER

    // Driver features: VIRTIO_F_VERSION_1 (word 1, bit 0).
    write32(0x08, 1);
    write32(0x0C, 1);

    write8(0x14, 3 | 8); // FEATURES_OK
    if read8(0x14) & 8 == 0 {
        console::write_str("VIRTIO-GPU: FEATURES_OK rejected\r\n");
        return None;
    }

    write16(0x16, 0); // queue 0: control
    let max_queue = read16(0x18);
    if max_queue < QUEUE_SIZE as u16 {
        console::write_str("VIRTIO-GPU: control queue too small\r\n");
        return None;
    }
    write16(0x18, QUEUE_SIZE as u16);
    if read16(0x18) != QUEUE_SIZE as u16 {
        console::write_str("VIRTIO-GPU: queue size rejected\r\n");
        return None;
    }

    let page = 4096usize;
    let descriptor_span = (QUEUE_SIZE * 16).div_ceil(page) * page;
    let available_span = (QUEUE_SIZE * 2 + 6).div_ceil(page) * page;
    let used_span = (QUEUE_SIZE * 8 + 6).div_ceil(page) * page;
    let total_span = descriptor_span + available_span + used_span;
    let (ring_pa, _tag) = frame_allocator::alloc_contiguous_pages_tagged(total_span / page)?;
    let ring_va = phys_to_virt(ring_pa);
    core::ptr::write_bytes(ring_va as *mut u8, 0, total_span);

    let available_pa = ring_pa + descriptor_span;
    let used_pa = available_pa + available_span;

    write64(0x20, ring_pa as u64);
    write64(0x28, available_pa as u64);
    write64(0x30, used_pa as u64);
    core::arch::asm!("dsb sy", options(nostack));
    write16(0x1C, 1);
    if read16(0x1C) != 1 {
        console::write_str("VIRTIO-GPU: queue enable failed\r\n");
        return None;
    }

    let notify_off = read16(0x1E) as usize;
    write8(0x14, 3 | 8 | 4); // DRIVER_OK

    Some(Queue {
        descriptors: ring_va,
        available: phys_to_virt(available_pa),
        used: phys_to_virt(used_pa),
        notify: phys_to_virt(GPU_BAR4_PA) + NOTIFY_OFF + notify_off * 4,
        last_used: 0,
    })
}

/// Submits one control command and waits for its response.
unsafe fn control(queue: &mut Queue, command: &[u8], response: &mut [u8]) -> bool {
    let page = 4096usize;
    let (buffer_pa, _tag) = match frame_allocator::alloc_contiguous_pages_tagged(2) {
        Some(value) => value,
        None => return false,
    };
    let buffer = phys_to_virt(buffer_pa);
    core::ptr::write_bytes(buffer as *mut u8, 0, page * 2);
    core::ptr::copy_nonoverlapping(command.as_ptr(), buffer as *mut u8, command.len());
    let response_va = buffer + page;

    let descriptor = queue.descriptors;
    core::ptr::write_volatile(descriptor as *mut u64, buffer_pa as u64);
    core::ptr::write_volatile((descriptor + 8) as *mut u32, command.len() as u32);
    core::ptr::write_volatile((descriptor + 12) as *mut u16, VIRTQ_DESC_F_NEXT);
    core::ptr::write_volatile((descriptor + 14) as *mut u16, 1);

    core::ptr::write_volatile((descriptor + 16) as *mut u64, (buffer_pa + page) as u64);
    core::ptr::write_volatile((descriptor + 24) as *mut u32, response.len() as u32);
    core::ptr::write_volatile((descriptor + 28) as *mut u16, VIRTQ_DESC_F_WRITE);
    core::ptr::write_volatile((descriptor + 30) as *mut u16, 0);

    let available_slot = core::ptr::read_volatile((queue.available + 2) as *const u16);
    core::ptr::write_volatile(
        (queue.available + 4 + (available_slot as usize % QUEUE_SIZE) * 2) as *mut u16,
        0,
    );
    core::arch::asm!("dmb sy", options(nostack));
    core::ptr::write_volatile(
        (queue.available + 2) as *mut u16,
        available_slot.wrapping_add(1),
    );

    clean(buffer, page);
    clean(queue.descriptors, 32);
    clean(queue.available, 6 + QUEUE_SIZE * 2);
    core::arch::asm!("dsb sy", options(nostack));
    core::ptr::write_volatile(queue.notify as *mut u16, 0);
    core::arch::asm!("dsb sy", options(nostack));

    let mut spins = 0u64;
    loop {
        invalidate(queue.used, 8);
        let used = core::ptr::read_volatile((queue.used + 2) as *const u16);
        if used != queue.last_used {
            break;
        }
        spins += 1;
        if spins > 50_000_000 {
            console::write_str("VIRTIO-GPU: control command timed out\r\n");
            return false;
        }
        core::hint::spin_loop();
    }
    queue.last_used = queue.last_used.wrapping_add(1);

    invalidate(response_va, response.len());
    core::ptr::copy_nonoverlapping(response_va as *const u8, response.as_mut_ptr(), response.len());
    true
}

fn write_hex(value: u32) {
    console::write_str("0x");
    console::write_hex_u64(value as u64);
}

/// Finds the device, brings up the control queue and prints the display info.
pub fn probe() -> bool {
    let Some((bus, device, function)) = find_device() else {
        return false;
    };
    console::write_str("VIRTIO-GPU: device ");
    console::write_dec_u64(device as u64);
    console::write_str(".");
    console::write_dec_u64(function as u64);
    console::write_str("\r\n");

    if !claim_device(bus, device, function) {
        return false;
    }

    unsafe {
        let Some(mut queue) = init_device() else {
            return false;
        };

        let mut command = [0u8; CTRL_HDR_LEN];
        command[0..4].copy_from_slice(&VIRTIO_GPU_CMD_GET_DISPLAY_INFO.to_le_bytes());
        let mut response = [0u8; DISPLAY_INFO_LEN];
        if !control(&mut queue, &command, &mut response) {
            QUEUE = Some(queue);
            return false;
        }
        QUEUE = Some(queue);

        let kind = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
        if kind != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            console::write_str("VIRTIO-GPU: display info failed, type ");
            write_hex(kind);
            console::write_str("\r\n");
            return false;
        }

        // struct virtio_gpu_display_one { rect x,y,w,h; u32 enabled; u32 flags; }
        for index in 0..16 {
            let base = CTRL_HDR_LEN + index * 24;
            let enabled = u32::from_le_bytes([
                response[base + 16],
                response[base + 17],
                response[base + 18],
                response[base + 19],
            ]);
            if enabled == 0 {
                continue;
            }
            let width = u32::from_le_bytes([
                response[base + 8],
                response[base + 9],
                response[base + 10],
                response[base + 11],
            ]);
            let height = u32::from_le_bytes([
                response[base + 12],
                response[base + 13],
                response[base + 14],
                response[base + 15],
            ]);
            console::write_str("VIRTIO-GPU: display ");
            console::write_dec_u64(index as u64);
            console::write_str(" ");
            console::write_dec_u64(width as u64);
            console::write_str("x");
            console::write_dec_u64(height as u64);
            console::write_str("\r\n");
            return true;
        }
        console::write_str("VIRTIO-GPU: no enabled display\r\n");
        false
    }
}
