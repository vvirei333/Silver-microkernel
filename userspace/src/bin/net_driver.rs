#![no_std]
#![no_main]

#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../net_ipc.rs"]
mod net_ipc;
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

struct Virtqueue {
    index: u16,
    size: u16,
    notify_addr: u64,
    descriptors_vaddr: u64,
    available_vaddr: u64,
    used_vaddr: u64,
    /// Frame buffers appended to the same DMA allocation as the rings. One
    /// descriptor always points at the buffer with the same index, so a used
    /// entry's `id` is also the slot to recycle.
    buffers_vaddr: u64,
    buffers_paddr: u64,
    buffer_size: u32,
    buffer_count: u16,
    /// Next available-ring slot the driver will fill, and the number of
    /// buffers ever published. Wrapping as u16 is part of the ring ABI.
    avail_idx: u16,
    /// RX: next used-ring entry to consume. TX: newest used-ring index seen,
    /// which makes `avail_idx - last_used_idx` the number of frames in flight.
    last_used_idx: u16,
    next_buffer: u16,
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

/// The device writes receive frames into descriptors with this flag set.
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Sixteen 2 KiB frames per queue is enough for a test exchange and keeps the
/// DMA allocation small (about 44 KiB per queue including the rings).
const BUFFER_COUNT_MAX: u16 = 16;
const BUFFER_SIZE: u32 = 2048;

/// QEMU negotiates `VIRTIO_F_VERSION_1`, and a modern device prefixes every
/// frame with the 12-byte header (the `num_buffers` field is present even
/// without `VIRTIO_NET_F_MRG_RXBUF`). The driver sends a zeroed header and
/// skips it on receive.
const VIRTIO_NET_HDR_SIZE: usize = 12;

/// Reasons `send_packet` can fail, reported like the init stages.
const SEND_NOT_READY: u8 = 1;
const SEND_NO_BUFFER: u8 = 2;
const SEND_TOO_LARGE: u8 = 3;

static mut RX_QUEUE: Option<Virtqueue> = None;
static mut TX_QUEUE: Option<Virtqueue> = None;

/// One received Ethernet frame, copied out of the virtqueue before the slot is
/// recycled. Static because the userspace stack is only 8 KiB.
static mut RX_FRAME_BUFFER: [u8; net_ipc::MAX_FRAME] = [0_u8; net_ipc::MAX_FRAME];
/// One frame copied out of the server's TX slot before the slot is released.
static mut TX_FRAME_BUFFER: [u8; net_ipc::MAX_FRAME] = [0_u8; net_ipc::MAX_FRAME];

/// The server's DMA mailbox, once it has been announced over IPC.
#[derive(Copy, Clone)]
struct Mailbox {
    vaddr: u64,
    paddr: u64,
    size: u64,
    /// Task ID that owns the window; doorbells and TX frames must come from it.
    server: u64,
}

static mut MAILBOX: Option<Mailbox> = None;

/// One timer tick is 10 ms. Receiving with this timeout wakes up for a frame
/// poll even when the server sends no IPC.
const IDLE_POLL_TICKS: u64 = 1;

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

    let mac = match unsafe { initialize(&device) } {
        Ok(mac) => {
            syscall::print("VIRTIO-NET INITIALIZED AND READY\r\n");
            print_mac(&mac);
            mac
        }
        Err(stage) => fail(stage, "VIRTIO-NET INITIALIZATION FAILED\r\n"),
    };

    // The server owns smoltcp and announces a DMA mailbox over IPC.
    // From here on this task only moves frames between virtio-net and that
    // window.
    serve(&mac);
}

/// IPC and frame forwarding loop. `receive_timeout` doubles as the idle sleep,
/// so a doorbell is handled immediately while RX is still polled every tick.
fn serve(mac: &[u8; 6]) -> ! {
    loop {
        let mut message = ipc::Message::new(0, &[]);
        if syscall::receive_timeout(&mut message, IDLE_POLL_TICKS).is_ok() {
            handle_message(&message, mac);
            // Drain anything that arrived while the first message was handled.
            while syscall::receive_timeout(&mut message, 0).is_ok() {
                handle_message(&message, mac);
            }
        }
        poll_receive();
    }
}

fn handle_message(message: &ipc::Message, mac: &[u8; 6]) {
    match message.payload[0] {
        net_ipc::OP_ATTACH => attach_mailbox(message, mac),
        net_ipc::OP_TX => transmit_from_mailbox(message),
        _ => {}
    }
}

