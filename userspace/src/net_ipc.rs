//! Frame mailbox shared by `net_driver` and `net_server`.
//!
//! An Ethernet frame does not fit in the 32-byte [`ipc::Message`], so the bytes
//! travel through one DMA window and IPC only carries doorbells. The window is
//! allocated by `net_server` and announced to the driver with [`OP_ATTACH`];
//! the driver maps it with `map_phys`.
//!
//! Layout (all offsets from the start of the window):
//!
//! | offset | contents                                            |
//! |--------|-----------------------------------------------------|
//! | 0x000  | `u32` RX length, written by the driver             |
//! | 0x004  | `u32` TX length, written by the server             |
//! | 0x800  | `MAX_FRAME` bytes, driver -> server                |
//! | 0x1000 | `MAX_FRAME` bytes, server -> driver                |
//!
//! There is exactly one slot per direction. The producer checks that the
//! length word is zero before writing a frame, copies the bytes, then publishes
//! the new length. The consumer reads the length, copies the frame, then
//! clears the length. Both sides issue a memory barrier around the length word
//! so that payload bytes are visible before the length is.

use crate::ipc;

/// Largest Ethernet frame without the FCS.
pub const MAX_FRAME: usize = 1536;

/// Two 4 KiB pages are enough for the headers and both frame slots.
pub const DMA_SIZE: u64 = 8192;

pub const RX_LENGTH: u64 = 0x000;
pub const TX_LENGTH: u64 = 0x004;
pub const RX_FRAME: u64 = 0x800;
pub const TX_FRAME: u64 = 0x1000;

/// Message opcodes, stored in `payload[0]`.
pub const OP_ATTACH: u8 = 1;
pub const OP_ATTACHED: u8 = 2;
pub const OP_FRAME: u8 = 3;
pub const OP_TX: u8 = 4;
/// Shell -> server: `payload[1..5]` is the IPv4 address to ping.
pub const OP_PING: u8 = 5;
/// Server -> shell: `payload[1]` is 1 on success and 0 on timeout/error,
/// `payload[2..6]` echoes the address the request asked about.
pub const OP_PING_REPLY: u8 = 6;

// ---------------------------------------------------------------------------
// Client data window
//
// A URL or an HTTP body does not fit in the 32-byte mailbox, so clients share
// one DMA window with the server:
//
// | offset | contents                                                     |
// |--------|--------------------------------------------------------------|
// | 0x0000 | `u16` request length (LE), then the request bytes (URL/name) |
// | 0x4000 | response body written by the server                          |
//
// The client asks for the window with `OP_SHARE`, maps it through
// `map_phys`, writes its request and only then sends the opcode. The server
// answers with a small `OP_*_REPLY` message; the payload stays in the window.

/// Size of the shared client window. 128 KiB keeps a typical text page whole.
pub const CLIENT_SIZE: u64 = 128 * 1024;
/// Where the request (length-prefixed) starts.
pub const CLIENT_REQUEST: u64 = 0x0000;
/// Where the server writes the HTTP body.
pub const CLIENT_BODY: u64 = 0x4000;
/// Maximum request bytes we accept (host + path).
pub const CLIENT_REQUEST_MAX: usize = CLIENT_BODY as usize - 2;
/// Maximum body bytes the window can carry.
pub const CLIENT_BODY_MAX: usize = CLIENT_SIZE as usize - CLIENT_BODY as usize;

/// Client -> server: "map my buffer, tell me its physical address".
pub const OP_SHARE: u8 = 7;
/// Server -> client: `payload[1..9]` physical address, `payload[9..13]` size.
pub const OP_SHARE_REPLY: u8 = 8;
/// Client -> server: resolve the name in the window request slot.
pub const OP_DNS: u8 = 9;
/// Server -> client: `payload[1]` result code, `payload[2..6]` IPv4 address.
pub const OP_DNS_REPLY: u8 = 10;
/// Client -> server: fetch the `http://` URL in the window request slot.
pub const OP_HTTP: u8 = 11;
/// Server -> client: `payload[1]` result code, `payload[2..4]` HTTP status,
/// `payload[4..8]` body length; the body is at `CLIENT_BODY`.
pub const OP_HTTP_REPLY: u8 = 12;
/// Client -> server: report the current IPv4 configuration.
pub const OP_IPCONFIG: u8 = 13;
/// Server -> client: `payload[1]` configured flag, `payload[2..6]` address,
/// `payload[6]` prefix, `payload[7..11]` gateway, `payload[11..15]` DNS.
pub const OP_IPCONFIG_REPLY: u8 = 14;

