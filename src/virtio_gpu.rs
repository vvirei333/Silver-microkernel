//! virtio-gpu driver: control queue, 2D resources and scanout.
//!
//! The driver claims the PCI device, brings up the control queue (queue 0) and
//! drives the 2D command set: `RESOURCE_CREATE_2D`, `ATTACH_BACKING`,
//! `SET_SCANOUT`, `TRANSFER_TO_HOST_2D` and `RESOURCE_FLUSH`. The framebuffer
//! is a contiguous kernel allocation, so presenting is a memcpy into it
//! followed by a transfer/flush pair; the device scans it out on its own
//! display head.
//!
//! The 3D (virgl) context and a DRM device build on the same control queue and
//! the same framebuffer allocation.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;

const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;
const PCI_DEVICE_GPU_MODERN: u16 = 0x1050;
const PCI_DEVICE_GPU_TRANSITIONAL: u16 = 0x0010;

/// A different 1 GiB device window than virtio-blk's 0x1010_0000.
const GPU_BAR4_PA: usize = 0x1020_0000;
const QUEUE_SIZE: usize = 64;
/// Staging area for `SUBMIT_3D` command buffers.
const SUBMIT_PAGES: usize = 32;
const SUBMIT_BYTES: usize = SUBMIT_PAGES * 4096;
/// The virtio-gpu 3D commands all belong to one virgl context.
const VIRGL_CONTEXT_ID: u32 = 1;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x0109;
const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x0201;
const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;

const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;

/// `VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM`: the byte order the compositor writes
/// (red first), so its frames go out unchanged.
const FORMAT_R8G8B8A8_UNORM: u32 = 67;
/// `VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`: the byte order of DRM `XRGB8888`
/// buffers, whose u32 value is 0x00RRGGBB.
const FORMAT_B8G8R8A8_UNORM: u32 = 1;

const CTRL_HDR_LEN: usize = 24;
const DISPLAY_INFO_LEN: usize = CTRL_HDR_LEN + 16 * 24;

struct Queue {
    descriptors: usize,
    available: usize,
    used: usize,
    notify: usize,
    last_used: u16,
    /// Two pages shared by every control command: request at offset 0,
    /// response at offset 4096.
    command_pa: usize,
    command: usize,
    /// 128 KiB staging area for SUBMIT_3D command buffers.
    submit_pa: usize,
    submit: usize,
}

/// The scanout the driver owns once `init` has run.
struct Display {
    queue: Queue,
    width: u32,
    height: u32,
    resource_id: u32,
    framebuffer_pa: usize,
    framebuffer_pages: usize,
    generation: u32,
}

static mut DISPLAY: Option<Display> = None;
/// Resource 1 belongs to the scanout framebuffer; DRM buffers start at 2.
static mut NEXT_RESOURCE: u32 = 2;
/// True when the device negotiated `VIRTIO_GPU_F_VIRGL`.
static VIRGL: AtomicBool = AtomicBool::new(false);
/// Cached capability sets, keyed by capset id (1 virgl, 2 virgl2, 6 drm).
#[derive(Clone, Copy)]
struct CapsEntry {
    id: u32,
    version: u32,
    len: usize,
    data: [u8; 4096],
}

static EMPTY_CAPS: CapsEntry = CapsEntry {
    id: 0,
    version: 0,
    len: 0,
    data: [0; 4096],
};
static mut CAPS: [CapsEntry; 4] = [EMPTY_CAPS; 4];

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

fn pci_read8(bus: u8, device: u8, function: u8, offset: usize) -> Option<u8> {
    if offset >= 0x1000 {
        return None;
    }
    let address = crate::memory::phys_to_virt(
        crate::mmu::PCI_ECAM_BASE
            + ((bus as usize) << 20)
            + ((device as usize) << 15)
            + ((function as usize) << 12)
            + offset,
    );
    Some(unsafe { core::ptr::read_volatile(address as *const u8) })
}

fn pci_read16(bus: u8, device: u8, function: u8, offset: usize) -> Option<u16> {
    let low = pci_read8(bus, device, function, offset)? as u16;
    let high = pci_read8(bus, device, function, offset + 1)? as u16;
    Some(low | (high << 8))
}

/// Byte-wise 32-bit read: capability entries are not guaranteed to start on a
/// 4-byte boundary, and an unaligned MMIO read would fault.
fn pci_read32_bytes(bus: u8, device: u8, function: u8, offset: usize) -> Option<u32> {
    let mut value = 0u32;
    for index in 0..4 {
        value |= (pci_read8(bus, device, function, offset + index)? as u32) << (index * 8);
    }
    Some(value)
}

