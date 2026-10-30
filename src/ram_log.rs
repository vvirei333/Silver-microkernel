//! RAM console: mirrors boot milestones into Android's pstore ramoops zone so
//! the log survives a crash reboot and can be read back from Linux.
//!
//! The Y700 has no accessible UART, and this kernel's panic is followed by a
//! Qualcomm crashdump + warm reboot — physical RAM survives both. The Android
//! kernel reserves a 4 MiB ramoops region (`/sys/module/ramoops/parameters`:
//! mem_address=0xA200_0000, ecc=0): console at 0xA200_0000, pmsg at
//! 0xA220_0000. This module writes into the *console* zone with a valid
//! `persistent_ram` header (sig/start/size), so after the reboot Android
//! exposes the text verbatim as `/sys/fs/pstore/pmsg-ramoops-0`.
//!
//! The address is Normal-cacheable DRAM through the kernel's first DRAM L2
//! (0x8000_0000..0xC000_0000), below the frame-allocator region and outside
//! the forbidden ABL chunk, so nothing else ever touches it. Every write is
//! cleaned to the point of coherency: a crash reset must not leave the lines
//! sitting dirty in a cache that is discarded by the reset.
//!
//! QEMU has no such region and would fault on the access, so the log is
//! tablet-only; on the emulator the UART is the debug channel anyway.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::memory::phys_to_virt;

/// The pmsg ramoops zone. The ramoops module params (mem_size=0x400000,
/// console_size=0x200000, pmsg_size=0x200000) define the layout:
/// console 0xA2000000..0xA2200000, pmsg 0xA2200000..0xA2400000.
pub const RAM_LOG_ADDR: usize = 0xA220_0000;
pub const RAM_LOG_SIZE: usize = 0x20_0000;
/// `struct persistent_ram_header`: sig, start, size (u32 LE each).
const HEADER_SIZE: usize = 12;
/// PERSISTENT_RAM_SIG from pstore_ram.c.
const RAMOOPS_SIG: u32 = 0x4347_4239;

static CURSOR: AtomicUsize = AtomicUsize::new(0);

/// QEMU faults on writes to unassigned memory, so the log is tablet-only.
const ENABLED: bool = cfg!(feature = "tablet");

/// Resets the zone header. Must run before the first `log`, and it must run
/// *after* `mmu::init` — the descriptor lives in the first DRAM L2, which the
/// boot stub already maps, but `phys_to_virt` itself relies on the high alias.
pub fn init() {
    if !ENABLED {
        return;
    }
    CURSOR.store(0, Ordering::Relaxed);
    write_header(0);
    log("RAM LOG READY");
}

/// Appends one line: `SILVER <text>\n`. Truncates at the zone end.
pub fn log(text: &str) {
    if !ENABLED {
        return;
    }
    append(b"SILVER ");
    append(text.as_bytes());
    append(b"\n");
}

// ---------------------------------------------------------------------------
// Framebuffer progress: the cont_splash region (reserved-memory/splash_region,
// 0xD5100000, 1600x2560 XRGB32) is live panel scanout — the firmware draws the
// boot logo there and GRUB rendered its menu through it. Painting colored bars
// after every milestone makes the boot progress visible with zero log
// infrastructure: white=entry, green=MMU, yellow=frames, cyan=heap,
// magenta=GIC, blue=VFS, full green=scheduler, full red=exception.
// ---------------------------------------------------------------------------

const FB_PHYS: usize = 0xD510_0000;
const FB_WIDTH: usize = 1600;
const FB_STRIDE: usize = FB_WIDTH * 4;
const BAR_HEIGHT: usize = 24;
const BAR_BASE_Y: usize = 40;

static BAR_INDEX: AtomicUsize = AtomicUsize::new(0);

