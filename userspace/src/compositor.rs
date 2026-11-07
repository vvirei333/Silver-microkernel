//! Shared compositor client helpers.
//!
//! Every UI process (SilverHome and the built-in apps) draws with these
//! messages instead of touching the framebuffer. The compositor owns the
//! pixels and forwards pointer samples back as `GUI_POINTER`.

use crate::{ipc, syscall};

pub const GUI_CLEAR: u8 = 0x10;
pub const GUI_RECT: u8 = 0x11;
pub const GUI_TEXT: u8 = 0x12;
pub const GUI_POINTER: u8 = 0x13;
pub const GUI_TEXT_SCALED: u8 = 0x14;
/// Host -> compositor: hand the screen to a task id (0 = back to the host).
pub const GUI_HANDOFF: u8 = 0x15;
/// Begin an offscreen frame; draw ops are buffered until PRESENT.
pub const GUI_SURFACE_BEGIN: u8 = 0x16;
pub const GUI_SURFACE_PRESENT: u8 = 0x17;
pub const GUI_SURFACE_ABORT: u8 = 0x18;

pub const COLOR_BLACK: u32 = 0xFF00_0000;
pub const COLOR_WHITE: u32 = 0xFFFF_FFFF;

/// Resolves the compositor, retrying while the driver boots.
pub fn connect() -> Option<u64> {
    for _ in 0..50 {
        if let Some(id) = syscall::lookup("gpu_driver") {
            return Some(id);
        }
        syscall::sleep(100);
    }
    None
}

/// Transfers a task id to the compositor as a handoff target.
pub fn handoff(gpu: u64, task: u64) {
    let mut payload = [0_u8; 9];
    payload[0] = GUI_HANDOFF;
    payload[1..9].copy_from_slice(&task.to_le_bytes());
    send(gpu, &payload);
}

fn send(server: u64, payload: &[u8]) {
    let mut data = [0_u8; ipc::PAYLOAD_SIZE];
    let len = payload.len().min(ipc::PAYLOAD_SIZE);
    data[..len].copy_from_slice(&payload[..len]);
    let _ = syscall::try_send(server, &ipc::Message::new(0, &data[..len]));
}

/// Starts an offscreen frame with a solid background.
pub fn surface_begin(gpu: u64, color: u32) {
    let mut payload = [0_u8; 5];
    payload[0] = GUI_SURFACE_BEGIN;
    payload[1..5].copy_from_slice(&color.to_le_bytes());
    send(gpu, &payload);
}

/// Publishes the buffered frame atomically.
pub fn surface_present(gpu: u64) {
    send(gpu, &[GUI_SURFACE_PRESENT]);
}

/// Drops the buffered frame without publishing it.
pub fn surface_abort(gpu: u64) {
    send(gpu, &[GUI_SURFACE_ABORT]);
}

pub fn clear(gpu: u64, color: u32) {
    let mut payload = [0_u8; 5];
    payload[0] = GUI_CLEAR;
    payload[1..5].copy_from_slice(&color.to_le_bytes());
    send(gpu, &payload);
}

pub fn rect(gpu: u64, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let mut payload = [0_u8; 13];
    payload[0] = GUI_RECT;
    payload[1..3].copy_from_slice(&(x as u16).to_le_bytes());
    payload[3..5].copy_from_slice(&(y as u16).to_le_bytes());
    payload[5..7].copy_from_slice(&(w as u16).to_le_bytes());
    payload[7..9].copy_from_slice(&(h as u16).to_le_bytes());
    payload[9..13].copy_from_slice(&color.to_le_bytes());
    send(gpu, &payload);
}

pub fn text(gpu: u64, x: u32, y: u32, color: u32, value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(ipc::PAYLOAD_SIZE - 10);
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = GUI_TEXT;
    payload[1..3].copy_from_slice(&(x as u16).to_le_bytes());
    payload[3..5].copy_from_slice(&(y as u16).to_le_bytes());
    payload[5..9].copy_from_slice(&color.to_le_bytes());
    payload[9] = len as u8;
    payload[10..10 + len].copy_from_slice(&bytes[..len]);
    send(gpu, &payload);
}

pub fn text_scaled(gpu: u64, x: u32, y: u32, color: u32, scale: u32, value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(ipc::PAYLOAD_SIZE - 11);
    let mut payload = [0_u8; ipc::PAYLOAD_SIZE];
    payload[0] = GUI_TEXT_SCALED;
    payload[1..3].copy_from_slice(&(x as u16).to_le_bytes());
    payload[3..5].copy_from_slice(&(y as u16).to_le_bytes());
    payload[5..9].copy_from_slice(&color.to_le_bytes());
    payload[9] = scale as u8;
    payload[10] = len as u8;
    payload[11..11 + len].copy_from_slice(&bytes[..len]);
    send(gpu, &payload);
}

pub fn text_centered(gpu: u64, center_x: u32, y: u32, color: u32, value: &str) {
    let width = value.len() as u32 * 8;
    text(gpu, center_x.saturating_sub(width / 2), y, color, value);
}

pub fn text_centered_scaled(
    gpu: u64,
    center_x: u32,
    y: u32,
    color: u32,
    scale: u32,
    value: &str,
) {
    let width = value.len() as u32 * 8 * scale;
    text_scaled(gpu, center_x.saturating_sub(width / 2), y, color, scale, value);
}

pub fn shade(color: u32) -> u32 {
    let r = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    let scale = |value: u32| (value * 3) / 4;
    0xFF00_0000 | (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

/// Standard app header: dark bar, a "< Home" button and the app title.
pub fn header(gpu: u64, width: u32, title: &str) {
    rect(gpu, 0, 0, width, 64, 0xFF11_1116);
    rect(gpu, 12, 12, 132, 40, 0xFF2C_2C_32);
    text_centered(gpu, 78, 24, COLOR_WHITE, "< Home");
    text_centered_scaled(gpu, width / 2, 18, COLOR_WHITE, 2, title);
}

/// The header's back button (also the home gesture).
pub fn back_hit(x: u32, y: u32) -> bool {
    x < 160 && y < 64
}
