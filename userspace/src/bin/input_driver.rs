//! Ring 3 input driver for QEMU `virt`'s `virtio-tablet-pci`.
//!
//! A virtio-input tablet is a modern-only virtio device (`VIRTIO_ID_INPUT`,
//! PCI device ID `0x1052`). It exposes a single event queue (queue 0) into
//! which the device writes 8-byte `virtio_input_event` records. The driver
//! keeps a pool of 8-byte buffers in the available ring; on a real input event
//! QEMU's `virtio_input_send` pops one buffer per event and pushes them to the
//! used ring once a `EV_SYN`/`SYN_REPORT` closes the batch, then fires an IRQ.
//!
//! This driver polls the used ring (no IRQ binding), turns each `EV_SYN`
//! batch into one [`input_proto::InputMsg`] with absolute coordinates already
//! scaled to the 640x480 screen, and sends it to `gpu_driver` over IPC.

#![no_std]
#![no_main]

#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../input_proto.rs"]
mod input_proto;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
/// `VIRTIO_ID_INPUT` (18): `0x1040 + 18`.
const VIRTIO_INPUT_DEVICE_ID: u16 = 0x1052;

const BAR_MAPPING_SIZE: u64 = 64 * 1024;
/// BARs of the `virt` machine start unassigned. The block driver claims
/// `0x1010_0000` and the network driver `0x1020_0000`, so input uses the next
/// 64 KiB slot.
const ASSIGNED_BAR_BASE: u64 = 0x1030_0000;

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

/// The event queue is virtqueue 0.
const QUEUE_EVT: u16 = 0;

/// One `virtio_input_event` is `{ type: u16, code: u16, value: u32 }`.
const EVENT_SIZE: u32 = 8;
/// QEMU creates a 64-entry event queue. The full size is provided to every
/// descriptor pool.
const BUFFER_COUNT_MAX: u16 = 64;

/// Linux `input.h` event types and codes used by a tablet.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x0110;

/// Screen geometry, kept in sync with `gpu_driver`.
const SCREEN_WIDTH: u32 = 640;
const SCREEN_HEIGHT: u32 = 480;

/// QEMU's virtio-input tablet reports absolute coordinates in `0..=32767`.
const COORD_MAX: u32 = 32767;

/// Init failure stages, reported via the exit status so they stay visible even
/// though this task never owns the console.
const INIT_NOT_FOUND: u8 = 1;
const INIT_RESET: u8 = 2;
const INIT_BAD_VERSION: u8 = 3;
const INIT_FEATURES: u8 = 4;
const INIT_NO_QUEUE: u8 = 5;
const INIT_QUEUE_SETUP: u8 = 6;
const INIT_DMA: u8 = 7;

/// Silverd lends this task the console for one second, but the scheduler may
/// run it before the handoff. Sleeping first keeps the init status lines in the
/// window where this task owns the console.
const CONSOLE_HANDOFF_DELAY_MS: u64 = 100;

/// Idle time between used-ring polls. The timer tick is 10 ms, so `sleep(1)`
/// polls at ~100 Hz — plenty for a cursor, without burning the core.
const POLL_DELAY_MS: u64 = 1;

#[allow(dead_code)]
#[derive(Copy, Clone)]
struct VirtioCapability {
    bar: u8,
    offset: u32,
}

#[allow(dead_code)]
struct VirtioInput {
    bus: u8,
    device: u8,
    function: u8,
    common_cfg_addr: u64,
    notify_bar_vaddr: u64,
    notify_cap: VirtioCapability,
    notify_off_multiplier: u32,
}