fn pci_write16(bus: u8, device: u8, function: u8, offset: usize, value: u16) -> bool {
    let Some(address) = pci_address(bus, device, function, offset & !3) else {
        return false;
    };
    let address = address + (offset & 3);
    unsafe { core::ptr::write_volatile(address as *mut u16, value) };
    true
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

/// A virtio PCI capability: which BAR and offset holds one of the modern
/// register banks.
#[derive(Clone, Copy)]
struct VirtioCap {
    bar: usize,
    offset: usize,
    length: usize,
    /// Notify capability only: bytes between two queue doorbells.
    multiplier: usize,
}

/// Programmes one BAR. Returns the assigned base address.
fn assign_bar(bus: u8, device: u8, function: u8, bar: usize, base: usize) -> Option<usize> {
    let offset = 0x10 + bar * 4;
    let original = pci_read32(bus, device, function, offset)?;
    if original & 0x1 != 0 {
        // I/O BAR: the modern capabilities never live there.
        return None;
    }
    let is_64 = original & 0x6 == 0x4;
    if !pci_write32(bus, device, function, offset, 0xFFFF_FFFF) {
        return None;
    }
    let mask = pci_read32(bus, device, function, offset)? & !0xF;
    if !pci_write32(bus, device, function, offset, original) {
        return None;
    }
    if mask == 0 {
        return None;
    }
    let size = (!mask).wrapping_add(1) as usize;
    if base % size != 0 {
        return None;
    }
    if !pci_write32(bus, device, function, offset, base as u32 | (original & 0xF)) {
        return None;
    }
    if is_64 && !pci_write32(bus, device, function, offset + 4, 0) {
        return None;
    }
    let assigned = pci_read32(bus, device, function, offset)? & !0xF;
    if assigned as usize != base {
        return None;
    }
    Some(base)
}

/// Walks the PCI capability list and programmes the BARs that hold the modern
/// virtio register banks. Without blob resources the banks live in BAR4; a
/// device with a shared-memory BAR (blobs) moves them to a smaller BAR and
/// gives BAR4 to the shared window, so the layout must be read, not assumed.
fn claim_device(bus: u8, device: u8, function: u8) -> bool {
    let Some(command) = pci_read32(bus, device, function, 0x04) else {
        return false;
    };
    if !pci_write32(bus, device, function, 0x04, command | 0x6) {
        return false;
    }
    let Some(status) = pci_read16(bus, device, function, 0x06) else {
        return false;
    };
    if status & (1 << 4) == 0 {
        console::write_str("VIRTIO-GPU: no PCI capabilities\r\n");
        return false;
    }
    let Some(mut pointer) = pci_read8(bus, device, function, 0x34) else {
        return false;
    };
    let mut common: Option<VirtioCap> = None;
    let mut notify: Option<VirtioCap> = None;
    let mut device_cfg: Option<VirtioCap> = None;
    let mut guard = 0;
    while pointer != 0 && guard < 48 {
        let Some(id) = pci_read8(bus, device, function, pointer as usize) else {
            return false;
        };
        let Some(next) = pci_read8(bus, device, function, pointer as usize + 1) else {
            return false;
        };
        if id == 0x09 {
            let base = pointer as usize;
            let Some(cfg_type) = pci_read8(bus, device, function, base + 3) else {
                return false;
            };
            let Some(bar) = pci_read8(bus, device, function, base + 4) else {
                return false;
            };
            let Some(offset) = pci_read32_bytes(bus, device, function, base + 8) else {
                return false;
            };
            let Some(length) = pci_read32_bytes(bus, device, function, base + 12) else {
                return false;
            };
            let multiplier = pci_read32_bytes(bus, device, function, base + 16)
                .unwrap_or(4) as usize;
            let cap = VirtioCap {
                bar: bar as usize,
                offset: offset as usize,
                length: length as usize,
                multiplier: if multiplier == 0 { 4 } else { multiplier },
            };
            match cfg_type {
                1 => common = Some(cap),
                2 => notify = Some(cap),
                4 => device_cfg = Some(cap),
                _ => {}
            }
        }
        pointer = next;
        guard += 1;
    }
    let (Some(common), Some(notify), Some(device_cfg)) = (common, notify, device_cfg) else {
        console::write_str("VIRTIO-GPU: missing virtio capabilities\r\n");
        return false;
    };

    // Give every BAR a window. The blob BAR (512 MiB with blobs enabled) is
    // not used: resources stay in guest memory.
    let mut bases = [0usize; 6];
    let mut next_base = GPU_BAR4_PA;
    for cap in [common, notify, device_cfg] {
        if bases[cap.bar] == 0 {
            let Some(base) = assign_bar(bus, device, function, cap.bar, next_base) else {
                console::write_str("VIRTIO-GPU: BAR relocation failed\r\n");
                return false;
            };
            bases[cap.bar] = base;
            next_base += 0x10_0000;
        }
    }
    unsafe {
        COMMON_VA = phys_to_virt(bases[common.bar] + common.offset);
        NOTIFY_VA = phys_to_virt(bases[notify.bar] + notify.offset);
        NOTIFY_MULTIPLIER = notify.multiplier;
        DEVICE_VA = phys_to_virt(bases[device_cfg.bar] + device_cfg.offset);
    }
    true
}

static mut COMMON_VA: usize = 0;
static mut NOTIFY_MULTIPLIER: usize = 4;
static mut NOTIFY_VA: usize = 0;
static mut DEVICE_VA: usize = 0;

fn common(offset: usize) -> usize {
    unsafe { COMMON_VA + offset }
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

    // Device features. Word 0 bit 0 is VIRTIO_GPU_F_VIRGL; word 1 bit 0 is
    // VIRTIO_F_VERSION_1.
    write32(0x00, 0);
    let device_features0 = read32(0x04);
    write32(0x00, 1);
    let device_features1 = read32(0x04);
    let virgl = device_features0 & 1 != 0;
    VIRGL.store(virgl, core::sync::atomic::Ordering::Release);

    write32(0x08, if virgl { 1 } else { 0 });
    write32(0x0C, device_features1 & 1);

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

    let (command_pa, _tag) = frame_allocator::alloc_contiguous_pages_tagged(2)?;
    let (submit_pa, _tag) = frame_allocator::alloc_contiguous_pages_tagged(SUBMIT_PAGES)?;

    Some(Queue {
        descriptors: ring_va,
        available: phys_to_virt(available_pa),
        used: phys_to_virt(used_pa),
        notify: unsafe { NOTIFY_VA + notify_off * NOTIFY_MULTIPLIER },
        last_used: 0,
        command_pa,
        command: phys_to_virt(command_pa),
        submit_pa,
        submit: phys_to_virt(submit_pa),
    })
}

/// Submits one control command and waits for its response. Returns the
/// response type, or `None` when the command timed out.
unsafe fn control(queue: &mut Queue, command: &[u8]) -> Option<u32> {
    let page = 4096usize;
    let buffer_pa = queue.command_pa;
    let buffer = queue.command;
    // The response page is cleared so a short response cannot be mistaken for
    // an older one.
    core::ptr::write_bytes(buffer as *mut u8, 0, page * 2);
    core::ptr::copy_nonoverlapping(command.as_ptr(), buffer as *mut u8, command.len());
    let response_va = buffer + page;

    let descriptor = queue.descriptors;
    core::ptr::write_volatile(descriptor as *mut u64, buffer_pa as u64);
    core::ptr::write_volatile((descriptor + 8) as *mut u32, command.len() as u32);
    core::ptr::write_volatile((descriptor + 12) as *mut u16, VIRTQ_DESC_F_NEXT);
    core::ptr::write_volatile((descriptor + 14) as *mut u16, 1);

    core::ptr::write_volatile((descriptor + 16) as *mut u64, (buffer_pa + page) as u64);
    core::ptr::write_volatile((descriptor + 24) as *mut u32, page as u32);
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
            return None;
        }
        core::hint::spin_loop();
    }
    queue.last_used = queue.last_used.wrapping_add(1);

    // The whole response page: commands such as GET_DISPLAY_INFO and
    // GET_CAPSET return payloads past the header, and the device wrote them
    // by DMA, so the cacheable alias must be invalidated before reading.
    invalidate(response_va, page);
    let kind = u32::from_le_bytes([
        core::ptr::read_volatile(response_va as *const u8),
        core::ptr::read_volatile((response_va + 1) as *const u8),
        core::ptr::read_volatile((response_va + 2) as *const u8),
        core::ptr::read_volatile((response_va + 3) as *const u8),
    ]);
    Some(kind)
}

