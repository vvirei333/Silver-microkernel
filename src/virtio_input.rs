//! Polled virtio-input-pci driver owned by the kernel.
//!
//! `/dev/input/eventN` for Linux-ABI tasks. QEMU exposes
//! `virtio-keyboard-pci`/`virtio-tablet-pci` as PCI vendor `0x1AF4`,
//! device `0x1052` (modern virtio-input). Each device has two queues:
//! eventq (0) and statusq (1); only eventq is enabled, statusq config changes
//! are ignored.
//!
//! Like `virtio_blk`, the driver is synchronous and polled: no interrupts,
//! the used ring is drained on demand (`pump`). QEMU pushes one
//! `virtio_input_event {type u16, code u16, value u32}` per available
//! buffer and fires the queue after every `EV_SYN` batch; the driver
//! recycles the buffers immediately and buffers the decoded events in a
//! bounded per-device ring that `read(2)` drains as 24-byte
//! `input_event` records.
//!
//! Up to `MAX_DEVICES` devices are served in PCI scan order (`event0`,
//! `event1`). The device-specific configuration is read at boot so the
//! evdev ioctls can answer capability queries (`EVIOCGNAME`,
//! `EVIOCGBIT`, `EVIOCGABS`, ...) from the kernel instead of pretending
//! every node is a generic keyboard.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;

const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
/// Modern virtio-input (keyboard, tablet, ...).
const PCI_DEVICE_INPUT_MODERN: u16 = 0x1052;

/// Most devices the driver serves: a keyboard plus one absolute pointer.
pub const MAX_DEVICES: usize = 2;

/// BAR4 windows inside the first 1 GiB device mapping. blk owns
/// `0x1010_0000`, gpu `0x1020_0000`, the SDHCI probe `0x1030_0000` and
/// ivshmem `0x1040_0000..0x1080_0000`, so input starts at `0x1080_0000`
/// and takes one 16 KiB window per device.
const BAR4_BASE: usize = 0x1080_0000;
const BAR4_STRIDE: usize = 0x4000;

const NOTIFY_OFF: usize = 0x3000;
const DEVICE_CFG_OFF: usize = 0x2000;

const QUEUE_SIZE: usize = 64;
const EVENT_COUNT: usize = 256;

const VIRTQ_DESC_F_WRITE: u16 = 2;

/// virtio-input config selectors.
const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_PROPS: u8 = 0x04;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

/// Event types enumerated by `EVIOCGBIT`/`VIRTIO_INPUT_CFG_EV_BITS`.
const EV_TYPE_COUNT: usize = 32;
/// 1024 bits, the widest bitmap a virtio-input config read returns.
const EV_BITS_BYTES: usize = 128;
/// `KEY_MAX` is `0x2ff`, so 768 bits.
const KEY_BITS_BYTES: usize = 96;
/// `ABS_MAX` is `0x3f`.
const ABS_COUNT: usize = 64;

/// `virtio_input_event` is 8 bytes: type, code, value.
const EVENT_SIZE: usize = 8;
/// A Linux `struct input_event` on AArch64: timeval (2x u64) + type, code,
/// value = 24 bytes.
pub const INPUT_EVENT_SIZE: usize = 24;

/// evdev character-device major and the minor of `event0`.
pub const EVDEV_MAJOR: u32 = 13;
pub const EVDEV_MINOR_BASE: u32 = 64;

/// One axis description, in both virtio and evdev terms.
#[derive(Copy, Clone)]
struct AbsInfo {
    value: i32,
    min: i32,
    max: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

struct Queue {
    bar: usize,
    descriptors: usize,
    available: usize,
    used: usize,
    notify: usize,
    last_used: u16,
    /// One page of `virtio_input_event` records handed to the device.
    buffers_va: usize,
    buffers_pa: usize,
}

struct Device {
    queue: Queue,
    name: [u8; 64],
    name_len: u8,
    props: [u8; 32],
    /// Capability bitmaps indexed by event type, as the device reports them.
    ev: [[u8; EV_BITS_BYTES]; EV_TYPE_COUNT],
    ev_size: [u8; EV_TYPE_COUNT],
    abs: [AbsInfo; ABS_COUNT],
    /// Current key state for `EVIOCGKEY`.
    keys: [u8; KEY_BITS_BYTES],
}

impl Device {
    fn new(queue: Queue) -> Self {
        Device {
            queue,
            name: [0; 64],
            name_len: 0,
            props: [0; 32],
            ev: [[0; EV_BITS_BYTES]; EV_TYPE_COUNT],
            ev_size: [0; EV_TYPE_COUNT],
            abs: [
                AbsInfo {
                    value: 0,
                    min: 0,
                    max: 0,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                };
                ABS_COUNT
            ],
            keys: [0; KEY_BITS_BYTES],
        }
    }