/// Maps the server's DMA window and acknowledges it with the NIC MAC. `net_server`
/// repeats `OP_ATTACH` once a second until this reply arrives, so the same
/// physical window is not mapped twice.
fn attach_mailbox(message: &ipc::Message, mac: &[u8; 6]) {
    let paddr = net_ipc::attach_paddr(message);
    let size = net_ipc::attach_size(message);
    if size < net_ipc::DMA_SIZE {
        syscall::print("NET DRIVER: MAILBOX TOO SMALL\r\n");
        return;
    }

    let vaddr = match current_mailbox() {
        Some(mailbox) if mailbox.paddr == paddr && mailbox.size == size => mailbox.vaddr,
        _ => {
            let Some(vaddr) = syscall::map_phys(paddr, size) else {
                syscall::print("NET DRIVER: MAILBOX MAP FAILED\r\n");
                return;
            };
            syscall::print("NET DRIVER: MAILBOX ATTACHED\r\n");
            vaddr
        }
    };

    // Re-announcing the same window is also how a restarted server refreshes
    // its task ID without forcing a second mapping.
    unsafe {
        *core::ptr::addr_of_mut!(MAILBOX) = Some(Mailbox {
            vaddr,
            paddr,
            size,
            server: message.sender,
        });
    }

    let _ = syscall::try_send(message.sender, &net_ipc::attached(mac));
}

/// Sends one frame that the server published in its TX slot. The frame is
/// copied out before the length is cleared so the server can publish the next
/// one without racing a virtqueue that is still reading the window.
fn transmit_from_mailbox(message: &ipc::Message) {
    let Some(mailbox) = current_mailbox() else {
        return;
    };
    if message.sender != mailbox.server {
        return;
    }

    let length = read_u32(mailbox.vaddr, net_ipc::TX_LENGTH) as usize;
    if length == 0 || length > net_ipc::MAX_FRAME {
        write_u32(mailbox.vaddr, net_ipc::TX_LENGTH, 0);
        return;
    }

    net_ipc::fence();
    let frame = unsafe { &mut *core::ptr::addr_of_mut!(TX_FRAME_BUFFER) };
    let source = mailbox.vaddr + net_ipc::TX_FRAME;
    for (index, byte) in frame[..length].iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile((source + index as u64) as *const u8) };
    }
    net_ipc::fence();
    write_u32(mailbox.vaddr, net_ipc::TX_LENGTH, 0);

    if send_packet(&frame[..length]).is_err() {
        syscall::print("NET DRIVER: TX FRAME DROPPED\r\n");
    }
}

/// Moves at most one received frame into the server's RX slot. The next frame
/// waits in virtio until the server clears `rx_len`, so a slow server applies
/// backpressure instead of losing frames.
fn poll_receive() {
    let Some(mailbox) = current_mailbox() else {
        return;
    };
    if read_u32(mailbox.vaddr, net_ipc::RX_LENGTH) != 0 {
        return;
    }

    let frame = unsafe { &mut *core::ptr::addr_of_mut!(RX_FRAME_BUFFER) };
    let Some(length) = receive_packet(frame) else {
        return;
    };
    if length == 0 {
        return;
    }

    let destination = mailbox.vaddr + net_ipc::RX_FRAME;
    for (index, byte) in frame[..length].iter().enumerate() {
        unsafe {
            core::ptr::write_volatile((destination + index as u64) as *mut u8, *byte);
        }
    }
    net_ipc::fence();
    write_u32(mailbox.vaddr, net_ipc::RX_LENGTH, length as u32);
    let _ = syscall::try_send(mailbox.server, &net_ipc::frame_doorbell(length as u16));
}

fn current_mailbox() -> Option<Mailbox> {
    unsafe { *core::ptr::addr_of!(MAILBOX) }
}

fn read_u32(base: u64, offset: u64) -> u32 {
    unsafe { core::ptr::read_volatile(base.wrapping_add(offset) as *const u32) }
}

fn write_u32(base: u64, offset: u64, value: u32) {
    unsafe { core::ptr::write_volatile(base.wrapping_add(offset) as *mut u32, value) };
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

        let mut rx = setup_virtqueue(device, QUEUE_RX)?;
        let tx = setup_virtqueue(device, QUEUE_TX)?;

        // Receive buffers must be in the available ring before the device is
        // told the driver is ready, but they must not be notified yet.
        publish_rx_buffers(&mut rx);

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
        core::arch::asm!("dsb sy", options(nostack));

        // Only a device that is DRIVER_OK may be notified that RX buffers are
        // waiting. TX has nothing outstanding yet.
        notify(&rx);

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

    let buffer_count = size.min(BUFFER_COUNT_MAX);
    let buffer_size = BUFFER_SIZE;
    let buffers_offset = used_offset.checked_add(used_span).ok_or(INIT_DMA)?;
    let buffers_span =
        align_page(u64::from(buffer_count) * u64::from(buffer_size)).ok_or(INIT_DMA)?;
    let total_size = buffers_offset.checked_add(buffers_span).ok_or(INIT_DMA)?;

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
        buffers_vaddr: vaddr + buffers_offset,
        buffers_paddr: paddr + buffers_offset,
        buffer_size,
        buffer_count,
        avail_idx: 0,
        last_used_idx: 0,
        next_buffer: 0,
    })
}

/// The DMA buffer behind a descriptor slot. Descriptor `slot` always points at
/// buffer `slot`, so used-ring `id` values are slot numbers.
impl Virtqueue {
    fn buffer_vaddr(&self, slot: u16) -> u64 {
        self.buffers_vaddr + u64::from(slot) * u64::from(self.buffer_size)
    }

    fn buffer_paddr(&self, slot: u16) -> u64 {
        self.buffers_paddr + u64::from(slot) * u64::from(self.buffer_size)
    }
}

