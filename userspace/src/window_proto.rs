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
/// The guest sets this when it wants the host window shown (on `wolfser`).
pub const FIELD_WINDOW_REQUEST: u64 = 0x2C;

// Host web proxy (HTTPS and anything the guest's smoltcp stack cannot speak).
// The guest publishes a URL in [`WEB_REQUEST`] and bumps `WEB_REQUEST_SEQ`;
// the host fetches it with Qt and publishes the result, bumping
// `WEB_DONE_SEQ` last.
pub const FIELD_WEB_REQUEST_SEQ: u64 = 0x30;
pub const FIELD_WEB_REQUEST_LEN: u64 = 0x34;
pub const FIELD_WEB_DONE_SEQ: u64 = 0x38;
pub const FIELD_WEB_STATUS: u64 = 0x3C;
pub const FIELD_WEB_BODY_LEN: u64 = 0x40;
/// 0 on success, otherwise the host's error class (see `WEB_ERROR_*`).
pub const FIELD_WEB_ERROR: u64 = 0x44;
pub const FIELD_WEB_FINAL_LEN: u64 = 0x48;
/// Set to 1 by a viewer that implements the proxy, so the guest can explain
/// an old viewer instead of waiting for a request that will never finish.
pub const FIELD_WEB_CAPABLE: u64 = 0x4C;
/// 1 while the pointer is over a link; the viewer shows a hand cursor.
pub const FIELD_CURSOR: u64 = 0x50;

pub const FORMAT_RGBA8888: u32 = 1;

pub const STATE_BLANK: u32 = 0;
pub const STATE_READY: u32 = 1;
pub const STATE_LOADING: u32 = 2;
pub const STATE_ERROR: u32 = 3;

// Click `value`: 1 = left, 2 = middle, 3 = Ctrl+left (open in a new tab).
// Move `value`: 0 = pointer position, 1 = pointer left the window.

// Input ring: 64 eight-byte entries. The host writes at `head`, the guest
// consumes at `tail`; both advance modulo `INPUT_SLOTS`.
pub const INPUT_RING: u64 = 0x0100;
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
pub const KEY_BACK: u16 = 14; // Alt+Left
pub const KEY_FORWARD: u16 = 15; // Alt+Right
pub const KEY_RELOAD: u16 = 16; // F5 / Ctrl+R
pub const KEY_QUIT: u16 = 17; // Ctrl+Q
pub const KEY_NEW_TAB: u16 = 18; // Ctrl+T
pub const KEY_CLOSE_TAB: u16 = 19; // Ctrl+W
pub const KEY_NEXT_TAB: u16 = 20; // Ctrl+Tab

pub const INPUT_KEY: u8 = 1;
pub const INPUT_SCROLL: u8 = 2;
pub const INPUT_CLICK: u8 = 3;
pub const INPUT_MOVE: u8 = 4;

// Web-proxy areas. They live after the pixel surface (which ends at
// `PIXELS` + 900*640*4); QEMU's ivshmem is 4 MiB.
pub const WEB_REQUEST: u64 = 0x24_0000;
pub const WEB_REQUEST_MAX: usize = 1024;
pub const WEB_FINAL_URL: u64 = 0x24_0800;
pub const WEB_FINAL_URL_MAX: usize = 1024;
pub const WEB_BODY: u64 = 0x24_1000;
pub const WEB_BODY_MAX: usize = 0x40_0000 - WEB_BODY as usize;

/// Host proxy error classes carried in `FIELD_WEB_ERROR`.
pub const WEB_ERROR_NONE: u32 = 0;
pub const WEB_ERROR_NETWORK: u32 = 1;
pub const WEB_ERROR_TOO_LARGE: u32 = 2;
pub const WEB_ERROR_UNSUPPORTED: u32 = 3;

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