fn put32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(buffer: &mut [u8], offset: usize, value: u64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn header(buffer: &mut [u8], kind: u32) {
    put32(buffer, 0, kind);
}

fn write_hex(value: u32) {
    console::write_str("0x");
    console::write_hex_u64(value as u64);
}

/// Reports a failed control command with its response type.
fn failed(what: &str, kind: u32) -> bool {
    console::write_str("VIRTIO-GPU: ");
    console::write_str(what);
    console::write_str(" failed, type ");
    write_hex(kind);
    console::write_str("\r\n");
    false
}

/// Asks the device for display 0 and returns its size.
unsafe fn display_size(queue: &mut Queue) -> Option<(u32, u32)> {
    let mut command = [0u8; CTRL_HDR_LEN];
    header(&mut command, VIRTIO_GPU_CMD_GET_DISPLAY_INFO);
    let kind = control(queue, &command)?;
    if kind != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
        failed("display info", kind);
        return None;
    }
    let response = queue.command + 4096;
    // struct virtio_gpu_display_one { rect x,y,w,h; u32 enabled; u32 flags; }
    for index in 0..16 {
        let base = response + CTRL_HDR_LEN + index * 24;
        let enabled = u32::from_le_bytes([
            core::ptr::read_volatile((base + 16) as *const u8),
            core::ptr::read_volatile((base + 17) as *const u8),
            core::ptr::read_volatile((base + 18) as *const u8),
            core::ptr::read_volatile((base + 19) as *const u8),
        ]);
        if enabled == 0 {
            continue;
        }
        let width = u32::from_le_bytes([
            core::ptr::read_volatile((base + 8) as *const u8),
            core::ptr::read_volatile((base + 9) as *const u8),
            core::ptr::read_volatile((base + 10) as *const u8),
            core::ptr::read_volatile((base + 11) as *const u8),
        ]);
        let height = u32::from_le_bytes([
            core::ptr::read_volatile((base + 12) as *const u8),
            core::ptr::read_volatile((base + 13) as *const u8),
            core::ptr::read_volatile((base + 14) as *const u8),
            core::ptr::read_volatile((base + 15) as *const u8),
        ]);
        if width == 0 || height == 0 {
            continue;
        }
        return Some((width, height));
    }
    console::write_str("VIRTIO-GPU: no enabled display\r\n");
    None
}

