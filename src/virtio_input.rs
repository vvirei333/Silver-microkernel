//! Polled virtio-input-pci driver owned by the kernel.
//!
//! `/dev/input/event0` for Linux-ABI tasks. QEMU exposes
//! `virtio-keyboard-pci`/`virtio-tablet-pci` as PCI vendor `0x1AF4`, device
//! `0x1052` (modern virtio-input). The device has two queues: eventq (0) and
//! statusq (1); only eventq is enabled, statusq config changes are ignored.
//!
//! Like `virtio_blk`, the driver is synchronous and polled: no interrupts, the
//! used ring is drained on demand (`pump`). QEMU pushes one
//! `virtio_input_event {type u16, code u16, value u32}` per available buffer and
//! fires the queue after every `EV_SYN` batch; the driver recycles the buffers
//! immediately and buffers the decoded events in a bounded local ring that
//! `read(2)` drains as 24-byte `input_event` records.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;

const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
/// Modern virtio-input (keyboard, tablet, ...).
const PCI_DEVICE_INPUT_MODERN: u16 = 0x1052;

/// QEMU's input BAR4 must not collide with the block driver's fixed
/// `0x1010_0000`; the EL0 network driver historically used `0x1020_0000`, so
/// input takes the next window.
const BAR4_PA: usize = 0x1030_0000;
const NOTIFY_OFF: usize = 0x3000;

const QUEUE_SIZE: usize = 64;
const EVENT_COUNT: usize = 256;

const VIRTQ_DESC_F_WRITE: u16 = 2;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_SW: u16 = 0x05;

/// `virtio_input_event` is 8 bytes: type, code, value.
const EVENT_SIZE: usize = 8;
/// A Linux `struct input_event` on AArch64: timeval (2×u64) + type, code,
/// value = 24 bytes.
pub const INPUT_EVENT_SIZE: usize = 24;

/// Bounded local event ring. `read(2)` is the only consumer and runs under the
/// kernel lock, so plain statics are fine.
static mut RING: [(u16, u16, i32); EVENT_COUNT] = [(0, 0, 0); EVENT_COUNT];
static RING_HEAD: AtomicUsize = AtomicUsize::new(0);
static RING_TAIL: AtomicUsize = AtomicUsize::new(0);

struct Queue {
    descriptors: usize,
    available: usize,
    used: usize,
    notify: usize,
    last_used: u16,
    /// One page of `virtio_input_event` records handed to the device.
    buffers_va: usize,
    buffers_pa: usize,
}

static mut QUEUE: Option<Queue> = None;
static PRESENT: AtomicUsize = AtomicUsize::new(0);

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

unsafe fn read32(offset: usize) -> u32 {
    core::ptr::read_volatile(common(offset) as *const u32)
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
                if id as u16 == PCI_VENDOR_VIRTIO && (id >> 16) as u16 == PCI_DEVICE_INPUT_MODERN {
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
    if base == !0xF {
        return false;
    }
    if base != BAR4_PA as u32 && !pci_write32(bus, device, function, 0x20, BAR4_PA as u32) {
        return false;
    }
    let Some(assigned) = pci_read32(bus, device, function, 0x20) else {
        return false;
    };
    if (assigned & !0xF) != BAR4_PA as u32 {
        console::write_str("VIRTIO-INPUT: BAR4 relocation failed\r\n");
        return false;
    }
    true
}

unsafe fn init_device() -> Option<Queue> {
    write8(0x14, 0); // reset
    write8(0x14, 1); // ACKNOWLEDGE
    write8(0x14, 3); // ACKNOWLEDGE | DRIVER

    // Feature word 1, bit 0 = VIRTIO_F_VERSION_1.
    write32(0x08, 0);
    write32(0x0C, 0);
    write32(0x08, 1);
    write32(0x0C, 1);

    write8(0x14, 3 | 8); // FEATURES_OK
    if read8(0x14) & 8 == 0 {
        return None;
    }

    // Only eventq (queue 0) is served. Statusq stays disabled: config-change
    // events are not needed by `/dev/input/event0` readers.
    write16(0x16, 0);
    let max_queue = read16(0x18);
    if max_queue == 0 {
        return None;
    }
    let size = max_queue.min(QUEUE_SIZE as u16);
    write16(0x18, size);
    if read16(0x18) != size {
        return None;
    }
    let size = size as usize;

    let page = 4096usize;
    let descriptor_span = (size * 16).div_ceil(page) * page;
    let available_span = (size * 2 + 6).div_ceil(page) * page;
    let used_span = (size * 8 + 6).div_ceil(page) * page;
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
        return None;
    }

    let (buffers_pa, _tag) = frame_allocator::alloc_page_tagged()?;
    let buffers_va = phys_to_virt(buffers_pa);
    core::ptr::write_bytes(buffers_va as *mut u8, 0, page);

    write8(0x14, 3 | 8 | 4); // DRIVER_OK

    Some(Queue {
        descriptors: ring_va,
        available: phys_to_virt(available_pa),
        used: phys_to_virt(used_pa),
        notify: phys_to_virt(BAR4_PA) + NOTIFY_OFF,
        last_used: 0,
        buffers_va,
        buffers_pa,
    })
}

