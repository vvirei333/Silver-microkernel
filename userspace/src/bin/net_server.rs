//! Userspace TCP/IP stack.
//!
//! `net_driver` owns virtio-net and moves raw Ethernet frames in and out of
//! the NIC. This server owns smoltcp: it announces a DMA mailbox to the
//! driver, feeds received frames to the interface, and hands generated frames
//! back.
//!
//! Services exposed to other user tasks over IPC:
//!
//! - `ping` (ICMP echo);
//! - `dns` (resolve a name, the name travels in the shared data window);
//! - `http` (GET a `http://` URL and return the body in the shared window);
//! - `ipconfig` (the current DHCP/static IPv4 configuration).
//!
//! The shared data window is a single DMA allocation owned by this task.
//! Clients map it with `map_phys` and exchange request/response bytes there;
//! the 32-byte mailbox only carries doorbells and small status replies.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../net_ipc.rs"]
mod net_ipc;
#[path = "../syscall.rs"]
mod syscall;
#[path = "../tls_client.rs"]
mod tls_client;
#[path = "../tls_roots.rs"]
mod tls_roots;

#[global_allocator]
static GLOBAL_ALLOCATOR: allocator::UserAllocator = allocator::UserAllocator;

use core::panic::PanicInfo;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, dns, icmp, tcp};
use smoltcp::storage::{PacketBuffer, PacketMetadata};
use smoltcp::time::Instant;
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr,
    IpEndpoint, Ipv4Address,
};

/// Interface polling period. It is also the clock granularity smoltcp sees.
const POLL_MILLIS: u64 = 10;
/// The driver may still be probing the NIC when this task starts, so the attach
/// request is retried instead of assuming a boot order.
const ATTACH_RETRY_MILLIS: u64 = 1000;
/// One socket each for ICMP, DHCP, DNS and TCP, plus slack.
const SOCKET_COUNT: usize = 5;

/// ICMP echo identifier; the stack only accepts replies carrying this one.
const PING_IDENT: u16 = 0x5349;
const PING_PAYLOAD: &[u8] = b"silver-ping";
const ICMP_METADATA_COUNT: usize = 4;
const ICMP_PAYLOAD_SIZE: usize = 256;
/// How long a shell-requested echo waits for its reply before the server
/// reports a timeout to the shell.
const PING_TIMEOUT_MILLIS: u64 = 2000;

/// TCP receive window. HTTP responses are drained into the shared window every
/// poll, so this only has to smooth over one scheduling delay.
const TCP_RX_SIZE: usize = 32 * 1024;
const TCP_TX_SIZE: usize = 8192;

const DNS_QUERY_SLOTS: usize = 2;
/// Standalone `dns` lookups are bounded so a lost query cannot hang a client.
const DNS_TIMEOUT_MILLIS: i64 = 8000;

/// Largest host name, request path and request line we accept.
const HOST_MAX: usize = 128;
const PATH_MAX: usize = 384;
const REQUEST_MAX: usize = 1024;
/// A whole fetch (DNS + connect + transfer) is bounded from the request time.
const HTTP_TIMEOUT_MILLIS: i64 = 20000;

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
/// QEMU slirp's built-in resolver. DHCP normally supplies the same address.
const DEFAULT_DNS: [u8; 4] = [10, 0, 2, 3];
const DEFAULT_HTTP_PORT: u16 = 80;
const DEFAULT_HTTPS_PORT: u16 = 443;

const EXIT_NO_DMA: u64 = 1;
const EXIT_NO_ICMP: u64 = 2;

/// The frame buffers live in `.bss`, not on the 8 KiB userspace stack.
static mut DEVICE: FrameDevice = FrameDevice::new();
static mut TCP_RX_PAYLOAD: [u8; TCP_RX_SIZE] = [0; TCP_RX_SIZE];
static mut TCP_TX_PAYLOAD: [u8; TCP_TX_SIZE] = [0; TCP_TX_SIZE];
static mut FETCH: Fetch = Fetch::empty();

/// Small positive DNS cache. Repeated navigation (search result -> article ->
/// back) otherwise pays a UDP round trip per host; five minutes is short
/// enough for QEMU/desktop use and long enough to matter.
const DNS_CACHE_SLOTS: usize = 8;
const DNS_CACHE_TTL_MILLIS: i64 = 5 * 60 * 1000;

#[derive(Copy, Clone)]
struct DnsCacheEntry {
    host: [u8; HOST_MAX],
    host_len: usize,
    address: [u8; 4],
    expires_millis: i64,
}

impl DnsCacheEntry {
    const EMPTY: DnsCacheEntry = DnsCacheEntry {
        host: [0; HOST_MAX],
        host_len: 0,
        address: [0; 4],
        expires_millis: 0,
    };
}

static mut DNS_CACHE: [DnsCacheEntry; DNS_CACHE_SLOTS] = [DnsCacheEntry::EMPTY; DNS_CACHE_SLOTS];
static mut DNS_CACHE_NEXT: usize = 0;

fn dns_cache_lookup(host: &str, now_millis: i64) -> Option<[u8; 4]> {
    unsafe {
        let cache = &*core::ptr::addr_of!(DNS_CACHE);
        for entry in cache.iter() {
            if entry.host_len == host.len()
                && entry.expires_millis > now_millis
                && entry.host[..entry.host_len].eq_ignore_ascii_case(host.as_bytes())
            {
                return Some(entry.address);
            }
        }
    }
    None
}