/// Creates a 2D resource and attaches a contiguous run of frames to it.
unsafe fn create_resource(
    queue: &mut Queue,
    resource_id: u32,
    framebuffer_pa: usize,
    length: usize,
    width: u32,
    height: u32,
    format: u32,
) -> bool {
    let mut command = [0u8; CTRL_HDR_LEN + 16];
    header(&mut command, VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
    put32(&mut command, CTRL_HDR_LEN, resource_id);
    put32(&mut command, CTRL_HDR_LEN + 4, format);
    put32(&mut command, CTRL_HDR_LEN + 8, width);
    put32(&mut command, CTRL_HDR_LEN + 12, height);
    match control(queue, &command) {
        Some(VIRTIO_GPU_RESP_OK_NODATA) => {}
        Some(kind) => return failed("resource create", kind),
        None => return false,
    }

    let mut command = [0u8; CTRL_HDR_LEN + 8 + 16];
    header(&mut command, VIRTIO_GPU_CMD_ATTACH_BACKING);
    put32(&mut command, CTRL_HDR_LEN, resource_id);
    put32(&mut command, CTRL_HDR_LEN + 4, 1); // one memory entry
    put64(&mut command, CTRL_HDR_LEN + 8, framebuffer_pa as u64);
    put32(&mut command, CTRL_HDR_LEN + 16, length as u32);
    match control(queue, &command) {
        Some(VIRTIO_GPU_RESP_OK_NODATA) => {}
        Some(kind) => return failed("attach backing", kind),
        None => return false,
    }

    true
}

/// Points scanout 0 at `resource_id`.
unsafe fn set_scanout(
    queue: &mut Queue,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    // struct virtio_gpu_set_scanout { rect r; u32 scanout_id; u32 resource_id; }
    let mut command = [0u8; CTRL_HDR_LEN + 24];
    header(&mut command, VIRTIO_GPU_CMD_SET_SCANOUT);
    put32(&mut command, CTRL_HDR_LEN + 8, width);
    put32(&mut command, CTRL_HDR_LEN + 12, height);
    put32(&mut command, CTRL_HDR_LEN + 16, 0); // scanout 0
    put32(&mut command, CTRL_HDR_LEN + 20, resource_id);
    match control(queue, &command) {
        Some(VIRTIO_GPU_RESP_OK_NODATA) => {}
        Some(kind) => return failed("set scanout", kind),
        None => return false,
    }
    true
}

/// Copies `length` bytes at `pa` into `resource_id` and flushes the whole
/// surface.
unsafe fn upload_resource(
    queue: &mut Queue,
    resource_id: u32,
    pa: usize,
    length: usize,
    width: u32,
    height: u32,
) -> bool {
    // struct virtio_gpu_transfer_to_host_2d { rect r; u64 offset; u32
    // resource_id; u32 padding; }
    let mut command = [0u8; CTRL_HDR_LEN + 32];
    header(&mut command, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
    put32(&mut command, CTRL_HDR_LEN + 8, width);
    put32(&mut command, CTRL_HDR_LEN + 12, height);
    put64(&mut command, CTRL_HDR_LEN + 16, 0);
    put32(&mut command, CTRL_HDR_LEN + 24, resource_id);
    clean(phys_to_virt(pa), length);
    match control(queue, &command) {
        Some(VIRTIO_GPU_RESP_OK_NODATA) => {}
        Some(kind) => return failed("transfer", kind),
        None => return false,
    }

    // struct virtio_gpu_resource_flush { rect r; u32 resource_id; u32 padding; }
    let mut command = [0u8; CTRL_HDR_LEN + 24];
    header(&mut command, VIRTIO_GPU_CMD_RESOURCE_FLUSH);
    put32(&mut command, CTRL_HDR_LEN + 8, width);
    put32(&mut command, CTRL_HDR_LEN + 12, height);
    put32(&mut command, CTRL_HDR_LEN + 16, resource_id);
    match control(queue, &command) {
        Some(VIRTIO_GPU_RESP_OK_NODATA) => {}
        Some(kind) => return failed("flush", kind),
        None => return false,
    }
    true
}

/// Finds the device, sets up the control queue and a 2D scanout, and draws a
/// colour-bar test pattern so a host screendump can prove the whole path.
pub fn init() -> bool {
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
        let Some((width, height)) = display_size(&mut queue) else {
            return false;
        };

        let length = (width as usize) * (height as usize) * 4;
        let pages = length.div_ceil(4096);
        let Some((framebuffer_pa, generation)) =
            frame_allocator::alloc_contiguous_pages_tagged(pages)
        else {
            console::write_str("VIRTIO-GPU: framebuffer allocation failed\r\n");
            return false;
        };
        core::ptr::write_bytes(phys_to_virt(framebuffer_pa) as *mut u8, 0, pages * 4096);

        let resource_id = 1;
        if !create_resource(
            &mut queue,
            resource_id,
            framebuffer_pa,
            length,
            width,
            height,
            FORMAT_R8G8B8A8_UNORM,
        ) {
            return false;
        }
        if !set_scanout(&mut queue, resource_id, width, height) {
            return false;
        }

        let mut display = Display {
            queue,
            width,
            height,
            resource_id,
            framebuffer_pa,
            framebuffer_pages: pages,
            generation,
        };

        // Colour bars plus a diagonal, so a screendump shows both solid
        // fields and geometry: a wrong stride or format is visible at once.
        let pixels = phys_to_virt(framebuffer_pa) as *mut u32;
        // Bytes are R,G,B,A (format 67), so a u32 is 0xAABBGGRR.
        let bars: [u32; 8] = [
            0xFFFF_FFFF, // white
            0xFFFF_FF00, // cyan
            0xFFFF_00FF, // magenta
            0xFFFF_0000, // blue
            0xFF00_FFFF, // yellow
            0xFF00_FF00, // green
            0xFF00_00FF, // red
            0xFF00_0000, // black
        ];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let mut value = bars[(x * bars.len()) / width as usize];
                if x / 8 == y / 8 {
                    value = 0xFFFF_FFFF;
                }
                core::ptr::write_volatile(pixels.add(y * width as usize + x), value);
            }
        }

        if !upload_resource(
            &mut display.queue,
            display.resource_id,
            display.framebuffer_pa,
            length,
            display.width,
            display.height,
        ) {
            return false;
        }
        DISPLAY = Some(display);

        console::write_str("VIRTIO-GPU: scanout ");
        console::write_dec_u64(width as u64);
        console::write_str("x");
        console::write_dec_u64(height as u64);
        console::write_str(" resource ");
        console::write_dec_u64(resource_id as u64);
        console::write_str("\r\n");
        true
    }
}

