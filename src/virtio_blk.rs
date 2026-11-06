//! Polled virtio-blk-pci driver owned by the kernel.
//!
//! First step of moving the filesystem into the kernel: VFS operations run
//! with IRQs disabled, so they cannot block on the EL0 block driver. The
//! kernel therefore needs its own synchronous disk path.
//!
//! One virtqueue, one outstanding request, no interrupts. The request page is
//! allocated once and reused. Rings and buffers live in Normal WB memory (the
//! kernel linear map), so every hand-off to the device is fenced with
//! `dc cvac` and every hand-back with `dc ivac`.
//!
//! QEMU's modern virtio-pci BAR4 layout is fixed: common 0x0000, ISR 0x1000,
//! device 0x2000, notify 0x3000 (the EL0 driver's log confirms notify at
//! BAR+0x3000). BAR4 is programmed to `BAR4_PA`, inside the first 1 GiB the
//! kernel maps Device-nGnRnE.

use core::sync::atomic::{compiler_fence, AtomicBool, Ordering};

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;

const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
const PCI_DEVICE_BLOCK_MODERN: u16 = 0x1042;
/// Transitional `virtio-blk-pci` (QEMU's default) still exposes the modern
/// capabilities in BAR4, which is all this driver needs.
const PCI_DEVICE_BLOCK_TRANSITIONAL: u16 = 0x1001;

/// `qemu virt` leaves BAR4 unassigned; the EL0 driver already used this
/// window, so the kernel claims the same one.
const BAR4_PA: usize = 0x1010_0000;
const NOTIFY_OFF: usize = 0x3000;

const QUEUE_SIZE: usize = 256;

/// Set when a request times out. The in-flight descriptor stays in the ring,
/// so reusing the slot would pair a new request with the stale completion;
/// the driver instead fail-stops and the filesystem reports I/O errors until
/// the next boot.
static STALLED: AtomicBool = AtomicBool::new(false);

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTIO_BLK_T_IN: u32 = 0;

struct Queue {
    descriptors: usize,
    available: usize,
    used: usize,
    notify: usize,
    last_used: u16,
    /// One page holding the 16-byte request header, the 512-byte sector and
    /// the one-byte status. Reused for every request. The descriptors carry
    /// the physical address; the CPU touches the same page through the
    /// linear map.
    request: usize,
    request_pa: usize,
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
    let Some(address) = pci_address(bus, device, function, offset) else {
        return false;
    };
    unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    true
}

unsafe fn clean(address: usize, length: usize) {
    let line = crate::cache::line_bytes().max(4);
    let mut current = address & !(line - 1);
    let end = address + length;
    while current < end {
        core::arch::asm!("dc cvac, {}", in(reg) current, options(nostack, preserves_flags));
        current += line;
    }
    core::arch::asm!("dsb sy", options(nostack));
}

unsafe fn invalidate(address: usize, length: usize) {
    let line = crate::cache::line_bytes().max(4);
    let mut current = address & !(line - 1);
    let end = address + length;
    while current < end {
        core::arch::asm!("dc ivac, {}", in(reg) current, options(nostack, preserves_flags));
        current += line;
    }
    core::arch::asm!("dsb sy", options(nostack));
}

fn common(offset: usize) -> usize {
    phys_to_virt(BAR4_PA) + offset
}

unsafe fn read8(offset: usize) -> u8 {
    core::ptr::read_volatile(common(offset) as *const u8)
}

unsafe fn read16(offset: usize) -> u16 {
    core::ptr::read_volatile(common(offset) as *const u16)
}

unsafe fn write8(offset: usize, value: u8) {
    core::ptr::write_volatile(common(offset) as *mut u8, value)
}

unsafe fn write16(offset: usize, value: u16) {
    core::ptr::write_volatile(common(offset) as *mut u16, value)
}

unsafe fn write32(offset: usize, value: u32) {
    core::ptr::write_volatile(common(offset) as *mut u32, value)
}

unsafe fn write64(offset: usize, value: u64) {
    core::ptr::write_volatile(common(offset) as *mut u64, value)
}