fn fb_color(r: u32, g: u32, b: u32) -> u32 {
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// Paints one progress bar and leaves the framebuffer base for the next.
pub fn fb_progress() {
    if !ENABLED {
        return;
    }
    let colors = [
        fb_color(255, 255, 255), // entry
        fb_color(0, 255, 0),     // MMU
        fb_color(255, 255, 0),   // frames
        fb_color(0, 255, 255),   // heap
        fb_color(255, 0, 255),   // GIC
        fb_color(80, 80, 255),   // VFS
        fb_color(0, 160, 0),     // scheduler
    ];
    let index = BAR_INDEX.fetch_add(1, Ordering::Relaxed);
    if index >= colors.len() {
        return;
    }
    fb_rect(0, BAR_BASE_Y + index * BAR_HEIGHT, FB_WIDTH, BAR_HEIGHT, colors[index]);
}

/// Fills the whole screen — red on a kernel panic, green after scheduler start.
pub fn fb_fill(r: u32, g: u32, b: u32) {
    if !ENABLED {
        return;
    }
    fb_rect(0, 0, FB_WIDTH, 2560, fb_color(r, g, b));
}

fn fb_rect(x: usize, y: usize, w: usize, h: usize, color: u32) {
    let base = phys_to_virt(FB_PHYS) as *mut u32;
    for row in 0..h {
        let line = y + row;
        if line >= 2560 {
            break;
        }
        unsafe {
            let row_ptr = base.add(line * FB_STRIDE / 4 + x);
            for col in 0..w {
                row_ptr.add(col).write_volatile(color);
            }
        }
    }
    clean_fb(x, y, w, h);
}

/// Pushes the drawn rect to the point of coherency (panel DMA reads DRAM).
fn clean_fb(x: usize, y: usize, w: usize, h: usize) {
    let start = phys_to_virt(FB_PHYS + y * FB_STRIDE + x * 4) & !63;
    let end = phys_to_virt(FB_PHYS + (y + h) * FB_STRIDE + (x + w) * 4 + 63) & !63;
    unsafe {
        let mut line = start;
        while line < end {
            core::arch::asm!(
                "dc cvac, {line}",
                line = in(reg) line,
                options(nostack, preserves_flags),
            );
            line += 64;
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Appends text verbatim (no prefix, no newline) — for building one log line
/// from pieces in exception context, where `alloc` is not available.
pub fn log_raw(text: &str) {
    if !ENABLED {
        return;
    }
    append(text.as_bytes());
}

/// Appends the value as 16 uppercase hex digits, verbatim.
pub fn log_hex(value: u64) {
    if !ENABLED {
        return;
    }
    let mut digits = [0_u8; 16];
    for (index, digit) in digits.iter_mut().rev().enumerate() {
        let nibble = ((value >> (index * 4)) & 0xF) as u8;
        *digit = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
    }
    append(&digits);
}

fn append(bytes: &[u8]) {
    let mut line = [0_u8; 64];
    let mut length = 0;
    for byte in bytes {
        if length >= line.len() - 1 || *byte == 0 {
            break;
        }
        line[length] = *byte;
        length += 1;
    }

    let cursor = CURSOR.load(Ordering::Relaxed);
    if cursor + length > RAM_LOG_SIZE - HEADER_SIZE {
        return;
    }
    unsafe {
        let base = phys_to_virt(RAM_LOG_ADDR + HEADER_SIZE + cursor) as *mut u8;
        for (offset, byte) in line[..length].iter().enumerate() {
            base.add(offset).write_volatile(*byte);
        }
    }
    CURSOR.store(cursor + length, Ordering::Relaxed);
    write_header(cursor + length);
}

/// Writes sig/start/size and pushes the zone to the point of coherency: the
/// header and the text must reach DRAM before any crash reset.
fn write_header(size: usize) {
    let header = phys_to_virt(RAM_LOG_ADDR) as *mut u32;
    unsafe {
        header.write_volatile(RAMOOPS_SIG.to_le()); // sig
        header.add(1).write_volatile(0); // start
        header.add(2).write_volatile(size as u32); // size
    }
    clean_zone_to_poc(HEADER_SIZE + size);
}

/// `dc cvac` up to the point of coherency for the touched range.
fn clean_zone_to_poc(bytes: usize) {
    let start = phys_to_virt(RAM_LOG_ADDR) & !63;
    let end = (phys_to_virt(RAM_LOG_ADDR) + bytes + 63) & !63;
    unsafe {
        let mut line = start;
        while line < end {
            core::arch::asm!(
                "dc cvac, {line}",
                line = in(reg) line,
                options(nostack, preserves_flags),
            );
            line += 64;
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}