/// Presents a BGRA/XRGB frame through the GPU. The slice must hold exactly
/// `width * height * 4` bytes.
pub fn present(pixels: &[u8]) -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        let length = (display.width as usize) * (display.height as usize) * 4;
        if pixels.len() < length {
            return false;
        }
        core::ptr::copy_nonoverlapping(
            pixels.as_ptr(),
            phys_to_virt(display.framebuffer_pa) as *mut u8,
            length,
        );
        upload_resource(
            &mut display.queue,
            display.resource_id,
            display.framebuffer_pa,
            length,
            display.width,
            display.height,
        )
    }
}

/// Pushes the current framebuffer contents to the device. The compositor
/// writes into the framebuffer through its own mapping and calls this once per
/// frame; no pixel copy happens here.
pub fn flush() -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        let length = (display.width as usize) * (display.height as usize) * 4;
        upload_resource(
            &mut display.queue,
            display.resource_id,
            display.framebuffer_pa,
            length,
            display.width,
            display.height,
        )
    }
}

/// Creates a 2D resource backed by `length` bytes at `pa` and returns its id.
pub fn create_resource_pages(
    pa: usize,
    length: usize,
    width: u32,
    height: u32,
    bgr: bool,
) -> Option<u32> {
    unsafe {
        let display = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut()?;
        let id = NEXT_RESOURCE;
        NEXT_RESOURCE = NEXT_RESOURCE.wrapping_add(1);
        let format = if bgr {
            FORMAT_B8G8R8A8_UNORM
        } else {
            FORMAT_R8G8B8A8_UNORM
        };
        if !create_resource(&mut display.queue, id, pa, length, width, height, format) {
            return None;
        }
        Some(id)
    }
}

/// Presents an existing resource: transfers its pixels and flushes it, then
/// points scanout 0 at it.
pub fn present_resource(
    resource_id: u32,
    pa: usize,
    length: usize,
    width: u32,
    height: u32,
) -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        if !upload_resource(&mut display.queue, resource_id, pa, length, width, height) {
            return false;
        }
        set_scanout(&mut display.queue, resource_id, width, height)
    }
}

/// True when 3D (virgl) commands are available.
pub fn virgl_available() -> bool {
    VIRGL.load(Ordering::Acquire)
}

/// Submits a virgl command buffer on the shared 3D context.
pub fn submit_3d(data: &[u8]) -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        submit_3d_queue(&mut display.queue, data)
    }
}

