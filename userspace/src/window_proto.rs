//! Shared layout of the Wolfser window.
//!
//! The guest draws one RGBA surface and publishes it through an ivshmem BAR
//! that the host maps from the same file. The header carries the geometry,
//! a frame counter and a small input ring; pixels start at [`PIXELS`].

pub const MAGIC: u32 = 0x5749_4E31; // "WIN1"
pub const VERSION: u32 = 1;

pub const HEADER_SIZE: u64 = 0x1000;
pub const PIXELS: u64 = 0x1000;

// Header fields (all little-endian u32).
pub const FIELD_MAGIC: u64 = 0x00;
pub const FIELD_VERSION: u64 = 0x04;
pub const FIELD_WIDTH: u64 = 0x08;
pub const FIELD_HEIGHT: u64 = 0x0C;
pub const FIELD_STRIDE: u64 = 0x10;
pub const FIELD_FORMAT: u64 = 0x14;
pub const FIELD_FRAME_SEQ: u64 = 0x18;
pub const FIELD_STATE: u64 = 0x1C;
pub const FIELD_INPUT_HEAD: u64 = 0x20;
pub const FIELD_INPUT_TAIL: u64 = 0x24;
pub const FIELD_SCROLL: u64 = 0x28;

pub const FORMAT_RGBA8888: u32 = 1;

pub const STATE_BLANK: u32 = 0;
pub const STATE_READY: u32 = 1;
pub const STATE_LOADING: u32 = 2;
pub const STATE_ERROR: u32 = 3;

// Input ring: 64 eight-byte entries. The host writes at `head`, the guest
// consumes at `tail`; both advance modulo `INPUT_SLOTS`.
pub const INPUT_RING: u64 = 0x0040;
pub const INPUT_SLOTS: u32 = 64;
pub const INPUT_ENTRY: u64 = 8;

// Key codes the host sends. Printable keys carry their ASCII/Unicode value.
pub const KEY_UP: u16 = 1;
pub const KEY_DOWN: u16 = 2;
pub const KEY_PAGE_UP: u16 = 3;
pub const KEY_PAGE_DOWN: u16 = 4;
pub const KEY_HOME: u16 = 5;
pub const KEY_END: u16 = 6;
pub const KEY_ENTER: u16 = 10;
pub const KEY_BACKSPACE: u16 = 11;
pub const KEY_ESCAPE: u16 = 12;
pub const KEY_FOCUS: u16 = 13; // Ctrl+L

pub const INPUT_KEY: u8 = 1;
pub const INPUT_SCROLL: u8 = 2;
pub const INPUT_CLICK: u8 = 3;

pub fn read_u32(base: u64, field: u64) -> u32 {
    unsafe { core::ptr::read_volatile((base + field) as *const u32) }
}

pub fn write_u32(base: u64, field: u64, value: u32) {
    unsafe { core::ptr::write_volatile((base + field) as *mut u32, value) };
}

/// One input entry as the guest reads it.
#[derive(Copy, Clone)]
pub struct InputEvent {
    pub kind: u8,
    pub value: u8,
    pub key: u16,
    pub x: u16,
    pub y: u16,
}

pub fn read_input(base: u64, index: u32) -> InputEvent {
    let address = base + INPUT_RING + u64::from(index % INPUT_SLOTS) * INPUT_ENTRY;
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, 8) };
    InputEvent {
        kind: bytes[0],
        value: bytes[1],
        key: u16::from_le_bytes([bytes[2], bytes[3]]),
        x: u16::from_le_bytes([bytes[4], bytes[5]]),
        y: u16::from_le_bytes([bytes[6], bytes[7]]),
    }
}