struct Virtqueue {
    size: u16,
    notify_addr: u64,
    descriptors_vaddr: u64,
    available_vaddr: u64,
    used_vaddr: u64,
    buffers_vaddr: u64,
    buffers_paddr: u64,
    buffer_count: u16,
    /// Next available-ring slot to fill; wraps as u16 per the ring ABI.
    avail_idx: u16,
    /// Next used-ring entry to consume.
    last_used_idx: u16,
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

/// The device writes events into descriptors carrying this flag.
const VIRTQ_DESC_F_WRITE: u16 = 2;

static mut EVENT_QUEUE: Option<Virtqueue> = None;

/// Raw absolute state, flushed into an IPC sample at each `EV_SYN`.
static mut ABS_X_RAW: u32 = 0;
static mut ABS_Y_RAW: u32 = 0;
static mut BTN_LEFT_DOWN: bool = false;
/// Cached task ID of `gpu_driver`; resolved lazily so a late registration is
/// still picked up.
static mut GPU_ID: Option<u64> = None;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    syscall::sleep(CONSOLE_HANDOFF_DELAY_MS);
    syscall::print("INPUT DRIVER STARTED\r\n");

    let Some(device) = find_virtio_input() else {
        fail(INIT_NOT_FOUND, "VIRTIO-INPUT NOT FOUND\r\n");
    };

    syscall::print("VIRTIO-INPUT FOUND ON PCI ");
    print_bdf(&device);
    syscall::print("\r\n");

    if let Err(stage) = unsafe { initialize(&device) } {
        fail(stage, "VIRTIO-INPUT INITIALIZATION FAILED\r\n");
    }

    syscall::print("VIRTIO-INPUT READY\r\n");
    serve();
}

/// Polls the used ring and forwards `EV_SYN` batches to `gpu_driver`.
fn serve() -> ! {
    loop {
        unsafe { poll_events() };
        syscall::sleep(POLL_DELAY_MS);
    }
}

unsafe fn poll_events() {
    let Some(queue) = (unsafe { (*core::ptr::addr_of_mut!(EVENT_QUEUE)).as_mut() }) else {
        return;
    };

    let used = queue.used_vaddr as *const VirtqUsed;
    let used_idx = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*used).idx)) };
    if used_idx == queue.last_used_idx {
        return;
    }
    // The device wrote the ring entries before bumping `idx`; order the reads.
    unsafe { core::arch::asm!("dmb sy", options(nostack)) };

    while queue.last_used_idx != used_idx {
        let element_index = usize::from(queue.last_used_idx % queue.size);
        let element = unsafe {
            let ring = core::ptr::addr_of!((*used).ring).cast::<VirtqUsedElement>();
            core::ptr::read_volatile(ring.add(element_index))
        };
        queue.last_used_idx = queue.last_used_idx.wrapping_add(1);

        let slot = element.id as u16;
        if slot >= queue.buffer_count {
            continue;
        }

        // Read the single event out of the slot and hand the slot straight back.
        let (event_type, code, value) = read_event(queue.buffer_vaddr(slot));
        unsafe {
            write_descriptor(queue, slot, EVENT_SIZE, VIRTQ_DESC_F_WRITE);
            publish_buffer(queue, slot);
        }
        handle_event(event_type, code, value);
    }

    // The event queue handler in QEMU is a no-op, but notifying after
    // replenishing the ring keeps the driver spec-conformant.
    unsafe { notify(queue) };
}

fn read_event(buffer: u64) -> (u16, u16, u32) {
    (read_le16(buffer), read_le16(buffer + 2), read_le32(buffer + 4))
}

fn handle_event(event_type: u16, code: u16, value: u32) {
    match event_type {
        EV_ABS => match code {
            ABS_X => unsafe { ABS_X_RAW = value },
            ABS_Y => unsafe { ABS_Y_RAW = value },
            _ => {}
        },
        EV_KEY if code == BTN_LEFT => unsafe { BTN_LEFT_DOWN = value != 0 },
        EV_SYN if code == SYN_REPORT => send_pointer(),
        _ => {}
    }
}

/// Scales one raw axis value from `0..=COORD_MAX` to `0..=screen_max` and
/// forwards the whole sample to `gpu_driver`.
fn send_pointer() {
    let raw_x = unsafe { ABS_X_RAW };
    let raw_y = unsafe { ABS_Y_RAW };
    let left = unsafe { BTN_LEFT_DOWN };

    let x = scale_coord(raw_x, SCREEN_WIDTH - 1);
    let y = scale_coord(raw_y, SCREEN_HEIGHT - 1);

    if let Some(target) = resolve_gpu() {
        let _ = syscall::try_send(target, &input_proto::pointer(x, y, left));
    }
}