// Result codes carried in the reply payload.
pub const NET_OK: u8 = 0;
pub const NET_ERR_DNS: u8 = 1;
pub const NET_ERR_CONNECT: u8 = 2;
pub const NET_ERR_TIMEOUT: u8 = 3;
pub const NET_ERR_URL: u8 = 4;
pub const NET_ERR_TLS: u8 = 5;
pub const NET_ERR_RESPONSE: u8 = 6;
pub const NET_ERR_BUSY: u8 = 7;

/// Shell -> server: one echo request to `address`.
pub fn ping(address: [u8; 4]) -> ipc::Message {
    let mut payload = [0_u8; 5];
    payload[0] = OP_PING;
    payload[1..5].copy_from_slice(&address);
    ipc::Message::new(0, &payload)
}

/// Server -> shell: the outcome of one `ping`.
pub fn ping_reply(address: [u8; 4], success: bool) -> ipc::Message {
    let mut payload = [0_u8; 6];
    payload[0] = OP_PING_REPLY;
    payload[1] = u8::from(success);
    payload[2..6].copy_from_slice(&address);
    ipc::Message::new(0, &payload)
}

/// Shell -> server: the address carried by an `OP_PING` request.
pub fn ping_address(message: &ipc::Message) -> [u8; 4] {
    [message.payload[1], message.payload[2], message.payload[3], message.payload[4]]
}

/// Server -> shell: decodes an `OP_PING_REPLY` into (address, success).
pub fn ping_reply_fields(message: &ipc::Message) -> Option<([u8; 4], bool)> {
    if message.payload[0] != OP_PING_REPLY {
        return None;
    }
    let address = [message.payload[2], message.payload[3], message.payload[4], message.payload[5]];
    Some((address, message.payload[1] == 1))
}

/// Server -> driver: the mailbox window and its size.
pub fn attach(dma_paddr: u64, dma_size: u64) -> ipc::Message {
    let mut payload = [0_u8; 13];
    payload[0] = OP_ATTACH;
    payload[1..9].copy_from_slice(&dma_paddr.to_le_bytes());
    payload[9..13].copy_from_slice(&(dma_size as u32).to_le_bytes());
    ipc::Message::new(0, &payload)
}

/// Driver -> server: mailbox accepted, carrying the NIC's hardware address.
pub fn attached(mac: &[u8; 6]) -> ipc::Message {
    let mut payload = [0_u8; 7];
    payload[0] = OP_ATTACHED;
    payload[1..7].copy_from_slice(mac);
    ipc::Message::new(0, &payload)
}

/// Driver -> server: a frame is waiting in the RX slot.
pub fn frame_doorbell(length: u16) -> ipc::Message {
    length_doorbell(OP_FRAME, length)
}

/// Server -> driver: a frame is waiting in the TX slot.
pub fn tx_doorbell(length: u16) -> ipc::Message {
    length_doorbell(OP_TX, length)
}

fn length_doorbell(opcode: u8, length: u16) -> ipc::Message {
    let mut payload = [0_u8; 3];
    payload[0] = opcode;
    payload[1..3].copy_from_slice(&length.to_le_bytes());
    ipc::Message::new(0, &payload)
}

pub fn attach_paddr(message: &ipc::Message) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&message.payload[1..9]);
    u64::from_le_bytes(bytes)
}

pub fn attach_size(message: &ipc::Message) -> u64 {
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&message.payload[9..13]);
    u32::from_le_bytes(bytes) as u64
}

pub fn doorbell_length(message: &ipc::Message) -> u16 {
    u16::from_le_bytes([message.payload[1], message.payload[2]])
}

/// Extracts a usable unicast MAC from an `OP_ATTACHED` reply.
pub fn attached_mac(message: &ipc::Message) -> Option<[u8; 6]> {
    let mut mac = [0_u8; 6];
    mac.copy_from_slice(&message.payload[1..7]);
    if mac == [0_u8; 6] || mac == [0xFF_u8; 6] || mac[0] & 1 != 0 {
        None
    } else {
        Some(mac)
    }
}

/// Client -> server: asks for the shared data window.
pub fn share_request() -> ipc::Message {
    ipc::Message::new(0, &[OP_SHARE])
}

/// Server -> client: the shared data window address and size.
pub fn share_reply(paddr: u64, size: u64) -> ipc::Message {
    let mut payload = [0_u8; 13];
    payload[0] = OP_SHARE_REPLY;
    payload[1..9].copy_from_slice(&paddr.to_le_bytes());
    payload[9..13].copy_from_slice(&(size as u32).to_le_bytes());
    ipc::Message::new(0, &payload)
}

