//! Input-event IPC shared by `input_driver` and `gpu_driver`.
//!
//! One pointer sample is small enough for the 32-byte [`ipc::Message`], so it
//! travels directly over IPC (unlike the frame mailbox in `net_ipc`). The
//! driver sends a sample at every `EV_SYN` report boundary; the graphics
//! driver moves the cursor and processes a button press/release per sample.

use crate::ipc;

/// Message opcode: one absolute pointer sample.
pub const OP_POINTER: u8 = 1;

/// One absolute pointer sample, sent at each `EV_SYN` report boundary.
///
/// `x`/`y` are already scaled to the screen: `0..=639` and `0..=479` for the
/// 640x480 engineer menu. `left_clicked` is the state of `BTN_LEFT`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputMsg {
    pub x: u16,
    pub y: u16,
    pub left_clicked: bool,
}

/// Encodes a pointer sample into an IPC message.
pub fn pointer(x: u16, y: u16, left_clicked: bool) -> ipc::Message {
    let mut payload = [0_u8; 6];
    payload[0] = OP_POINTER;
    payload[1..3].copy_from_slice(&x.to_le_bytes());
    payload[3..5].copy_from_slice(&y.to_le_bytes());
    payload[5] = u8::from(left_clicked);
    ipc::Message::new(0, &payload)
}

/// Decodes a pointer sample from an IPC message.
pub fn decode(message: &ipc::Message) -> Option<InputMsg> {
    if message.payload[0] != OP_POINTER {
        return None;
    }
    Some(InputMsg {
        x: u16::from_le_bytes([message.payload[1], message.payload[2]]),
        y: u16::from_le_bytes([message.payload[3], message.payload[4]]),
        left_clicked: message.payload[5] != 0,
    })
}