unsafe fn submit_3d_queue(queue: &mut Queue, data: &[u8]) -> bool {
    if data.is_empty() || data.len() > SUBMIT_BYTES - 64 {
        return false;
    }
    let buffer = queue.submit;
    core::ptr::write_bytes(buffer as *mut u8, 0, data.len() + 64);
    // struct virtio_gpu_cmd_submit { u32 size; u32 padding; }
    // The fence flag makes the device hold the response until the host has
    // actually executed the command buffer. Without it the response arrives
    // immediately (QEMU renders on a worker thread) and a following readback
    // sees the old contents.
    static FENCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
    let fence = FENCE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    put32_bytes(buffer, 0, VIRTIO_GPU_CMD_SUBMIT_3D);
    put32_bytes(buffer, 4, 1); // VIRTIO_GPU_FLAG_FENCE
    put32_bytes(buffer, 8, fence as u32);
    put32_bytes(buffer, 12, (fence >> 32) as u32);
    // ctx_id: the virgl context created by CTX_CREATE. Without it the host
    // executes the command buffer in the default (non-virgl) context and the
    // commands are silently ignored.
    put32_bytes(buffer, 16, VIRGL_CONTEXT_ID);
    put32_bytes(buffer, 24, data.len() as u32);
    core::ptr::copy_nonoverlapping(
        data.as_ptr(),
        (buffer + 32) as *mut u8,
        data.len(),
    );

    let descriptor = queue.descriptors;
    // [header][command buffer][response]
    core::ptr::write_volatile(descriptor as *mut u64, queue.submit_pa as u64);
    core::ptr::write_volatile((descriptor + 8) as *mut u32, 32);
    core::ptr::write_volatile((descriptor + 12) as *mut u16, VIRTQ_DESC_F_NEXT);
    core::ptr::write_volatile((descriptor + 14) as *mut u16, 1);

    core::ptr::write_volatile((descriptor + 16) as *mut u64, (queue.submit_pa + 32) as u64);
    core::ptr::write_volatile((descriptor + 24) as *mut u32, data.len() as u32);
    core::ptr::write_volatile((descriptor + 28) as *mut u16, VIRTQ_DESC_F_NEXT);
    core::ptr::write_volatile((descriptor + 30) as *mut u16, 2);

    core::ptr::write_volatile((descriptor + 32) as *mut u64, (queue.command_pa + 4096) as u64);
    core::ptr::write_volatile((descriptor + 40) as *mut u32, 4096);
    core::ptr::write_volatile((descriptor + 44) as *mut u16, VIRTQ_DESC_F_WRITE);
    core::ptr::write_volatile((descriptor + 46) as *mut u16, 0);

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

    clean(buffer, 32 + data.len());
    clean(queue.descriptors, 48);
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
            console::write_str("VIRTIO-GPU: 3D submit timed out\r\n");
            return false;
        }
        core::hint::spin_loop();
    }
    queue.last_used = queue.last_used.wrapping_add(1);

    let response = queue.command + 4096;
    invalidate(response, CTRL_HDR_LEN);
    let kind = u32::from_le_bytes([
        core::ptr::read_volatile(response as *const u8),
        core::ptr::read_volatile((response + 1) as *const u8),
        core::ptr::read_volatile((response + 2) as *const u8),
        core::ptr::read_volatile((response + 3) as *const u8),
    ]);
    kind == VIRTIO_GPU_RESP_OK_NODATA
}

fn put32_bytes(buffer: usize, offset: usize, value: u32) {
    unsafe { core::ptr::write_unaligned((buffer + offset) as *mut u32, value) }
}

/// Creates a 3D resource and returns its id.
pub fn create_resource_3d(
    width: u32,
    height: u32,
    target: u32,
    format: u32,
    bind: u32,
) -> Option<u32> {
    unsafe {
        let display = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut()?;
        if !virgl_available() {
            return None;
        }
        let id = NEXT_RESOURCE;
        // struct virtio_gpu_resource_create_3d
        let mut command = [0u8; CTRL_HDR_LEN + 48];
        header(&mut command, VIRTIO_GPU_CMD_RESOURCE_CREATE_3D);
        put32(&mut command, CTRL_HDR_LEN, id);
        put32(&mut command, CTRL_HDR_LEN + 4, target);
        put32(&mut command, CTRL_HDR_LEN + 8, format);
        put32(&mut command, CTRL_HDR_LEN + 12, bind);
        put32(&mut command, CTRL_HDR_LEN + 16, width);
        put32(&mut command, CTRL_HDR_LEN + 20, height);
        put32(&mut command, CTRL_HDR_LEN + 24, 1); // depth
        put32(&mut command, CTRL_HDR_LEN + 28, 1); // array_size
        put32(&mut command, CTRL_HDR_LEN + 32, 0); // last_level
        put32(&mut command, CTRL_HDR_LEN + 36, 0); // nr_samples
        put32(&mut command, CTRL_HDR_LEN + 40, 0); // flags
        match control(&mut display.queue, &command) {
            Some(VIRTIO_GPU_RESP_OK_NODATA) => {}
            Some(kind) => {
                failed("resource create 3d", kind);
                return None;
            }
            None => return None,
        }
        NEXT_RESOURCE = NEXT_RESOURCE.wrapping_add(1);
        Some(id)
    }
}

/// Pushes a guest range to the point of coherency (before the device reads).
pub fn clean_range(pa: usize, length: usize) {
    unsafe { clean(phys_to_virt(pa), length) }
}