fn find_device() -> Option<(u8, u8, u8)> {
    for bus in 0..=u8::MAX {
        for device in 0..32 {
            for function in 0..8 {
                let Some(id) = pci_read32(bus, device, function, 0x00) else {
                    continue;
                };
                let product = (id >> 16) as u16;
                if id as u16 == PCI_VENDOR_VIRTIO
                    && (product == PCI_DEVICE_BLOCK_MODERN
                        || product == PCI_DEVICE_BLOCK_TRANSITIONAL)
                {
                    return Some((bus, device, function));
                }
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
    let Some(bar4) = pci_read32(bus, device, function, 0x20) else {
        return false;
    };
    let base = bar4 & !0xF;
    if base == 0 || base == !0xF {
        return pci_write32(bus, device, function, 0x20, BAR4_PA as u32);
    }
    true
}

unsafe fn init_device() -> Option<Queue> {
    write8(0x14, 0); // reset
    write8(0x14, 1); // ACKNOWLEDGE
    write8(0x14, 3); // ACKNOWLEDGE | DRIVER

    // Feature word 1, bit 0 = VIRTIO_F_VERSION_1 (bit 32).
    write32(0x08, 0);
    write32(0x0C, 0);
    write32(0x08, 1);
    write32(0x0C, 1);

    write8(0x14, 3 | 8); // FEATURES_OK
    if read8(0x14) & 8 == 0 {
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

    write16(0x16, 0); // queue select 0
    let size = read16(0x18);
    if size == 0 {
        return None;
    }
    write64(0x20, ring_pa as u64);
    write64(0x28, available_pa as u64);
    write64(0x30, used_pa as u64);
    core::arch::asm!("dsb sy", options(nostack));
    write16(0x1C, 1);
    if read16(0x1C) != 1 {
        return None;
    }

    let (request_pa, _tag) = frame_allocator::alloc_page_tagged()?;
    let request_va = phys_to_virt(request_pa);
    core::ptr::write_bytes(request_va as *mut u8, 0, page);

    write8(0x14, 3 | 8 | 4); // DRIVER_OK

    Some(Queue {
        descriptors: ring_va,
        available: phys_to_virt(available_pa),
        used: phys_to_virt(used_pa),
        notify: phys_to_virt(BAR4_PA) + NOTIFY_OFF,
        last_used: 0,
        request: request_va,
        request_pa,
    })
}

/// Reads one 512-byte sector through queue 0.
pub fn read_sector(sector: u64, buffer: &mut [u8; 512]) -> bool {
    request(sector, Some(buffer), false)
}

/// Writes one 512-byte sector through queue 0.
pub fn write_sector(sector: u64, buffer: &[u8; 512]) -> bool {
    let mut copy = *buffer;
    request(sector, Some(&mut copy), true)
}

fn request(sector: u64, buffer: Option<&mut [u8; 512]>, write: bool) -> bool {
    if STALLED.load(Ordering::Acquire) {
        return false;
    }
    unsafe {
        let slot = core::ptr::addr_of_mut!(QUEUE) as *mut Option<Queue>;
        let Some(queue) = (*slot).as_mut() else {
            return false;
        };

        let header = queue.request;
        let data = queue.request + 16;
        let status = queue.request + 16 + 512;
        let header_pa = queue.request_pa;
        let data_pa = queue.request_pa + 16;
        let status_pa = queue.request_pa + 16 + 512;

        core::ptr::write_volatile(
            header as *mut u32,
            if write { 1 } else { VIRTIO_BLK_T_IN },
        );
        core::ptr::write_volatile((header + 4) as *mut u32, 0);
        core::ptr::write_volatile((header + 8) as *mut u64, sector);
        core::ptr::write_volatile(status as *mut u8, 0xFF_u8);
        if let Some(source) = buffer.as_ref() {
            if write {
                core::ptr::copy_nonoverlapping(source.as_ptr(), data as *mut u8, 512);
            }
        }

        let desc = queue.descriptors;
        core::ptr::write_volatile(desc as *mut u64, header_pa as u64);
        core::ptr::write_volatile((desc + 8) as *mut u32, 16);
        core::ptr::write_volatile((desc + 12) as *mut u16, VIRTQ_DESC_F_NEXT);
        core::ptr::write_volatile((desc + 14) as *mut u16, 1);

        core::ptr::write_volatile((desc + 16) as *mut u64, data_pa as u64);
        core::ptr::write_volatile((desc + 24) as *mut u32, 512);
        core::ptr::write_volatile(
            (desc + 28) as *mut u16,
            if write {
                VIRTQ_DESC_F_NEXT
            } else {
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE
            },
        );
        core::ptr::write_volatile((desc + 30) as *mut u16, 2);

        core::ptr::write_volatile((desc + 32) as *mut u64, status_pa as u64);
        core::ptr::write_volatile((desc + 40) as *mut u32, 1);
        core::ptr::write_volatile((desc + 44) as *mut u16, VIRTQ_DESC_F_WRITE);
        core::ptr::write_volatile((desc + 46) as *mut u16, 0);

        // available: u16 flags at +0, u16 idx at +2, ring[QUEUE_SIZE] at +4.
        let available_slot = core::ptr::read_volatile((queue.available + 2) as *const u16);
        core::ptr::write_volatile(
            (queue.available + 4 + (available_slot as usize % QUEUE_SIZE) * 2) as *mut u16,
            0,
        );
        compiler_fence(Ordering::SeqCst);
        core::ptr::write_volatile(
            (queue.available + 2) as *mut u16,
            available_slot.wrapping_add(1),
        );

        clean(header, 16 + 512 + 1);
        clean(queue.descriptors, 48);
        clean(queue.available, 6 + QUEUE_SIZE * 2);
        compiler_fence(Ordering::SeqCst);

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
                // Never hand this descriptor to another request: `used.idx`
                // would still point at the abandoned completion.
                if !STALLED.swap(true, Ordering::AcqRel) {
                    console::write_str(
                        "VIRTIO: device stalled; filesystem is read-only until reboot\r\n",
                    );
                }
                return false;
            }
            core::hint::spin_loop();
        }
        queue.last_used = queue.last_used.wrapping_add(1);

        invalidate(status, 1);
        let ok = core::ptr::read_volatile(status as *const u8) == 0;
        if ok && !write {
            if let Some(target) = buffer {
                invalidate(data, 512);
                core::ptr::copy_nonoverlapping(data as *const u8, target.as_mut_ptr(), 512);
            }
        }
        ok
    }
}

/// Puts the device back into reset so the EL0 driver can re-initialize it.
pub fn reset() {
    unsafe {
        write8(0x14, 0);
        let slot = core::ptr::addr_of_mut!(QUEUE) as *mut Option<Queue>;
        *slot = None;
    }
}

/// Claims the device and installs queue 0.
pub fn init() -> bool {
    let Some((bus, device, function)) = find_device() else {
        console::write_str("KBLK: no virtio-blk device\r\n");
        return false;
    };
    if !claim_device(bus, device, function) {
        console::write_str("KBLK: BAR4 claim failed\r\n");
        return false;
    }
    let Some(queue) = (unsafe { init_device() }) else {
        console::write_str("KBLK: queue init failed\r\n");
        return false;
    };
    unsafe {
        let slot = core::ptr::addr_of_mut!(QUEUE) as *mut Option<Queue>;
        *slot = Some(queue);
    }
    true
}

/// Boot probe: reads sector 0 and verifies the SFS magic.
pub fn self_test() -> bool {
    if !init() {
        return false;
    }
    let mut sector = [0_u8; 512];
    if !read_sector(0, &mut sector) {
        console::write_str("KBLK: sector 0 read failed\r\n");
        reset();
        return false;
    }
    let magic = u32::from_le_bytes([sector[0], sector[1], sector[2], sector[3]]);
    console::write_str("KBLK: sector 0 magic 0x");
    console::write_hex_u64(u64::from(magic));
    console::write_str("\r\n");
    reset();
    magic == 0x5346_5321
}
