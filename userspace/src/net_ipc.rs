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

/// Orders payload writes before a published length word (and the matching
/// reads after observing it). Normal Non-Cacheable memory is still Normal
/// memory, so ARM permits this reordering without an explicit barrier.
pub fn fence() {
    unsafe { core::arch::asm!("dmb ish", options(nostack, preserves_flags)) };
}