/// Drops stale cache lines so host writes become visible (after the device
/// wrote guest memory).
pub fn invalidate_range(pa: usize, length: usize) {
    unsafe { invalidate(phys_to_virt(pa), length) }
}

/// Attaches a contiguous run of guest frames to a 3D resource. Without this
/// the host has no memory to render into, and `TRANSFER_FROM_HOST` returns
/// whatever the resource was created with (zeros).
pub fn attach_backing(resource_id: u32, pa: usize, length: usize) -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        let mut command = [0u8; CTRL_HDR_LEN + 8 + 16];
        header(&mut command, VIRTIO_GPU_CMD_ATTACH_BACKING);
        put32(&mut command, CTRL_HDR_LEN, resource_id);
        put32(&mut command, CTRL_HDR_LEN + 4, 1);
        put64(&mut command, CTRL_HDR_LEN + 8, pa as u64);
        put32(&mut command, CTRL_HDR_LEN + 16, length as u32);
        matches!(
            control(&mut display.queue, &command),
            Some(VIRTIO_GPU_RESP_OK_NODATA)
        )
    }
}

/// Attaches `resource_id` to the virgl context so its command buffers can
/// reference it.
pub fn context_attach(resource_id: u32) -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        let mut command = [0u8; CTRL_HDR_LEN + 8];
        header(&mut command, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE);
        put32(&mut command, 16, VIRGL_CONTEXT_ID); // ctrl_hdr.ctx_id
        put32(&mut command, CTRL_HDR_LEN, resource_id);
        matches!(
            control(&mut display.queue, &command),
            Some(VIRTIO_GPU_RESP_OK_NODATA)
        )
    }
}

/// Creates the virgl context. Idempotent.
pub fn create_context() -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        if !virgl_available() {
            return false;
        }
        // struct virtio_gpu_ctx_create { u32 context_init; u32 padding;
        // char debug_name[64]; } with context_init = VIRTIO_GPU_CONTEXT_INIT_
        // VIRGL (1).
        let mut command = [0u8; CTRL_HDR_LEN + 72];
        header(&mut command, VIRTIO_GPU_CMD_CTX_CREATE);
        put32(&mut command, 16, VIRGL_CONTEXT_ID); // ctrl_hdr.ctx_id
        put32(&mut command, CTRL_HDR_LEN, 1);
        let name = b"werewolf";
        command[CTRL_HDR_LEN + 8..CTRL_HDR_LEN + 8 + name.len()].copy_from_slice(name);
        matches!(
            control(&mut display.queue, &command),
            Some(VIRTIO_GPU_RESP_OK_NODATA)
        )
    }
}

/// Transfers a box of a 3D resource to the host.
pub fn transfer_3d(
    resource_id: u32,
    pa: usize,
    length: usize,
    width: u32,
    height: u32,
    to_host: bool,
) -> bool {
    unsafe {
        let Some(display) = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() else {
            return false;
        };
        // struct virtio_gpu_transfer_host_3d { box b; u64 offset; u32
        // resource_id; u32 level; u32 stride; u32 layer_stride; }
        let mut command = [0u8; CTRL_HDR_LEN + 48];
        header(
            &mut command,
            if to_host {
                VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D
            } else {
                VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D
            },
        );
        put32(&mut command, 16, VIRGL_CONTEXT_ID); // ctrl_hdr.ctx_id
        put32(&mut command, CTRL_HDR_LEN + 12, width);
        put32(&mut command, CTRL_HDR_LEN + 16, height);
        put32(&mut command, CTRL_HDR_LEN + 20, 1);
        put64(&mut command, CTRL_HDR_LEN + 24, 0);
        put32(&mut command, CTRL_HDR_LEN + 32, resource_id);
        put32(&mut command, CTRL_HDR_LEN + 36, 0); // level
        put32(&mut command, CTRL_HDR_LEN + 40, width * 4); // stride
        put32(&mut command, CTRL_HDR_LEN + 44, 0); // layer_stride
        if to_host {
            clean(phys_to_virt(pa), length);
        }
        let ok = matches!(
            control(&mut display.queue, &command),
            Some(VIRTIO_GPU_RESP_OK_NODATA)
        );
        if ok && !to_host {
            invalidate(phys_to_virt(pa), length);
        }
        ok
    }
}

