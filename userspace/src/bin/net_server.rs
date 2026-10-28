//! Userspace TCP/IP stack.
//!
//! `net_driver` owns virtio-net and moves raw Ethernet frames in and out of
//! the NIC. This server owns smoltcp: it announces a DMA mailbox to the
//! driver, feeds received frames to the interface, and hands generated frames
//! back. An ICMP echo request to the QEMU slirp gateway exercises the whole
//! path: ARP resolution, frame TX, frame RX, and checksum verification.

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
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::socket::icmp;
use smoltcp::storage::{PacketBuffer, PacketMetadata};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address,
};

/// Interface polling period. It is also the clock granularity smoltcp sees.
const POLL_MILLIS: u64 = 10;
/// The driver may still be probing the NIC when this task starts, so the attach
/// request is retried instead of assuming a boot order.
const ATTACH_RETRY_MILLIS: u64 = 1000;
/// Storage reserved for the ICMP socket and future TCP/UDP sockets.
const SOCKET_COUNT: usize = 4;

/// ICMP echo identifier; the stack only accepts replies carrying this one.
const PING_IDENT: u16 = 0x5349;
const PING_PAYLOAD: &[u8] = b"silver-ping";
const ICMP_METADATA_COUNT: usize = 4;
const ICMP_PAYLOAD_SIZE: usize = 256;

const TASK_LIST_CAPACITY: usize = 32;
const EMPTY_TASK: syscall::TaskInfo = syscall::TaskInfo {
    id: 0,
    state: 0,
    program: 0,
    argument: 0,
};
const TASK_DEAD: u64 = 6;
const TASK_STOPPED: u64 = 7;

/// QEMU's default `virt` NIC address. `OP_ATTACHED` replaces it with the MAC
/// read from the device.
const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// QEMU user-mode networking: guest 10.0.2.15/24 behind gateway 10.0.2.2.
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GUEST_PREFIX: u8 = 24;
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

const EXIT_NO_DMA: u64 = 1;
const EXIT_NO_ICMP: u64 = 2;

