//! Framebuffer status panel for the UART-less Rain bring-up.
//!
//! Once the timer path works, the raw hex dump is no longer the point: this
//! module paints a readable, live panel on `cont_splash` showing that GICv3,
//! the Generic Timer, the scheduler and the Howl task are running. It is an
//! opt-in diagnostic (`fog-ui`), never part of a normal image.
//!
//! Everything here is presentation only: it reads `timer::ticks()`, `CNTPCT`
//! and the GIC availability flag, and never writes a device register.

use crate::memory::phys_to_virt;

const FB_BASE: usize = 0x5C00_0000;
const FB_WIDTH: usize = 720;
const FB_HEIGHT: usize = 1650;
const FB_STRIDE: usize = 720;

const BG_TOP: (u32, u32, u32) = (10, 16, 30);
const BG_BOTTOM: (u32, u32, u32) = (3, 5, 10);
const PANEL: u32 = 0xFF11_1826;
const PANEL_EDGE: u32 = 0xFF22_3048;
const ACCENT: u32 = 0xFF39_D2FF;
const ACCENT_DIM: u32 = 0xFF1B_6E88;
const TEXT: u32 = 0xFFE9_F0FA;
const MUTED: u32 = 0xFF8B_9AB1;
const GREEN: u32 = 0xFF3D_DC97;
const AMBER: u32 = 0xFFFF_C857;

/// One frame per 200 ms; fast enough to look alive, slow enough not to burn
/// the idle slot.
const FRAME_MS: u64 = 200;

pub fn run() -> ! {
    loop {
        render();
        delay_ms(FRAME_MS);
    }
}

fn render() {
    background();
    header();
    panel();
    pulse();
    footer();
}

// --- panels ---------------------------------------------------------------

fn background() {
    for y in 0..FB_HEIGHT {
        let t = y as u32;
        let r = lerp(BG_TOP.0, BG_BOTTOM.0, t);
        let g = lerp(BG_TOP.1, BG_BOTTOM.1, t);
        let b = lerp(BG_TOP.2, BG_BOTTOM.2, t);
        let color = argb(r, g, b);
        fill_rect(0, y, FB_WIDTH, 1, color);
    }
    // A quiet top glow so the panel does not look like a flat test pattern.
    for step in 0..10u32 {
        let alpha_color = blend(ACCENT, 0xFF00_0000, 92 - step * 9);
        fill_rect(0, (step as usize) * 2, FB_WIDTH, 2, alpha_color);
    }
}

fn lerp(from: u32, to: u32, y: u32) -> u32 {
    if from >= to {
        from - (from - to) * y / FB_HEIGHT as u32
    } else {
        from + (to - from) * y / FB_HEIGHT as u32
    }
}

fn header() {
    let title = b"SILVER";
    let scale = 7;
    let width = text_width(title, scale);
    draw_text(
        (FB_WIDTH - width) / 2,
        110,
        title,
        scale,
        0xFFF4_F8FF,
    );

    let tagline = b"MICROKERNEL";
    let scale = 2;
    let width = text_width(tagline, scale);
    draw_text((FB_WIDTH - width) / 2, 186, tagline, scale, ACCENT);

    // Divider under the wordmark.
    fill_rect(80, 236, 560, 1, PANEL_EDGE);
    fill_rect(80, 236, 140, 2, ACCENT);

    let board = b"KHAJE  /  SM6225  /  REDMI 10C";
    let scale = 2;
    let width = text_width(board, scale);
    draw_text((FB_WIDTH - width) / 2, 268, board, scale, MUTED);
}