/// One device-writable 8-byte buffer per descriptor, all published at init.
unsafe fn publish_buffers(queue: &mut Queue, size: usize) {
    let descriptor_base = queue.descriptors;
    for index in 0..size {
        let descriptor = descriptor_base + index * 16;
        core::ptr::write_volatile(
            descriptor as *mut u64,
            (queue.buffers_pa + index * EVENT_SIZE) as u64,
        );
        core::ptr::write_volatile((descriptor + 8) as *mut u32, EVENT_SIZE as u32);
        core::ptr::write_volatile((descriptor + 12) as *mut u16, VIRTQ_DESC_F_WRITE);
        core::ptr::write_volatile((descriptor + 14) as *mut u16, 0);
    }
    let available = queue.available;
    // avail: flags u16, idx u16, ring[size] u16, used_event u16.
    core::ptr::write_volatile(available as *mut u16, 0);
    core::ptr::write_volatile((available + 2) as *mut u16, size as u16);
    for index in 0..size {
        core::ptr::write_volatile((available + 4 + index * 2) as *mut u16, index as u16);
    }
    clean(available, 4 + size * 2);
    core::arch::asm!("dsb sy", options(nostack));
    core::ptr::write_volatile(queue.notify as *mut u16, 0);
}

/// Probes for the device and brings up eventq. A missing device is not a boot
/// failure; `/dev/input/event0` then simply does not exist.
pub fn init() -> bool {
    let Some((bus, device, function)) = find_device() else {
        return false;
    };
    if !claim_device(bus, device, function) {
        return false;
    }
    let Some(mut queue) = (unsafe { init_device() }) else {
        console::write_str("VIRTIO-INPUT: queue init failed\r\n");
        return false;
    };
    unsafe {
        publish_buffers(&mut queue, QUEUE_SIZE);
        let slot = core::ptr::addr_of_mut!(QUEUE) as *mut Option<Queue>;
        *slot = Some(queue);
    }
    PRESENT.store(1, Ordering::Release);
    console::write_str("VIRTIO-INPUT READY\r\n");
    true
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire) != 0
}