fn dns_cache_store(host: &str, address: [u8; 4], now_millis: i64) {
    if host.is_empty() || host.len() >= HOST_MAX {
        return;
    }
    unsafe {
        let cache = &mut *core::ptr::addr_of_mut!(DNS_CACHE);
        let slot = *core::ptr::addr_of!(DNS_CACHE_NEXT) % DNS_CACHE_SLOTS;
        *core::ptr::addr_of_mut!(DNS_CACHE_NEXT) = slot + 1;
        cache[slot].host_len = host.len();
        cache[slot].host[..host.len()].copy_from_slice(host.as_bytes());
        cache[slot].address = address;
        cache[slot].expires_millis = now_millis.saturating_add(DNS_CACHE_TTL_MILLIS);
    }
}
static mut TLS_READ_BUFFER: [u8; 16640] = [0; 16640];
static mut TLS_WRITE_BUFFER: [u8; 16640] = [0; 16640];

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(driver_hint: u64) -> ! {
    syscall::print("NET SERVER STARTING\r\n");

    // Driver mailbox.
    let Some((dma_vaddr, dma_paddr)) = syscall::alloc_dma(net_ipc::DMA_SIZE) else {
        fail(EXIT_NO_DMA, "NET SERVER: DMA ALLOCATION FAILED\r\n");
    };
    write_u32(dma_vaddr, net_ipc::RX_LENGTH, 0);
    write_u32(dma_vaddr, net_ipc::TX_LENGTH, 0);

    // Client data window (URLs in, HTTP bodies out).
    let Some((client_vaddr, client_paddr)) = syscall::alloc_dma(net_ipc::CLIENT_SIZE) else {
        fail(EXIT_NO_DMA, "NET SERVER: CLIENT WINDOW FAILED\r\n");
    };
    write_u16(client_vaddr, net_ipc::CLIENT_REQUEST, 0);

    let device = unsafe { &mut *core::ptr::addr_of_mut!(DEVICE) };
    let fetch = unsafe { &mut *core::ptr::addr_of_mut!(FETCH) };

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(DEFAULT_MAC)));
    config.random_seed = syscall::uptime_ticks();
    let mut interface = Interface::new(config, device, Instant::from_millis(0));

    // Start from the static QEMU/slirp configuration so the stack works even
    // if DHCP is unavailable; a successful lease replaces it below.
    let mut local_ip = GUEST_IP;
    let mut prefix = GUEST_PREFIX;
    let mut gateway = GATEWAY_IP;
    let mut dns_server = DEFAULT_DNS;
    let mut dhcp_lease = false;
    apply_ipv4(
        &mut interface,
        local_ip,
        prefix,
        Some(gateway),
    );

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

    let dhcp_handle = sockets.add(dhcpv4::Socket::new());

    let mut dns_servers = [IpAddress::v4(
        DEFAULT_DNS[0],
        DEFAULT_DNS[1],
        DEFAULT_DNS[2],
        DEFAULT_DNS[3],
    ); 4];
    let mut dns_queries: [Option<dns::DnsQuery>; DNS_QUERY_SLOTS] = [None, None];
    let mut dns_socket = dns::Socket::new(&dns_servers[..1], &mut dns_queries[..]);
    let dns_handle = sockets.add(dns_socket);

    let tcp_rx_payload = unsafe { &mut *core::ptr::addr_of_mut!(TCP_RX_PAYLOAD) };
    let tcp_tx_payload = unsafe { &mut *core::ptr::addr_of_mut!(TCP_TX_PAYLOAD) };
    let tcp_socket = tcp::Socket::new(
        tcp::SocketBuffer::new(&mut tcp_rx_payload[..]),
        tcp::SocketBuffer::new(&mut tcp_tx_payload[..]),
    );
    let tcp_handle = sockets.add(tcp_socket);

    let capabilities = device.capabilities();

    let mut driver = resolve_driver(driver_hint);
    let mut attached = false;
    let mut next_attach = 0_u64;
    let mut now_millis = 0_i64;
    let mut next_seq = 0_u16;
    /// The echo request awaiting its reply, if any. One at a time: the ICMP
    /// socket is identified, so every pending request would match every reply.
    let mut pending_ping: Option<PendingPing> = None;
    /// Result waiting for the shell to accept it, if any.
    let mut ping_reply: Option<([u8; 4], bool)> = None;
    /// A standalone DNS lookup started by a client.
    let mut pending_dns: Option<PendingDns> = None;
    /// Replies the loop still has to hand to their clients.
    let mut outbox = Outbox::new();

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
        // Client requests ride the same drain.
        let mut message = ipc::Message::new(0, &[]);
        while syscall::receive_timeout(&mut message, 0).is_ok() {
            if Some(message.sender) == driver {
                handle_doorbell(&message, &mut interface, &mut attached);
                continue;
            }
            match message.payload[0] {
                net_ipc::OP_PING => {
                    if ping_reply.is_none() {
                        let address = net_ipc::ping_address(&message);
                        ping_reply = start_ping(
                            address,
                            attached,
                            &mut pending_ping,
                            &mut next_seq,
                            now_millis,
                            &mut sockets,
                            icmp_handle,
                            &capabilities.checksum,
                        );
                    }
                }
                net_ipc::OP_SHARE => {
                    outbox.push(
                        message.sender,
                        net_ipc::share_reply(client_paddr, net_ipc::CLIENT_SIZE),
                    );
                }
                net_ipc::OP_DNS => {
                    start_dns(
                        &mut message,
                        client_vaddr,
                        &mut interface,
                        &mut sockets,
                        dns_handle,
                        &mut pending_dns,
                        now_millis,
                        &mut outbox,
                    );
                }
                net_ipc::OP_HTTP => {
                    start_http(
                        &mut message,
                        client_vaddr,
                        &mut interface,
                        &mut sockets,
                        dns_handle,
                        fetch,
                        now_millis,
                        &mut outbox,
                    );
                }
                net_ipc::OP_IPCONFIG => {
                    outbox.push(
                        message.sender,
                        net_ipc::ipconfig_reply(
                            dhcp_lease,
                            local_ip,
                            prefix,
                            gateway,
                            dns_server,
                        ),
                    );
                }
                _ => {}
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

        let _ = interface.poll(now, device, &mut sockets);

        // DHCP: apply a lease as soon as one arrives; renewals keep polling.
        // Copy the lease out first so the socket borrow ends before the
        // interface and DNS socket are updated.
        let mut lease: Option<([u8; 4], u8, Option<[u8; 4]>)> = None;
        let mut lease_dns = [Ipv4Address::new(0, 0, 0, 0); 4];
        let mut lease_dns_count = 0usize;
        {
            let dhcp = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
            match dhcp.poll() {
                Some(dhcpv4::Event::Configured(config)) => {
                    lease = Some((
                        config.address.address().octets(),
                        config.address.prefix_len(),
                        config.router.map(|router| router.octets()),
                    ));
                    for server in config.dns_servers.iter() {
                        if lease_dns_count < lease_dns.len() {
                            lease_dns[lease_dns_count] = *server;
                            lease_dns_count += 1;
                        }
                    }
                }
                Some(dhcpv4::Event::Deconfigured) => {
                    // Keep the last usable configuration: losing a lease must
                    // not take the console offline mid-session.
                }
                None => {}
            }
        }
        if let Some((address, leased_prefix, router)) = lease {
            local_ip = address;
            prefix = leased_prefix;
            if let Some(router) = router {
                if usable_unicast(router) {
                    gateway = router;
                }
            }
            apply_ipv4(&mut interface, local_ip, prefix, Some(gateway));
            if lease_dns_count > 0 {
                dns_server = lease_dns[0].octets();
                for (slot, server) in dns_servers.iter_mut().zip(lease_dns[..lease_dns_count].iter()) {
                    *slot = IpAddress::v4(
                        server.octets()[0],
                        server.octets()[1],
                        server.octets()[2],
                        server.octets()[3],
                    );
                }
                sockets
                    .get_mut::<dns::Socket>(dns_handle)
                    .update_servers(&dns_servers[..lease_dns_count]);
            }
            dhcp_lease = true;
        }

        if let Some(source) = receive_pings(&mut sockets, icmp_handle) {
            if let Some(pending) = &pending_ping {
                if source == pending.address {
                    let address = pending.address;
                    pending_ping = None;
                    ping_reply = Some((address, true));
                }
            }
        }

        // A request that outlives its two-second window is answered with a
        // timeout even if the reply arrives right after: the shell gets a
        // bounded answer either way.
        if let Some(pending) = &pending_ping {
            if now_millis >= pending.deadline_millis {
                let address = pending.address;
                pending_ping = None;
                ping_reply = Some((address, false));
            }
        }

        // Store the ping result in the shared reply slot.
        if let Some((address, success)) = ping_reply {
            if let Some(shell) = resolve_task(syscall::PROGRAM_HOWL) {
                if syscall::try_send(shell, &net_ipc::ping_reply(address, success)) {
                    ping_reply = None;
                }
            } else {
                ping_reply = None;
            }
        }

        // Standalone DNS lookups.
        if let Some(request) = pending_dns {
            let result = {
                let socket = sockets.get_mut::<dns::Socket>(dns_handle);
                socket.get_query_result(request.handle)
            };
            match result {
                Ok(addresses) => {
                    let address = addresses
                        .iter()
                        .find_map(|address| match address {
                            IpAddress::Ipv4(v4) => Some(v4.octets()),
                            #[allow(unreachable_patterns)]
                            _ => None,
                        })
                        .unwrap_or([0; 4]);
                    let code = if address == [0; 4] {
                        net_ipc::NET_ERR_DNS
                    } else {
                        net_ipc::NET_OK
                    };
                    outbox.push(request.requester, net_ipc::dns_reply(code, address));
                    pending_dns = None;
                }
                Err(dns::GetQueryResultError::Pending) => {
                    if now_millis >= request.deadline_millis {
                        outbox.push(
                            request.requester,
                            net_ipc::dns_reply(net_ipc::NET_ERR_TIMEOUT, [0; 4]),
                        );
                        pending_dns = None;
                    }
                }
                Err(dns::GetQueryResultError::Failed) => {
                    outbox.push(
                        request.requester,
                        net_ipc::dns_reply(net_ipc::NET_ERR_DNS, [0; 4]),
                    );
                    pending_dns = None;
                }
            }
        }

        // HTTP fetch state machine.
        drive_fetch(
            fetch,
            &mut interface,
            device,
            &mut sockets,
            dns_handle,
            tcp_handle,
            client_vaddr,
            dma_vaddr,
            dma_paddr,
            driver,
            &mut now_millis,
            &mut outbox,
        );

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

        // One reply at a time; retry until the target accepts it. A dead
        // target is dropped so the queue cannot fill with zombies.
        if let Some((target, message)) = outbox.front() {
            if syscall::task_alive(target) {
                if syscall::try_send(target, &message) {
                    outbox.pop();
                }
            } else {
                outbox.pop();
            }
        }

        syscall::sleep(POLL_MILLIS);
    }
}