    fn has_event(&self, kind: u16) -> bool {
        kind < EV_TYPE_COUNT as u16 && self.ev_size[kind as usize] != 0
    }

    fn event_bit(&self, kind: u16, code: u16) -> bool {
        if kind >= EV_TYPE_COUNT as u16 || code >= (EV_BITS_BYTES * 8) as u16 {
            return false;
        }
        let byte = self.ev[kind as usize][code as usize / 8];
        byte & (1 << (code % 8)) != 0
    }
}

/// Bounded per-device event rings. `read(2)` is the consumer and runs under
/// the kernel lock, so plain statics are fine.
static mut RINGS: [[(u16, u16, i32); EVENT_COUNT]; MAX_DEVICES] =
    [[(0, 0, 0); EVENT_COUNT]; MAX_DEVICES];
static HEADS: [AtomicUsize; MAX_DEVICES] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static TAILS: [AtomicUsize; MAX_DEVICES] = [AtomicUsize::new(0), AtomicUsize::new(0)];

static mut DEVICES: [Option<Device>; MAX_DEVICES] = [None, None];
static COUNT: AtomicUsize = AtomicUsize::new(0);

static ZERO_BITS: [u8; EV_BITS_BYTES] = [0; EV_BITS_BYTES];

fn pci_address(bus: u8, device: u8, function: u8, offset: usize) -> Option<usize> {
    if offset % 4 != 0 || offset >= 0x1000 {
        return None;
    }
    Some(phys_to_virt(
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

/// Common-configuration register of the device whose BAR4 starts at `bar`.
fn common(bar: usize, offset: usize) -> usize {
    phys_to_virt(bar) + offset
}

unsafe fn read8(bar: usize, offset: usize) -> u8 {
    core::ptr::read_volatile(common(bar, offset) as *const u8)
}

unsafe fn read16(bar: usize, offset: usize) -> u16 {
    core::ptr::read_volatile(common(bar, offset) as *const u16)
}

unsafe fn write8(bar: usize, offset: usize, value: u8) {
    core::ptr::write_volatile(common(bar, offset) as *mut u8, value)
}

unsafe fn write16(bar: usize, offset: usize, value: u16) {
    core::ptr::write_volatile(common(bar, offset) as *mut u16, value)
}

unsafe fn write32(bar: usize, offset: usize, value: u32) {
    core::ptr::write_volatile(common(bar, offset) as *mut u32, value)
}

unsafe fn write64(bar: usize, offset: usize, value: u64) {
    core::ptr::write_volatile(common(bar, offset) as *mut u64, value)
}

/// Selects one device-configuration field and copies up to `out.len()`
/// bytes. Returns the field size the device reports (which may exceed
/// `out.len()`).
unsafe fn config(bar: usize, select: u8, subsel: u8, out: &mut [u8]) -> usize {
    // struct virtio_input_config: select u8, subsel u8, size u8,
    // reserved[5], data[128]. The payload starts at offset 8.
    const DATA_OFF: usize = 8;
    let base = phys_to_virt(bar) + DEVICE_CFG_OFF;
    core::ptr::write_volatile(base as *mut u8, select);
    core::ptr::write_volatile((base + 1) as *mut u8, subsel);
    // The device fills `size` and `data` from the selection. Device memory
    // reads are ordered after the writes with a full barrier.
    core::arch::asm!("dsb sy", options(nostack));
    let size = core::ptr::read_volatile((base + 2) as *const u8) as usize;
    let take = size.min(out.len());
    for (index, byte) in out.iter_mut().take(take).enumerate() {
        *byte = core::ptr::read_volatile((base + DATA_OFF + index) as *const u8);
    }
    size
}

unsafe fn load_caps(device: &mut Device) {
    let bar = device.queue.bar;
    let mut scratch = [0u8; EV_BITS_BYTES];
    let name_len = config(bar, CFG_ID_NAME, 0, &mut scratch).min(device.name.len());
    device.name[..name_len].copy_from_slice(&scratch[..name_len]);
    device.name_len = name_len as u8;

    let mut props = [0u8; 32];
    let props_len = config(bar, CFG_ID_PROPS, 0, &mut props).min(props.len());
    device.props[..props_len].copy_from_slice(&props[..props_len]);

    for kind in 0..EV_TYPE_COUNT as u8 {
        let mut bits = [0u8; EV_BITS_BYTES];
        let size = config(bar, CFG_EV_BITS, kind, &mut bits).min(EV_BITS_BYTES);
        device.ev[kind as usize] = bits;
        device.ev_size[kind as usize] = size as u8;
    }

    // virtio-input does not enumerate EV_SYN: every evdev node can report
    // SYN_REPORT and SYN_DROPPED, and Linux userspace expects those bits.
    if !device.has_event(EV_SYN) {
        device.ev[EV_SYN as usize][0] = 0x09;
        device.ev_size[EV_SYN as usize] = 1;
    }

    if device.has_event(EV_ABS) {
        for code in 0..ABS_COUNT as u16 {
            if !device.event_bit(EV_ABS, code) {
                continue;
            }
            // A sane default before the config read: clients that only trust
            // EVIOCGABS must not see a degenerate 0..0 axis.
            device.abs[code as usize].max = 0x7fff;
            let mut raw = [0u8; 20];
            if config(bar, CFG_ABS_INFO, code as u8, &mut raw) >= 20 {
                let word = |offset: usize| {
                    i32::from_le_bytes([
                        raw[offset],
                        raw[offset + 1],
                        raw[offset + 2],
                        raw[offset + 3],
                    ])
                };
                let axis = &mut device.abs[code as usize];
                axis.min = word(0);
                axis.max = word(4);
                axis.fuzz = word(8);
                axis.flat = word(12);
                axis.resolution = word(16);
            }
        }
    }
}

/// Claims the device's BAR4 and moves it to `bar` when QEMU left it at zero.
fn claim_device(bus: u8, device: u8, function: u8, bar: usize) -> bool {
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
    if base != bar as u32 && !pci_write32(bus, device, function, 0x20, bar as u32) {
        return false;
    }
    let Some(assigned) = pci_read32(bus, device, function, 0x20) else {
        return false;
    };
    if (assigned & !0xF) != bar as u32 {
        console::write_str("VIRTIO-INPUT: BAR4 relocation failed\r\n");
        return false;
    }
    true
}

unsafe fn init_device(bar: usize) -> Option<Queue> {
    write8(bar, 0x14, 0); // reset
    write8(bar, 0x14, 1); // ACKNOWLEDGE
    write8(bar, 0x14, 3); // ACKNOWLEDGE | DRIVER

    // Feature word 1, bit 0 = VIRTIO_F_VERSION_1.
    write32(bar, 0x08, 0);
    write32(bar, 0x0C, 0);
    write32(bar, 0x08, 1);
    write32(bar, 0x0C, 1);

    write8(bar, 0x14, 3 | 8); // FEATURES_OK
    if read8(bar, 0x14) & 8 == 0 {
        return None;
    }

    // Only eventq (queue 0) is served. Statusq stays disabled: config-change
    // events are not needed by evdev readers.
    write16(bar, 0x16, 0);
    let max_queue = read16(bar, 0x18);
    if max_queue == 0 {
        return None;
    }
    let size = max_queue.min(QUEUE_SIZE as u16);
    write16(bar, 0x18, size);
    if read16(bar, 0x18) != size {
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

    write64(bar, 0x20, ring_pa as u64);
    write64(bar, 0x28, available_pa as u64);
    write64(bar, 0x30, used_pa as u64);
    core::arch::asm!("dsb sy", options(nostack));
    write16(bar, 0x1C, 1);
    if read16(bar, 0x1C) != 1 {
        return None;
    }

    let (buffers_pa, _tag) = frame_allocator::alloc_page_tagged()?;
    let buffers_va = phys_to_virt(buffers_pa);
    core::ptr::write_bytes(buffers_va as *mut u8, 0, page);

    write8(bar, 0x14, 3 | 8 | 4); // DRIVER_OK

    Some(Queue {
        bar,
        descriptors: ring_va,
        available: phys_to_virt(available_pa),
        used: phys_to_virt(used_pa),
        notify: phys_to_virt(bar) + NOTIFY_OFF,
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

unsafe fn device(index: usize) -> Option<&'static mut Device> {
    let devices = core::ptr::addr_of_mut!(DEVICES);
    (*devices)[index].as_mut()
}

/// Probes every virtio-input device on the PCI bus and brings up eventq on
/// each. A missing device is not a boot failure; the matching
/// `/dev/input/eventN` then simply does not exist.
pub fn init() -> bool {
    let mut found = 0;
    'scan: for bus in 0..=u8::MAX {
        for device_number in 0..32u8 {
            for function in 0..8u8 {
                if found == MAX_DEVICES {
                    break 'scan;
                }
                let Some(id) = pci_read32(bus, device_number, function, 0x00) else {
                    continue;
                };
                if id as u16 != PCI_VENDOR_VIRTIO
                    || (id >> 16) as u16 != PCI_DEVICE_INPUT_MODERN
                {
                    continue;
                }
                let bar = BAR4_BASE + found * BAR4_STRIDE;
                if !claim_device(bus, device_number, function, bar) {
                    continue;
                }
                let Some(mut queue) = (unsafe { init_device(bar) }) else {
                    console::write_str("VIRTIO-INPUT: queue init failed\r\n");
                    continue;
                };
                unsafe {
                    publish_buffers(&mut queue, QUEUE_SIZE);
                    let mut entry = Device::new(queue);
                    load_caps(&mut entry);
                    let devices = core::ptr::addr_of_mut!(DEVICES);
                    (*devices)[found] = Some(entry);
                }
                found += 1;
            }
        }
    }
    COUNT.store(found, Ordering::Release);
    if found == 0 {
        return false;
    }
    console::write_str("VIRTIO-INPUT READY: ");
    console::write_dec_u64(found as u64);
    console::write_str("\r\n");
    true
}

pub fn present() -> bool {
    COUNT.load(Ordering::Acquire) != 0
}

/// Number of live devices; `event0..event{count-1}` exist.
pub fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

pub fn name(index: usize) -> &'static [u8] {
    unsafe {
        match device(index) {
            Some(device) => &device.name[..device.name_len as usize],
            None => &[],
        }
    }
}

pub fn props(index: usize) -> &'static [u8; 32] {
    unsafe {
        let devices = core::ptr::addr_of!(DEVICES);
        match &(*devices)[index] {
            Some(device) => &device.props,
            None => &[0; 32],
        }
    }
}

pub fn ev_bits(index: usize, kind: u8) -> &'static [u8; EV_BITS_BYTES] {
    unsafe {
        let devices = core::ptr::addr_of!(DEVICES);
        match &(*devices)[index] {
            Some(device) if (kind as usize) < EV_TYPE_COUNT => &device.ev[kind as usize],
            _ => &ZERO_BITS,
        }
    }
}

pub fn key_state(index: usize) -> &'static [u8; KEY_BITS_BYTES] {
    unsafe {
        let devices = core::ptr::addr_of!(DEVICES);
        match &(*devices)[index] {
            Some(device) => &device.keys,
            None => &[0; KEY_BITS_BYTES],
        }
    }
}

/// `(value, minimum, maximum, fuzz, flat, resolution)` for one axis.
pub fn abs_info(index: usize, code: u8) -> Option<(i32, i32, i32, i32, i32, i32)> {
    unsafe {
        let devices = core::ptr::addr_of!(DEVICES);
        let device = (*devices)[index].as_ref()?;
        if code as usize >= ABS_COUNT {
            return None;
        }
        let axis = &device.abs[code as usize];
        Some((
            axis.value,
            axis.min,
            axis.max,
            axis.fuzz,
            axis.flat,
            axis.resolution,
        ))
    }
}

fn push_event(index: usize, kind: u16, code: u16, value: i32) {
    let head = HEADS[index].load(Ordering::Relaxed);
    if head.wrapping_sub(TAILS[index].load(Ordering::Relaxed)) >= EVENT_COUNT {
        // A full ring drops the event: input is lossy by nature.
        return;
    }
    let slot = head % EVENT_COUNT;
    unsafe {
        let rings = core::ptr::addr_of_mut!(RINGS) as *mut (u16, u16, i32);
        core::ptr::write_volatile(rings.add(index * EVENT_COUNT + slot), (kind, code, value));
    }
    HEADS[index].store(head.wrapping_add(1), Ordering::Release);
}

fn take_event(index: usize) -> Option<(u16, u16, i32)> {
    let tail = TAILS[index].load(Ordering::Relaxed);
    if tail == HEADS[index].load(Ordering::Acquire) {
        return None;
    }
    let slot = tail % EVENT_COUNT;
    let event = unsafe {
        let rings = core::ptr::addr_of!(RINGS) as *const (u16, u16, i32);
        core::ptr::read_volatile(rings.add(index * EVENT_COUNT + slot))
    };
    TAILS[index].store(tail.wrapping_add(1), Ordering::Release);
    Some(event)
}

fn record_state(device: &mut Device, kind: u16, code: u16, value: i32) {
    if kind == EV_KEY && (code as usize) < KEY_BITS_BYTES * 8 {
        let byte = &mut device.keys[code as usize / 8];
        let bit = 1 << (code % 8);
        if value != 0 {
            *byte |= bit;
        } else {
            *byte &= !bit;
        }
    } else if kind == EV_ABS && (code as usize) < ABS_COUNT {
        device.abs[code as usize].value = value;
    }
}

/// Drains every device's used ring: each completed buffer yields one event
/// and is reposted. Cheap when the rings are empty (one volatile read each).
pub fn pump() {
    let count = count();
    for index in 0..count {
        let Some(device) = (unsafe { device(index) }) else {
            continue;
        };
        let (used, buffers_va, buffers_pa, descriptors, available, notify, mut last_used) = {
            let queue = &device.queue;
            (
                queue.used,
                queue.buffers_va,
                queue.buffers_pa,
                queue.descriptors,
                queue.available,
                queue.notify,
                queue.last_used,
            )
        };
        unsafe {
            // used: flags u16, idx u16, ring[size] { id u16, len u32 }.
            let device_index = core::ptr::read_volatile((used + 2) as *const u16);
            if last_used == device_index {
                continue;
            }
            invalidate(buffers_va, QUEUE_SIZE * EVENT_SIZE);
            while last_used != device_index {
                let slot = (last_used % QUEUE_SIZE as u16) as usize;
                let entry = used + 4 + slot * 8;
                let id = core::ptr::read_volatile(entry as *const u16);
                let buffer = buffers_va + (id as usize) * EVENT_SIZE;
                let kind = core::ptr::read_volatile(buffer as *const u16);
                let code = core::ptr::read_volatile((buffer + 2) as *const u16);
                let value = core::ptr::read_volatile((buffer + 4) as *const u32) as i32;
                record_state(device, kind, code, value);
                push_event(index, kind, code, value);
                last_used = last_used.wrapping_add(1);
                core::ptr::write_volatile(
                    (descriptors + (id as usize) * 16) as *mut u64,
                    (buffers_pa + (id as usize) * EVENT_SIZE) as u64,
                );
                core::ptr::write_volatile(
                    (descriptors + (id as usize) * 16 + 8) as *mut u32,
                    EVENT_SIZE as u32,
                );
                core::ptr::write_volatile(
                    (descriptors + (id as usize) * 16 + 12) as *mut u16,
                    VIRTQ_DESC_F_WRITE,
                );
                core::ptr::write_volatile(
                    (descriptors + (id as usize) * 16 + 14) as *mut u16,
                    0,
                );
                // Repost: push the descriptor id into the available ring.
                let avail_index = core::ptr::read_volatile((available + 2) as *const u16);
                core::ptr::write_volatile(
                    (available + 4 + (avail_index as usize % QUEUE_SIZE) * 2) as *mut u16,
                    id,
                );
                core::arch::asm!("dsb sy", options(nostack));
                core::ptr::write_volatile((available + 2) as *mut u16, avail_index + 1);
                clean(available, 4);
                core::ptr::write_volatile(notify as *mut u16, 0);
            }
            device.queue.last_used = last_used;
        }
    }
}

/// Events waiting in one device's local ring.
pub fn available(index: usize) -> usize {
    if index >= MAX_DEVICES {
        return 0;
    }
    HEADS[index]
        .load(Ordering::Acquire)
        .wrapping_sub(TAILS[index].load(Ordering::Acquire))
}

/// Fills `buffer` with as many whole 24-byte `input_event` records as fit.
/// Returns the byte count; zero means "no event right now".
pub fn read_events(index: usize, buffer: &mut [u8]) -> usize {
    let mut written = 0;
    let ticks = crate::timer::ticks();
    while written + INPUT_EVENT_SIZE <= buffer.len() {
        let Some((kind, code, value)) = take_event(index) else {
            break;
        };
        let record = &mut buffer[written..written + INPUT_EVENT_SIZE];
        record[0..8].copy_from_slice(&0_u64.to_le_bytes());
        let usec = (ticks % 100) * 10_000;
        record[8..16].copy_from_slice(&usec.to_le_bytes());
        record[16..18].copy_from_slice(&kind.to_le_bytes());
        record[18..20].copy_from_slice(&code.to_le_bytes());
        record[20..24].copy_from_slice(&value.to_le_bytes());
        written += INPUT_EVENT_SIZE;
    }
    written
}