/// Decodes [`share_reply`]. Returns `(paddr, size)`.
pub fn share_fields(message: &ipc::Message) -> Option<(u64, u64)> {
    if message.payload[0] != OP_SHARE_REPLY {
        return None;
    }
    let mut address = [0_u8; 8];
    address.copy_from_slice(&message.payload[1..9]);
    let mut size = [0_u8; 4];
    size.copy_from_slice(&message.payload[9..13]);
    let paddr = u64::from_le_bytes(address);
    let size = u64::from(u32::from_le_bytes(size));
    if paddr == 0 || size == 0 {
        None
    } else {
        Some((paddr, size))
    }
}

/// Client -> server: resolve the name in the request slot.
pub fn dns_request() -> ipc::Message {
    ipc::Message::new(0, &[OP_DNS])
}

/// Server -> client: DNS outcome plus the first IPv4 result.
pub fn dns_reply(code: u8, address: [u8; 4]) -> ipc::Message {
    let mut payload = [0_u8; 6];
    payload[0] = OP_DNS_REPLY;
    payload[1] = code;
    payload[2..6].copy_from_slice(&address);
    ipc::Message::new(0, &payload)
}

/// Decodes [`dns_reply`]. The address is meaningful only when `code` is
/// [`NET_OK`].
pub fn dns_fields(message: &ipc::Message) -> Option<(u8, [u8; 4])> {
    if message.payload[0] != OP_DNS_REPLY {
        return None;
    }
    Some((
        message.payload[1],
        [message.payload[2], message.payload[3], message.payload[4], message.payload[5]],
    ))
}

/// Client -> server: fetch the URL in the request slot.
pub fn http_request() -> ipc::Message {
    ipc::Message::new(0, &[OP_HTTP])
}

/// Server -> client: fetch outcome, HTTP status and body length.
pub fn http_reply(code: u8, status: u16, body_len: u32) -> ipc::Message {
    let mut payload = [0_u8; 8];
    payload[0] = OP_HTTP_REPLY;
    payload[1] = code;
    payload[2..4].copy_from_slice(&status.to_le_bytes());
    payload[4..8].copy_from_slice(&body_len.to_le_bytes());
    ipc::Message::new(0, &payload)
}

/// Decodes [`http_reply`] into `(code, status, body_len)`.
pub fn http_fields(message: &ipc::Message) -> Option<(u8, u16, u32)> {
    if message.payload[0] != OP_HTTP_REPLY {
        return None;
    }
    Some((
        message.payload[1],
        u16::from_le_bytes([message.payload[2], message.payload[3]]),
        u32::from_le_bytes([
            message.payload[4],
            message.payload[5],
            message.payload[6],
            message.payload[7],
        ]),
    ))
}

/// Client -> server: ask for the current IPv4 configuration.
pub fn ipconfig_request() -> ipc::Message {
    ipc::Message::new(0, &[OP_IPCONFIG])
}

/// Server -> client: current IPv4 configuration.
pub fn ipconfig_reply(
    configured: bool,
    address: [u8; 4],
    prefix: u8,
    gateway: [u8; 4],
    dns: [u8; 4],
) -> ipc::Message {
    let mut payload = [0_u8; 15];
    payload[0] = OP_IPCONFIG_REPLY;
    payload[1] = u8::from(configured);
    payload[2..6].copy_from_slice(&address);
    payload[6] = prefix;
    payload[7..11].copy_from_slice(&gateway);
    payload[11..15].copy_from_slice(&dns);
    ipc::Message::new(0, &payload)
}

/// Decodes [`ipconfig_reply`].
pub fn ipconfig_fields(message: &ipc::Message) -> Option<(bool, [u8; 4], u8, [u8; 4], [u8; 4])> {
    if message.payload[0] != OP_IPCONFIG_REPLY {
        return None;
    }
    Some((
        message.payload[1] == 1,
        [message.payload[2], message.payload[3], message.payload[4], message.payload[5]],
        message.payload[6],
        [message.payload[7], message.payload[8], message.payload[9], message.payload[10]],
        [message.payload[11], message.payload[12], message.payload[13], message.payload[14]],
    ))
}

/// Orders payload writes before a published length word (and the matching
/// reads after observing it). Normal Non-Cacheable memory is still Normal
/// memory, so ARM permits this reordering without an explicit barrier.
pub fn fence() {
    unsafe { core::arch::asm!("dmb ish", options(nostack, preserves_flags)) };
}