/// The frame buffers live in `.bss`, not on the 8 KiB userspace stack.
static mut DEVICE: FrameDevice = FrameDevice::new();

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(driver_hint: u64) -> ! {
    syscall::print("NET SERVER STARTING\r\n");

    let Some((dma_vaddr, dma_paddr)) = syscall::alloc_dma(net_ipc::DMA_SIZE) else {
        fail(EXIT_NO_DMA, "NET SERVER: DMA ALLOCATION FAILED\r\n");
    };
    write_u32(dma_vaddr, net_ipc::RX_LENGTH, 0);
    write_u32(dma_vaddr, net_ipc::TX_LENGTH, 0);

    let device = unsafe { &mut *core::ptr::addr_of_mut!(DEVICE) };

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(DEFAULT_MAC)));
    config.random_seed = syscall::uptime_ticks();
    let mut interface = Interface::new(config, device, Instant::from_millis(0));
    interface.update_ip_addrs(|addresses| {
        let _ = addresses.push(IpCidr::new(
            IpAddress::v4(GUEST_IP[0], GUEST_IP[1], GUEST_IP[2], GUEST_IP[3]),
            GUEST_PREFIX,
        ));
    });
    let _ = interface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
        GATEWAY_IP[0],
        GATEWAY_IP[1],
        GATEWAY_IP[2],
        GATEWAY_IP[3],
    ));

    // `ManagedSlice` borrows this storage, so it must outlive the socket set.
    let mut socket_storage: [SocketStorage; SOCKET_COUNT] =
        core::array::from_fn(|_| SocketStorage::EMPTY);
    let mut sockets = SocketSet::new(&mut socket_storage[..]);

    // ICMP echo storage. The packet buffers borrow these arrays for the whole
    // run, exactly like `socket_storage` above.
    let mut icmp_rx_metadata = [PacketMetadata::<IpAddress>::EMPTY; ICMP_METADATA_COUNT];
    let mut icmp_rx_payload = [0_u8; ICMP_PAYLOAD_SIZE];
    let mut icmp_tx_metadata = [PacketMetadata::<IpAddress>::EMPTY; ICMP_METADATA_COUNT];
    let mut icmp_tx_payload = [0_u8; ICMP_PAYLOAD_SIZE];
    let mut icmp_socket = icmp::Socket::new(
        PacketBuffer::new(&mut icmp_rx_metadata[..], &mut icmp_rx_payload[..]),
        PacketBuffer::new(&mut icmp_tx_metadata[..], &mut icmp_tx_payload[..]),
    );
    if icmp_socket.bind(icmp::Endpoint::Ident(PING_IDENT)).is_err() {
        fail(EXIT_NO_ICMP, "NET SERVER: ICMP BIND FAILED\r\n");
    }
    let icmp_handle = sockets.add(icmp_socket);
    let capabilities = device.capabilities();

    let mut driver = resolve_driver(driver_hint);
    let mut attached = false;
    let mut next_attach = 0_u64;
    let mut now_millis = 0_i64;
    let mut ping_sent = false;
    let mut ping_status_pending = false;

    syscall::print("NET SERVER READY\r\n");

    loop {
        now_millis += POLL_MILLIS as i64;
        let now = Instant::from_millis(now_millis);

        if driver.map_or(true, |id| !syscall::task_alive(id)) {
            driver = resolve_driver(driver_hint);
            attached = false;
            next_attach = 0;
        }

        if !attached && now_millis as u64 >= next_attach {
            if let Some(id) = driver {
                let _ = syscall::try_send(id, &net_ipc::attach(dma_paddr, net_ipc::DMA_SIZE));
            }
            next_attach = now_millis as u64 + ATTACH_RETRY_MILLIS;
        }

        // Doorbells only say "look at the mailbox". Draining them keeps the
        // driver's mailbox from filling up; the frame itself is read below.
        let mut message = ipc::Message::new(0, &[]);
        while syscall::receive_timeout(&mut message, 0).is_ok() {
            if Some(message.sender) == driver {
                handle_doorbell(&message, &mut interface, &mut attached);
            }
        }

        // Polling the length word makes frame delivery independent of the
        // doorbell: a lost message cannot strand a frame.
        let rx_length = read_u32(dma_vaddr, net_ipc::RX_LENGTH) as usize;
        if rx_length > 0 {
            let length = rx_length.min(net_ipc::MAX_FRAME);
            net_ipc::fence();
            device.read_dma_frame(dma_vaddr, net_ipc::RX_FRAME, length);
            net_ipc::fence();
            write_u32(dma_vaddr, net_ipc::RX_LENGTH, 0);
        }

        // The driver has mapped the mailbox: queue one echo request to the
        // slirp gateway. smoltcp resolves 10.0.2.2 with ARP first and keeps
        // the ICMP packet queued in the socket until the neighbour is usable.
        if attached && !ping_sent {
            if send_ping(&mut sockets, icmp_handle, &capabilities.checksum, 0) {
                ping_sent = true;
                syscall::print("NET SERVER: ICMP ECHO SENT TO 10.0.2.2\r\n");
            }
        }

        let _ = interface.poll(now, device, &mut sockets);
        if receive_pings(&mut sockets, icmp_handle) {
            ping_status_pending = true;
        }

        // This task owns no console, so hand the result to the shell, which
        // does. Retry until the shell exists and its mailbox accepts the text.
        if ping_status_pending {
            if let Some(shell) = resolve_task(syscall::PROGRAM_HOWL) {
                if syscall::try_send(shell, &ipc::Message::new(0, b"NET ICMP REPLY OK")) {
                    ping_status_pending = false;
                }
            }
        }

        // smoltcp delivers at most one frame per poll (`max_burst_size`), so
        // one TX slot is enough. Wait until the driver has released the
        // previous slot before publishing the next frame.
        if device.tx_len > 0 && read_u32(dma_vaddr, net_ipc::TX_LENGTH) == 0 {
            write_dma_frame(dma_vaddr, net_ipc::TX_FRAME, &device.tx[..device.tx_len]);
            net_ipc::fence();
            write_u32(dma_vaddr, net_ipc::TX_LENGTH, device.tx_len as u32);
            if let Some(id) = driver {
                let _ = syscall::try_send(id, &net_ipc::tx_doorbell(device.tx_len as u16));
            }
            device.tx_len = 0;
        }

        syscall::sleep(POLL_MILLIS);
    }
}

/// Queues an ICMPv4 echo request. The socket owns the bytes; `emit` fills in
/// the type/code/identifier/sequence header and the checksum.
fn send_ping(
    sockets: &mut SocketSet<'_>,
    handle: SocketHandle,
    checksum: &ChecksumCapabilities,
    seq_no: u16,
) -> bool {
    let repr = Icmpv4Repr::EchoRequest {
        ident: PING_IDENT,
        seq_no,
        data: PING_PAYLOAD,
    };
    let destination = IpAddress::v4(GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3]);
    let socket = sockets.get_mut::<icmp::Socket>(handle);
    let Ok(buffer) = socket.send(repr.buffer_len(), destination) else {
        return false;
    };
    let mut packet = Icmpv4Packet::new_unchecked(buffer);
    repr.emit(&mut packet, checksum);
    true
}

/// Drains every echo reply smoltcp has delivered to the ICMP socket. Returns
/// true if at least one reply was consumed.
fn receive_pings(sockets: &mut SocketSet<'_>, handle: SocketHandle) -> bool {
    let mut payload = [0_u8; ICMP_PAYLOAD_SIZE];
    let mut received = false;
    loop {
        let socket = sockets.get_mut::<icmp::Socket>(handle);
        let Ok((length, source)) = socket.recv_slice(&mut payload) else {
            return received;
        };
        received = true;
        syscall::print("NET SERVER: ICMP REPLY ");
        syscall::print_dec_u64(length as u64);
        syscall::print(" BYTES FROM ");
        print_ipv4(source);
        syscall::print("\r\n");
    }
}