// ---------------------------------------------------------------------------
// DHCP / static configuration

/// Whether an address can be used as a next hop / interface address.
fn usable_unicast(address: [u8; 4]) -> bool {
    address != [0, 0, 0, 0] && address != [255, 255, 255, 255] && address[0] < 224
}

fn apply_ipv4(interface: &mut Interface, address: [u8; 4], prefix: u8, router: Option<[u8; 4]>) {
    if !usable_unicast(address) || prefix == 0 || prefix > 32 {
        return;
    }
    interface.update_ip_addrs(|addresses| {
        addresses.clear();
        let _ = addresses.push(IpCidr::new(
            IpAddress::v4(address[0], address[1], address[2], address[3]),
            prefix,
        ));
    });
    // A default route through 0.0.0.0 (an empty DHCP option) makes smoltcp ask
    // its neighbour cache about a non-unicast address, which asserts. Only a
    // real unicast next hop may become the default route.
    if let Some(router) = router {
        if usable_unicast(router) {
            let _ = interface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
                router[0], router[1], router[2], router[3],
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// DNS / HTTP requests

/// Small FIFO of reply messages waiting for their target's mailbox. Replies
/// for different clients must not overwrite one another when several requests
/// arrive in the same drain.
const OUTBOX_SLOTS: usize = 8;

struct Outbox {
    slots: [(u64, ipc::Message); OUTBOX_SLOTS],
    head: usize,
    tail: usize,
}

impl Outbox {
    const fn new() -> Self {
        Self {
            slots: [(0, ipc::Message { sender: 0, payload: [0; ipc::PAYLOAD_SIZE] }); OUTBOX_SLOTS],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, target: u64, message: ipc::Message) -> bool {
        let next = (self.tail + 1) % OUTBOX_SLOTS;
        if next == self.head {
            return false;
        }
        self.slots[self.tail] = (target, message);
        self.tail = next;
        true
    }

    fn front(&self) -> Option<(u64, ipc::Message)> {
        if self.head == self.tail {
            None
        } else {
            Some(self.slots[self.head])
        }
    }

    fn pop(&mut self) {
        if self.head != self.tail {
            self.head = (self.head + 1) % OUTBOX_SLOTS;
        }
    }
}

/// One standalone name lookup.
#[derive(Copy, Clone)]
struct PendingDns {
    requester: u64,
    handle: dns::QueryHandle,
    deadline_millis: i64,
}

fn start_dns(
    message: &mut ipc::Message,
    client_base: u64,
    interface: &mut Interface,
    sockets: &mut SocketSet<'_>,
    dns_handle: SocketHandle,
    pending: &mut Option<PendingDns>,
    now_millis: i64,
    outbox: &mut Outbox,
) {
    if pending.is_some() {
        outbox.push(message.sender, net_ipc::dns_reply(net_ipc::NET_ERR_BUSY, [0; 4]));
        return;
    }
    let Some(name) = read_request(client_base) else {
        outbox.push(message.sender, net_ipc::dns_reply(net_ipc::NET_ERR_URL, [0; 4]));
        return;
    };
    let socket = sockets.get_mut::<dns::Socket>(dns_handle);
    match socket.start_query(interface.context(), name, DnsQueryType::A) {
        Ok(handle) => {
            *pending = Some(PendingDns {
                requester: message.sender,
                handle,
                deadline_millis: now_millis + DNS_TIMEOUT_MILLIS,
            });
        }
        Err(_) => {
            outbox.push(message.sender, net_ipc::dns_reply(net_ipc::NET_ERR_DNS, [0; 4]));
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum FetchStage {
    Idle,
    Resolving,
    Connecting,
    Connected,
    Tls,
    Sending,
    Receiving,
}

/// One in-flight HTTP fetch. Only one runs at a time: a single TCP socket and
/// a single body window are reserved for it, which also makes the client
/// protocol deterministic.
struct Fetch {
    stage: FetchStage,
    requester: u64,
    request_id: u32,
    query: Option<dns::QueryHandle>,
    host: [u8; HOST_MAX],
    host_len: usize,
    path: [u8; PATH_MAX],
    path_len: usize,
    port: u16,
    remote: [u8; 4],
    request: [u8; REQUEST_MAX],
    request_len: usize,
    sent: usize,
    received: usize,
    tls: bool,
    started_millis: i64,
    connect_started: bool,
    local_port: u16,
}

impl Fetch {
    const fn empty() -> Self {
        Self {
            stage: FetchStage::Idle,
            requester: 0,
            request_id: 0,
            query: None,
            host: [0; HOST_MAX],
            host_len: 0,
            path: [0; PATH_MAX],
            path_len: 0,
            port: 0,
            remote: [0; 4],
            request: [0; REQUEST_MAX],
            request_len: 0,
            sent: 0,
            received: 0,
            tls: false,
            started_millis: 0,
            connect_started: false,
            local_port: 0,
        }
    }
}

fn start_http(
    message: &mut ipc::Message,
    client_base: u64,
    interface: &mut Interface,
    sockets: &mut SocketSet<'_>,
    dns_handle: SocketHandle,
    fetch: &mut Fetch,
    now_millis: i64,
    outbox: &mut Outbox,
) {
    let request_id = net_ipc::http_request_fields(message);
    if fetch.stage != FetchStage::Idle {
        if fetch.requester != message.sender {
            outbox.push(
                message.sender,
                net_ipc::http_reply_id(request_id, net_ipc::NET_ERR_BUSY, 0, 0),
            );
            return;
        }
        // The same client abandoned its previous request (a browser
        // navigation replaces the old page). Drop the old transaction; the
        // Connecting stage aborts the stale TCP socket.
        fetch.stage = FetchStage::Idle;
        fetch.query = None;
    }
    let Some(url) = read_request(client_base) else {
        outbox.push(
            message.sender,
            net_ipc::http_reply_id(request_id, net_ipc::NET_ERR_URL, 0, 0),
        );
        return;
    };
    let Some(parsed) = parse_url(url) else {
        outbox.push(
            message.sender,
            net_ipc::http_reply_id(request_id, net_ipc::NET_ERR_URL, 0, 0),
        );
        return;
    };
    if parsed.host.len() > HOST_MAX - 1 || parsed.path.len() > PATH_MAX - 1 {
        outbox.push(
            message.sender,
            net_ipc::http_reply_id(request_id, net_ipc::NET_ERR_URL, 0, 0),
        );
        return;
    }

    fetch.host[..parsed.host.len()].copy_from_slice(parsed.host.as_bytes());
    fetch.host_len = parsed.host.len();
    fetch.path[..parsed.path.len()].copy_from_slice(parsed.path.as_bytes());
    fetch.path_len = parsed.path.len();
    fetch.port = parsed.port;
    fetch.tls = parsed.tls;
    fetch.requester = message.sender;
    fetch.request_id = request_id;
    fetch.sent = 0;
    fetch.received = 0;
    fetch.started_millis = now_millis;
    fetch.query = None;
    fetch.connect_started = false;
    // smoltcp refuses `connect` with local port 0. Pick an ephemeral port in
    // the IANA range; one fetch at a time means a simple rotation is enough.
    fetch.local_port = 49152 + (now_millis as u16 % 16384);

    if let Some(address) = parse_ipv4(parsed.host) {
        fetch.remote = address;
        fetch.stage = FetchStage::Connecting;
    } else if let Some(address) = dns_cache_lookup(parsed.host, now_millis) {
        fetch.remote = address;
        fetch.stage = FetchStage::Connecting;
    } else {
        // Resolve through the DNS socket; `drive_fetch` picks up the result.
        let handle = {
            let socket = sockets.get_mut::<dns::Socket>(dns_handle);
            socket.start_query(interface.context(), parsed.host, DnsQueryType::A)
        };
        match handle {
            Ok(handle) => {
                fetch.query = Some(handle);
                fetch.stage = FetchStage::Resolving;
            }
            Err(_) => {
                fetch.stage = FetchStage::Idle;
                outbox.push(
                    message.sender,
                    net_ipc::http_reply_id(request_id, net_ipc::NET_ERR_BUSY, 0, 0),
                );
            }
        }
    }
}

/// Blocking `embedded-io` adapter over the smoltcp socket. It pumps the
/// interface and the virtio-net mailbox while TLS waits for a record, so the
/// handshake cannot stall behind the normal polling loop.
struct TlsTransport<'a, 'b> {
    interface: *mut Interface,
    device: *mut FrameDevice,
    sockets: *mut SocketSet<'a>,
    handle: SocketHandle,
    dma_vaddr: u64,
    dma_paddr: u64,
    driver: Option<u64>,
    now: &'b mut i64,
    deadline: i64,
    spin: u32,
}

impl TlsTransport<'_, '_> {
    fn pump(&mut self) {
        unsafe {
            let length = read_u32(self.dma_vaddr, net_ipc::RX_LENGTH) as usize;
            if length > 0 {
                let length = length.min(net_ipc::MAX_FRAME);
                net_ipc::fence();
                (*self.device).read_dma_frame(self.dma_vaddr, net_ipc::RX_FRAME, length);
                net_ipc::fence();
                write_u32(self.dma_vaddr, net_ipc::RX_LENGTH, 0);
            }
            let now = Instant::from_millis(*self.now);
            (*self.interface).poll(now, &mut *self.device, &mut *self.sockets);
            let device = &mut *self.device;
            let tx_length = device.tx_len;
            if tx_length > 0 && read_u32(self.dma_vaddr, net_ipc::TX_LENGTH) == 0 {
                write_dma_frame(
                    self.dma_vaddr,
                    net_ipc::TX_FRAME,
                    &device.tx[..tx_length],
                );
                net_ipc::fence();
                write_u32(self.dma_vaddr, net_ipc::TX_LENGTH, tx_length as u32);
                if let Some(driver) = self.driver {
                    let _ = syscall::try_send(
                        driver,
                        &net_ipc::tx_doorbell(tx_length as u16),
                    );
                }
                device.tx_len = 0;
            }
        }
        // The kernel timer is 100 Hz, so sleeping every poll would cap the
        // TLS stream at a few KiB per tick. Spin and yield only occasionally.
        if self.spin % 16 == 0 {
            syscall::sleep(1);
        }
        self.spin = self.spin.wrapping_add(1);
        *self.now += 1;
    }
}

impl embedded_io::ErrorType for TlsTransport<'_, '_> {
    type Error = embedded_io::ErrorKind;
}

impl embedded_io::Read for TlsTransport<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        loop {
            {
                let socket =
                    unsafe { (&mut *self.sockets).get_mut::<tcp::Socket>(self.handle) };
                if socket.can_recv() {
                    match socket.recv_slice(buffer) {
                        Ok(count) if count > 0 => return Ok(count),
                        Ok(_) => {}
                        Err(_) => return Err(embedded_io::ErrorKind::BrokenPipe),
                    }
                } else if matches!(
                    socket.state(),
                    tcp::State::Closed | tcp::State::TimeWait
                ) {
                    return Err(embedded_io::ErrorKind::BrokenPipe);
                }
            }
            if *self.now >= self.deadline {
                return Err(embedded_io::ErrorKind::TimedOut);
            }
            self.pump();
        }
    }
}

impl embedded_io::Write for TlsTransport<'_, '_> {
    fn write(&mut self, buffer: &[u8]) -> Result<usize, Self::Error> {
        loop {
            {
                let socket =
                    unsafe { (&mut *self.sockets).get_mut::<tcp::Socket>(self.handle) };
                if socket.may_send() {
                    match socket.send_slice(buffer) {
                        Ok(count) if count > 0 => return Ok(count),
                        Ok(_) => {}
                        Err(_) => return Err(embedded_io::ErrorKind::BrokenPipe),
                    }
                }
                if matches!(socket.state(), tcp::State::Closed | tcp::State::TimeWait) {
                    return Err(embedded_io::ErrorKind::BrokenPipe);
                }
            }
            if *self.now >= self.deadline {
                return Err(embedded_io::ErrorKind::TimedOut);
            }
            self.pump();
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tls_fetch(
    fetch: &mut Fetch,
    interface: &mut Interface,
    device: &mut FrameDevice,
    sockets: &mut SocketSet<'_>,
    tcp_handle: SocketHandle,
    client_base: u64,
    dma_vaddr: u64,
    dma_paddr: u64,
    driver: Option<u64>,
    now_millis: &mut i64,
    outbox: &mut Outbox,
) {
    build_request(fetch);
    let host = core::str::from_utf8(&fetch.host[..fetch.host_len]).unwrap_or("");
    let read_buffer = unsafe { &mut *core::ptr::addr_of_mut!(TLS_READ_BUFFER) };
    let write_buffer = unsafe { &mut *core::ptr::addr_of_mut!(TLS_WRITE_BUFFER) };
    let response = unsafe {
        core::slice::from_raw_parts_mut(
            (client_base + net_ipc::CLIENT_BODY) as *mut u8,
            net_ipc::CLIENT_BODY_MAX,
        )
    };
    let deadline = *now_millis + HTTP_TIMEOUT_MILLIS;
    let transport = TlsTransport {
        interface: interface as *mut Interface,
        device: device as *mut FrameDevice,
        sockets: sockets as *mut SocketSet<'_>,
        handle: tcp_handle,
        dma_vaddr,
        dma_paddr,
        driver,
        deadline,
        now: &mut *now_millis,
        spin: 0,
    };
    let result = tls_client::fetch(
        transport,
        host,
        &fetch.request[..fetch.request_len],
        read_buffer,
        write_buffer,
        response,
    );
    {
        let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.state() != tcp::State::Closed {
            socket.abort();
        }
    }
    match result {
        Ok(length) if length > 0 => {
            fetch.received = length.min(net_ipc::CLIENT_BODY_MAX);
            finish_fetch(fetch, client_base, outbox);
        }
        _ => fail_fetch(fetch, net_ipc::NET_ERR_TLS, 0, outbox),
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_fetch(
    fetch: &mut Fetch,
    interface: &mut Interface,
    device: &mut FrameDevice,
    sockets: &mut SocketSet<'_>,
    dns_handle: SocketHandle,
    tcp_handle: SocketHandle,
    client_base: u64,
    dma_vaddr: u64,
    dma_paddr: u64,
    driver: Option<u64>,
    now_millis: &mut i64,
    outbox: &mut Outbox,
) {
    if fetch.stage == FetchStage::Idle {
        return;
    }
    if *now_millis - fetch.started_millis > HTTP_TIMEOUT_MILLIS {
        let stage = fetch.stage as u16;
        fail_fetch(fetch, net_ipc::NET_ERR_TIMEOUT, stage, outbox);
        return;
    }

    match fetch.stage {
        FetchStage::Resolving => {
            let Some(query) = fetch.query else {
                fail_fetch(fetch, net_ipc::NET_ERR_DNS, 0, outbox);
                return;
            };
            let result = {
                let socket = sockets.get_mut::<dns::Socket>(dns_handle);
                socket.get_query_result(query)
            };
            match result {
                Ok(addresses) => {
                    fetch.query = None;
                    match addresses.iter().find_map(|address| match address {
                        IpAddress::Ipv4(v4) => Some(v4.octets()),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    }) {
                        Some(address) => {
                            let mut host = [0_u8; HOST_MAX];
                            let length = fetch.host_len.min(HOST_MAX);
                            host[..length].copy_from_slice(&fetch.host[..length]);
                            if let Ok(host) = core::str::from_utf8(&host[..length]) {
                                dns_cache_store(host, address, *now_millis);
                            }
                            fetch.remote = address;
                            fetch.stage = FetchStage::Connecting;
                        }
                        None => fail_fetch(fetch, net_ipc::NET_ERR_RESPONSE, 0, outbox),
                    }
                }
                Err(dns::GetQueryResultError::Pending) => {}
                Err(dns::GetQueryResultError::Failed) => {
                    fail_fetch(fetch, net_ipc::NET_ERR_DNS, 0, outbox)
                }
            }
        }
        FetchStage::Connecting => {
            if !fetch.connect_started {
                let remote = IpEndpoint {
                    addr: IpAddress::v4(
                        fetch.remote[0],
                        fetch.remote[1],
                        fetch.remote[2],
                        fetch.remote[3],
                    ),
                    port: fetch.port,
                };
                let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
                // A previous fetch can still sit in TIME-WAIT (the peer's FIN
                // arrived but the last ACK has not been reaped). `connect`
                // refuses an open socket, so drop the old connection first.
                if socket.state() != tcp::State::Closed {
                    socket.abort();
                }
                if socket
                    .connect(interface.context(), remote, fetch.local_port)
                    .is_err()
                {
                    fail_fetch(fetch, net_ipc::NET_ERR_CONNECT, 0, outbox);
                    return;
                }
                fetch.connect_started = true;
            } else {
                let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
                match socket.state() {
                    tcp::State::Established => {
                        fetch.stage = if fetch.tls {
                            FetchStage::Tls
                        } else {
                            FetchStage::Connected
                        }
                    }
                    tcp::State::Closed | tcp::State::TimeWait => {
                        fail_fetch(fetch, net_ipc::NET_ERR_CONNECT, 0, outbox);
                    }
                    _ => {}
                }
            }
        }
        FetchStage::Tls => {
            run_tls_fetch(
                fetch,
                interface,
                device,
                sockets,
                tcp_handle,
                client_base,
                dma_vaddr,
                dma_paddr,
                driver,
                now_millis,
                outbox,
            );
        }
        FetchStage::Connected => {
            build_request(fetch);
            fetch.stage = FetchStage::Sending;
        }
        FetchStage::Sending => {
            let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            while fetch.sent < fetch.request_len && socket.may_send() {
                match socket.send_slice(&fetch.request[fetch.sent..fetch.request_len]) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => fetch.sent += count,
                }
            }
            if fetch.sent >= fetch.request_len {
                fetch.stage = FetchStage::Receiving;
            } else if socket.state() == tcp::State::Closed {
                fail_fetch(fetch, net_ipc::NET_ERR_CONNECT, 0, outbox);
            }
        }
        FetchStage::Receiving => {
            let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            let mut progress = true;
            while progress && fetch.received < net_ipc::CLIENT_BODY_MAX {
                let available = net_ipc::CLIENT_BODY_MAX - fetch.received;
                let slice = unsafe {
                    core::slice::from_raw_parts_mut(
                        (client_base + net_ipc::CLIENT_BODY) as *mut u8,
                        net_ipc::CLIENT_BODY_MAX,
                    )
                };
                match socket.recv_slice(&mut slice[fetch.received..fetch.received + available]) {
                    Ok(0) => progress = false,
                    Ok(count) => fetch.received += count,
                    Err(_) => progress = false,
                }
            }
            let finished = matches!(
                socket.state(),
                tcp::State::Closed | tcp::State::TimeWait
            ) || (matches!(socket.state(), tcp::State::CloseWait) && !socket.may_recv())
                || fetch.received >= net_ipc::CLIENT_BODY_MAX;
            if finished {
                finish_fetch(fetch, client_base, outbox);
            }
        }
        FetchStage::Idle => {}
    }
}

fn fail_fetch(fetch: &mut Fetch, code: u8, stage: u16, outbox: &mut Outbox) {
    let requester = fetch.requester;
    let request_id = fetch.request_id;
    fetch.stage = FetchStage::Idle;
    fetch.query = None;
    outbox.push(requester, net_ipc::http_reply_id(request_id, code, stage, 0));
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn finish_fetch(fetch: &mut Fetch, client_base: u64, outbox: &mut Outbox) {
    let buffer = unsafe {
        core::slice::from_raw_parts_mut(
            (client_base + net_ipc::CLIENT_BODY) as *mut u8,
            net_ipc::CLIENT_BODY_MAX,
        )
    };
    let (code, status, body_len) = parse_response(buffer, fetch.received);
    let requester = fetch.requester;
    let request_id = fetch.request_id;
    // Redirects are the client's decision: hand it the Location header and
    // let the browser resolve it (a target may need the host's HTTPS path).
    if code == net_ipc::NET_OK && is_redirect(status) {
        if let Some(header_end) = find_header_end(&buffer[..fetch.received]) {
            if let Some(location) = find_header(&buffer[..header_end], b"location") {
                let offset = (location.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize);
                let length = location.len().min(net_ipc::CLIENT_BODY_MAX);
                unsafe {
                    core::ptr::copy(
                        buffer.as_ptr().add(offset),
                        buffer.as_mut_ptr(),
                        length,
                    );
                }
                fetch.stage = FetchStage::Idle;
                fetch.query = None;
                outbox.push(
                    requester,
                    net_ipc::http_reply_id(
                        request_id,
                        net_ipc::NET_REDIRECT,
                        status,
                        length as u32,
                    ),
                );
                return;
            }
        }
    }
    // Move the body to the start of the body window: clients read it from
    // `CLIENT_BODY`, not from after the header.
    if code == net_ipc::NET_OK {
        let header_end = find_header_end(&buffer[..fetch.received]).unwrap_or(0) + 4;
        if header_end <= fetch.received {
            let body = &mut buffer[header_end..fetch.received];
            unsafe {
                core::ptr::copy(body.as_ptr(), buffer.as_mut_ptr(), body_len);
            }
        }
    }
    fetch.stage = FetchStage::Idle;
    outbox.push(
        requester,
        net_ipc::http_reply_id(request_id, code, status, body_len as u32),
    );
}

// ---------------------------------------------------------------------------
// HTTP parsing

struct ParsedUrl<'a> {
    tls: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Byte search helpers. `core::str::find`/`rfind` with a `char` pattern
/// misbehaves on this bare-metal target when the haystack lives in the
/// non-cacheable shared window (the vectorised char search reports no match
/// for a byte that is clearly present), so the network parser scans bytes.
fn find_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|&byte| byte == needle)
}

fn rfind_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().rposition(|&byte| byte == needle)
}

fn parse_url(url: &str) -> Option<ParsedUrl<'_>> {
    let bytes = url.as_bytes();
    let (tls, offset) = if bytes.starts_with(b"http://") {
        (false, 7)
    } else if bytes.starts_with(b"https://") {
        (true, 8)
    } else {
        (false, 0)
    };
    if url.len() <= offset {
        return None;
    }
    let rest = &url[offset..];
    let (host_port, path) = match find_byte(rest.as_bytes(), b'/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if host_port.is_empty() {
        return None;
    }
    let (host, port) = match rfind_byte(host_port.as_bytes(), b':') {
        Some(index) if index + 1 < host_port.len() => {
            let port = host_port[index + 1..].parse::<u16>().ok()?;
            (&host_port[..index], port)
        }
        _ => (host_port, if tls { DEFAULT_HTTPS_PORT } else { DEFAULT_HTTP_PORT }),
    };
    if host.is_empty() {
        return None;
    }
    Some(ParsedUrl {
        tls,
        host,
        port,
        path,
    })
}

fn build_request(fetch: &mut Fetch) {
    let mut length = 0usize;
    let pieces: [&[u8]; 8] = [
        b"GET ",
        &fetch.path[..fetch.path_len],
        b" HTTP/1.0\r\nHost: ",
        &fetch.host[..fetch.host_len],
        b"\r\nUser-Agent: WerewolfOS/0.1\r\nAccept: text/html,text/plain,*/*\r\n\
          Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
        b"",
        b"",
        b"",
    ];
    for piece in pieces {
        let count = piece.len().min(fetch.request.len() - length);
        fetch.request[length..length + count].copy_from_slice(&piece[..count]);
        length += count;
    }
    fetch.request_len = length;
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Finds a header value (case-insensitive) after the status line.
fn find_header<'a>(headers: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for line in headers.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() > name.len()
            && line[..name.len()].eq_ignore_ascii_case(name)
            && line[name.len()] == b':'
        {
            let value = &line[name.len() + 1..];
            let start = value.iter().position(|byte| !byte.is_ascii_whitespace())?;
            return Some(&value[start..]);
        }
    }
    None
}

fn parse_response(buffer: &mut [u8], received: usize) -> (u8, u16, usize) {
    let Some(end) = find_header_end(&buffer[..received]) else {
        return (net_ipc::NET_ERR_RESPONSE, 0, 0);
    };
    let headers = &buffer[..end];
    let body_start = end + 4;

    // "HTTP/1.1 200 OK"
    let status = headers
        .split(|&byte| byte == b' ')
        .nth(1)
        .and_then(|digits| core::str::from_utf8(digits).ok())
        .and_then(|digits| digits.trim().parse::<u16>().ok())
        .unwrap_or(0);

    if let Some(value) = find_header(headers, b"transfer-encoding") {
        if value.eq_ignore_ascii_case(b"chunked") {
            let length = dechunk(&mut buffer[body_start..received]);
            return (net_ipc::NET_OK, status, length);
        }
    }
    if let Some(value) = find_header(headers, b"content-length") {
        if let Ok(text) = core::str::from_utf8(value) {
            if let Ok(length) = text.trim().parse::<usize>() {
                let available = received - body_start;
                return (net_ipc::NET_OK, status, length.min(available));
            }
        }
    }
    (net_ipc::NET_OK, status, received - body_start)
}

/// Decodes a chunked body in place. Returns the decoded length.
fn dechunk(buffer: &mut [u8]) -> usize {
    let mut input = 0usize;
    let mut output = 0usize;
    while input < buffer.len() {
        // Parse the hex chunk size.
        let mut size = 0usize;
        let mut digits = 0usize;
        while input < buffer.len() {
            let byte = buffer[input];
            let digit = match byte {
                b'0'..=b'9' => Some(usize::from(byte - b'0')),
                b'a'..=b'f' => Some(usize::from(byte - b'a') + 10),
                b'A'..=b'F' => Some(usize::from(byte - b'A') + 10),
                _ => None,
            };
            let Some(digit) = digit else { break };
            size = size.saturating_mul(16).saturating_add(digit);
            digits += 1;
            input += 1;
        }
        if digits == 0 {
            break;
        }
        // Skip the CRLF after the size.
        while input < buffer.len() && buffer[input] != b'\n' {
            input += 1;
        }
        input = (input + 1).min(buffer.len());
        if size == 0 {
            break;
        }
        let count = size.min(buffer.len() - input).min(buffer.len() - output);
        buffer.copy_within(input..input + count, output);
        input += size;
        output += count;
        // Skip the CRLF after the chunk data.
        while input < buffer.len() && buffer[input] != b'\n' {
            input += 1;
        }
        input = (input + 1).min(buffer.len());
    }
    output
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let bytes = text.as_bytes();
    let mut address = [0_u8; 4];
    let mut octet = 0usize;
    let mut value = 0u16;
    let mut digits = 0usize;
    for &byte in bytes {
        if byte == b'.' {
            if digits == 0 || digits > 3 || value > 255 || octet >= 3 {
                return None;
            }
            address[octet] = value as u8;
            octet += 1;
            value = 0;
            digits = 0;
        } else if byte.is_ascii_digit() {
            if digits == 3 {
                return None;
            }
            value = value * 10 + u16::from(byte - b'0');
            digits += 1;
        } else {
            return None;
        }
    }
    if digits == 0 || digits > 3 || value > 255 || octet != 3 {
        return None;
    }
    address[3] = value as u8;
    Some(address)
}

/// Reads the length-prefixed request string from the shared window.
fn read_request(client_base: u64) -> Option<&'static str> {
    let length = unsafe { core::ptr::read_volatile(client_base as *const u16) } as usize;
    if length == 0 || length > net_ipc::CLIENT_REQUEST_MAX {
        return None;
    }
    let bytes = unsafe {
        core::slice::from_raw_parts((client_base + net_ipc::CLIENT_REQUEST + 2) as *const u8, length)
    };
    core::str::from_utf8(bytes).ok()
}

// ---------------------------------------------------------------------------
// Ping

/// One shell-requested echo in flight.
struct PendingPing {
    address: [u8; 4],
    deadline_millis: i64,
}

/// Starts a shell-requested ping. `Some` becomes the immediate error reply
/// when the request cannot even be queued (driver not attached, another ping
/// still in flight, or the socket rejected the packet); `None` means the
/// request is now pending and the reply arrives within two seconds either way.
#[allow(clippy::too_many_arguments)]
fn start_ping(
    address: [u8; 4],
    attached: bool,
    pending_ping: &mut Option<PendingPing>,
    next_seq: &mut u16,
    now_millis: i64,
    sockets: &mut SocketSet<'_>,
    handle: SocketHandle,
    checksum: &ChecksumCapabilities,
) -> Option<([u8; 4], bool)> {
    if !attached || pending_ping.is_some() {
        return Some((address, false));
    }
    // The socket buffers the request, so a missing ARP entry does not fail
    // here: smoltcp resolves the neighbour at poll time and emits the packet
    // once the neighbour is usable.
    if !send_ping(sockets, handle, checksum, next_seq.wrapping_add(1), address) {
        return Some((address, false));
    }
    *next_seq = next_seq.wrapping_add(1);
    *pending_ping = Some(PendingPing {
        address,
        deadline_millis: now_millis + PING_TIMEOUT_MILLIS as i64,
    });
    None
}

/// Queues an ICMPv4 echo request. The socket owns the bytes; `emit` fills in
/// the type/code/identifier/sequence header and the checksum.
fn send_ping(
    sockets: &mut SocketSet<'_>,
    handle: SocketHandle,
    checksum: &ChecksumCapabilities,
    seq_no: u16,
    address: [u8; 4],
) -> bool {
    let repr = Icmpv4Repr::EchoRequest {
        ident: PING_IDENT,
        seq_no,
        data: PING_PAYLOAD,
    };
    let destination = IpAddress::v4(address[0], address[1], address[2], address[3]);
    let socket = sockets.get_mut::<icmp::Socket>(handle);
    let Ok(buffer) = socket.send(repr.buffer_len(), destination) else {
        return false;
    };
    let mut packet = Icmpv4Packet::new_unchecked(buffer);
    repr.emit(&mut packet, checksum);
    true
}

/// Drains every echo reply smoltcp has delivered to the ICMP socket. Returns
/// the octets of the last consumed reply's source, if any.
fn receive_pings(sockets: &mut SocketSet<'_>, handle: SocketHandle) -> Option<[u8; 4]> {
    let mut payload = [0_u8; ICMP_PAYLOAD_SIZE];
    let mut last_source = None;
    loop {
        let socket = sockets.get_mut::<icmp::Socket>(handle);
        let Ok((length, source)) = socket.recv_slice(&mut payload) else {
            return last_source;
        };
        // Only `proto-ipv4` is enabled, so `IpAddress` has no other variant.
        let IpAddress::Ipv4(address) = source;
        last_source = Some(address.octets());
        syscall::print("NET SERVER: ICMP REPLY ");
        syscall::print_dec_u64(length as u64);
        syscall::print(" BYTES FROM ");
        print_ipv4(source);
        syscall::print("\r\n");
    }
}

fn print_ipv4(address: IpAddress) {
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

fn write_u16(base: u64, offset: u64, value: u16) {
    unsafe { core::ptr::write_volatile(base.wrapping_add(offset) as *mut u16, value) };
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
    // Surface a panic to the kernel's task-exit line: a `wfe` loop keeps the
    // task looking alive, which makes a stuck network service indistinguishable
    // from a busy one.
    // The kernel's task-exit line carries the panic's source line, so an
    // intermittent fault can be located from the serial log alone.
    // A `wfe` loop would keep the task looking alive forever, so a panic
    // always exits. The status names the source line (`0xDEAD_0000 | line`),
    // which the kernel prints in the task-exit line on the serial console.
    let line = _info
        .location()
        .map(|location| location.line() as u64)
        .unwrap_or(0);
    syscall::exit_with_status(0xDEAD_0000 | line)
}