fn scale_coord(raw: u32, screen_max: u32) -> u16 {
    let scaled = (u64::from(raw) * u64::from(screen_max) + u64::from(screen_max) / 2)
        / u64::from(COORD_MAX);
    scaled.min(u64::from(screen_max)) as u16
}

fn resolve_gpu() -> Option<u64> {
    let cached = unsafe { GPU_ID };
    if cached.is_some() {
        return cached;
    }
    let id = syscall::lookup("gpu_driver");
    unsafe { GPU_ID = id };
    id
}

// ---------------------------------------------------------------------------
// virtqueue management (single event queue)
// ---------------------------------------------------------------------------

impl Virtqueue {
    fn buffer_vaddr(&self, slot: u16) -> u64 {
        self.buffers_vaddr + u64::from(slot) * u64::from(EVENT_SIZE)
    }

    fn buffer_paddr(&self, slot: u16) -> u64 {
        self.buffers_paddr + u64::from(slot) * u64::from(EVENT_SIZE)
    }
}

unsafe fn write_descriptor(queue: &Virtqueue, slot: u16, length: u32, flags: u16) {
    let descriptor =
        unsafe { (queue.descriptors_vaddr as *mut VirtqDescriptor).add(usize::from(slot)) };
    unsafe {
        core::ptr::write_volatile(
            descriptor,
            VirtqDescriptor {
                addr: queue.buffer_paddr(slot),
                len: length,
                flags,
                next: 0,
            },
        );
    }
}

unsafe fn publish_buffer(queue: &mut Virtqueue, slot: u16) {
    let available = queue.available_vaddr as *mut VirtqAvailable;
    let ring = unsafe { core::ptr::addr_of_mut!((*available).ring).cast::<u16>() };
    let ring_index = usize::from(queue.avail_idx % queue.size);
    unsafe { core::ptr::write_volatile(ring.add(ring_index), slot) };
    core::arch::asm!("dsb sy", options(nostack));
    queue.avail_idx = queue.avail_idx.wrapping_add(1);
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*available).idx), queue.avail_idx);
    }
    core::arch::asm!("dsb sy", options(nostack));
}

unsafe fn notify(queue: &Virtqueue) {
    core::arch::asm!("dsb sy", options(nostack));
    unsafe { core::ptr::write_volatile(queue.notify_addr as *mut u16, QUEUE_EVT) };
}

/// Posts every event buffer before `DRIVER_OK`. The device only starts
/// consuming the ring after it is marked ready, so no notify is needed here.
unsafe fn publish_all_buffers(queue: &mut Virtqueue) {
    for slot in 0..queue.buffer_count {
        unsafe { write_descriptor(queue, slot, EVENT_SIZE, VIRTQ_DESC_F_WRITE) };
        unsafe { publish_buffer(queue, slot) };
    }
}

// ---------------------------------------------------------------------------
// Device initialization
// ---------------------------------------------------------------------------

unsafe fn initialize(device: &VirtioInput) -> Result<(), u8> {
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

        let mut queue = setup_virtqueue(device, QUEUE_EVT)?;
        publish_all_buffers(&mut queue);

        syscall::print("QUEUE 0 (EVT): SIZE ");
        syscall::print_dec_u64(u64::from(queue.size));
        syscall::print("\r\n");

        let status = core::ptr::read_volatile((common + COMMON_DEVICE_STATUS) as *const u8);
        core::ptr::write_volatile(
            (common + COMMON_DEVICE_STATUS) as *mut u8,
            status | VIRTIO_STATUS_DRIVER_OK,
        );
        core::arch::asm!("dsb sy", options(nostack));

        EVENT_QUEUE = Some(queue);
        Ok(())
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

unsafe fn setup_virtqueue(device: &VirtioInput, index: u16) -> Result<Virtqueue, u8> {
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

    let buffer_count = size.min(BUFFER_COUNT_MAX);
    let buffers_offset = used_offset.checked_add(used_span).ok_or(INIT_DMA)?;
    let buffers_span = align_page(u64::from(buffer_count) * u64::from(EVENT_SIZE))
        .ok_or(INIT_DMA)?;
    let total_size = buffers_offset.checked_add(buffers_span).ok_or(INIT_DMA)?;

    let (vaddr, paddr) = syscall::alloc_dma(total_size).ok_or(INIT_DMA)?;
    unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, total_size as usize) };

    let notify_addr = device.notify_bar_vaddr
        + u64::from(device.notify_cap.offset)
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
        size,
        notify_addr,
        descriptors_vaddr: vaddr,
        available_vaddr: vaddr + available_offset,
        used_vaddr: vaddr + used_offset,
        buffers_vaddr: vaddr + buffers_offset,
        buffers_paddr: paddr + buffers_offset,
        buffer_count,
        avail_idx: 0,
        last_used_idx: 0,
    })
}