fn print_ipv4(address: IpAddress) {
    // Only `proto-ipv4` is enabled, so `IpAddress` has no other variant.
    let IpAddress::Ipv4(address) = address;
    for (index, byte) in address.octets().iter().enumerate() {
        if index > 0 {
            syscall::print(".");
        }
        syscall::print_dec_u64(u64::from(*byte));
    }
}

/// Handles the two control messages the driver can send. `OP_FRAME` needs no
/// action here because the loop polls the RX length word anyway.
fn handle_doorbell(message: &ipc::Message, interface: &mut Interface, attached: &mut bool) {
    match message.payload[0] {
        net_ipc::OP_ATTACHED => {
            if let Some(mac) = net_ipc::attached_mac(message) {
                interface.set_hardware_addr(HardwareAddress::Ethernet(EthernetAddress(mac)));
                *attached = true;
                syscall::print("NET SERVER: DRIVER ATTACHED\r\n");
            }
        }
        _ => {}
    }
}

/// Finds the newest live task running `program`.
fn resolve_task(program: u64) -> Option<u64> {
    let mut tasks = [EMPTY_TASK; TASK_LIST_CAPACITY];
    let count = syscall::task_list(&mut tasks)?;
    tasks[..count.min(TASK_LIST_CAPACITY)]
        .iter()
        .rev()
        .find(|task| {
            task.program == program && task.state != TASK_DEAD && task.state != TASK_STOPPED
        })
        .map(|task| task.id)
}

/// Resolves the live net driver task. The hint is the task ID silverd passed
/// at spawn time; the task list is the fallback after a driver restart.
fn resolve_driver(preferred: u64) -> Option<u64> {
    let mut tasks = [EMPTY_TASK; TASK_LIST_CAPACITY];
    let count = syscall::task_list(&mut tasks)?;
    let tasks = &tasks[..count.min(TASK_LIST_CAPACITY)];
    let live = |task: &syscall::TaskInfo| {
        task.program == syscall::PROGRAM_NET_DRIVER
            && task.state != TASK_DEAD
            && task.state != TASK_STOPPED
    };
    tasks
        .iter()
        .rev()
        .find(|task| task.id == preferred && live(task))
        .or_else(|| tasks.iter().rev().find(|task| live(task)))
        .map(|task| task.id)
}

fn read_u32(base: u64, offset: u64) -> u32 {
    unsafe { core::ptr::read_volatile(base.wrapping_add(offset) as *const u32) }
}

fn write_u32(base: u64, offset: u64, value: u32) {
    unsafe { core::ptr::write_volatile(base.wrapping_add(offset) as *mut u32, value) };
}

fn write_dma_frame(base: u64, offset: u64, frame: &[u8]) {
    let destination = base.wrapping_add(offset) as *mut u8;
    for (index, byte) in frame.iter().enumerate() {
        unsafe { core::ptr::write_volatile(destination.add(index), *byte) };
    }
}

/// smoltcp device backed by one RX slot and one TX slot.
///
/// The RX slot is filled from the shared DMA window before each poll. The TX
/// slot is filled by smoltcp and drained by the main loop after the poll.
struct FrameDevice {
    rx: [u8; net_ipc::MAX_FRAME],
    rx_len: usize,
    tx: [u8; net_ipc::MAX_FRAME],
    tx_len: usize,
}

impl FrameDevice {
    const fn new() -> Self {
        Self {
            rx: [0_u8; net_ipc::MAX_FRAME],
            rx_len: 0,
            tx: [0_u8; net_ipc::MAX_FRAME],
            tx_len: 0,
        }
    }

    fn read_dma_frame(&mut self, base: u64, offset: u64, length: usize) {
        let length = length.min(net_ipc::MAX_FRAME);
        let source = base.wrapping_add(offset) as *const u8;
        for (index, byte) in self.rx[..length].iter_mut().enumerate() {
            *byte = unsafe { core::ptr::read_volatile(source.add(index)) };
        }
        self.rx_len = length;
    }
}

struct RxToken<'a> {
    frame: &'a [u8],
}

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.frame)
    }
}

struct TxToken<'a> {
    buffer: &'a mut [u8],
    length: &'a mut usize,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // The interface respects the advertised MTU; clamping keeps a stack
        // bug from panicking inside smoltcp.
        let len = len.min(self.buffer.len());
        let result = f(&mut self.buffer[..len]);
        *self.length = len;
        result
    }
}

impl Device for FrameDevice {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_len == 0 {
            return None;
        }
        let length = self.rx_len;
        self.rx_len = 0;
        // Disjoint field borrows let the reply token coexist with the frame.
        let rx = &self.rx[..length];
        let tx = &mut self.tx[..];
        let tx_len = &mut self.tx_len;
        Some((RxToken { frame: rx }, TxToken { buffer: tx, length: tx_len }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            buffer: &mut self.tx[..],
            length: &mut self.tx_len,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = net_ipc::MAX_FRAME;
        // One frame per poll matches the single TX slot above.
        capabilities.max_burst_size = Some(1);
        capabilities
    }
}

fn fail(status: u64, message: &str) -> ! {
    syscall::print(message);
    syscall::exit_with_status(status)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