/// Bitmask of capset ids the device advertises (bit `id` set).
pub fn supported_capsets() -> u64 {
    unsafe {
        let display = match (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut() {
            Some(display) => display,
            None => return 0,
        };
        let mut mask = 0u64;
        for index in 0..8u32 {
            let mut command = [0u8; CTRL_HDR_LEN + 8];
            header(&mut command, VIRTIO_GPU_CMD_GET_CAPSET_INFO);
            put32(&mut command, CTRL_HDR_LEN, index);
            let Some(kind) = control(&mut display.queue, &command) else {
                break;
            };
            if kind != VIRTIO_GPU_RESP_OK_CAPSET_INFO {
                break;
            }
            let response = display.queue.command + 4096;
            let word = |offset: usize| -> u32 {
                u32::from_le_bytes([
                    core::ptr::read_volatile((response + offset) as *const u8),
                    core::ptr::read_volatile((response + offset + 1) as *const u8),
                    core::ptr::read_volatile((response + offset + 2) as *const u8),
                    core::ptr::read_volatile((response + offset + 3) as *const u8),
                ])
            };
            let entry_id = word(CTRL_HDR_LEN);
            let size = word(CTRL_HDR_LEN + 8);
            if size == 0 {
                continue;
            }
            if entry_id < 64 {
                mask |= 1 << entry_id;
            }
        }
        mask
    }
}

/// Reads one capability set from the device and caches it by capset id.
/// `GET_CAPSET_INFO` takes an index into the device's capset list (QEMU's
/// list is `[VIRGL, VIRGL2, DRM]` when the DRM native context is enabled),
/// while `GET_CAPSET` takes the id the device reports.
pub fn capset_caps(id: u32) -> Option<(&'static [u8], u32)> {
    unsafe {
        let table = &mut *core::ptr::addr_of_mut!(CAPS);
        if let Some(index) = table
            .iter()
            .position(|entry| entry.len != 0 && entry.id == id)
        {
            let entry = &table[index];
            return Some((&entry.data[..entry.len], entry.version));
        }
        let display = (&mut *core::ptr::addr_of_mut!(DISPLAY)).as_mut()?;
        if !virgl_available() {
            return None;
        }
        let mut found_id = 0u32;
        let mut found_version = 0u32;
        let mut found_size = 0usize;
        for index in 0..8u32 {
            let mut command = [0u8; CTRL_HDR_LEN + 8];
            header(&mut command, VIRTIO_GPU_CMD_GET_CAPSET_INFO);
            put32(&mut command, CTRL_HDR_LEN, index);
            let Some(kind) = control(&mut display.queue, &command) else {
                break;
            };
            if kind != VIRTIO_GPU_RESP_OK_CAPSET_INFO {
                break;
            }
            let response = display.queue.command + 4096;
            let word = |offset: usize| -> u32 {
                u32::from_le_bytes([
                    core::ptr::read_volatile((response + offset) as *const u8),
                    core::ptr::read_volatile((response + offset + 1) as *const u8),
                    core::ptr::read_volatile((response + offset + 2) as *const u8),
                    core::ptr::read_volatile((response + offset + 3) as *const u8),
                ])
            };
            let entry_id = word(CTRL_HDR_LEN);
            let size = word(CTRL_HDR_LEN + 8) as usize;
            if size == 0 {
                // End of the list (or an entry the host cannot describe).
                continue;
            }
            if entry_id == id {
                found_id = entry_id;
                found_version = word(CTRL_HDR_LEN + 4);
                found_size = size;
                break;
            }
        }
        if found_size == 0 || found_size > 4096 {
            return None;
        }
        let mut command = [0u8; CTRL_HDR_LEN + 8];
        header(&mut command, VIRTIO_GPU_CMD_GET_CAPSET);
        put32(&mut command, CTRL_HDR_LEN, found_id);
        put32(&mut command, CTRL_HDR_LEN + 4, found_version.min(2));
        let kind = control(&mut display.queue, &command)?;
        if kind != VIRTIO_GPU_RESP_OK_CAPSET {
            failed("capset", kind);
            return None;
        }
        // struct virtio_gpu_resp_capset { struct virtio_gpu_ctrl_hdr hdr;
        // __u8 capset_data[]; } — the blob starts right after the header.
        let source = display.queue.command + 4096 + CTRL_HDR_LEN;
        invalidate(source, found_size);
        let slot = table.iter_mut().position(|entry| entry.len == 0)?;
        table[slot].id = found_id;
        table[slot].version = found_version;
        table[slot].len = found_size;
        core::ptr::copy_nonoverlapping(
            source as *const u8,
            table[slot].data.as_mut_ptr(),
            found_size,
        );
        console::write_str("VIRTIO-GPU: capset ");
        console::write_dec_u64(found_id as u64);
        console::write_str(" ");
        console::write_dec_u64(found_size as u64);
        console::write_str(" bytes\r\n");
        let index = table
            .iter()
            .position(|entry| entry.id == found_id && entry.len != 0)?;
        let entry = &table[index];
        Some((&entry.data[..entry.len], entry.version))
    }
}

/// Size of the scanout, when the GPU is up.
pub fn size() -> Option<(u32, u32)> {
    unsafe {
        let Some(display) = (&*core::ptr::addr_of!(DISPLAY)).as_ref() else {
            return None;
        };
        Some((display.width, display.height))
    }
}

/// Guest-physical base of the framebuffer, for a future `/dev/gpufb` mapping.
pub fn framebuffer_phys() -> Option<(usize, usize)> {
    unsafe {
        let Some(display) = (&*core::ptr::addr_of!(DISPLAY)).as_ref() else {
            return None;
        };
        Some((display.framebuffer_pa, display.framebuffer_pages * 4096))
    }
}