/// Drains the used ring: every completed buffer yields one event and is
/// reposted to the device. Cheap when the ring is empty (one volatile read).
pub fn pump() {
    if PRESENT.load(Ordering::Acquire) == 0 {
        return;
    }
    let queue = unsafe {
        let slot = core::ptr::addr_of_mut!(QUEUE) as *mut Option<Queue>;
        let Some(queue) = (*slot).as_mut() else {
            return;
        };
        queue
    };
    unsafe {
        let used = queue.used;
        // used: flags u16, idx u16, ring[size] { id u16, len u32 }.
        let device_index = core::ptr::read_volatile((used + 2) as *const u16);
        let mut index = queue.last_used;
        if index == device_index {
            return;
        }
        invalidate(queue.buffers_va, QUEUE_SIZE * EVENT_SIZE);
        while index != device_index {
            let slot = (index % QUEUE_SIZE as u16) as usize;
            let entry = used + 4 + slot * 8;
            let id = core::ptr::read_volatile(entry as *const u16);
            let buffer = queue.buffers_va + (id as usize) * EVENT_SIZE;
            let kind = core::ptr::read_volatile(buffer as *const u16);
            let code = core::ptr::read_volatile((buffer + 2) as *const u16);
            let value = core::ptr::read_volatile((buffer + 4) as *const u32) as i32;
            // EV_SYN/SYN_REPORT batches are protocol bookkeeping; a reader that
            // wants them can read the raw stream later if ever needed.
            if matches!(kind, EV_KEY | EV_ABS | EV_SW) || (kind == EV_SYN && code != 0) {
                let head = RING_HEAD.load(Ordering::Relaxed);
                if head.wrapping_sub(RING_TAIL.load(Ordering::Relaxed)) < EVENT_COUNT {
                    let slot = head % EVENT_COUNT;
                    core::ptr::write_volatile(
                        core::ptr::addr_of_mut!(RING).cast::<(u16, u16, i32)>()
                            .add(slot),
                        (kind, code, value),
                    );
                    RING_HEAD.store(head.wrapping_add(1), Ordering::Release);
                }
                // A full ring drops the event: input is lossy by nature.
            }
            queue.last_used = index.wrapping_add(1);
            core::ptr::write_volatile(
                (queue.descriptors + (id as usize) * 16) as *mut u64,
                (queue.buffers_pa + (id as usize) * EVENT_SIZE) as u64,
            );
            core::ptr::write_volatile(
                (queue.descriptors + (id as usize) * 16 + 8) as *mut u32,
                EVENT_SIZE as u32,
            );
            core::ptr::write_volatile(
                (queue.descriptors + (id as usize) * 16 + 12) as *mut u16,
                VIRTQ_DESC_F_WRITE,
            );
            core::ptr::write_volatile(
                (queue.descriptors + (id as usize) * 16 + 14) as *mut u16,
                0,
            );
            // Repost: push the descriptor id into the available ring.
            let available = queue.available;
            let avail_index = core::ptr::read_volatile((available + 2) as *const u16);
            core::ptr::write_volatile(
                (available + 4 + (avail_index as usize % QUEUE_SIZE) * 2) as *mut u16,
                id,
            );
            core::arch::asm!("dsb sy", options(nostack));
            core::ptr::write_volatile((available + 2) as *mut u16, avail_index + 1);
            clean(available, 4);
            core::ptr::write_volatile(queue.notify as *mut u16, 0);
            index = queue.last_used;
        }
    }
}

/// Events waiting in the local ring.
pub fn available() -> usize {
    RING_HEAD.load(Ordering::Acquire).wrapping_sub(RING_TAIL.load(Ordering::Acquire))
}

/// Pops one decoded event, if any.
pub fn take() -> Option<(u16, u16, i32)> {
    let tail = RING_TAIL.load(Ordering::Acquire);
    if tail == RING_HEAD.load(Ordering::Acquire) {
        return None;
    }
    let slot = tail % EVENT_COUNT;
    let event = unsafe {
        core::ptr::read_volatile(
            core::ptr::addr_of!(RING).cast::<(u16, u16, i32)>().add(slot),
        )
    };
    RING_TAIL.store(tail.wrapping_add(1), Ordering::Release);
    Some(event)
}

/// Fills `buffer` with as many whole 24-byte `input_event` records as fit.
/// Returns the byte count; zero means "no event right now".
pub fn read_events(buffer: &mut [u8]) -> usize {
    let mut written = 0;
    while written + INPUT_EVENT_SIZE <= buffer.len() {
        let Some((kind, code, value)) = take() else {
            break;
        };
        let record = &mut buffer[written..written + INPUT_EVENT_SIZE];
        // timeval: a monotonic 0-second epoch; readers that care about time
        // use their own clock. `sec=0, usec=uptime` is fine for now.
        record[0..8].copy_from_slice(&0_u64.to_le_bytes());
        let ticks = crate::timer::ticks();
        let usec = (ticks % 100) * 10_000;
        record[8..16].copy_from_slice(&usec.to_le_bytes());
        record[16..18].copy_from_slice(&kind.to_le_bytes());
        record[18..20].copy_from_slice(&code.to_le_bytes());
        record[20..24].copy_from_slice(&value.to_le_bytes());
        written += INPUT_EVENT_SIZE;
    }
    written
}