/// Fills a descriptor with the slot's buffer address. `flags` is
/// `VIRTQ_DESC_F_WRITE` for receive slots and zero for transmit slots, where
/// the device only reads.
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

/// Appends `slot` to the available ring and publishes the new index. The
/// caller has already written the descriptor and (for TX) the frame. Both
/// barriers are mandatory: the device may read the ring entry only after it
/// observes the index, and it may read the index only after the entry landed.
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
    unsafe { core::ptr::write_volatile(queue.notify_addr as *mut u16, queue.index) };
}

/// Posts every RX buffer before `DRIVER_OK`. No notification is sent here:
/// the spec forbids touching the notify register until the device is ready.
unsafe fn publish_rx_buffers(queue: &mut Virtqueue) {
    for slot in 0..queue.buffer_count {
        unsafe { write_descriptor(queue, slot, queue.buffer_size, VIRTQ_DESC_F_WRITE) };
        unsafe { publish_buffer(queue, slot) };
    }
}

/// Collects completed TX descriptors. TX frames retire in submission order, so
/// once the used index advances, the oldest outstanding slot is reusable and
/// the difference to `avail_idx` is the number of frames still in flight.
unsafe fn reclaim_tx(queue: &mut Virtqueue) -> u16 {
    let used = queue.used_vaddr as *const VirtqUsed;
    let used_idx = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*used).idx)) };
    core::arch::asm!("dmb sy", options(nostack));
    queue.last_used_idx = used_idx;
    queue.avail_idx.wrapping_sub(used_idx)
}

/// Sends one Ethernet frame. Returns a stage code when the queue is full or the
/// frame does not fit in a 2 KiB buffer.
fn send_packet(data: &[u8]) -> Result<(), u8> {
    let queue = unsafe { (*core::ptr::addr_of_mut!(TX_QUEUE)).as_mut() }.ok_or(SEND_NOT_READY)?;

    let in_flight = unsafe { reclaim_tx(queue) };
    if in_flight >= queue.buffer_count {
        return Err(SEND_NO_BUFFER);
    }

    let total = VIRTIO_NET_HDR_SIZE + data.len();
    if total > queue.buffer_size as usize {
        return Err(SEND_TOO_LARGE);
    }

    // Round-robin slot selection is safe because the device completes in
    // order: with fewer than `buffer_count` frames in flight, `next_buffer` is
    // always the oldest, already-completed slot.
    let slot = queue.next_buffer;
    queue.next_buffer = (slot + 1) % queue.buffer_count;

    let buffer = queue.buffer_vaddr(slot);
    unsafe {
        // A zeroed virtio_net_hdr asks for no offloads: plain Ethernet frame.
        for index in 0..VIRTIO_NET_HDR_SIZE {
            core::ptr::write_volatile((buffer as *mut u8).add(index), 0);
        }
        core::ptr::copy_nonoverlapping(
            data.as_ptr(),
            (buffer as *mut u8).add(VIRTIO_NET_HDR_SIZE),
            data.len(),
        );
        write_descriptor(queue, slot, total as u32, 0);
        publish_buffer(queue, slot);
        notify(queue);
    }

    Ok(())
}

/// Copies at most one received frame into `buffer`, skipping the virtio header,
/// and returns the number of payload bytes. The RX slot is immediately
/// recycled, so the device can fill it again while the caller processes the
/// copy. Returns `None` when the used ring has no new entry.
fn receive_packet(buffer: &mut [u8]) -> Option<usize> {
    let queue = unsafe { (*core::ptr::addr_of_mut!(RX_QUEUE)).as_mut() }?;
    let used = queue.used_vaddr as *const VirtqUsed;
    let used_idx = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*used).idx)) };
    if used_idx == queue.last_used_idx {
        return None;
    }
    // The device wrote the ring entry before bumping `idx`; order the reads.
    unsafe { core::arch::asm!("dmb sy", options(nostack)) };

    let element_index = usize::from(queue.last_used_idx % queue.size);
    let element = unsafe {
        let ring = core::ptr::addr_of!((*used).ring).cast::<VirtqUsedElement>();
        core::ptr::read_volatile(ring.add(element_index))
    };
    queue.last_used_idx = queue.last_used_idx.wrapping_add(1);

    let slot = element.id as u16;
    if slot >= queue.buffer_count {
        return None;
    }

    let length = element.len as usize;
    let payload_length = length.saturating_sub(VIRTIO_NET_HDR_SIZE);
    let copied = payload_length.min(buffer.len());
    unsafe {
        let source = (queue.buffer_vaddr(slot) as *const u8).add(VIRTIO_NET_HDR_SIZE);
        for (index, byte) in buffer.iter_mut().take(copied).enumerate() {
            *byte = core::ptr::read_volatile(source.add(index));
        }
        // Hand the slot straight back: the caller keeps only the copy.
        write_descriptor(queue, slot, queue.buffer_size, VIRTQ_DESC_F_WRITE);
        publish_buffer(queue, slot);
        notify(queue);
    }

    Some(copied)
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
