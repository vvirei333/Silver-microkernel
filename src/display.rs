//! Platform display registry.
//!
//! QEMU's compositor owns `ramfb` through `fw_cfg`, so the kernel is not
//! involved. A phone bootloader, on the other hand, has already configured the
//! panel and left a scanout buffer in reserved memory (`cont_splash` on the
//! Redmi 10C). This module records that buffer, validates it against the live
//! memory map, and exposes it to a userspace compositor through
//! `SYS_DISPLAY_INFO` / `SYS_DISPLAY_MAP`.
//!
//! Registration is conditional (`fog-display`), so normal fog/QEMU builds keep
//! no platform framebuffer and the display syscalls report "unavailable".

use crate::memory::DRAM;
use crate::smp::SpinLock;

/// The Redmi 10C panel geometry. This is a configured value: the live FDT
/// describes the `cont_splash` carve-out, not the panel mode. A port for a
/// different panel must change these and the matching `fog` probe check.
pub const FOG_PANEL_WIDTH: u32 = 720;
pub const FOG_PANEL_HEIGHT: u32 = 1650;
pub const FOG_PANEL_STRIDE: u32 = 720;
pub const BYTES_PER_PIXEL: u32 = 4;

/// A validated, currently-scanned-out platform framebuffer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DisplayInfo {
    pub base: usize,
    pub width: u32,
    pub height: u32,
    /// Pixels per row; the buffer is `stride * height` pixels.
    pub stride: u32,
    pub bytes_per_pixel: u32,
}

impl DisplayInfo {
    /// Byte length of the visible buffer (including row padding).
    pub fn byte_len(self) -> usize {
        self.stride as usize * self.height as usize * self.bytes_per_pixel as usize
    }
}

static DISPLAY: SpinLock<Option<DisplayInfo>> = SpinLock::new(None);

/// Registers a display buffer after proving it is inside the mapped DRAM
/// window. The caller (the fog probe) has already checked the live FDT.
pub fn register(info: DisplayInfo) -> bool {
    if info.width == 0
        || info.height == 0
        || info.bytes_per_pixel != BYTES_PER_PIXEL
        || info.stride < info.width
    {
        return false;
    }
    let Some(bytes) = info
        .stride
        .checked_mul(info.height)
        .and_then(|pixels| pixels.checked_mul(info.bytes_per_pixel))
    else {
        return false;
    };
    let bytes = bytes as usize;
    let Some(end) = info.base.checked_add(bytes) else {
        return false;
    };
    if info.base < DRAM.start || end > DRAM.end || info.base % 4 != 0 {
        return false;
    }
    *DISPLAY.lock() = Some(info);
    true
}

pub fn info() -> Option<DisplayInfo> {
    *DISPLAY.lock()
}