fn panel() {
    let x = 56;
    let y = 336;
    let w = 608;
    let h = 812;
    fill_rect(x, y, w, h, PANEL);
    outline(x, y, w, h, PANEL_EDGE);
    fill_rect(x, y, 6, h, ACCENT);

    let ticks = crate::timer::ticks();
    let gic_ok = crate::gicv3::timer_available();
    let timer_ok = ticks > 0;

    let rows: [(&[u8], Status); 6] = [
        (b"GICV3", Status::Text(if gic_ok { b"ONLINE" as &[u8] } else { b"OFFLINE" }, gic_ok)),
        (b"TIMER", Status::Text(if timer_ok { b"ONLINE" as &[u8] } else { b"WAITING" }, timer_ok)),
        (b"TICKS", Status::Number(ticks)),
        (b"UPTIME", Status::Uptime),
        (b"SCHED", Status::Text(b"RUNNING", true)),
        (b"SHELL", Status::Text(b"HOWL>", true)),
    ];

    let mut row_y = 384;
    for (index, (label, value)) in rows.iter().enumerate() {
        if index > 0 {
            fill_rect(96, row_y - 26, 528, 1, 0xFF1A_2438);
        }
        draw_text(104, row_y, label, 3, MUTED);
        draw_value(568, row_y, value, index == 5);
        row_y += 126;
    }
}