// ---------------------------------------------------------------------------
// PCI enumeration
// ---------------------------------------------------------------------------

fn pci_config_address(bus: u8, device: u8, function: u8, offset: u16) -> u64 {
    (u64::from(bus) << 20)
        | (u64::from(device) << 15)
        | (u64::from(function) << 12)
        | (u64::from(offset) & 0xFFC)
}

fn find_virtio_input() -> Option<VirtioInput> {
    for bus in 0..=u8::MAX {
        for device in 0..32 {
            for function in 0..8 {
                let Some(id) = syscall::pci_read32(pci_config_address(bus, device, function, 0x00))
                else {
                    continue;
                };
                let vendor_id = id as u16;
                let device_id = (id >> 16) as u16;
                if vendor_id != VIRTIO_VENDOR_ID || device_id != VIRTIO_INPUT_DEVICE_ID {
                    continue;
                }

                let command_address = pci_config_address(bus, device, function, 0x04);
                let Some(command) = syscall::pci_read32(command_address) else {
                    continue;
                };
                if !syscall::pci_write32(command_address, command | 0x6) {
                    continue;
                }

                let Some((common_cfg, notify_cfg, notify_off_multiplier)) =
                    read_virtio_capabilities(bus, device, function)
                else {
                    continue;
                };
                let (Some(notify_cfg), Some(notify_off_multiplier)) =
                    (notify_cfg, notify_off_multiplier)
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

                return Some(VirtioInput {
                    bus,
                    device,
                    function,
                    common_cfg_addr: common_bar_vaddr + u64::from(common_cfg.offset),
                    notify_bar_vaddr,
                    notify_cap: notify_cfg,
                    notify_off_multiplier,
                });
            }
        }
    }
    None
}

/// Maps one BAR per PCI function at most once, so the common and notify
/// capabilities that share QEMU's modern BAR4 do not consume two windows.
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
) -> Option<(VirtioCapability, Option<VirtioCapability>, Option<u32>)> {
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
                _ => {}
            }
        }
        cap_ptr = next_ptr;
        visited += 1;
    }

    Some((common_cfg?, notify_cfg, notify_off_multiplier))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_le16(addr: u64) -> u16 {
    let low = unsafe { core::ptr::read_volatile(addr as *const u8) };
    let high = unsafe { core::ptr::read_volatile((addr + 1) as *const u8) };
    u16::from(low) | (u16::from(high) << 8)
}

fn read_le32(addr: u64) -> u32 {
    let low = read_le16(addr);
    let high = read_le16(addr + 2);
    u32::from(low) | (u32::from(high) << 16)
}

fn fail(stage: u8, message: &str) -> ! {
    syscall::print(message);
    syscall::print("VIRTIO-INPUT FAILURE STAGE ");
    syscall::print_dec_u64(u64::from(stage));
    syscall::print("\r\n");
    syscall::exit_with_status(u64::from(stage))
}

fn print_bdf(device: &VirtioInput) {
    syscall::print_dec_u64(u64::from(device.bus));
    syscall::print(":");
    syscall::print_dec_u64(u64::from(device.device));
    syscall::print(".");
    syscall::print_dec_u64(u64::from(device.function));
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