enum Status {
    Text(&'static [u8], bool),
    Number(u64),
    Uptime,
}

fn draw_value(right: usize, y: usize, value: &Status, cursor: bool) {
    let mut buffer = [0u8; 24];
    let text: &[u8] = match value {
        Status::Text(text, _) => text,
        Status::Number(number) => format_u64(*number, &mut buffer),
        Status::Uptime => {
            let seconds = uptime_seconds();
            let digits = format_u64(seconds, &mut buffer);
            // Append " S" in the same buffer.
            let mut out = [0u8; 24];
            let mut n = 0;
            for &byte in digits {
                out[n] = byte;
                n += 1;
            }
            out[n] = b' ';
            out[n + 1] = b'S';
            n += 2;
            draw_text(right - text_width(&out[..n], 3), y, &out[..n], 3, TEXT);
            return;
        }
    };

    let color = match value {
        Status::Text(_, true) => GREEN,
        Status::Text(_, false) => AMBER,
        _ => TEXT,
    };
    let width = text_width(text, 3);
    let x = right - width;
    draw_text(x, y, text, 3, color);

    if cursor {
        // Blinking block after the prompt, driven by the counter, not a timer.
        let phase = (read_cntpct() / 20_000_000) & 1;
        if phase == 0 {
            fill_rect(x + width + 8, y + 2, 14, 24, ACCENT);
        }
    }
}

fn pulse() {
    const X: usize = 104;
    const Y: usize = 1210;
    const WIDTH: usize = 512;
    const BARS: usize = 48;
    const PATTERN: [u8; 16] = [6, 12, 20, 30, 18, 10, 26, 34, 16, 8, 22, 30, 14, 6, 18, 28];

    let frame = (crate::timer::ticks() / 2) as usize;
    let bar_w = WIDTH / BARS;
    for bar in 0..BARS {
        let level = PATTERN[(bar + frame) % PATTERN.len()] as usize;
        let height = 8 + level * 2;
        let color = if bar % 3 == 0 { ACCENT } else { ACCENT_DIM };
        fill_rect(
            X + bar * bar_w,
            Y + 72 - height,
            bar_w.saturating_sub(2),
            height,
            color,
        );
    }
    fill_rect(X, Y + 76, WIDTH, 1, PANEL_EDGE);

    let caption = b"PREEMPTION  /  GENERIC TIMER  /  GICV3";
    let width = text_width(caption, 2);
    draw_text((FB_WIDTH - width) / 2, Y + 96, caption, 2, MUTED);
}

fn footer() {
    let text = b"SILVER  /  AARCH64  /  RAM-ONLY BOOT";
    let scale = 2;
    let width = text_width(text, scale);
    draw_text((FB_WIDTH - width) / 2, 1568, text, scale, 0xFF54_647C);
}

// --- small helpers --------------------------------------------------------

fn uptime_seconds() -> u64 {
    let frequency = read_cntfrq();
    if frequency == 0 {
        return 0;
    }
    read_cntpct() / frequency
}

fn read_cntpct() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

fn read_cntfrq() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

fn delay_ms(milliseconds: u64) {
    let frequency = read_cntfrq();
    if frequency == 0 {
        return;
    }
    let target = frequency * milliseconds / 1000;
    let start = read_cntpct();
    while read_cntpct().wrapping_sub(start) < target {
        core::hint::spin_loop();
    }
}

fn format_u64(mut value: u64, buffer: &mut [u8; 24]) -> &[u8] {
    let mut digits = [0u8; 20];
    let mut count = 0;
    if value == 0 {
        digits[0] = b'0';
        count = 1;
    } else {
        while value > 0 && count < digits.len() {
            digits[count] = b'0' + (value % 10) as u8;
            value /= 10;
            count += 1;
        }
    }
    for index in 0..count {
        buffer[index] = digits[count - 1 - index];
    }
    &buffer[..count]
}

fn text_width(text: &[u8], scale: usize) -> usize {
    text.len() * 6 * scale
}

fn draw_text(x: usize, y: usize, text: &[u8], scale: usize, color: u32) {
    let mut cursor = x;
    for &character in text {
        draw_glyph(cursor, y, character, scale, color);
        cursor += 6 * scale;
    }
}

fn draw_glyph(x: usize, y: usize, character: u8, scale: usize, color: u32) {
    let glyph = glyph(character);
    for (row, bits) in glyph.iter().enumerate() {
        for column in 0..5 {
            if bits & (0x10 >> column) != 0 {
                fill_rect(
                    x + column * scale,
                    y + row * scale,
                    scale,
                    scale,
                    color,
                );
            }
        }
    }
}

fn outline(x: usize, y: usize, w: usize, h: usize, color: u32) {
    fill_rect(x, y, w, 2, color);
    fill_rect(x, y + h - 2, w, 2, color);
    fill_rect(x, y, 2, h, color);
    fill_rect(x + w - 2, y, 2, h, color);
}

fn fill_rect(x: usize, y: usize, w: usize, h: usize, color: u32) {
    if x >= FB_WIDTH || y >= FB_HEIGHT {
        return;
    }
    let base = phys_to_virt(FB_BASE) as *mut u32;
    let width = w.min(FB_WIDTH - x);
    for row in 0..h {
        if y + row >= FB_HEIGHT {
            break;
        }
        let line = unsafe { base.add((y + row) * FB_STRIDE + x) };
        for column in 0..width {
            unsafe { line.add(column).write_volatile(color) };
        }
    }
}

fn argb(r: u32, g: u32, b: u32) -> u32 {
    0xFF00_0000 | ((r & 0xFF) << 16) | ((g & 0xFF) << 8) | (b & 0xFF)
}

/// Simple alpha blend of `color` over `under`, `alpha` in 0..=255.
fn blend(color: u32, under: u32, alpha: u32) -> u32 {
    let a = alpha.min(255);
    let cr = (color >> 16) & 0xFF;
    let cg = (color >> 8) & 0xFF;
    let cb = color & 0xFF;
    let ur = (under >> 16) & 0xFF;
    let ug = (under >> 8) & 0xFF;
    let ub = under & 0xFF;
    let r = (cr * a + ur * (255 - a)) / 255;
    let g = (cg * a + ug * (255 - a)) / 255;
    let b = (cb * a + ub * (255 - a)) / 255;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// 5x7 uppercase font. Each byte is one row, bits [4:0] left to right.
fn glyph(character: u8) -> [u8; 7] {
    match character {
        b'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        b'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        b'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        b'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        b'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        b'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        b'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        b'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        b'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        b'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        b'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        b'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        b'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        b'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        b'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        b'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        b'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        b'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        b'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        b'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        b'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        b'0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        b'1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        b'3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        b'4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        b'5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        b'6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        b'7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        b'8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        b'9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        b' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        b':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        b'.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        b'/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        b'>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        b'-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        _ => [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F],
    }
}
